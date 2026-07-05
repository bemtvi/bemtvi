//! Mouse input: hit-testing a global screen cell back to a window and buffer
//! position, and the gesture handlers.
//!
//! The editor owns the whole pipeline (matching neovim's single-grid model and
//! nxvim's "the core owns *which* cells" split — see `docs/architecture.md`): a
//! [`MouseEvent`] carries a global, zero-based screen cell, and [`Editor::mouse`]
//! resolves it to a window + buffer position itself, so every front end only has
//! to forward the raw cell. The inverse map ([`Editor::hit_test`]) is the exact
//! reverse of the forward layout the [`crate::view`] projection computes — the
//! same chrome offset, window rects, number gutter, horizontal scroll, and
//! tab/wide-char [`virtcol`](crate::unicode::virtcol) math, run backwards.

use super::*;
use crate::input::{MouseAction, MouseButton, MouseEvent, MouseKind, WheelDir};

/// Place a docs sidebar of `content_w` columns beside a popup box whose content
/// starts at `box_col` and is `box_width` wide, within a `bound_w`-column area.
/// Prefers the right of the box, flipping left when that side has more room, and
/// returns `(docs_col, docs_w)` — the float's **content** top-left column and its
/// width, in the bound area's own (region) cells. `None` when neither side fits a
/// readable width, so the caller shows no sidebar rather than a one-column sliver.
fn place_docs_beside(
    box_col: usize,
    box_width: usize,
    content_w: usize,
    bound_w: usize,
) -> Option<(usize, usize)> {
    /// Below this, a sidebar is a useless sliver — better none than a 1-col float.
    const MIN_DOCS_W: usize = 10;
    // Right of the box: its content spans `[box_col, box_col+box_width)`; the box's
    // right border sits at `box_col+box_width` and the docs float's own left border one
    // cell past it → content at `+2`. A trailing 1-col margin keeps it off the bound's
    // right edge. Left of the box: the docs float's right border one cell left of the
    // box's left border, so its content ends at `box_col-3`, starting from the bound's left.
    let right_start = box_col + box_width + 2;
    let right_avail = bound_w
        .saturating_sub(right_start)
        .saturating_sub(1)
        .min(content_w);
    let left_avail = box_col.saturating_sub(3).min(content_w);
    let (docs_col, docs_w) = if right_avail >= left_avail {
        (right_start, right_avail)
    } else {
        (box_col.saturating_sub(2 + left_avail), left_avail)
    };
    // A naturally short doc (already narrower than the minimum) is exempt — it's as
    // wide as it gets, so accept it; otherwise demand a readable width.
    (docs_w >= MIN_DOCS_W.min(content_w)).then_some((docs_col, docs_w))
}

/// In-flight left-button selection: the multi-click counter (vim's
/// `check_multiclick` — a same-cell repeat within `'mousetime'` escalates the
/// selected unit) plus the anchor a drag extends from. One value spans a whole
/// press → drag → release gesture and persists into the gap before the next press
/// so a quick same-cell repeat is counted as a double-/triple-click.
#[derive(Debug, Clone, Copy)]
pub(crate) struct MouseSelect {
    /// Screen cell of the press, to detect a same-cell repeat.
    row: usize,
    col: usize,
    /// Time of the press (ms, server-stamped), for the `'mousetime'` window.
    stamp_ms: u64,
    /// Click count: 1 = char, 2 = word, 3 = line. Capped at 3 — vim's quad-click
    /// blockwise selection awaits a blockwise Visual mode (not yet in nxvim).
    count: u8,
    /// What the drag pivots around: the press point (single click), or the whole
    /// word / line first selected (so a drag extends by whole units).
    anchor: SelectAnchor,
}

/// In-flight separator / status-line drag (Phase 5): a left-press that landed on
/// a divider grabs the edge next to it; subsequent drags resize to follow the
/// pointer. Both variants track the pointer **absolutely**, so pushing past a
/// minimum and dragging back tracks cleanly instead of drifting.
#[derive(Debug, Clone, Copy)]
pub(crate) enum ResizeDrag {
    /// A split divider *inside* a region (a window separator or a status line with
    /// a window below it): the grabbed window edge is resized by the drag delta.
    Window {
        /// The window whose edge is grabbed — the one *left of* a vertical divider
        /// or *above* a horizontal one, so a drag toward it (right / down) grows it.
        win: WindowId,
        /// The divider's orientation: `true` for a vertical separator (resize
        /// width), `false` for a horizontal separator or status line (resize height).
        vertical: bool,
        /// The press cell along the drag axis (the column for a vertical divider,
        /// the row for a horizontal one), the fixed point the drag measures from.
        origin: usize,
        /// Total cells already applied to the resize, so each drag issues only the
        /// remaining delta to reach the pointer's current offset from `origin`.
        applied: isize,
    },
    /// The **edge** of a dock band (the separator between a dock and the main
    /// area): the drag sets that dock's size to the pointer's position directly.
    /// No `origin`/`applied` is needed — the new size is read absolutely from the
    /// pointer each drag, so it self-corrects when the band clamps.
    Dock { side: DockSide },
}

/// A mouse **gesture** the server still has to resolve against the keymaps: the button,
/// its phase ([`kind`](MouseClick::kind) — press / drag / release), multi-click count,
/// active modifiers, and the screen cell, recorded on [`Editor::mouse_clicks`] by the
/// gesture handler. The server turns it into a [`Key`](crate::input::Key) (`<n-LeftMouse>`
/// / `<C-RightMouse>` / `<MiddleMouse>` / `<LeftDrag>` / `<LeftRelease>` / …), fires the
/// bound mapping if there is one, else runs the per-gesture default
/// ([`Editor::mouse_apply_default`]).
///
/// **All three buttons** (`Left`/`Right`/`Middle`), in every phase (press / drag /
/// release), with **modifiers**, are mappable.
/// A plain-left press places the cursor *eagerly* (so a `<LeftMouse>` / `<C-LeftMouse>`
/// map and the default both act on the click), so its default is just the word/line
/// escalation. The right / middle / shift-left presses defer their *whole* default to
/// [`Editor::mouse_apply_default`] (selection-aware cursor placement, the `'mousemodel'`
/// dispatch, the `"*` paste) — which is why the cell (`row`/`col`/`stamp_ms`) rides
/// along: an unclaimed press re-hit-tests from it. A *mapped* right/middle does not
/// move the cursor — the map reads the clicked cell via [`mouse_pos`](Editor::mouse_pos)
/// / `vim.fn.getmousepos()` instead. Right/middle multi-click (`<2-RightMouse>`) is not
/// yet counted (`clicks` is always 1 for them).
#[derive(Debug, Clone, Copy)]
pub struct MouseClick {
    /// The button — `Left`/`Right`/`Middle` (never the wheel/move/thumb).
    pub button: MouseButton,
    /// Which phase of the gesture this is — `Press` (`<LeftMouse>`), `Drag`
    /// (`<LeftDrag>`), or `Release` (`<LeftRelease>`). Part of the looked-up key's
    /// identity, so each is separately mappable.
    pub kind: MouseKind,
    /// The multi-click count (1 = single, 2 = double, 3 = triple), per `'mousetime'`.
    /// Counted for a left *press* only; right/middle and every drag/release are `1`.
    pub clicks: u8,
    /// Active modifiers at the press, so `<C-LeftMouse>` is distinguished from a plain
    /// `<LeftMouse>`. `shift` on a left press is the extend gesture (default), still
    /// mappable as `<S-LeftMouse>`.
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    /// The press's global screen cell and server-stamped time, so a deferred default
    /// (right / middle / shift-left, run only on a keymap miss) can re-hit-test it.
    pub row: usize,
    pub col: usize,
    pub stamp_ms: u64,
}

/// The last mouse event's resolved position — the fields `vim.fn.getmousepos()`
/// surfaces (see [`Editor::mouse_pos`]). All 1-based. `winid` / `line` / `column` are
/// `0` when the last click missed a window's text (a separator, status line, or off the
/// grid), and every field is `0` before the first mouse event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MousePos {
    /// Global screen cell (1-based) — set for every event, even off a window.
    pub screenrow: u64,
    pub screencol: u64,
    /// The window the cell lands in, `0` if none.
    pub winid: u64,
    /// The cell relative to that window's top-left (1-based; includes the gutter).
    pub winrow: u64,
    pub wincol: u64,
    /// The buffer position: 1-based `line`, and `column` the 1-based byte column.
    pub line: u64,
    pub column: u64,
}

/// A scroll-wheel notch the server still has to resolve against the keymaps — the
/// wheel counterpart of [`MouseClick`]. The server turns it into a
/// [`Key`](crate::input::Key) (`<ScrollWheelUp>` / `<S-ScrollWheelDown>` / …), fires the
/// bound mapping if there is one, else runs the default scroll
/// ([`Editor::mouse_apply_wheel_default`]). Carries the cell so the default scrolls the
/// window under the pointer, and `shift` so an unmapped `<S-ScrollWheel*>` still
/// page-scrolls.
#[derive(Debug, Clone, Copy)]
pub struct WheelGesture {
    pub dir: WheelDir,
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub row: usize,
    pub col: usize,
}

/// The anchored extent a left-drag extends from, set by the press by click count.
#[derive(Debug, Clone, Copy)]
enum SelectAnchor {
    /// Single click: the press position. Visual is not entered until the first
    /// drag (vim's `<LeftMouse>` then `<LeftDrag>`).
    Char(Cursor),
    /// Double click: the byte range `[lo, hi)` of the word under the press.
    Word { lo: usize, hi: usize },
    /// Triple click: the (0-based) line the press landed on.
    Line(usize),
}

/// The `'mousemodel'` value, deciding what the right button does (and, by the
/// same token, which gesture is the selection-extend one). Unknown strings fall
/// back to the default, mirroring the permissive `:set` of the sibling mouse
/// string options.
enum MouseModel {
    /// `popup_setpos` (default): right-click moves the cursor (keeping a selection
    /// the click lands inside) and would pop a context menu.
    PopupSetpos,
    /// `popup`: right-click pops a context menu without moving the cursor.
    Popup,
    /// `extend`: right-click extends the selection toward the click.
    Extend,
}

/// Where a screen cell landed once hit-tested. Only the variants the implemented
/// phases act on are produced; the rest of the surface (separators, the tabline,
/// the panel) grows here as later phases wire those regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MouseTarget {
    /// A buffer cell in window `win`: 0-based buffer `line` and byte `col`. A
    /// click in the number gutter resolves to `col = 0` on that line.
    Text {
        win: WindowId,
        line: usize,
        col: usize,
    },
    /// The window's status row (its bottom line), with the **window-relative**
    /// (0-based) column the cell sits at — which is the status line's own column,
    /// so the server can resolve it to a `%@…%X` click region. A status row with a
    /// window below it is grabbed as a resize handle earlier in [`Editor::mouse`],
    /// before the hit-test runs, so this is only ever a real status-line click.
    StatusLine { win: WindowId, col: usize },
    /// The single **global** status bar (`'laststatus'`=3), with the (0-based)
    /// column the cell sits at. The bar spans the full editor width and shows the
    /// focused window's `%`-context, so the server resolves the click against that
    /// window's status line at the full width (not a per-window rect).
    GlobalStatusLine { col: usize },
}

/// Where a global screen cell landed on an open menu overlay (the completion popup,
/// a picker, or a `select`). `None` from [`Editor::menu_hit`] means the cell is off
/// the menu entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuHit {
    /// A selectable list row, as the absolute index into the menu view.
    Item(usize),
    /// The picker's preview pane (a wheel notch here scrolls the preview).
    Preview,
    /// On the box but not a selectable row — a border, the prompt / separator, or a
    /// blank filler past the list end. Consumed (so it doesn't fall through to the
    /// text beneath), but selects nothing.
    Chrome,
}

/// The open menu's resolved screen rectangle (global cells) and the sub-rects its
/// selectable rows and preview pane occupy — the inverse of the box
/// [`Editor::menu_geom`] projects, plus the focused window's screen origin and the
/// client border convention, so a click maps to the row painted there.
/// Window-anchored placements only.
struct MenuScreen {
    /// Outer box `(x, y, w, h)`, including borders.
    box_rect: (usize, usize, usize, usize),
    /// List content `(x, y, w, rows)` — where selectable rows are drawn.
    list: (usize, usize, usize, usize),
    /// The picker's preview pane `(x, y, w, h)`, when it carries one.
    preview: Option<(usize, usize, usize, usize)>,
    /// Scroll offset of the first visible list row.
    start: usize,
    /// Total rows in the menu view (bounds the absolute index).
    total: usize,
    /// The list is painted bottom-up: logical row 0 sits on the *last* visible
    /// line, growing upward. Set for the command-line wildmenu, which floats above
    /// its input so the best match kisses the cursor (every client flips the rows —
    /// TUI `lines.reverse()`, GUI `list_rows - 1 - r`, web `listEls.reverse()`), so
    /// the hit-test must flip the clicked offset to match what was drawn.
    inverted: bool,
}

/// Which `%`-format-rendered chrome row a [`StatuslineClick`] landed on — it tells
/// the server which format to re-run and at what width when resolving the click's
/// column to a [`ClickAction`](crate::statusline::ClickAction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickSurface {
    /// A per-window status line (`'statusline'` / its segment layout) at the
    /// window's content width.
    Window,
    /// The single global status bar (`'laststatus'`=3) — the focused window's
    /// `'statusline'` at the full editor width.
    Global,
    /// The main region's custom tabline (`'tabline'`) — the focused window's
    /// context at the full editor width. Carries `%nT` tab-select regions.
    Tabline,
}

/// A status/tabline click awaiting the server's region resolution. The core
/// hit-tests the click to a window + column but can't run the `%`-format (it needs
/// the Lua eval for `%{}`/`%!`), so it records the click here; the server drains
/// [`Editor::statusline_clicks`] after the gesture, recomputes the relevant
/// format's click regions for the [`surface`](ClickSurface), and runs the action
/// whose span covers `col` (a Lua handler, or a tab-select via
/// [`Editor::select_main_tab`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineClick {
    /// The window the click resolves against (its context the format renders from).
    pub win: WindowId,
    /// The display column of the click — window-relative for a per-window status
    /// line, editor-absolute for the global bar / tabline (both 0-based; each starts
    /// at the window/editor left edge, so they coincide with its own column).
    pub col: usize,
    /// Which chrome row this was, deciding the format + width the server re-runs.
    pub surface: ClickSurface,
    /// Multi-click count (1 = single, 2 = double, …), per `'mousetime'`.
    pub clicks: u8,
    /// The mouse button: `'l'` / `'r'` / `'m'` (v1 fires on left only).
    pub button: char,
    /// Active modifiers as a string — `s` shift, `c` ctrl, `a` alt, in that order.
    pub modifiers: String,
}

impl Editor {
    /// Apply a mouse gesture. A no-op when `'mouse'` does not enable the current
    /// mode (vim-faithful — the gesture is simply ignored, not an error). Only the
    /// gestures implemented so far act; the rest are no-ops until their phase.
    pub fn mouse(&mut self, ev: MouseEvent) {
        // The command-line wildmenu is nxvim's own interactive overlay (neovim doesn't
        // click it at all), so a press / wheel that lands on it acts regardless of the
        // command-mode `'c'` flag — which the default `'mouse'` ("nvi") omits, and which
        // governs command-line *text* mouse, not this UI affordance. Every other gesture
        // still obeys `'mouse'` for the current mode. Guarded on the cell hitting the box
        // (`menu_hit`) so a command-mode click off the wildmenu stays disabled.
        let on_wildmenu = self.cmdline_complete_active() && self.menu_hit(ev.row, ev.col).is_some();
        if !self.mouse_enabled() && !on_wildmenu {
            return;
        }
        // Remember the cell so `vim.fn.getmousepos()` reports this event's position —
        // every processed gesture updates it (press / drag / release / wheel).
        self.last_mouse = Some((ev.row, ev.col));
        // A disrupting gesture (a click anywhere, or a wheel that scrolls the text)
        // moves the cursor / scrolls the view, so it dismisses the cursor-anchored
        // transient popups — the hover / signature **doc floats** and the completion
        // popup — instead of letting them trail the cursor. The one exception is a
        // wheel *on a doc float*, which scrolls the float and keeps it open. This is
        // the mouse counterpart of the next-key dismissal in [`Editor::input`].
        self.dismiss_cursor_popups_on_mouse(&ev);
        match (ev.button, ev.action) {
            // In multi-cursor placement mode a left-click *toggles* a cursor at the
            // clicked cell — drop one if it's bare, remove it if one is there — the
            // mouse form of the `c` placement command. Drag/release don't place.
            (MouseButton::Left, MouseAction::Press) if self.mode == Mode::MultiCursor => {
                self.mouse_toggle_cursor(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Drag | MouseAction::Release)
                if self.mode == Mode::MultiCursor => {}
            // A left-press or wheel on the open insert-mode completion popup drives
            // it — highlight a row, accept the already-highlighted row, or scroll the
            // highlight — instead of falling through to the text beneath. The popup
            // doesn't grab input (keys keep editing the document), so the mouse is the
            // one surface that acts on it directly; pickers / selects (which *do* grab
            // input) are wired in their own phase. Guarded on the cell landing on the
            // popup, so a click elsewhere still reaches the text.
            (MouseButton::Left, MouseAction::Press)
                if self.completion_active()
                    && (self.menu_hit(ev.row, ev.col).is_some()
                        || self.doc_float_at(ev.row, ev.col)) =>
            {
                // A press on the docs float (a real window now) is a no-op —
                // `mouse_complete_press` acts only on a menu-box row hit.
                self.mouse_complete_press(ev.row, ev.col)
            }
            // A wheel over the popup **box** moves the highlight one row; a wheel over
            // the docs float beside it falls through to the native window-scroll path
            // (`mouse_queue_wheel`), since the docs float is now a real scrollable window.
            (
                MouseButton::Wheel,
                MouseAction::WheelUp
                | MouseAction::WheelDown
                | MouseAction::WheelLeft
                | MouseAction::WheelRight,
            ) if self.completion_active() && self.menu_hit(ev.row, ev.col).is_some() => {
                let (positive, horizontal) = Self::wheel_axis(&ev);
                self.mouse_complete_wheel(positive, horizontal, ev.row, ev.col)
            }
            // A picker / `select` grabs the mouse modally while open (like it grabs the
            // keyboard): a left-press highlights or confirms a row — or cancels the
            // widget when it lands off the box — and a wheel scrolls the list or the
            // preview. Drag / release are swallowed so a stray drag can't start a text
            // selection through the box. These run before the chrome / text arms.
            (MouseButton::Left, MouseAction::Press) if self.picker_or_select_active() => {
                self.mouse_menu_press(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Drag | MouseAction::Release)
                if self.picker_or_select_active() => {}
            (
                MouseButton::Wheel,
                MouseAction::WheelUp
                | MouseAction::WheelDown
                | MouseAction::WheelLeft
                | MouseAction::WheelRight,
            ) if self.picker_or_select_active() => {
                let (positive, horizontal) = Self::wheel_axis(&ev);
                self.mouse_menu_wheel(positive, horizontal, ev.row, ev.col)
            }
            // The command-line wildmenu (`nx.cmdline_complete`) is non-grabbing like the
            // completion popup: a left-press on a candidate highlights it (and previews
            // it on the command line), clicking the highlighted one accepts it into the
            // line, and a wheel cycles the highlight. Guarded on the cell landing on the
            // box, so a press elsewhere still reaches the line / text.
            (MouseButton::Left, MouseAction::Press)
                if self.cmdline_complete_active() && self.menu_hit(ev.row, ev.col).is_some() =>
            {
                self.mouse_cmdline_press(ev.row, ev.col)
            }
            (MouseButton::Wheel, MouseAction::WheelUp | MouseAction::WheelDown)
                if self.cmdline_complete_active() && self.menu_hit(ev.row, ev.col).is_some() =>
            {
                self.mouse_cmdline_wheel(ev.action == MouseAction::WheelDown)
            }
            // A press on any region's shown tabline switches that region to the
            // clicked tab (vim's tabline click, generalized per region — main and
            // each open dock each have their own). Resolved before the text-press
            // arms so a tab click never places a cursor or starts a selection, and
            // it doesn't go through the window hit-test at all (the tabline is
            // chrome, not a window). Drag/release on the tabline do nothing.
            (MouseButton::Left, MouseAction::Press)
                if self.region_tabline_at(ev.row, ev.col).is_some() =>
            {
                self.mouse_click_tab(ev.row, ev.col)
            }
            // A press on the main *custom* `'tabline'` (which carries no built-in
            // click cells, so the arm above misses it): record a tabline click for
            // the server to resolve against the format's `%nT` regions.
            (MouseButton::Left, MouseAction::Press)
                if self.custom_main_tabline_col(ev.row, ev.col).is_some() =>
            {
                self.mouse_tabline_press(ev)
            }
            // A press on a divider grabs that edge; drags resize, release lets go.
            // Two kinds: a dock band's edge (between a dock and the main area) and a
            // split divider *inside* a region (a separator or a status line with a
            // window below it). Checked before the text-press arms so a divider
            // click never places the cursor or starts a selection.
            (MouseButton::Left, MouseAction::Press)
                if self.dock_handle_at(ev.row, ev.col).is_some()
                    || self.resize_handle_at(ev.row, ev.col).is_some() =>
            {
                self.mouse_begin_resize(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Drag) if self.mouse_resize.is_some() => {
                self.mouse_resize_drag(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Release) if self.mouse_resize.is_some() => {
                self.mouse_resize = None
            }
            // A left-press on a window's status line (one without a window below it —
            // a status-with-window-below is grabbed as a resize handle above) focuses
            // the window and records a click for the server to resolve against the
            // line's `%@…%X` regions. Checked before the selection arms so a status
            // click never places the cursor or starts a drag; before the shift arm so
            // a `<S-click>` records its modifier rather than trying to extend.
            (MouseButton::Left, MouseAction::Press)
                if matches!(
                    self.hit_test(ev.row, ev.col),
                    Some(MouseTarget::StatusLine { .. } | MouseTarget::GlobalStatusLine { .. })
                ) =>
            {
                self.mouse_statusline_press(ev)
            }
            // Shift+left-press is the selection-extend gesture (vim's `<S-LeftMouse>`
            // under the default `popup_setpos` mousemodel). It's a mappable key like
            // any other mouse button, so it's *queued* for the server: a bound
            // `<S-LeftMouse>` map fires, else [`Editor::mouse_apply_default`] runs the
            // extend. (`shift` distinguishes it from a plain left in the keymap.)
            (MouseButton::Left, MouseAction::Press) if ev.shift => {
                self.mouse_queue_press(&ev, MouseButton::Left, 1)
            }
            (MouseButton::Left, MouseAction::Press) => self.mouse_left_press(&ev),
            // A plain-text drag / release is a mappable gesture (`<LeftDrag>` /
            // `<LeftRelease>`): queue it for the server, which fires a bound map or runs
            // the default ([`Editor::mouse_apply_default`] — the drag-select for a left
            // drag; nothing for a release, which vim leaves the selection put for). The
            // widget / resize / multi-cursor arms above already claimed their drags, so
            // only a text drag reaches here.
            (MouseButton::Left, MouseAction::Drag) => {
                self.mouse_queue_gesture(&ev, MouseButton::Left, MouseKind::Drag)
            }
            (MouseButton::Left, MouseAction::Release) => {
                self.mouse_queue_gesture(&ev, MouseButton::Left, MouseKind::Release)
            }
            // Right- and middle-press are mappable too (`<RightMouse>` / `<MiddleMouse>`,
            // with modifiers): queue the press, and the server fires a bound map or runs
            // the default ([`Editor::mouse_apply_default`] — the `'mousemodel'` dispatch
            // for right, the `"*` paste for middle). Unlike left, nothing is placed
            // eagerly: the default does its own selection-aware placement, and a mapped
            // right/middle reads the click via `getmousepos()`. Drag / release are
            // mappable (`<RightDrag>` / `<MiddleRelease>` / …) with no default.
            (MouseButton::Right, MouseAction::Press) => {
                let count = self.next_button_click(MouseButton::Right, ev.row, ev.col, ev.stamp_ms);
                self.mouse_queue_press(&ev, MouseButton::Right, count)
            }
            (MouseButton::Right, MouseAction::Drag) => {
                self.mouse_queue_gesture(&ev, MouseButton::Right, MouseKind::Drag)
            }
            (MouseButton::Right, MouseAction::Release) => {
                self.mouse_queue_gesture(&ev, MouseButton::Right, MouseKind::Release)
            }
            (MouseButton::Middle, MouseAction::Press) => {
                let count =
                    self.next_button_click(MouseButton::Middle, ev.row, ev.col, ev.stamp_ms);
                self.mouse_queue_press(&ev, MouseButton::Middle, count)
            }
            (MouseButton::Middle, MouseAction::Drag) => {
                self.mouse_queue_gesture(&ev, MouseButton::Middle, MouseKind::Drag)
            }
            (MouseButton::Middle, MouseAction::Release) => {
                self.mouse_queue_gesture(&ev, MouseButton::Middle, MouseKind::Release)
            }
            // The wheel is a mappable key (`<ScrollWheelUp>` / …): queue it for the
            // server, which fires a bound map or runs the default — scrolling the window
            // *under the pointer* without moving focus or (unless a line scrolls off) the
            // cursor. The widget-wheel arms above (completion / picker / cmdline / doc
            // float) already claimed their scrolls, so only a text scroll reaches here.
            (MouseButton::Wheel, action) => self.mouse_queue_wheel(&ev, action),
            // The remaining buttons (X1/X2) and bare moves have no binding; ignore.
            _ => {}
        }
    }

    /// Dismiss the cursor-anchored transient popups — the hover / signature **doc
    /// floats** and the completion popup — when a mouse gesture disrupts their
    /// anchor. A click anywhere, or a wheel that scrolls the *text*, moves the cursor
    /// / view, so the popup must close rather than trail it (the bug being fixed:
    /// these floats followed the cursor on a mouse scroll / click-elsewhere). The one
    /// keep-open interaction is a wheel **on a doc float**, which scrolls that float.
    /// A click / wheel **on the completion menu or its docs sidebar** is the widget's
    /// own interaction (its arms handle it), not a disruption, so it is left alone.
    fn dismiss_cursor_popups_on_mouse(&mut self, ev: &MouseEvent) {
        let is_wheel = matches!(ev.action, MouseAction::WheelUp | MouseAction::WheelDown);
        // Drag / release continue an in-progress gesture (a text selection) the
        // initiating press already dismissed for — don't re-dismiss on every drag.
        if !matches!(ev.action, MouseAction::Press) && !is_wheel {
            return;
        }
        // A wheel scrolling a doc float keeps it open (and scrolls only it). Otherwise
        // dismiss the *transient* doc floats (hover) but keep the ones owned by a live
        // widget — the signature session and, crucially here, the completion docs float
        // while its popup is open (so a wheel over the text doesn't wipe it).
        if !(is_wheel && self.doc_float_at(ev.row, ev.col)) {
            self.close_transient_doc_floats();
        }
        // The completion popup closes when the gesture lands away from it — over the
        // text, not on the popup box or its docs float (those have their own
        // wheel/click handlers and don't disrupt the cursor anchor).
        if self.completion_active()
            && self.menu_hit(ev.row, ev.col).is_none()
            && !self.doc_float_at(ev.row, ev.col)
        {
            self.close_completion();
        }
    }

    /// Whether `(row, col)` lands on an open hover / signature **doc float** window.
    fn doc_float_at(&self, row: usize, col: usize) -> bool {
        !self.doc_float_wins.is_empty()
            && matches!(
                self.hit_test(row, col),
                Some(MouseTarget::Text { win, .. }) if self.doc_float_wins.iter().any(|(_, w)| *w == win)
            )
    }

    /// Left-press: focus the clicked window, place the cursor, and start a
    /// selection sized by the click count — single = char (vim's `<LeftMouse>`),
    /// double = the word, triple = the line. A same-cell press within `'mousetime'`
    /// of the last escalates the count; otherwise it resets to one. An active
    /// Visual selection is torn down first (also vim's behavior). For a single
    /// click no selection starts until the first drag; double/triple enter Visual
    /// immediately.
    fn mouse_left_press(&mut self, ev: &MouseEvent) {
        let (row, col, stamp_ms) = (ev.row, ev.col, ev.stamp_ms);
        // A press on a collapsed-dock chip (on the idle command-line row) re-shows
        // that dock — the click affordance for the toggle / auto-hide indicator.
        if let Some(side) = self.hidden_chip_at(row, col) {
            self.mouse_select = None;
            self.show_dock(side);
            return;
        }
        let target = self.hit_test(row, col);
        // A status-line press is dispatched by its own arm in `mouse` (it needs the
        // event's modifiers), so it never reaches here.
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = target
        else {
            // A press outside any window (or on a status line) clears the gesture
            // (and resets the count).
            self.mouse_select = None;
            return;
        };
        // Focus follows the click; focusing first makes `win` current so the
        // cursor/selection edits below act on the right window's state.
        self.set_current_window(win);
        if self.mode.is_visual() {
            // A click ends any active selection, stamping the `< / `> marks first
            // (the same teardown as Esc — see `command.rs`).
            self.record_visual_marks();
            self.mode = Mode::Normal;
        }
        self.set_window_cursor(win, line, bcol);

        let count = self.next_click_count(row, col, stamp_ms);
        // Record the gesture with a provisional single-click (char) anchor — enough
        // for a following drag and for the next press's multi-click counting. The
        // word/line escalation for a double/triple click is **deferred** to
        // [`mouse_apply_default_select`], which the server runs only if no
        // `<n-LeftMouse>` mapping claimed the click: the keymap engine lives in the
        // server (design D1), so the map-vs-default decision is made there. The click
        // is queued on `mouse_clicks` for it to resolve.
        self.mouse_select = Some(MouseSelect {
            row,
            col,
            stamp_ms,
            count,
            anchor: SelectAnchor::Char(self.cursor),
        });
        // A plain-left click carries its `<C-…>` / `<A-…>` modifiers so a
        // `<C-LeftMouse>` map is distinguished from a bare `<LeftMouse>`; `shift` is
        // never set here (a shift-left is the extend gesture, routed to its own arm).
        // The cursor is already placed above, so a `<C-LeftMouse>` map and the default
        // both act on the click.
        self.mouse_clicks.push(MouseClick {
            button: MouseButton::Left,
            kind: MouseKind::Press,
            clicks: count,
            shift: false,
            ctrl: ev.ctrl,
            alt: ev.alt,
            row,
            col,
            stamp_ms,
        });
    }

    /// Queue a press whose default the server runs only on a keymap miss — a right /
    /// middle button, or a shift-left (the extend gesture). No eager placement: unlike
    /// the plain-left press, the default ([`Editor::mouse_apply_default`]) does its own
    /// selection-aware cursor move, so the press must carry its cell to re-hit-test from.
    /// A *mapped* right/middle reads the click via `getmousepos()`. `clicks` is the
    /// multi-click count (always 1 for right/middle — only the left button is counted).
    fn mouse_queue_press(&mut self, ev: &MouseEvent, button: MouseButton, clicks: u8) {
        self.mouse_clicks.push(MouseClick {
            button,
            kind: MouseKind::Press,
            clicks,
            shift: ev.shift,
            ctrl: ev.ctrl,
            alt: ev.alt,
            row: ev.row,
            col: ev.col,
            stamp_ms: ev.stamp_ms,
        });
    }

    /// Queue a drag or release gesture (`<LeftDrag>` / `<LeftRelease>` / `<RightDrag>` /
    /// …) for the server to resolve: a bound map fires, else the default runs (the
    /// drag-select for a left drag; nothing for a release or a right/middle drag). Like
    /// the deferred presses it carries the cell so the default re-hit-tests; `clicks` is
    /// always 1 (a drag / release has no multi-click count).
    fn mouse_queue_gesture(&mut self, ev: &MouseEvent, button: MouseButton, kind: MouseKind) {
        self.mouse_clicks.push(MouseClick {
            button,
            kind,
            clicks: 1,
            shift: ev.shift,
            ctrl: ev.ctrl,
            alt: ev.alt,
            row: ev.row,
            col: ev.col,
            stamp_ms: ev.stamp_ms,
        });
    }

    /// The multi-click count for a **right / middle** press at `(row, col)` stamped
    /// `stamp_ms`: one more than the previous same-button same-cell press within
    /// `'mousetime'` (capped at 4, vim's quad-click), else 1. The left button's count is
    /// woven into the drag tracker ([`next_click_count`](Self::next_click_count)); this
    /// is the separate counter for the buttons with no drag gesture, so `<2-RightMouse>`
    /// / `<3-MiddleMouse>` map. Records the press for the next call.
    fn next_button_click(
        &mut self,
        button: MouseButton,
        row: usize,
        col: usize,
        stamp_ms: u64,
    ) -> u8 {
        let count = match self.mouse_button_seq {
            Some((b, r, c, stamp, n))
                if b == button
                    && r == row
                    && c == col
                    && stamp_ms.saturating_sub(stamp) <= self.options.mousetime as u64 =>
            {
                (n + 1).min(4)
            }
            _ => 1,
        };
        self.mouse_button_seq = Some((button, row, col, stamp_ms, count));
        count
    }

    /// Decode a wheel gesture into `(positive, horizontal)` for the overlay scroll
    /// handlers: a native horizontal wheel, or a vertical wheel with `Shift` (vim's
    /// `<S-ScrollWheel>`), scrolls **horizontally**; `positive` means *down* for a
    /// vertical scroll and *right* for a horizontal one.
    fn wheel_axis(ev: &MouseEvent) -> (bool, bool) {
        match ev.action {
            MouseAction::WheelDown => (true, ev.shift),
            MouseAction::WheelUp => (false, ev.shift),
            MouseAction::WheelRight => (true, true),
            MouseAction::WheelLeft => (false, true),
            _ => (false, false),
        }
    }

    /// Queue a scroll-wheel notch (`<ScrollWheelUp>` / …) for the server to resolve: a
    /// bound map fires, else [`mouse_apply_wheel_default`](Self::mouse_apply_wheel_default)
    /// scrolls. Carries the cell (the default scrolls the window under the pointer) and
    /// the modifiers (so `<S-ScrollWheelUp>` is distinguished, and an unmapped one still
    /// page-scrolls). A non-wheel action can't reach here (the `Wheel` button only ever
    /// parses to the four directions), so it is dropped.
    fn mouse_queue_wheel(&mut self, ev: &MouseEvent, action: MouseAction) {
        let dir = match action {
            MouseAction::WheelUp => WheelDir::Up,
            MouseAction::WheelDown => WheelDir::Down,
            MouseAction::WheelLeft => WheelDir::Left,
            MouseAction::WheelRight => WheelDir::Right,
            _ => return,
        };
        self.mouse_wheels.push(WheelGesture {
            dir,
            shift: ev.shift,
            ctrl: ev.ctrl,
            alt: ev.alt,
            row: ev.row,
            col: ev.col,
        });
    }

    /// Drain the scroll-wheel gestures awaiting keymap resolution (the server calls this
    /// right after a gesture). See [`WheelGesture`] / [`Editor::mouse_wheels`].
    pub fn take_mouse_wheels(&mut self) -> Vec<WheelGesture> {
        std::mem::take(&mut self.mouse_wheels)
    }

    /// The default scroll for a wheel notch the keymaps did **not** claim — the back
    /// half of the old eager wheel handler, split out so a bound `<ScrollWheel*>` map
    /// suppresses it (the server runs this only on a keymap miss).
    pub fn mouse_apply_wheel_default(&mut self, g: WheelGesture) {
        let action = match g.dir {
            WheelDir::Up => MouseAction::WheelUp,
            WheelDir::Down => MouseAction::WheelDown,
            WheelDir::Left => MouseAction::WheelLeft,
            WheelDir::Right => MouseAction::WheelRight,
        };
        self.mouse_wheel(action, g.row, g.col, g.shift);
    }

    /// Run a queued press's default behavior — the server calls this for each press the
    /// keymaps did **not** claim, so a bound `<…Mouse>` map *suppresses* the default
    /// rather than both running. Dispatched by button + modifier:
    ///
    /// - **plain / ctrl / alt left** — the word/line selection escalation
    ///   ([`mouse_apply_default_select`](Self::mouse_apply_default_select); the base
    ///   cursor was already placed eagerly by [`mouse_left_press`]).
    /// - **shift-left** — extend the selection to the click
    ///   ([`mouse_left_extend`](Self::mouse_left_extend)).
    /// - **right** — the `'mousemodel'` dispatch
    ///   ([`mouse_right_press`](Self::mouse_right_press)).
    /// - **middle** — paste the `"*` register ([`mouse_middle_press`](Self::mouse_middle_press)).
    ///
    /// The right / middle / shift-left defaults re-hit-test from the click's stored cell
    /// (they were not placed eagerly), so the gesture is applied exactly as the old
    /// eager handlers did, just deferred behind the keymap lookup.
    pub fn mouse_apply_default(&mut self, click: MouseClick) {
        match (click.button, click.kind) {
            // A shift-left press extends; a plain/ctrl/alt left press escalates the
            // word/line selection (its base cursor was already placed eagerly).
            (MouseButton::Left, MouseKind::Press) if click.shift => {
                self.mouse_left_extend(click.row, click.col, click.stamp_ms)
            }
            (MouseButton::Left, MouseKind::Press) => self.mouse_apply_default_select(click.clicks),
            // A left drag extends the in-flight selection; a release leaves it put.
            (MouseButton::Left, MouseKind::Drag) => self.mouse_left_drag(click.row, click.col),
            (MouseButton::Left, MouseKind::Release) => {}
            // Right press → the `'mousemodel'` dispatch; middle press → `"*` paste.
            (MouseButton::Right, MouseKind::Press) => {
                self.mouse_right_press(click.row, click.col, click.stamp_ms)
            }
            (MouseButton::Middle, MouseKind::Press) => {
                self.mouse_middle_press(click.row, click.col)
            }
            // Right/middle drag & release have no built-in behavior (mapping-only).
            (MouseButton::Right | MouseButton::Middle, MouseKind::Drag | MouseKind::Release) => {}
            // The wheel / move / thumb buttons are never queued as a `MouseClick`.
            (MouseButton::Wheel | MouseButton::Move | MouseButton::X1 | MouseButton::X2, _) => {}
        }
    }

    /// The default `<LeftMouse>` selection escalation, applied by [`mouse_apply_default`]
    /// when **no** `<n-LeftMouse>` mapping claimed a plain left press: a single click
    /// leaves the cursor where the press placed it (no Visual), a double click selects
    /// the word, a triple the line. This is the back half of [`mouse_left_press`], split
    /// out so a bound mouse mapping can suppress it. Updates the in-flight
    /// [`MouseSelect`]'s anchor so a following drag extends by the selected unit.
    pub fn mouse_apply_default_select(&mut self, clicks: u8) {
        let anchor = match clicks {
            0 | 1 => return,
            2 => self.mouse_select_word(),
            _ => self.mouse_select_line(),
        };
        if let Some(sel) = self.mouse_select.as_mut() {
            sel.anchor = anchor;
        }
    }

    /// Drain the mouse-button presses awaiting keymap resolution (the server calls this
    /// right after a gesture). See [`MouseClick`] / [`Editor::mouse_clicks`].
    pub fn take_mouse_clicks(&mut self) -> Vec<MouseClick> {
        std::mem::take(&mut self.mouse_clicks)
    }

    /// The position of the most recent mouse event, in the shape `vim.fn.getmousepos()`
    /// returns — global screen cell, the window the cell lands in, the window-relative
    /// cell, and the buffer position. Resolved through the same [`hit_test`](Self::hit_test)
    /// the gestures use, so a mouse mapping (`<RightMouse>`, `<MiddleMouse>`, …) can act on
    /// the *clicked* position rather than the cursor. All-zero before the first mouse
    /// event; off a window's text only the screen cell is set. The server mirrors this to
    /// Lua before every callback (`nx._mouse_pos`).
    pub fn mouse_pos(&self) -> MousePos {
        let Some((row, col)) = self.last_mouse else {
            return MousePos::default();
        };
        let mut mp = MousePos {
            screenrow: row as u64 + 1,
            screencol: col as u64 + 1,
            ..MousePos::default()
        };
        // Only a click on a window's text resolves to a window / buffer position; a
        // separator / status-line / off-grid cell leaves those fields 0 (as in vim).
        if let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        {
            mp.winid = win.0;
            // `window_screen_pos` is the window's *global* top-left (chrome included),
            // the exact inverse of the cell `hit_test` consumed, so the difference is
            // the 1-based window-relative cell.
            if let Some((wx, wy)) = self.window_screen_pos(win) {
                mp.winrow = (row.saturating_sub(wy) + 1) as u64;
                mp.wincol = (col.saturating_sub(wx) + 1) as u64;
            }
            mp.line = (line + 1) as u64;
            mp.column = (bcol + 1) as u64;
        }
        mp
    }

    /// Advance the shared status-line / tabline multi-click counter for a press at
    /// `(row, col)` stamped `stamp_ms`: bump the run (capped at a triple-click) when
    /// it repeats on the same cell within `'mousetime'`, else restart at 1. Records
    /// the new state and returns the click count.
    fn next_statusline_click(&mut self, row: usize, col: usize, stamp_ms: u64) -> u8 {
        let clicks = match self.statusline_click_seq {
            Some((r, c, stamp, count))
                if r == row
                    && c == col
                    && stamp_ms.saturating_sub(stamp) <= self.options.mousetime as u64 =>
            {
                (count + 1).min(3)
            }
            _ => 1,
        };
        self.statusline_click_seq = Some((row, col, stamp_ms, clicks));
        clicks
    }

    /// A left-press that hit-tested to a window's status line: focus the window
    /// (vim — crossing into its region if it is a dock) and record a
    /// [`StatuslineClick`] for the server to resolve against the line's `%@…%X`
    /// click regions. No cursor move, no selection. The multi-click count is read
    /// from the same `'mousetime'` machinery the text path uses, so a double-click on
    /// a region reports `clicks = 2`.
    fn mouse_statusline_press(&mut self, ev: MouseEvent) {
        // Per-window status line vs the single global bar (`laststatus=3`). The
        // global bar shows the focused window's facts, so its click resolves against
        // the current window — at the full editor width (the server keys off
        // `global`).
        let (win, col, surface) = match self.hit_test(ev.row, ev.col) {
            Some(MouseTarget::StatusLine { win, col }) => (win, col, ClickSurface::Window),
            Some(MouseTarget::GlobalStatusLine { col }) => {
                (self.current_window_id(), col, ClickSurface::Global)
            }
            _ => return,
        };
        // Clear any in-flight text selection, but count status-line multi-clicks on
        // their own tracker so this press doesn't seed a text drag.
        self.mouse_select = None;
        self.set_current_window(win);
        let clicks = self.next_statusline_click(ev.row, ev.col, ev.stamp_ms);
        let modifiers = mouse_modifier_str(ev.shift, ev.ctrl, ev.alt);
        self.statusline_clicks.push(StatuslineClick {
            win,
            col,
            surface,
            clicks,
            button: 'l',
            modifiers,
        });
    }

    /// A left-press on the **main custom tabline** (a non-empty `'tabline'`): record
    /// a [`ClickSurface::Tabline`] click for the server to resolve against the
    /// `'tabline'` format's `%nT` regions (→ [`Editor::select_main_tab`]). The
    /// built-in (structured) tabline is handled earlier by [`Editor::mouse_click_tab`];
    /// this is only reached when a custom `'tabline'` is in effect, where the cells
    /// carry no built-in click regions. No focus change here — switching the tab does
    /// that. The multi-click counter is shared with the status-line tracker (keyed on
    /// the cell, so the tabline row and a status row never cross-count).
    fn mouse_tabline_press(&mut self, ev: MouseEvent) {
        let Some(col) = self.custom_main_tabline_col(ev.row, ev.col) else {
            return;
        };
        self.mouse_select = None;
        let clicks = self.next_statusline_click(ev.row, ev.col, ev.stamp_ms);
        let modifiers = mouse_modifier_str(ev.shift, ev.ctrl, ev.alt);
        self.statusline_clicks.push(StatuslineClick {
            win: self.current_window_id(),
            col,
            surface: ClickSurface::Tabline,
            clicks,
            button: 'l',
            modifiers,
        });
    }

    /// The (0-based) column of a cell on the **main custom tabline**, or `None`. The
    /// main tabline is the top row at [`region_geoms`](Self::region_geoms)'s
    /// `bands.reserved_top()`, spanning the full width; this fires only when a custom
    /// `'tabline'` is set (an empty one uses the built-in structured tabline, handled
    /// by [`Editor::region_tabline_at`]) and the tabline is shown.
    fn custom_main_tabline_col(&self, row: usize, col: usize) -> Option<usize> {
        if self.global_options().tabline.is_empty() || self.tabline_rows() == 0 {
            return None;
        }
        let trow = self.dock_bands().reserved_top();
        (row == trow && col < self.width).then_some(col)
    }

    /// If the global cell `(row, col)` lands on a built-in tabline cell of *some*
    /// region — the main editor area or any open dock — the `(layer, tab index)`
    /// (the index 0-based in that region's tabline order) it covers. `None` when:
    ///
    /// - no region's tabline is shown on that cell's row/column;
    /// - a custom `'tabline'` is in effect for the main bar — its cells carry no
    ///   built-in click regions (vim needs explicit `%nT` items there, which we
    ///   don't model), so clicking it is a no-op (docks have no custom tabline);
    /// - the cell is on a dock's leading title label, or on the blank fill past
    ///   the last tab (vim's `TabLineFill`).
    ///
    /// The geometry mirrors the client `DockLayout` (`nxvim-tui` `render.rs`): the
    /// main tabline is the global top row (below any top dock), each dock's tabline
    /// is the first row of its band, after a one-cell separator toward the main
    /// area. Cell widths mirror `render_tab_cells` exactly — an optional ` title `
    /// prefix then one ` {count}{name}{+} ` cell per tab — so a click lands on the
    /// tab it visually covers.
    fn region_tabline_at(&self, row: usize, col: usize) -> Option<(Layer, usize)> {
        let (layer, x0) = self.region_geoms().into_iter().find_map(|g| {
            let (ty, x0, w) = g.tabline?;
            (row == ty && (x0..x0.saturating_add(w)).contains(&col)).then_some((g.layer, x0))
        })?;
        if layer == Layer::Main && !self.global_options().tabline.is_empty() {
            return None; // a custom main tabline has no built-in click regions.
        }
        // Walk the painted cells from the strip's left edge: a dock's bold title
        // label first (` {title} `, no click region), then one cell per tab.
        let mut x = x0;
        if let Layer::Dock(s) = layer {
            let title = self.dock_title(s);
            if !title.is_empty() {
                x = x.saturating_add(crate::unicode::display_width(&format!(" {title} ")));
            }
        }
        for (i, label) in self.tab_labels_for(layer).into_iter().enumerate() {
            let width = tab_cell_width(&label);
            if (x..x.saturating_add(width)).contains(&col) {
                return Some((layer, i));
            }
            x = x.saturating_add(width);
        }
        None
    }

    /// Every open region's absolute on-screen placement this frame, mirroring the
    /// client `DockLayout` (`nxvim-tui` `render.rs`): where each region's window
    /// tree paints, and — when its own tabline shows — that tabline's row and column
    /// span. The inverse of the per-client band math (the core owns *which* cells,
    /// the client owns *where*), shared by the mouse hit-tests so a global cell maps
    /// back to the region the user sees. Geometry is read for `self.height`, which
    /// is the windows-area height the client reports (cmdline excluded).
    /// The absolute screen row of the single global status bar (`'laststatus'`=3),
    /// or `None` when it isn't shown. It sits just below the middle band (main +
    /// side docks), above the bottom-dock band — the `mid_y + mid_h` row in
    /// [`Editor::region_geoms`]'s layout (the same chrome math, run for that one
    /// row). The inverse of where the client docks the global bar.
    fn global_statusline_row(&self) -> Option<usize> {
        if !self.global_statusline_visible() {
            return None;
        }
        let bands = self.dock_bands();
        let main_tabline = self.tabline_rows();
        let chrome = main_tabline + self.global_statusline_rows();
        let mid_y = bands.reserved_top().saturating_add(main_tabline);
        let mid_h = self
            .height
            .saturating_sub(bands.reserved_top())
            .saturating_sub(bands.reserved_bottom())
            .saturating_sub(chrome)
            .max(1);
        Some(mid_y.saturating_add(mid_h))
    }

    fn region_geoms(&self) -> Vec<RegionGeom> {
        let bands = self.dock_bands();
        let main_tabline = self.tabline_rows();
        let gstatus = self.global_statusline_rows();
        let chrome = main_tabline + gstatus;
        // The middle band (left dock | main | right docks): its top row, height, and
        // the main tree's width — what's left after the docks and the global chrome.
        let mid_y = bands.reserved_top().saturating_add(main_tabline);
        let mid_h = self
            .height
            .saturating_sub(bands.reserved_top())
            .saturating_sub(bands.reserved_bottom())
            .saturating_sub(chrome)
            .max(1);
        let main_w = self
            .width
            .saturating_sub(bands.reserved_left())
            .saturating_sub(bands.reserved_right())
            .max(1);
        // The bottom dock's band content starts past its separator, below the middle
        // band and the global status line; the right dock sits past main + its sep.
        let bottom_y = mid_y
            .saturating_add(mid_h)
            .saturating_add(gstatus)
            .saturating_add(1);
        let right_x = bands
            .reserved_left()
            .saturating_add(main_w)
            .saturating_add(1);
        self.open_layers()
            .into_iter()
            .map(|layer| {
                // The region's full content rect (its own tabline row, if any, plus
                // the tree below it), absolute.
                let (cx, cy, cw, ch) = match layer {
                    Layer::Main => (bands.reserved_left(), mid_y, main_w, mid_h),
                    Layer::Dock(DockSide::Left) => (0, mid_y, bands.left, mid_h),
                    Layer::Dock(DockSide::Right) => (right_x, mid_y, bands.right, mid_h),
                    Layer::Dock(DockSide::Top) => (0, 0, self.width, bands.top),
                    Layer::Dock(DockSide::Bottom) => (0, bottom_y, self.width, bands.bottom),
                };
                // Rows this region's own tabline eats off the top of its content (0
                // for main — its tabline is the global top bar, handled below).
                let tlr = match layer {
                    Layer::Main => 0,
                    dock => self.tabline_rows_for(dock),
                };
                let tree = (
                    cx,
                    cy.saturating_add(tlr),
                    cw,
                    ch.saturating_sub(tlr).max(1),
                );
                // The tabline strip: main's is the global top bar (full width, at the
                // reserved-top row); a dock's is its content's first row, shown only
                // when the band has room for both it and ≥1 content row (the client's
                // `content.height > 1` guard) and a non-zero width.
                let tabline = match layer {
                    Layer::Main => {
                        (main_tabline > 0).then_some((bands.reserved_top(), 0, self.width))
                    }
                    _ => (tlr > 0 && ch > 1 && cw > 0).then_some((cy, cx, cw)),
                };
                RegionGeom {
                    layer,
                    tree,
                    tabline,
                }
            })
            .collect()
    }

    /// If the global cell `(row, col)` lands on a collapsed-dock chip, the
    /// [`DockSide`] it would re-show. Chips live on the command-line row
    /// (`row == self.height`, the row just below the windows area) and only while
    /// that row is idle — the projected message is empty and we're not in
    /// command-line mode — mirroring the client, which yields the row to a message
    /// or a typed command. They start at col 0, each `▸{label}` (the dock title or
    /// side keyword) separated by a single space, in [`Editor::hidden_dock_chips`]
    /// order. The geometry mirrors the client chip painter exactly so a click lands
    /// on the chip it visually covers (cf. [`Editor::region_tabline_at`]).
    fn hidden_chip_at(&self, row: usize, col: usize) -> Option<DockSide> {
        if row != self.height {
            return None;
        }
        // The View blanks `message` everywhere except a real message / the terminal
        // hint; chips show only when that row would otherwise be blank.
        let message_shown = !self.message.is_empty() || self.mode == Mode::Terminal;
        if message_shown || self.mode == Mode::Command {
            return None;
        }
        let mut x = 0;
        for (side, label) in self.hidden_dock_chips() {
            let w = crate::unicode::display_width(&format!("▸{label}"));
            if (x..x + w).contains(&col) {
                return Some(side);
            }
            x += w + 1; // chip width plus the one-cell space separator
        }
        None
    }

    /// The open region — and the absolute top-left of its window-tree area — whose
    /// tree contains the global cell `(row, col)`: the main area or a dock band,
    /// below that region's own tabline row. `None` on chrome (a tabline, a
    /// separator, the panel) or outside every region. Regions are disjoint, so at
    /// most one matches.
    fn region_at(&self, row: usize, col: usize) -> Option<(Layer, usize, usize)> {
        self.region_geoms().into_iter().find_map(|g| {
            let (x, y, w, h) = g.tree;
            rect_contains(x, y, w, h, col, row).then_some((g.layer, x, y))
        })
    }

    /// Switch the region whose tabline cell holds the click to that tab, moving
    /// focus into it (vim's tabline click, per region). A click on the
    /// already-active tab of the focused region is a no-op. No cursor is placed in
    /// any text window — the press is consumed by the tabline.
    fn mouse_click_tab(&mut self, row: usize, col: usize) {
        if let Some((layer, idx)) = self.region_tabline_at(row, col) {
            self.focus_region_tab(layer, idx);
        }
    }

    /// Resolve a **global** screen cell to the split divider it grabs, as the
    /// window whose edge is dragged plus the divider orientation (`true` =
    /// vertical separator → resize width, `false` = horizontal separator or status
    /// line → resize height). The cell grabs a divider when it is:
    ///
    /// 1. on a vertical separator — the window directly to its left is grown;
    /// 2. on a horizontal separator — the window directly above it is grown;
    /// 3. on a window's status row that has a horizontal separator one row below
    ///    (i.e. another window beneath it) — that window is grown.
    ///
    /// `None` otherwise (text, gutter, the bottom-most status line, the tabline, or
    /// outside every window), so the press falls through to the normal handling.
    ///
    /// The cell is resolved within the **region** it lands in (the main area or a
    /// dock), against that region's own tree separators — so dragging a split inside
    /// a dock resizes that dock, without crossing focus. The dock↔main edge itself
    /// (the band size) is not a handle here.
    fn resize_handle_at(&self, row: usize, col: usize) -> Option<(WindowId, bool)> {
        let (layer, ox, oy) = self.region_at(row, col)?;
        let tree = self.layer_tree(layer)?;
        // Region-relative cell — each tree lays out at its own origin (0, 0).
        let (x, y) = (col - ox, row - oy);
        for sep in &tree.separators {
            if sep.vertical {
                if x == sep.x && y >= sep.y && y < sep.y.saturating_add(sep.length) {
                    // The window left of the divider grows when dragged right.
                    let (win, ..) = window_at_in(tree, sep.x.checked_sub(1)?, y)?;
                    return Some((win, true));
                }
            } else if y == sep.y && x >= sep.x && x < sep.x.saturating_add(sep.length) {
                // The window above the divider grows when dragged down.
                let (win, ..) = window_at_in(tree, x, sep.y.checked_sub(1)?)?;
                return Some((win, false));
            }
        }
        // Not on a separator: a window's own status row is a drag handle too, but
        // only when a horizontal separator sits just below it — otherwise it is the
        // bottom-most window and there is nothing beneath to resize against.
        let (win, _, rel_y) = window_at_in(tree, x, y)?;
        let (_, text_height) = self.window_text_area(win)?;
        if rel_y == text_height {
            let below = y.saturating_add(1);
            let has_window_below = tree.separators.iter().any(|s| {
                !s.vertical && s.y == below && x >= s.x && x < s.x.saturating_add(s.length)
            });
            if has_window_below {
                return Some((win, false));
            }
        }
        None
    }

    /// Resolve a **global** screen cell to the dock band **edge** it grabs — the
    /// separator between a dock and the main area, the cell whose drag resizes that
    /// dock's reserved width (left/right) or height (top/bottom). `None` if the cell
    /// is not on any open dock's edge. The geometry mirrors [`Editor::region_geoms`]
    /// (the inverse of the per-client band math): each open dock reserves its
    /// content plus one separator cell toward the main area, and that separator is
    /// the handle. The dock↔main edge is *between* regions, so [`Editor::region_at`]
    /// (and thus [`Editor::resize_handle_at`]) never claims it — the two hit-tests
    /// are disjoint.
    fn dock_handle_at(&self, row: usize, col: usize) -> Option<DockSide> {
        let bands = self.dock_bands();
        let main_tabline = self.tabline_rows();
        let gstatus = self.global_statusline_rows();
        let chrome = main_tabline + gstatus;
        // The middle band (left dock | main | right dock): its top row and height.
        let mid_y = bands.reserved_top().saturating_add(main_tabline);
        let mid_h = self
            .height
            .saturating_sub(bands.reserved_top())
            .saturating_sub(bands.reserved_bottom())
            .saturating_sub(chrome)
            .max(1);
        let in_mid = (mid_y..mid_y.saturating_add(mid_h)).contains(&row);
        // Left/right dock edges are vertical separators spanning the middle band;
        // top/bottom edges are horizontal separators spanning the full width.
        if self.dock_is_open(DockSide::Left) && bands.left > 0 && in_mid && col == bands.left {
            return Some(DockSide::Left);
        }
        if self.dock_is_open(DockSide::Right) && bands.right > 0 && in_mid {
            // The right dock occupies the right-most columns; its edge sits one cell
            // left of its content (`width − reserved_right`).
            let sep = self.width.saturating_sub(bands.reserved_right());
            if col == sep {
                return Some(DockSide::Right);
            }
        }
        if self.dock_is_open(DockSide::Top) && bands.top > 0 && col < self.width && row == bands.top
        {
            return Some(DockSide::Top);
        }
        if self.dock_is_open(DockSide::Bottom) && bands.bottom > 0 && col < self.width {
            // The bottom dock occupies the bottom-most rows of the windows area; its
            // edge sits one row above its content (`height − reserved_bottom`).
            let sep = self.height.saturating_sub(bands.reserved_bottom());
            if row == sep {
                return Some(DockSide::Bottom);
            }
        }
        None
    }

    /// Begin a divider drag: stash which edge is grabbed — a dock band edge or a
    /// window split — and the press cell the resize measures from. Clears any
    /// pending text selection so the divider press can't leave a stale anchor
    /// behind. A no-op (leaving `mouse_resize` unset) if the cell isn't a divider —
    /// the dispatch guard already checked, so this only re-resolves it.
    fn mouse_begin_resize(&mut self, row: usize, col: usize) {
        // A dock edge takes precedence: it lives between regions, where no window
        // split can also be, so checking it first is unambiguous.
        if let Some(side) = self.dock_handle_at(row, col) {
            self.mouse_select = None;
            self.mouse_resize = Some(ResizeDrag::Dock { side });
            return;
        }
        let Some((win, vertical)) = self.resize_handle_at(row, col) else {
            return;
        };
        self.mouse_select = None;
        self.mouse_resize = Some(ResizeDrag::Window {
            win,
            vertical,
            origin: if vertical { col } else { row },
            applied: 0,
        });
    }

    /// Continue a divider drag so the grabbed edge follows the pointer. For a window
    /// split the target offset from the press `origin` is absolute and `applied`
    /// records how much has been issued, so each drag sends only the remaining
    /// delta. For a dock edge the new band size is read directly from the pointer's
    /// position. Both push past a minimum and drag back without drifting.
    fn mouse_resize_drag(&mut self, row: usize, col: usize) {
        match self.mouse_resize {
            Some(ResizeDrag::Window {
                win,
                vertical,
                origin,
                applied,
            }) => {
                let current = if vertical { col } else { row };
                let want = current as isize - origin as isize;
                let step = want - applied;
                if step == 0 {
                    return;
                }
                let axis = if vertical {
                    SplitDir::Vertical
                } else {
                    SplitDir::Horizontal
                };
                self.resize_window_id(win, axis, step);
                if let Some(ResizeDrag::Window { applied, .. }) = self.mouse_resize.as_mut() {
                    *applied = want;
                }
            }
            Some(ResizeDrag::Dock { side }) => {
                // The new size places the dock's content edge at the pointer: for
                // left/top the content runs from 0 to the pointer; for right/bottom
                // it runs from the pointer to the far edge. Floored at 1; `set_dock_size`
                // / `dock_bands` clamp it back if the main area would vanish.
                let new_size = match side {
                    DockSide::Left => col,
                    DockSide::Right => self.width.saturating_sub(col).saturating_sub(1),
                    DockSide::Top => row,
                    DockSide::Bottom => self.height.saturating_sub(row).saturating_sub(1),
                };
                self.set_dock_size(side, new_size.max(1));
            }
            None => {}
        }
    }

    /// Shift+left-press (`<S-LeftMouse>`): extend the selection to the click,
    /// keeping the existing anchor. If a Visual selection is already active the
    /// live end moves to the click (charwise or linewise, matching the current
    /// mode); otherwise a charwise Visual is started from the cursor's current
    /// position to the click. A following plain drag keeps extending in the same
    /// unit. Ignored if the click lands outside the focused window — the selection
    /// it would extend lives there.
    fn mouse_left_extend(&mut self, row: usize, col: usize, stamp_ms: u64) {
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            return;
        };
        if win != self.current_window_id() {
            return;
        }
        if self.mode == Mode::VisualLine {
            // Linewise: keep the anchored line, move the active line to the click.
            let anchor_line = self.visual_anchor.line;
            self.cursor = Cursor { line, col: 0 };
            self.clamp_cursor();
            self.mouse_select = Some(MouseSelect {
                row,
                col,
                stamp_ms,
                count: 3,
                anchor: SelectAnchor::Line(anchor_line),
            });
            return;
        }
        // Charwise: anchor at the current cursor when not already selecting, then
        // move the live end to the click.
        if !self.mode.is_visual() {
            self.visual_anchor = self.cursor;
            self.mode = Mode::Visual;
        }
        let anchor = self.visual_anchor;
        self.set_window_cursor(win, line, bcol);
        self.mouse_select = Some(MouseSelect {
            row,
            col,
            stamp_ms,
            count: 1,
            anchor: SelectAnchor::Char(anchor),
        });
    }

    /// Right-press, dispatched by `'mousemodel'`:
    /// - `extend` — extend the selection to the click, exactly like
    ///   `<S-LeftMouse>` ([`Editor::mouse_left_extend`]).
    /// - `popup_setpos` (default) — move the cursor to the click, ending any
    ///   Visual selection, *unless* the click lands inside the current selection,
    ///   which is kept so a (deferred) popup menu could act on it.
    /// - `popup` — pop a context menu without moving the cursor; the menu widget
    ///   isn't built yet, so this is a no-op (tracked as its own feature).
    fn mouse_right_press(&mut self, row: usize, col: usize, stamp_ms: u64) {
        match self.mousemodel() {
            MouseModel::Extend => self.mouse_left_extend(row, col, stamp_ms),
            MouseModel::PopupSetpos => {
                let Some(MouseTarget::Text {
                    win,
                    line,
                    col: bcol,
                }) = self.hit_test(row, col)
                else {
                    return;
                };
                // A click inside the active selection keeps it (the menu would act
                // on the selection); elsewhere move the cursor and end Visual.
                if win == self.current_window_id() && self.pos_in_visual(line, bcol) {
                    return;
                }
                self.set_current_window(win);
                if self.mode.is_visual() {
                    self.record_visual_marks();
                    self.mode = Mode::Normal;
                }
                self.set_window_cursor(win, line, bcol);
            }
            MouseModel::Popup => {}
        }
    }

    /// Middle-press: paste the `"*` clipboard (primary-selection) register at the
    /// click — vim's `gP`: move the cursor to the clicked cell, splice the
    /// register in, and leave the cursor just past the pasted text. A no-op when
    /// the click misses a text cell or the `"*` register is empty / has no
    /// provider — nothing to paste, exactly like middle-clicking with an empty
    /// primary selection.
    fn mouse_middle_press(&mut self, row: usize, col: usize) {
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            return;
        };
        let Some((text, kind)) = self.register_text(Some('*')) else {
            return;
        };
        if text.is_empty() {
            return;
        }
        self.set_current_window(win);
        if self.mode.is_visual() {
            self.record_visual_marks();
            self.mode = Mode::Normal;
        }
        self.set_window_cursor(win, line, bcol);
        self.paste_text(&text, kind == RegKind::Line, 1, true);
    }

    /// The active `'mousemodel'`. Unknown values fall back to the `popup_setpos`
    /// default — the option layer stores the string without validation (like its
    /// `'mouse'` / `'mousescroll'` siblings), so the interpretation is here.
    fn mousemodel(&self) -> MouseModel {
        match self.options.mousemodel.as_str() {
            "extend" => MouseModel::Extend,
            "popup" => MouseModel::Popup,
            _ => MouseModel::PopupSetpos,
        }
    }

    /// Whether buffer position `(line, col)` lies within the active Visual
    /// selection (inclusive of both ends, the cells vim paints). `false` when not
    /// in a Visual mode. Charwise compares `(line, col)` against the ordered
    /// endpoints; linewise tests the line range only.
    fn pos_in_visual(&self, line: usize, col: usize) -> bool {
        if !self.mode.is_visual() {
            return false;
        }
        let a = self.visual_anchor;
        let b = self.cursor;
        let (lo, hi) = if (a.line, a.col) <= (b.line, b.col) {
            (a, b)
        } else {
            (b, a)
        };
        if self.mode == Mode::VisualLine {
            (lo.line..=hi.line).contains(&line)
        } else {
            (lo.line, lo.col) <= (line, col) && (line, col) <= (hi.line, hi.col)
        }
    }

    /// Left-click in [`Mode::MultiCursor`]: move the primary to the clicked cell
    /// and toggle a secondary cursor there — the mouse form of the `c` placement
    /// command, so clicking a bare cell drops a cursor and clicking a placed one
    /// removes it ([`place_cursor_here`](Editor::place_cursor_here) does the
    /// toggle; [`record_placement_undo`](Editor::record_placement_undo) makes it a
    /// single `u` step, exactly like keyboard `c`). Ignored outside the focused
    /// window — the cursor set lives in its buffer. Clears any pending drag
    /// selection so a stray drag can't start a Visual here.
    fn mouse_toggle_cursor(&mut self, row: usize, col: usize) {
        self.mouse_select = None;
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            return;
        };
        if win != self.current_window_id() {
            return;
        }
        self.set_window_cursor(win, line, bcol);
        self.record_placement_undo();
        self.place_cursor_here();
    }

    /// The click count for a press at screen cell `(row, col)` stamped `stamp_ms`:
    /// one more than the previous (capped at 3) when it repeats the same cell
    /// within `'mousetime'`, else 1. Mirrors `check_multiclick`
    /// (`vendor/neovim/src/nvim/os/input.c`).
    fn next_click_count(&self, row: usize, col: usize, stamp_ms: u64) -> u8 {
        match self.mouse_select {
            Some(p)
                if p.row == row
                    && p.col == col
                    && stamp_ms.saturating_sub(p.stamp_ms) <= self.options.mousetime as u64 =>
            {
                (p.count + 1).min(3)
            }
            _ => 1,
        }
    }

    /// Double-click: select the word under the cursor as a charwise Visual,
    /// returning its byte range as the drag anchor. Uses the same `iskeyword`-class
    /// run as `iw` ([`class_span`](Editor::class_span)).
    fn mouse_select_word(&mut self) -> SelectAnchor {
        let (lo, hi) = self.class_span(self.cursor_char(), false);
        self.mode = Mode::Visual;
        self.set_visual_span(lo, hi);
        SelectAnchor::Word { lo, hi }
    }

    /// Triple-click: select the cursor's line as a linewise Visual, returning the
    /// line index as the drag anchor.
    fn mouse_select_line(&mut self) -> SelectAnchor {
        let line = self.cursor.line;
        self.mode = Mode::VisualLine;
        self.visual_anchor = Cursor { line, col: 0 };
        self.cursor = Cursor { line, col: 0 };
        SelectAnchor::Line(line)
    }

    /// Left-drag: extend the selection from its press anchor to the drag cell, in
    /// the unit the press chose — charwise for a single click, by whole words for a
    /// double click, by whole lines for a triple. Ignored if no press is in flight.
    ///
    /// The drag always extends the selection in the window the press focused (the
    /// one the selection lives in), never hijacking another window the pointer
    /// wanders into. When the pointer crosses above or below that window's text
    /// band the window **auto-scrolls** one line that way ([`mouse_drag_target`]),
    /// so the selection can grow past the viewport — vim's mouse drag-scroll. A
    /// client repeats the drag while the button is held at the edge, turning the
    /// per-event one-line step into a continuous scroll.
    fn mouse_left_drag(&mut self, row: usize, col: usize) {
        let Some(sel) = self.mouse_select else {
            return;
        };
        let win = self.current_window_id();
        let Some((line, bcol)) = self.mouse_drag_target(win, row, col) else {
            return;
        };
        match sel.anchor {
            SelectAnchor::Char(anchor) => {
                // The first drag after a single click enters charwise Visual,
                // anchored where the press landed; later drags just move the end.
                if !self.mode.is_visual() {
                    self.visual_anchor = anchor;
                    self.mode = Mode::Visual;
                }
                self.set_window_cursor(win, line, bcol);
            }
            SelectAnchor::Word { lo, hi } => self.mouse_extend_word(line, bcol, lo, hi),
            SelectAnchor::Line(anchor_line) => self.mouse_extend_line(line, anchor_line),
        }
    }

    /// Resolve a left-drag at global cell `(row, col)` to the buffer position the
    /// selection extends to, in the focused window `win`. When the drag reaches (or
    /// passes) `win`'s first or last text row the window auto-scrolls one line that
    /// way (vim's mouse drag-scroll) and the returned line is the newly-exposed edge
    /// line; the column is clamped into the window so a drag off the side selects to
    /// the line's edge. `None` only when `win` has no geometry.
    ///
    /// The trigger is the edge *line itself*, not the row beyond it: the topmost
    /// window's first text row is global row 0, with nothing above to drag onto (the
    /// client clamps the pointer at 0), so a strictly-beyond test could never scroll
    /// it up. Reaching the top/bottom visible line is the gesture — `drag_scroll`
    /// no-ops at the buffer ends, so an edge line with nothing past it just extends.
    fn mouse_drag_target(
        &mut self,
        win: WindowId,
        row: usize,
        col: usize,
    ) -> Option<(usize, usize)> {
        let (abs_x, abs_y) = self.window_screen_pos(win)?;
        let (text_width, text_height) = self.window_text_area(win)?;
        // `'padding'` insets the text body from the window's top-left, so the band
        // starts a margin in; `text_cell_to_buf` expects padded-content-relative
        // coords, so `rel_x`/`rel_y` are measured from the padded origin too.
        let pad = self.window_options(win)?.padding;
        // The window's text band in global screen rows: `[top_edge, bottom_edge]`.
        // `abs_y` is the window's absolute top, so this is correct in any region (a
        // dock band as much as the main area), not just below the main tabline.
        let top_edge = abs_y.saturating_add(pad.top);
        let bottom_edge = top_edge.saturating_add(text_height.saturating_sub(1));
        let rel_y = if row <= top_edge {
            self.drag_scroll(false); // at/above the first line → reveal the line above
            0
        } else if row >= bottom_edge {
            self.drag_scroll(true); // at/below the last line → reveal the line below
            text_height.saturating_sub(1)
        } else {
            row - top_edge
        };
        let rel_x = col
            .saturating_sub(abs_x.saturating_add(pad.left))
            .min(text_width.saturating_sub(1));
        self.text_cell_to_buf(win, rel_x, rel_y)
    }

    /// Scroll the focused window's viewport one line toward an out-of-band drag
    /// (`down` = the drag ran below the text, scroll toward the buffer's end),
    /// clamped so `top` stays in `[0, last_line]`. A no-op at the clamp. The caller
    /// then parks the cursor on the newly-exposed edge line, so the per-redraw
    /// [`ensure_visible`](Self::ensure_visible) leaves the scroll alone (it would
    /// otherwise snap `top` straight back).
    fn drag_scroll(&mut self, down: bool) {
        let last = self.window_last_line(self.current_window_id());
        self.top = if down {
            (self.top + 1).min(last)
        } else {
            self.top.saturating_sub(1)
        };
    }

    /// Word-wise drag: grow the selection to cover whole words from the anchor word
    /// `[a_lo, a_hi)` to the word under the drag cell. Dragging forward keeps the
    /// anchor at the word's start; dragging back past it pivots — the anchor flips
    /// to the word's last char and the cursor leads at the far word's start (vim).
    fn mouse_extend_word(&mut self, line: usize, bcol: usize, a_lo: usize, a_hi: usize) {
        self.mode = Mode::Visual;
        let at = self.buffer().byte_at(line, bcol);
        let (b_lo, b_hi) = self.class_span(at, false);
        if b_lo >= a_lo {
            // Forward (or within the anchor word): anchor at the word's start, the
            // cursor on the last char of whichever word reaches furthest right.
            let end = self.prev_grapheme_idx(a_hi.max(b_hi));
            self.set_visual_chars(a_lo, end);
        } else {
            // Backward: anchor on the anchor word's last char, cursor at the far
            // word's start.
            let anchor = self.prev_grapheme_idx(a_hi);
            self.set_visual_chars(anchor, b_lo);
        }
    }

    /// Line-wise drag: extend the linewise Visual from the anchor line to the line
    /// under the drag cell. Direction is handled by the selection projection, which
    /// orders anchor and cursor, so this only moves the live end.
    fn mouse_extend_line(&mut self, line: usize, anchor_line: usize) {
        self.mode = Mode::VisualLine;
        self.visual_anchor = Cursor {
            line: anchor_line,
            col: 0,
        };
        self.cursor = Cursor { line, col: 0 };
        self.clamp_cursor();
    }

    /// Set a charwise Visual selection with the anchor at byte `anchor` and the
    /// live cursor at byte `cursor` (both clamped to grapheme boundaries) — unlike
    /// [`set_visual_span`](Editor::set_visual_span), the anchor may sit *after* the
    /// cursor, which a backward word-drag needs.
    fn set_visual_chars(&mut self, anchor: usize, cursor: usize) {
        self.set_cursor_char(anchor);
        self.visual_anchor = self.cursor;
        self.set_cursor_char(cursor);
    }

    /// A left-press on the open completion popup: clicking the already-highlighted
    /// row accepts it (like `<C-y>`); clicking any other row highlights it (like
    /// navigating to it with `<C-n>`/`<C-p>`). A press on the box border / a blank
    /// filler is consumed but selects nothing. Never starts a text drag underneath.
    fn mouse_complete_press(&mut self, row: usize, col: usize) {
        self.mouse_select = None;
        let Some(MenuHit::Item(idx)) = self.menu_hit(row, col) else {
            return;
        };
        let selected = self
            .menu_view()
            .and_then(|m| m.selected_active.then_some(m.selected));
        if selected == Some(idx) {
            self.complete_accept();
        } else {
            self.complete_select_index(idx);
        }
    }

    /// A wheel notch over the completion popup box moves the highlight one row,
    /// non-wrapping (like dragging a scrollbar — it stops at the ends, unlike
    /// `<C-n>`'s wrap). A noselect popup highlights the first row. A horizontal notch
    /// over the list does nothing — the list has no horizontal extent. (The docs float
    /// beside the popup is a real window; a wheel over it scrolls it via the native
    /// window mouse path, not here.)
    fn mouse_complete_wheel(&mut self, positive: bool, horizontal: bool, _row: usize, _col: usize) {
        if horizontal {
            return;
        }
        let down = positive;
        let Some(m) = self.menu_view() else {
            return;
        };
        let n = m.total;
        if n == 0 {
            return;
        }
        let next = match m.selected_active.then_some(m.selected) {
            Some(i) if down => (i + 1).min(n - 1),
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.complete_select_index(next);
    }

    /// A left-press while an input-grabbing menu (picker / `select`) is open: click a
    /// row to highlight it, click the already-highlighted row to confirm it, click
    /// the preview / chrome to no-op, and click off the box to cancel (a picker) or
    /// ignore (a `select`). Never starts a text drag underneath.
    fn mouse_menu_press(&mut self, row: usize, col: usize) {
        self.mouse_select = None;
        match self.menu_hit(row, col) {
            Some(MenuHit::Item(idx)) => {
                // A picker / select always has an active highlight; clicking it again
                // confirms (like `<CR>`), clicking another row moves it (like `<C-n>`).
                if self.menu_view().map(|m| m.selected) == Some(idx) {
                    self.menu_confirm();
                } else {
                    self.menu_cursor_to(idx);
                }
            }
            Some(MenuHit::Preview | MenuHit::Chrome) => {}
            // A click off the box cancels the chooser — the mouse form of `<Esc>`,
            // for a picker and a promptless `select` alike (routed by kind in
            // [`Editor::menu_cancel`]).
            None => self.menu_cancel(),
        }
    }

    /// A wheel notch while a picker / `select` is open: over the preview pane it
    /// scrolls the preview — vertically, or **horizontally** when `horizontal` (a
    /// `<S-ScrollWheel>` / horizontal wheel) so a wide file reads past the pane edge;
    /// over the list (or its chrome) it moves the highlight one row, non-wrapping (a
    /// horizontal notch over the list does nothing — the list has no horizontal
    /// extent); off the box it is ignored.
    fn mouse_menu_wheel(&mut self, positive: bool, horizontal: bool, row: usize, col: usize) {
        match self.menu_hit(row, col) {
            Some(MenuHit::Preview) if horizontal => self.menu_preview_scroll_h(positive),
            Some(MenuHit::Preview) => self.menu_preview_scroll(positive),
            Some(MenuHit::Item(_) | MenuHit::Chrome) if horizontal => {}
            Some(MenuHit::Item(_) | MenuHit::Chrome) => self.menu_step(positive),
            None => {}
        }
    }

    /// A left-press on the command-line wildmenu: clicking the highlighted candidate
    /// accepts it into the command line (like `<CR>` on it); clicking any other
    /// candidate highlights it (and previews it on the line). A press on the box
    /// border / a filler is consumed but selects nothing.
    fn mouse_cmdline_press(&mut self, row: usize, col: usize) {
        self.mouse_select = None;
        let Some(MenuHit::Item(idx)) = self.menu_hit(row, col) else {
            return;
        };
        if self
            .menu_view()
            .and_then(|m| m.selected_active.then_some(m.selected))
            == Some(idx)
        {
            self.cmdline_complete_accept();
        } else {
            self.cmdline_complete_select_index(idx);
        }
    }

    /// A wheel notch over the wildmenu moves the highlight one row, non-wrapping (like
    /// a scrollbar). A noselect wildmenu highlights the first row.
    fn mouse_cmdline_wheel(&mut self, down: bool) {
        let Some(m) = self.menu_view() else {
            return;
        };
        let n = m.total;
        if n == 0 {
            return;
        }
        let next = match m.selected_active.then_some(m.selected) {
            Some(i) if down => (i + 1).min(n - 1),
            Some(i) => i.saturating_sub(1),
            None => 0,
        };
        self.cmdline_complete_select_index(next);
    }

    /// Resolve a **global** screen cell to a spot on the open menu overlay: a
    /// selectable list row (the absolute view index), some other part of the box
    /// ([`MenuHit::Chrome`] — a border / prompt / filler), or `None` when the cell
    /// is off the menu. Covers every placement [`Self::menu_screen`] resolves,
    /// including the bottom-up cmdline wildmenu (its clicked offset is flipped to
    /// match the painted rows); `None` when no menu is open.
    fn menu_hit(&self, row: usize, col: usize) -> Option<MenuHit> {
        let s = self.menu_screen()?;
        let (bx, by, bw, bh) = s.box_rect;
        if !rect_contains(bx, by, bw, bh, col, row) {
            return None;
        }
        if let Some((px, py, pw, ph)) = s.preview {
            if rect_contains(px, py, pw, ph, col, row) {
                return Some(MenuHit::Preview);
            }
        }
        let (lx, ly, lw, lrows) = s.list;
        if rect_contains(lx, ly, lw, lrows, col, row) {
            // The clicked offset into the list rect. When the list is painted
            // bottom-up (the cmdline wildmenu) the top visual row is the last
            // logical one, so flip the offset before adding the scroll start.
            let off = row - ly;
            let off = if s.inverted { lrows - 1 - off } else { off };
            let idx = s.start + off;
            if idx < s.total {
                return Some(MenuHit::Item(idx));
            }
        }
        Some(MenuHit::Chrome)
    }

    /// The open menu's screen rectangle (global cells) and its selectable-row
    /// sub-rect — the inverse of the box [`menu_geom`](Self::menu_geom) projects,
    /// offset by the focused window's screen origin and adjusted for the client
    /// border convention, so a click lands on the row painted there. `None` for the
    /// command-line wildmenu (its own frame) or when no menu is open.
    fn menu_screen(&self) -> Option<MenuScreen> {
        let m = self.menu_view()?;
        let (metrics, win, gutter) = self.menu_anchor()?;
        let geom = self.menu_geom(&m, metrics);
        // The box's outer top-left + border layout, in global cells. Three frames: the
        // command-line wildmenu anchors to the command-line area (global x, the row
        // just below the windows area) and grows *upward* with a top border and no
        // bottom one; the `Editor` / `Bottom` picker overlay is already editor-absolute
        // (its `geom` is in windows-area cells), so it anchors at the windows-area
        // origin; every other menu anchors to the focused window's text inner. All but
        // the wildmenu grow downward.
        let (box_x, box_y, top_border, vborder) = if matches!(m.placement, MenuPlacement::Cmdline) {
            // `self.height` is the windows-area height, so the command-line row is at
            // that global row and the box's bottom border abuts it; the token column is
            // a global column (the command line spans the full width from x = 0).
            let vborder = 1; // top border only
            let box_y = self
                .height
                .checked_sub(geom.height.saturating_add(vborder))?;
            (geom.col, box_y, 1, vborder)
        } else if matches!(m.placement, MenuPlacement::Editor | MenuPlacement::Bottom) {
            // Editor-absolute: `geom.col`/`geom.row` are the outer box's top-left in
            // windows-area cells (origin 0,0), so a split's focused-pane origin does
            // not enter — the box floats over the whole editor, mirroring the client's
            // `editor_relative` anchor. A picker is always fully bordered (2 rows).
            (geom.col, geom.row, 1, 2)
        } else {
            let (wx, wy) = self.window_screen_pos(win)?;
            // The text inner sits past this window's `'padding'` (left + top) and its
            // number gutter, matching where the client paints the body.
            let pad = self
                .window_options(win)
                .map(|o| o.padding)
                .unwrap_or_default();
            let inner_x = wx.saturating_add(pad.left).saturating_add(gutter); // text inner: past padding + the number gutter
                                                                              // A full border for select / picker; the completion popup omits its top
                                                                              // border and shifts one cell left so its left border doesn't cover the word
                                                                              // it completes. `geom.col` is the content anchor for `Cursor` placement and
                                                                              // the outer-box left for `Editor`; either way the outer box left is
                                                                              // `geom.col - left_shift` and the content sits one cell in.
            let border_top = !m.completion;
            let left_shift = usize::from(!border_top);
            let vborder = if border_top { 2 } else { 1 };
            let box_x = inner_x.saturating_add(geom.col).saturating_sub(left_shift);
            (
                box_x,
                wy.saturating_add(pad.top).saturating_add(geom.row),
                usize::from(border_top),
                vborder,
            )
        };
        let box_w = geom.width.saturating_add(2);
        let box_h = geom.height.saturating_add(vborder);
        // The content rect (inside the borders): the list + prompt + preview live here.
        let content_x = box_x.saturating_add(1); // past the left border (present in every variant)
        let content_y = box_y.saturating_add(top_border);
        let content_w = geom.width;
        let content_h = geom.height;
        // A picker with a preview pane splits the content into a list column (left) +
        // a 1-col separator + the preview (right `~60%`, the same fraction the server's
        // `project_preview` reserves). No preview ⇒ the list fills the content.
        let (list_w, preview) = if m.has_preview {
            let preview_w = ((content_w as f32 * 0.6) as usize)
                .min(content_w.saturating_sub(2))
                .max(1);
            let list_w = content_w.saturating_sub(preview_w.saturating_add(1)).max(1);
            let preview_x = content_x.saturating_add(list_w).saturating_add(1);
            (list_w, Some((preview_x, content_y, preview_w, content_h)))
        } else {
            (content_w, None)
        };
        // The selectable rows sit below the prompt + separator chrome (a picker's
        // prompt is on top by default, on the bottom when asked); a promptless `select`
        // / completion popup has none, so the list fills the content height.
        let prompt_rows = usize::from(m.query.is_some());
        let chrome = prompt_rows * 2;
        let prompt_top = prompt_rows > 0 && m.prompt_pos == PromptPos::Top;
        let list_y = content_y.saturating_add(if prompt_top { chrome } else { 0 });
        let list_rows = content_h.saturating_sub(chrome);
        Some(MenuScreen {
            box_rect: (box_x, box_y, box_w, box_h),
            list: (content_x, list_y, list_w, list_rows),
            preview,
            start: geom.start,
            total: m.total,
            inverted: matches!(m.placement, MenuPlacement::Cmdline),
        })
    }

    /// The focused window's cursor-screen metrics + screen origin for placing the
    /// open menu's box, recomputed from the same projection the redraw uses so the
    /// hit-test inverts exactly what was painted. Returns the metrics, the focused
    /// window, and its number-gutter width. Only called while a menu is open, so the
    /// projection build (bounded by the viewport, not the buffer) is paid rarely.
    fn menu_anchor(&self) -> Option<(MenuMetrics, WindowId, usize)> {
        let view = crate::view::View::from_editor(self);
        let f = view.focused();
        let (editor_w, editor_h) = self.screen_size();
        let metrics = MenuMetrics {
            cursor_row: f.cursor_row,
            cursor_screen_col: f.cursor_screen_col,
            leftcol: f.leftcol,
            text_width: f.rect.width.saturating_sub(f.number_width),
            text_height: f.rows.len(),
            editor_w,
            editor_h,
        };
        Some((metrics, f.id, f.number_width))
    }

    /// The completion **docs float**'s placement beside the popup box: its outer
    /// top-left `(row, col)` and inner `(width, height)`, all in the focused window's
    /// **region cells** (its layer's tree lays out at origin `0,0`; the client offsets
    /// by the region's screen origin) — the space a `FloatRelative::Editor` float is
    /// positioned in, so the float lands exactly where the server-projected popup
    /// overlay does. `content_lines` (the rendered doc lines) sizes it: widest line ×
    /// count, each clamped so a long doc scrolls rather than filling the screen. The
    /// float butts against the popup — its content one cell past the popup's right
    /// border, flipping to the left when that side has more room — and top-aligns its
    /// content with the popup's first row, so the outer box (border included) sits one
    /// row/col out. `None` when no completion popup is open, or neither side fits a
    /// readable width. Region math mirrors the old `redraw.rs::project_complete_docs`.
    /// The open completion popup box's `menu_geom` col/row/width plus its window — the
    /// content-independent part of the docs float's placement, used as the signature
    /// that decides whether [`open_completion_docs_float`](Self::open_completion_docs_float)
    /// can skip a redundant reopen. `None` when no completion popup is open.
    pub(crate) fn complete_docs_box_geom(&self) -> Option<(usize, usize, usize, WindowId)> {
        let m = self.menu_view()?;
        if !m.completion || !matches!(m.placement, MenuPlacement::Cursor) {
            return None;
        }
        let (metrics, win, _num) = self.menu_anchor()?;
        let geom = self.menu_geom(&m, metrics);
        Some((geom.col, geom.row, geom.width, win))
    }

    /// The open cmdline **wildmenu** box's `menu_geom` row/col/width/height (windows-area
    /// frame) plus the editor width — what the server's cmdline-docs sync needs to place
    /// the docs float beside / below it (the same inputs the old `project_cmdline_docs`
    /// received at redraw). `None` unless a [`MenuPlacement::Cmdline`] menu is open.
    pub fn cmdline_menu_box(&self) -> Option<(usize, usize, usize, usize, usize)> {
        let m = self.menu_view()?;
        if !matches!(m.placement, MenuPlacement::Cmdline) {
            return None;
        }
        let (metrics, _win, _num) = self.menu_anchor()?;
        let geom = self.menu_geom(&m, metrics);
        let (editor_w, _) = self.screen_size();
        Some((geom.row, geom.col, geom.width, geom.height, editor_w))
    }

    pub(crate) fn complete_docs_geom(
        &self,
        content_lines: &[String],
        wrap: bool,
    ) -> Option<(usize, usize, u16, u16)> {
        /// Cap the docs float's content width — a long signature wraps off-screen otherwise.
        const MAX_DOCS_W: usize = 60;
        /// Cap its height — a huge docstring shouldn't fill the screen beside a popup.
        const MAX_DOCS_H: usize = 12;
        let m = self.menu_view()?;
        if !m.completion || !matches!(m.placement, MenuPlacement::Cursor) {
            return None;
        }
        let (metrics, win, _num) = self.menu_anchor()?;
        let geom = self.menu_geom(&m, metrics);
        let (rx, ry, _rw, _rh) = self.window_rect(win)?;
        let pad = self
            .window_options(win)
            .map(|o| o.padding)
            .unwrap_or_default();
        // The gutter the popup box sits behind is the window's *whole* text offset —
        // the sign column AND the number column — not just the number width, or the
        // sidebar slides `sign_width` cells left of the popup it butts against.
        let gutter = self.window_textoff(win).unwrap_or(0);
        // The popup box's content top-left, region-relative.
        let content_col = rx + pad.left + gutter + geom.col;
        let content_row = ry + pad.top + geom.row;
        // Bound by the editor edges MINUS this region's screen origin (the dock bands +
        // global chrome), so the float can't overrun the editor's right / bottom edge.
        let (region_x, region_y) = self.window_region_origin(win).unwrap_or((0, 0));
        let (editor_w, editor_h) = self.screen_size();
        let bound_w = editor_w.saturating_sub(region_x);
        let bound_h = editor_h.saturating_sub(region_y);
        let content_w = content_lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_DOCS_W);
        let (docs_col, docs_w) = place_docs_beside(content_col, geom.width, content_w, bound_w)?;
        // Height counts the **wrapped** display rows: with `wrap` on a doc line wider than
        // `docs_w` (a reflowed markdown paragraph is one long line) spans several rows, so
        // sizing to the raw line count would leave the body one row tall with the rest
        // clipped. Clamped to the cap and to the room below the float's top.
        let docs_h = crate::unicode::wrapped_row_count(content_lines, docs_w, wrap)
            .min(MAX_DOCS_H)
            .min(bound_h.saturating_sub(content_row).saturating_sub(2).max(1));
        // The outer box (border included) sits one row/col out from the content, so the
        // content lands at `(content_row, docs_col)` — flush one cell past the popup border.
        Some((
            content_row.saturating_sub(1),
            docs_col.saturating_sub(1),
            docs_w as u16,
            docs_h as u16,
        ))
    }

    /// Scroll wheel: scroll the window **under the pointer** by `'mousescroll'`
    /// (`Shift` makes a vertical notch a full page), leaving focus and — unless a
    /// line scrolls off — the cursor where they are. A notch over no window (the
    /// tabline, a separator, the panel) is ignored, as is a direction `'mousescroll'`
    /// disables with a `0` step. Vertical maps to `top`, horizontal to `leftcol`.
    fn mouse_wheel(&mut self, action: MouseAction, row: usize, col: usize, shift: bool) {
        let Some(win) = self.window_at_cell(row, col) else {
            return;
        };
        let (ver, hor) = self.mousescroll_steps();
        match action {
            MouseAction::WheelUp | MouseAction::WheelDown => {
                // Shift escalates a notch to a screenful (vim's `<S-ScrollWheel*>`
                // → `<C-b>`/`<C-f>`), keeping a two-line overlap like `scroll_page`.
                let step = if shift {
                    self.window_text_height(win).saturating_sub(2).max(1)
                } else {
                    ver
                };
                if step == 0 {
                    return; // `mousescroll=ver:0` disables vertical wheel
                }
                let down = action == MouseAction::WheelDown;
                let delta = if down { step as i64 } else { -(step as i64) };
                self.wheel_scroll_vertical(win, delta);
            }
            MouseAction::WheelLeft | MouseAction::WheelRight => {
                if hor == 0 {
                    return; // `mousescroll=hor:0` disables horizontal wheel
                }
                let right = action == MouseAction::WheelRight;
                let delta = if right { hor as i64 } else { -(hor as i64) };
                self.wheel_scroll_horizontal(win, delta);
            }
            // `mouse_wheel` is only reached for `MouseButton::Wheel`, whose parse
            // only ever yields the four wheel directions.
            _ => {}
        }
    }

    /// Parse `'mousescroll'` (`"ver:{lines},hor:{cols}"`) into the `(vertical,
    /// horizontal)` step counts. A missing field falls back to vim's default
    /// (`ver:3` / `hor:6`); a `0` count disables that direction.
    fn mousescroll_steps(&self) -> (usize, usize) {
        let (mut ver, mut hor) = (3, 6);
        for part in self.options.mousescroll.split(',') {
            match part.split_once(':') {
                Some(("ver", n)) => ver = n.parse().unwrap_or(ver),
                Some(("hor", n)) => hor = n.parse().unwrap_or(hor),
                _ => {}
            }
        }
        (ver, hor)
    }

    /// Scroll window `win` vertically by `delta` lines (negative = toward the top
    /// of the buffer), clamped so the first line can't pass the top row. The cursor
    /// stays on its buffer line while that line is still visible; once the scroll
    /// would push it off, it is pulled to the nearest visible edge (vim's wheel
    /// with `scrolloff` 0). The focused window moves its live viewport and emits the
    /// smooth-scroll gesture; an inactive window updates its stashed scroll — the
    /// wheel famously scrolls a window you are not focused in. Pulling the cursor
    /// onto a visible line on the focused window is load-bearing, not cosmetic: the
    /// per-redraw `ensure_visible` would otherwise snap `top` straight back.
    fn wheel_scroll_vertical(&mut self, win: WindowId, delta: i64) {
        let last = self.window_last_line(win);
        let th = self.window_text_height(win);
        if win == self.current_window_id() {
            let old_top = self.top;
            let new_top = (old_top as i64 + delta).clamp(0, last as i64) as usize;
            if new_top == old_top {
                return;
            }
            self.scroll_from = Some((old_top, self.cursor.line));
            self.top = new_top;
            let bottom = self.top.saturating_add(th.saturating_sub(1));
            if self.cursor.line < self.top {
                self.cursor.line = self.top;
            } else if self.cursor.line > bottom {
                self.cursor.line = bottom.min(last);
            } else {
                // Cursor still visible — leave it (and its `curswant`) untouched.
                self.finalize_scroll_gesture();
                return;
            }
            self.settle_desired_col(false);
            self.preserve_desired = true;
            self.finalize_scroll_gesture();
        } else {
            // An inactive window (a split in another layer, or a non-focused dock)
            // updates its stashed scroll — resolve its tree, which may be parked.
            let Some(tree) = self.tree_of_window_mut(win) else {
                return;
            };
            let w = tree.get_mut(win);
            let old_top = w.saved_top;
            let new_top = (old_top as i64 + delta).clamp(0, last as i64) as usize;
            if new_top == old_top {
                return;
            }
            let bottom = new_top.saturating_add(th.saturating_sub(1));
            w.saved_top = new_top;
            if w.saved_cursor.line < new_top {
                w.saved_cursor.line = new_top;
            } else if w.saved_cursor.line > bottom {
                w.saved_cursor.line = bottom.min(last);
            }
        }
    }

    /// Scroll window `win` horizontally by `delta` columns (negative = left),
    /// clamped to `[0, max_leftcol]` so it can't scroll past the content — when
    /// every visible line already fits there is nothing off-screen and a notch is a
    /// no-op (vim doesn't scroll into empty space). Like the vertical wheel this
    /// moves `leftcol` without changing focus; on the focused window the cursor is
    /// pulled back into the visible band (honoring `sidescrolloff`) so the
    /// per-redraw `ensure_visible_horizontal` doesn't immediately undo the scroll.
    /// Only meaningful under `nowrap`.
    fn wheel_scroll_horizontal(&mut self, win: WindowId, delta: i64) {
        let max = self.window_max_leftcol(win) as i64;
        if win == self.current_window_id() {
            let old = self.leftcol;
            let new = (old as i64 + delta).clamp(0, max) as usize;
            if new == old {
                return;
            }
            self.leftcol = new;
            self.keep_cursor_in_leftcol();
        } else {
            let Some(tree) = self.tree_of_window_mut(win) else {
                return;
            };
            let w = tree.get_mut(win);
            let new = (w.saved_leftcol as i64 + delta).clamp(0, max) as usize;
            w.saved_leftcol = new;
        }
    }

    /// The furthest right `leftcol` window `win` may scroll to: the widest line in
    /// its current viewport minus the text width, floored at 0. At this offset the
    /// widest visible line's last column sits at the right edge, so a window whose
    /// lines all fit (`widest <= text width`) has a max of 0 — no horizontal scroll.
    fn window_max_leftcol(&self, win: WindowId) -> usize {
        let (Some((top, _)), Some((content_w, text_h)), Some(buf_id), Some(opts)) = (
            self.window_scroll(win),
            self.window_text_area(win),
            self.window_buffer(win),
            self.window_options(win),
        ) else {
            return 0;
        };
        // A `wrap`ped window never scrolls horizontally (vim disables it under `wrap`):
        // a long line flows onto the next screen row instead of running off the right
        // edge, so there is nothing off-screen to reach — the docs float (`wrap` on)
        // relies on this so a wide code line can't be wheeled sideways.
        if opts.wrap {
            return 0;
        }
        let buf = &self.buffers.get(buf_id).buffer;
        let line_count = buf.line_count();
        let text_w = content_w.saturating_sub(self.number_width_for(&opts, line_count));
        let ts = buf.options.effective_tabstop();
        let widest = (top..top.saturating_add(text_h).min(line_count))
            .map(|l| {
                let s = buf.line(l);
                crate::unicode::virtcol(&s, s.len(), ts)
            })
            .max()
            .unwrap_or(0);
        widest.saturating_sub(text_w)
    }

    /// Pull the focused window's cursor into the visible horizontal band
    /// `[leftcol + sidescrolloff, leftcol + width - sidescrolloff)` by moving its
    /// column, mirroring [`ensure_visible_horizontal`](Editor::ensure_visible_horizontal)'s
    /// bounds so that — once the cursor sits inside them — that pass is a no-op and
    /// the wheel's `leftcol` survives the redraw.
    fn keep_cursor_in_leftcol(&mut self) {
        let tw = self.text_width();
        if tw == 0 {
            return;
        }
        let so = self
            .windows
            .cur()
            .options
            .sidescrolloff
            .min(tw.saturating_sub(1) / 2);
        let lo = self.leftcol.saturating_add(so);
        let hi = self
            .leftcol
            .saturating_add(tw)
            .saturating_sub(so.saturating_add(1));
        let vc = self.cursor_virtcol();
        let target = if vc < lo {
            lo
        } else if vc > hi {
            hi
        } else {
            return;
        };
        let line = self.buffer().line(self.cursor.line);
        let ts = self.buffer().options.effective_tabstop();
        self.cursor.col = crate::unicode::byte_at_virtcol(&line, target, ts);
        self.snap_cursor();
        self.desired_col = self.cursor_virtcol();
        self.preserve_desired = true;
    }

    /// The window whose content area is under the **global** screen cell `(row,
    /// col)` — in *any* region (the main area or a dock), so the wheel scrolls a dock
    /// you are not focused in. `None` when the cell is on a tabline, a window
    /// separator, or outside every window. Unlike [`hit_test`](Self::hit_test) this
    /// stops at the window — the wheel needs only *which* window to scroll, not a
    /// buffer cell.
    fn window_at_cell(&self, row: usize, col: usize) -> Option<WindowId> {
        let (layer, ox, oy) = self.region_at(row, col)?;
        let tree = self.layer_tree(layer)?;
        window_at_in(tree, col - ox, row - oy).map(|(win, ..)| win)
    }

    /// Window `win`'s text-area height in rows (its content height minus the status
    /// line), at least 1 — the page size for a `Shift`+wheel notch.
    fn window_text_height(&self, win: WindowId) -> usize {
        self.window_text_area(win).map_or(1, |(_, h)| h).max(1)
    }

    /// Window `win`'s last real buffer line (0-based), the floor `top` can scroll
    /// to. `0` for an unknown window.
    fn window_last_line(&self, win: WindowId) -> usize {
        self.window_buffer(win).map_or(0, |b| {
            self.buffers.get(b).buffer.line_count().saturating_sub(1)
        })
    }

    /// Whether `'mouse'` enables mouse input for the current mode. `a` enables
    /// every mode; otherwise the mode's own char must be present (`n`/`v`/`i`/`c`).
    fn mouse_enabled(&self) -> bool {
        let m = &self.options.mouse;
        if m.contains('a') {
            return true;
        }
        let flag = match self.mode {
            // MultiCursor is a normal-like placement mode: mouse maps gate on the
            // same `n` flag (independent of its now-distinct `mode()` code `m`).
            Mode::Normal | Mode::MultiCursor => 'n',
            Mode::Visual | Mode::VisualLine => 'v',
            Mode::Insert | Mode::Replace => 'i',
            Mode::Command => 'c',
            // vim gates terminal-mode mouse on the `t` flag (we treat it like
            // insert for click-to-position purposes).
            Mode::Terminal => 't',
        };
        m.contains(flag)
    }

    /// Resolve a **global** screen cell `(row, col)` to a [`MouseTarget`], or
    /// `None` if it lands on no actionable region (a tabline, a separator, the
    /// panel, or outside every window). This is the reverse of the forward layout:
    /// find the **region** under the cell (the main area or a dock band), probe that
    /// region's (live or parked) window tree at the region-relative cell, then turn
    /// the window-relative cell into a buffer line/col through that window's scroll
    /// offset, number gutter, and tab/wide-char column math. Resolving across every
    /// region — not just the focused one — is what lets a click in any dock land
    /// there; the press handler then focuses that region via `set_current_window`.
    fn hit_test(&self, row: usize, col: usize) -> Option<MouseTarget> {
        // The global status bar (`laststatus=3`) is chrome below every region, so it
        // matches no region tree; resolve it first. It spans the full width and shows
        // the focused window's facts, so the click resolves against that window.
        if self.global_statusline_row() == Some(row) {
            return Some(MouseTarget::GlobalStatusLine { col });
        }
        // Each region's tree lays out at its own origin (0, 0) and the client offsets
        // it by the region's screen origin; this runs that offset backwards. A cell
        // on chrome (a tabline, a separator, the panel) matches no region's tree.
        let (layer, ox, oy) = self.region_at(row, col)?;
        let tree = self.layer_tree(layer)?;
        let (win, rel_x, rel_y) = window_at_in(tree, col - ox, row - oy)?;
        let (_, text_height) = self.window_text_area(win)?;
        if rel_y >= text_height {
            // Below the text body: the status row (the last content line). `rel_x`
            // is the window-relative column, which is the status line's own column
            // (it spans the window's content width from the left edge).
            return Some(MouseTarget::StatusLine { win, col: rel_x });
        }
        let (line, col) = self.text_cell_to_buf(win, rel_x, rel_y)?;
        Some(MouseTarget::Text { win, line, col })
    }

    /// Map window `win`'s content-relative cell — `rel_y` a text row counted from
    /// the window's first visible line, `rel_x` a column from its left edge — back
    /// to a buffer `(line, col)`. The shared tail of [`hit_test`](Self::hit_test)
    /// and the drag resolver: it undoes the window's vertical scroll, number
    /// gutter, and horizontal scroll + tab/wide-char column math. Geometry is read
    /// for `win` whether or not it is focused (its live offset if focused, its
    /// stashed one otherwise). `None` only for an unknown window.
    ///
    /// `rel_y` is a **screen** row, which under `'wrap'` (or with extmark
    /// `virt_lines`) is *not* a one-to-one offset onto buffer lines — a wrapped
    /// line spans several rows. The vertical map walks the same interleaved layout
    /// the view projects ([`row_skeleton`](crate::view)) backwards from the first
    /// visible line `top`, so a click on a wrapped line's continuation row resolves
    /// to that line (at the wrapped column) rather than the next buffer line.
    fn text_cell_to_buf(
        &self,
        win: WindowId,
        rel_x: usize,
        rel_y: usize,
    ) -> Option<(usize, usize)> {
        let (top, leftcol) = self.window_scroll(win)?;
        let opts = self.window_options(win)?;
        let buf_id = self.window_buffer(win)?;
        let buf = &self.buffers.get(buf_id).buffer;
        let line_count = buf.line_count();
        // The full left gutter — number column **plus** the sign column. The sign
        // width is the window's last-rendered one (`window_textoff`), so a click
        // skips the same dynamic gutter the client drew; using only the number
        // width shifts every column right by the sign column (see the
        // dynamic-sign-column mouse test).
        let gutter = self
            .window_textoff(win)
            .unwrap_or_else(|| self.number_width_for(&opts, line_count));
        let ts = buf.options.effective_tabstop();
        // The soft-wrap text width: the content area past the number gutter (the
        // same `width` the view wraps into). Under `nowrap` wrapping is skipped, so
        // this is unused; the horizontal `leftcol` scroll applies instead.
        let wrap = opts.wrap;
        let (text_width, _) = self.window_text_area(win)?;
        let width = text_width.saturating_sub(gutter);
        let wp = opts.wrap_prefix();
        let virt = buf.virt_lines_by_line();

        // Walk display rows from `top`, counting each buffer line's `virt_lines`
        // rows and soft-wrap segments, until the target screen row `rel_y` is
        // reached. Without wrapping / virtual lines this is one row per line and
        // resolves to `top + rel_y`, the simple inverse.
        let mut line = top;
        let mut remaining = rel_y;
        let seg;
        let indent; // continuation-prefix cells baked onto the resolved row
        loop {
            if line >= line_count {
                // A cell below the last line lands on the last line's last segment
                // (vim's "click past the buffer end" behavior).
                line = line_count.saturating_sub(1);
                let (segs, ci) = wrap_segs(&buf.line(line), ts, width, wrap, wp);
                seg = *segs.last().expect("wrap_segs is never empty");
                indent = if seg.start_col > 0 { ci } else { 0 };
                break;
            }
            let v = virt.get(&line);
            // `virt_lines` drawn above / below the line are non-text rows; a click
            // on one resolves to the owning buffer line at column 0.
            let above = v.map_or(0, |r| r.above.len());
            if remaining < above {
                return Some((line, 0));
            }
            remaining -= above;
            let (segs, ci) = wrap_segs(&buf.line(line), ts, width, wrap, wp);
            if remaining < segs.len() {
                seg = segs[remaining];
                indent = if seg.start_col > 0 { ci } else { 0 };
                break;
            }
            remaining -= segs.len();
            let below = v.map_or(0, |r| r.below.len());
            if remaining < below {
                return Some((line, 0));
            }
            remaining -= below;
            line += 1;
        }

        let col = if rel_x < gutter {
            // The number column: place the cursor at the line's start.
            0
        } else if wrap {
            // Within the wrapped row: a click on a continuation row's baked-in
            // `'breakindent'`/`'showbreak'` prefix lands on the segment's first
            // byte; past it, the row-local cell is rebased onto the segment's start
            // column before mapping back to a byte (clamped to the segment so it
            // can't spill onto the next display row).
            let cell = rel_x - gutter;
            if cell < indent {
                seg.start_byte
            } else {
                let screen_col = seg.start_col + (cell - indent);
                crate::unicode::byte_at_virtcol(&buf.line(line), screen_col, ts).min(seg.end_byte)
            }
        } else {
            // Screen column within the text, undoing the horizontal scroll, then
            // mapped back to a byte offset (rounding a between-cells click to the
            // nearest grapheme). `set_window_cursor`'s clamp pulls a past-EOL
            // result onto the last char in Normal mode.
            let screen_col = (rel_x - gutter).saturating_add(leftcol);
            crate::unicode::byte_at_virtcol(&buf.line(line), screen_col, ts)
        };
        Some((line, col))
    }

    /// The absolute screen top-left of window `win`: its region's tree origin (from
    /// [`Editor::region_geoms`]) plus its region-relative rect — the same place
    /// every client paints it. `None` for an unknown window.
    fn window_screen_pos(&self, win: WindowId) -> Option<(usize, usize)> {
        let (ox, oy) = self.window_region_origin(win)?;
        let (wx, wy, _, _) = self.window_rect(win)?;
        Some((ox.saturating_add(wx), oy.saturating_add(wy)))
    }

    /// The global screen origin (top-left) of the window-tree area of the region
    /// (layer) that `win` belongs to — the cell a client offsets that region's
    /// region-relative geometry by (past the region's own tabline row, dock band,
    /// and the global chrome). `None` for an unknown id. The server uses it to bound
    /// the completion docs sidebar's region-relative box by the editor edges and to
    /// map it into the global cells the wheel hit-test compares against.
    pub fn window_region_origin(&self, win: WindowId) -> Option<(usize, usize)> {
        let (layer, _) = self.tree_of_window(win)?;
        self.region_geoms()
            .into_iter()
            .find(|g| g.layer == layer)
            .map(|g| (g.tree.0, g.tree.1))
    }
}

/// Find the window in `tree` whose on-screen content area contains the
/// **tree-relative** cell `(x, y)`, returning its id and the cell made
/// **content-relative** (past a bordered float's border). Floats are tested first,
/// top-most by z-order (`floats` is sorted bottom-to-top), then the tiled windows;
/// this matches the paint order so the cell resolves to the window drawn on top.
/// `None` when the cell is on a separator or outside every window. `tree` lays out
/// at its own origin `(0, 0)`, so the caller subtracts the region's screen origin
/// before calling (see [`Editor::region_at`]).
/// The vim mouse-modifier string for a click — `'s'` (shift), `'c'` (ctrl),
/// `'a'` (alt) in that order, empty when none are held. The form the status-line
/// / tabline `%@` click regions carry.
fn mouse_modifier_str(shift: bool, ctrl: bool, alt: bool) -> String {
    let mut modifiers = String::new();
    if shift {
        modifiers.push('s');
    }
    if ctrl {
        modifiers.push('c');
    }
    if alt {
        modifiers.push('a');
    }
    modifiers
}

/// Whether the screen cell `(col, row)` lies inside the `w`×`h` rect anchored at
/// `(x, y)` — the shared point-in-rect test for the float / menu / docs-sidebar
/// hit-testing.
fn rect_contains(x: usize, y: usize, w: usize, h: usize, col: usize, row: usize) -> bool {
    (x..x.saturating_add(w)).contains(&col) && (y..y.saturating_add(h)).contains(&row)
}

fn window_at_in(tree: &WindowTree, x: usize, y: usize) -> Option<(WindowId, usize, usize)> {
    let probe = |id: WindowId| -> Option<(WindowId, usize, usize)> {
        let w = tree.get(id);
        // A bordered float spends one cell per side on its border; its content
        // is the rect inset by one. Tiled windows and borderless floats use the
        // whole rect. `'padding'` insets the content box a further per-side margin,
        // so a click in the margin matches no window (returns past this probe), and
        // the returned cell is **padded-content-relative** — the coordinate
        // `text_cell_to_buf` / the status-row check expect.
        let inset = matches!(&w.float, Some(c) if c.border != BorderStyle::None) as usize;
        let pad = w.options.padding;
        let r = w.rect;
        let x0 = r.x.saturating_add(inset).saturating_add(pad.left);
        let y0 = r.y.saturating_add(inset).saturating_add(pad.top);
        let x1 =
            r.x.saturating_add(r.width)
                .saturating_sub(inset.saturating_add(pad.right));
        let y1 =
            r.y.saturating_add(r.height)
                .saturating_sub(inset.saturating_add(pad.bottom));
        (x >= x0 && x < x1 && y >= y0 && y < y1).then(|| (id, x - x0, y - y0))
    };
    tree.floats
        .iter()
        .rev()
        .copied()
        .chain(tree.leaves())
        .find_map(probe)
}

/// The soft-wrap display segments of `text` and the continuation-prefix width that
/// rebases overlay columns on its continuation rows — the inverse-side companion of
/// the view's `row_skeleton` wrap split. Under `nowrap` (or `width == 0`) the whole
/// line is one segment spanning byte `0..len` at column 0 with no prefix.
fn wrap_segs(
    text: &str,
    tabstop: usize,
    width: usize,
    wrap: bool,
    wp: crate::unicode::WrapPrefix,
) -> (Vec<crate::unicode::WrapSeg>, usize) {
    if wrap && width > 0 {
        let indent = crate::unicode::cont_indent(text, tabstop, width, wp);
        (
            crate::unicode::wrap_segments_indented(text, tabstop, width, indent),
            indent,
        )
    } else {
        (
            vec![crate::unicode::WrapSeg {
                start_byte: 0,
                end_byte: text.len(),
                start_col: 0,
            }],
            0,
        )
    }
}

/// One open region's absolute on-screen placement this frame (see
/// [`Editor::region_geoms`]): where its window tree paints and, when shown, its own
/// tabline strip.
struct RegionGeom {
    layer: Layer,
    /// Absolute `(x, y, width, height)` of the region's window-tree area — below its
    /// own tabline row, the rect the tree lays out into.
    tree: (usize, usize, usize, usize),
    /// Absolute `(row, x_start, width)` of this region's tabline strip, or `None`
    /// when it isn't shown this frame.
    tabline: Option<(usize, usize, usize)>,
}

/// Display width of one built-in tabline cell — the screen columns the client's
/// `render_tabline` paints for this tab. Mirrors that formatter exactly:
/// ` {count}{name}{+} ` — a leading and trailing space, the window count (with a
/// trailing space) only when the tab holds more than one window, and a `+` when
/// its buffer is modified. The two must stay in lockstep so [`Editor::region_tabline_at`]
/// maps a click to the tab it visually covers.
fn tab_cell_width(label: &TabLabel) -> usize {
    let count = if label.window_count > 1 {
        format!("{} ", label.window_count)
    } else {
        String::new()
    };
    let modified = if label.modified { "+" } else { "" };
    crate::unicode::display_width(&format!(" {count}{}{modified} ", label.name))
}
