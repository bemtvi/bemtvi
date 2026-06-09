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
use crate::input::{MouseAction, MouseButton, MouseEvent};

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
/// a split divider grabs the window edge next to it; subsequent drags resize that
/// window to follow the pointer. The resize is **absolute** against the press
/// `origin` (not incremental), so pushing past a window's minimum and dragging
/// back tracks the pointer cleanly instead of drifting.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ResizeDrag {
    /// The window whose edge is grabbed — the one *left of* a vertical divider or
    /// *above* a horizontal one, so a drag toward it (right / down) grows it.
    win: WindowId,
    /// The divider's orientation: `true` for a vertical separator (resize width),
    /// `false` for a horizontal separator or a status line (resize height).
    vertical: bool,
    /// The press cell along the drag axis (the column for a vertical divider, the
    /// row for a horizontal one), the fixed point the drag measures from.
    origin: usize,
    /// Total cells already applied to the resize, so each drag issues only the
    /// remaining delta to reach the pointer's current offset from `origin`.
    applied: isize,
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
    /// The window's status row (its bottom line). No-op until split-resize drag
    /// lands; modeled now so a status-line click doesn't fall through to the text
    /// below it.
    StatusLine { win: WindowId },
}

impl Editor {
    /// Apply a mouse gesture. A no-op when `'mouse'` does not enable the current
    /// mode (vim-faithful — the gesture is simply ignored, not an error). Only the
    /// gestures implemented so far act; the rest are no-ops until their phase.
    pub fn mouse(&mut self, ev: MouseEvent) {
        if !self.mouse_enabled() {
            return;
        }
        match (ev.button, ev.action) {
            // In multi-cursor placement mode a left-click *toggles* a cursor at the
            // clicked cell — drop one if it's bare, remove it if one is there — the
            // mouse form of the `c` placement command. Drag/release don't place.
            (MouseButton::Left, MouseAction::Press) if self.mode == Mode::MultiCursor => {
                self.mouse_toggle_cursor(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Drag | MouseAction::Release)
                if self.mode == Mode::MultiCursor => {}
            // A press on a shown tabline switches to the clicked tab (vim's
            // tabline click). Resolved before the text-press arms so a tab click
            // never places a cursor or starts a selection, and it doesn't go
            // through the window hit-test at all (the tabline is chrome, not a
            // window). Drag/release on the tabline do nothing.
            (MouseButton::Left, MouseAction::Press)
                if self.tabline_tab_at(ev.row, ev.col).is_some() =>
            {
                self.mouse_click_tab(ev.row, ev.col)
            }
            // A press on a split divider (a separator or a status line with a
            // window below it) grabs that edge; drags resize, release lets go.
            // Checked before the text-press arms so a divider click never places
            // the cursor or starts a selection.
            (MouseButton::Left, MouseAction::Press)
                if self.resize_handle_at(ev.row, ev.col).is_some() =>
            {
                self.mouse_begin_resize(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Drag) if self.mouse_resize.is_some() => {
                self.mouse_resize_drag(ev.row, ev.col)
            }
            (MouseButton::Left, MouseAction::Release) if self.mouse_resize.is_some() => {
                self.mouse_resize = None
            }
            // Shift+left-press extends the selection to the click (vim's
            // `<S-LeftMouse>` under the default `popup_setpos` mousemodel) instead
            // of placing the cursor and starting fresh.
            (MouseButton::Left, MouseAction::Press) if ev.shift => {
                self.mouse_left_extend(ev.row, ev.col, ev.stamp_ms)
            }
            (MouseButton::Left, MouseAction::Press) => {
                self.mouse_left_press(ev.row, ev.col, ev.stamp_ms)
            }
            (MouseButton::Left, MouseAction::Drag) => self.mouse_left_drag(ev.row, ev.col),
            // Release ends the drag but keeps `mouse_select` so the next press can
            // still see this one for multi-click counting; vim keeps the selection.
            (MouseButton::Left, MouseAction::Release) => {}
            // Right-press is dispatched by `'mousemodel'` (extend the selection, or
            // move the cursor / pop a — deferred — menu). Drag/release do nothing.
            (MouseButton::Right, MouseAction::Press) => {
                self.mouse_right_press(ev.row, ev.col, ev.stamp_ms)
            }
            (MouseButton::Right, MouseAction::Drag | MouseAction::Release) => {}
            // Middle-press pastes the `"*` clipboard register at the click (vim's
            // `gP`). Drag/release do nothing.
            (MouseButton::Middle, MouseAction::Press) => self.mouse_middle_press(ev.row, ev.col),
            (MouseButton::Middle, MouseAction::Drag | MouseAction::Release) => {}
            // The wheel scrolls the window *under the pointer* without moving focus
            // or (unless a line scrolls off) the cursor.
            (MouseButton::Wheel, action) => self.mouse_wheel(action, ev.row, ev.col, ev.shift),
            // The remaining buttons (X1/X2) and bare moves have no binding; ignore.
            _ => {}
        }
    }

    /// Left-press: focus the clicked window, place the cursor, and start a
    /// selection sized by the click count — single = char (vim's `<LeftMouse>`),
    /// double = the word, triple = the line. A same-cell press within `'mousetime'`
    /// of the last escalates the count; otherwise it resets to one. An active
    /// Visual selection is torn down first (also vim's behavior). For a single
    /// click no selection starts until the first drag; double/triple enter Visual
    /// immediately.
    fn mouse_left_press(&mut self, row: usize, col: usize, stamp_ms: u64) {
        let Some(MouseTarget::Text {
            win,
            line,
            col: bcol,
        }) = self.hit_test(row, col)
        else {
            // A press outside any window clears the gesture (and resets the count).
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
        let anchor = match count {
            1 => SelectAnchor::Char(self.cursor),
            2 => self.mouse_select_word(),
            _ => self.mouse_select_line(),
        };
        self.mouse_select = Some(MouseSelect {
            row,
            col,
            stamp_ms,
            count,
            anchor,
        });
    }

    /// If the global cell `(row, col)` lands on a built-in tabline cell, the
    /// 0-based index (in tabline order) of the tab whose cell contains it.
    /// `None` when:
    ///
    /// - the tabline isn't shown ([`Editor::tabline_visible`]) or the cell isn't
    ///   on its row;
    /// - a custom `'tabline'` is in effect — its cells carry no built-in click
    ///   regions (vim needs explicit `%nT` items there, which we don't model), so
    ///   clicking it is a no-op;
    /// - the cell is on the blank fill past the last tab (vim's `TabLineFill`).
    ///
    /// The cell widths must stay in lockstep with what the client paints in
    /// `render_tabline` (`nxvim-tui`): one cell per tab, ` {count}{name}{+} ` —
    /// the window count (with a trailing space) only when >1, a `+` when the tab's
    /// buffer is modified, a space on each side — so a click lands on the tab it
    /// visually covers.
    fn tabline_tab_at(&self, row: usize, col: usize) -> Option<usize> {
        if row >= self.tabline_rows() {
            return None; // no tabline shown, or the cell is below it (a window).
        }
        if !self.global_options().tabline.is_empty() {
            return None; // a custom tabline has no built-in click regions.
        }
        let mut x = 0;
        for (i, label) in self.tab_labels().into_iter().enumerate() {
            let width = tab_cell_width(&label);
            if (x..x + width).contains(&col) {
                return Some(i);
            }
            x += width;
        }
        None
    }

    /// Switch to the tab whose tabline cell holds the click. Focus moves to that
    /// tab's focused window (vim's tabline click); a click on the already-active
    /// tab is a no-op. No cursor is placed in any text window — the press is
    /// consumed by the tabline.
    fn mouse_click_tab(&mut self, row: usize, col: usize) {
        if let Some(idx) = self.tabline_tab_at(row, col) {
            self.set_current_tabpage(self.tab_ids()[idx]);
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
    fn resize_handle_at(&self, row: usize, col: usize) -> Option<(WindowId, bool)> {
        let top_chrome = self.tabline_rows();
        if row < top_chrome {
            return None;
        }
        let (x, y) = (col, row - top_chrome);
        for sep in self.separators() {
            if sep.vertical {
                if x == sep.x && y >= sep.y && y < sep.y + sep.length {
                    // The window left of the divider grows when dragged right.
                    let (win, ..) = self.window_at(sep.x.checked_sub(1)?, y)?;
                    return Some((win, true));
                }
            } else if y == sep.y && x >= sep.x && x < sep.x + sep.length {
                // The window above the divider grows when dragged down.
                let (win, ..) = self.window_at(x, sep.y.checked_sub(1)?)?;
                return Some((win, false));
            }
        }
        // Not on a separator: a window's own status row is a drag handle too, but
        // only when a horizontal separator sits just below it — otherwise it is the
        // bottom-most window and there is nothing beneath to resize against.
        let (win, _, rel_y) = self.window_at(x, y)?;
        let (_, text_height) = self.window_content_size(win)?;
        if rel_y == text_height {
            let below = y + 1;
            let has_window_below = self
                .separators()
                .iter()
                .any(|s| !s.vertical && s.y == below && x >= s.x && x < s.x + s.length);
            if has_window_below {
                return Some((win, false));
            }
        }
        None
    }

    /// Begin a separator / status-line drag: stash which window edge is grabbed and
    /// the press cell the resize measures from. Clears any pending text selection so
    /// the divider press can't leave a stale anchor behind. A no-op (leaving
    /// `mouse_resize` unset) if the cell isn't actually a divider — the dispatch
    /// guard already checked, so this only re-resolves it.
    fn mouse_begin_resize(&mut self, row: usize, col: usize) {
        let Some((win, vertical)) = self.resize_handle_at(row, col) else {
            return;
        };
        self.mouse_select = None;
        self.mouse_resize = Some(ResizeDrag {
            win,
            vertical,
            origin: if vertical { col } else { row },
            applied: 0,
        });
    }

    /// Continue a separator / status-line drag: resize the grabbed window so its
    /// edge follows the pointer. The target offset from the press `origin` is
    /// absolute, and `applied` records how much has been issued, so each drag sends
    /// only the remaining delta — pushing past a window's minimum and dragging back
    /// tracks the pointer instead of drifting.
    fn mouse_resize_drag(&mut self, row: usize, col: usize) {
        let Some(rd) = self.mouse_resize else {
            return;
        };
        let current = if rd.vertical { col } else { row };
        let want = current as isize - rd.origin as isize;
        let step = want - rd.applied;
        if step == 0 {
            return;
        }
        let axis = if rd.vertical {
            SplitDir::Vertical
        } else {
            SplitDir::Horizontal
        };
        self.resize_window_id(rd.win, axis, step);
        if let Some(rd) = self.mouse_resize.as_mut() {
            rd.applied = want;
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
        let (wx, wy, ww, _) = self.window_rect(win)?;
        let (_, text_height) = self.window_content_size(win)?;
        // The window's text band in global screen rows: `[top_edge, bottom_edge]`.
        let top_edge = self.tabline_rows() + wy;
        let bottom_edge = top_edge + text_height.saturating_sub(1);
        let rel_y = if row <= top_edge {
            self.drag_scroll(false); // at/above the first line → reveal the line above
            0
        } else if row >= bottom_edge {
            self.drag_scroll(true); // at/below the last line → reveal the line below
            text_height.saturating_sub(1)
        } else {
            row - top_edge
        };
        let rel_x = col.saturating_sub(wx).min(ww.saturating_sub(1));
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
            let bottom = self.top + th.saturating_sub(1);
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
            let old_top = self.windows.get(win).saved_top;
            let new_top = (old_top as i64 + delta).clamp(0, last as i64) as usize;
            if new_top == old_top {
                return;
            }
            let bottom = new_top + th.saturating_sub(1);
            let w = self.windows.get_mut(win);
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
            let w = self.windows.get_mut(win);
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
            self.window_content_size(win),
            self.window_buffer(win),
            self.window_options(win),
        ) else {
            return 0;
        };
        let buf = &self.buffers.get(buf_id).buffer;
        let line_count = buf.line_count();
        let text_w = content_w.saturating_sub(self.number_width_for(opts, line_count));
        let ts = buf.options.effective_tabstop();
        let widest = (top..(top + text_h).min(line_count))
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
        let opts = self.windows.cur().options;
        let so = opts.sidescrolloff.min(tw.saturating_sub(1) / 2);
        let lo = self.leftcol + so;
        let hi = (self.leftcol + tw).saturating_sub(so + 1);
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
    /// col)`, or `None` when the cell is on the tabline, a window separator, or
    /// outside every window. Unlike [`hit_test`](Self::hit_test) this stops at the
    /// window — the wheel needs only *which* window to scroll, not a buffer cell.
    fn window_at_cell(&self, row: usize, col: usize) -> Option<WindowId> {
        let top_chrome = self.tabline_rows();
        if row < top_chrome {
            return None;
        }
        self.window_at(col, row - top_chrome).map(|(win, ..)| win)
    }

    /// Window `win`'s text-area height in rows (its content height minus the status
    /// line), at least 1 — the page size for a `Shift`+wheel notch.
    fn window_text_height(&self, win: WindowId) -> usize {
        self.window_content_size(win).map_or(1, |(_, h)| h).max(1)
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
            // MultiCursor is a normal-like placement mode (its `mode()` code is
            // `n`), so it gates on the same `n` flag.
            Mode::Normal | Mode::MultiCursor => 'n',
            Mode::Visual | Mode::VisualLine => 'v',
            Mode::Insert | Mode::Replace => 'i',
            Mode::Command => 'c',
        };
        m.contains(flag)
    }

    /// Resolve a **global** screen cell `(row, col)` to a [`MouseTarget`], or
    /// `None` if it lands on no actionable region (the tabline, a separator, the
    /// panel, or outside every window). This is the reverse of the forward layout:
    /// strip the top chrome (the tabline row), find the window under the cell,
    /// then turn the window-relative cell into a buffer line/col through that
    /// window's scroll offset, number gutter, and tab/wide-char column math.
    fn hit_test(&self, row: usize, col: usize) -> Option<MouseTarget> {
        // The windows area sits below the tabline (when shown); the bottom chrome
        // (panel, command line) is already excluded by the window rects, so a cell
        // there simply matches no window. Columns are not inset (the area is full
        // width).
        let top_chrome = self.tabline_rows();
        if row < top_chrome {
            return None; // the tabline — a later phase resolves tab clicks
        }
        let (win, rel_x, rel_y) = self.window_at(col, row - top_chrome)?;
        let (_, text_height) = self.window_content_size(win)?;
        if rel_y >= text_height {
            // Below the text body: the status row (the last content line).
            return Some(MouseTarget::StatusLine { win });
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
        // A cell below the last line lands on the last line (vim's behavior).
        let line = (top + rel_y).min(line_count.saturating_sub(1));
        let gutter = self.number_width_for(opts, line_count);
        let col = if rel_x < gutter {
            // The number column: place the cursor at the line's start.
            0
        } else {
            // Screen column within the text, undoing the horizontal scroll, then
            // mapped back to a byte offset (rounding a between-cells click to the
            // nearest grapheme). `set_window_cursor`'s clamp pulls a past-EOL
            // result onto the last char in Normal mode.
            let screen_col = (rel_x - gutter) + leftcol;
            let text = buf.line(line);
            crate::unicode::byte_at_virtcol(&text, screen_col, buf.options.effective_tabstop())
        };
        Some((line, col))
    }

    /// Find the window whose on-screen content area contains the windows-area cell
    /// `(x, y)`, returning its id and the cell made **content-relative** (past a
    /// bordered float's border). Floats are tested first, top-most by z-order
    /// (`floats` is sorted bottom-to-top), then the tiled windows; this matches
    /// the paint order so the cell resolves to the window drawn on top. `None`
    /// when the cell is on a separator or outside every window.
    fn window_at(&self, x: usize, y: usize) -> Option<(WindowId, usize, usize)> {
        let probe = |id: WindowId| -> Option<(WindowId, usize, usize)> {
            let w = self.windows.get(id);
            // A bordered float spends one cell per side on its border; its content
            // is the rect inset by one. Tiled windows and borderless floats use the
            // whole rect.
            let inset = matches!(&w.float, Some(c) if c.border != BorderStyle::None) as usize;
            let r = w.rect;
            let x0 = r.x + inset;
            let y0 = r.y + inset;
            let x1 = (r.x + r.width).saturating_sub(inset);
            let y1 = (r.y + r.height).saturating_sub(inset);
            (x >= x0 && x < x1 && y >= y0 && y < y1).then(|| (id, x - x0, y - y0))
        };
        self.windows
            .floats
            .iter()
            .rev()
            .copied()
            .chain(self.windows.leaves())
            .find_map(probe)
    }
}

/// Display width of one built-in tabline cell — the screen columns the client's
/// `render_tabline` paints for this tab. Mirrors that formatter exactly:
/// ` {count}{name}{+} ` — a leading and trailing space, the window count (with a
/// trailing space) only when the tab holds more than one window, and a `+` when
/// its buffer is modified. The two must stay in lockstep so [`Editor::tabline_tab_at`]
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
