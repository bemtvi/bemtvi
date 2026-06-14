//! The renderable view of the editor: semantic regions, not a baked grid.
//!
//! The core no longer lays out a flat screen (status/command lines are not
//! painted into text rows). Instead it produces a [`View`] describing *what* to
//! show in each region, and the client arranges those regions with its own
//! widgets. This keeps layout and styling a UI concern while the core stays the
//! single source of truth for content, scrolling, and cursor placement.
//!
//! Columns are byte offsets (ropey's native metric and vim's column model);
//! `cursor_screen_col` additionally carries the cursor's screen-cell column,
//! accounting for wide characters and tabs.

use crate::buffer::Buffer;
use crate::editor::{BorderStyle, BufferId, Cursor, Editor, MenuPlacement, TabLabel, WindowLayout};
use crate::mode::Mode;
use crate::statusline::StatuslineCtx;
use crate::unicode;

/// A scroll gesture for the client to animate. Self-contained: it carries its
/// own band of rendered lines (`lines`) and selection spans covering every row
/// visible during the slide, anchored at `base_line`. The client interpolates
/// `from`→`to` against its local clock and slices `lines` per frame; the main
/// `View` fields stay the *destination* viewport for clients that don't animate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnim {
    pub from_top: usize,
    pub to_top: usize,
    pub from_cursor: usize,
    pub to_cursor: usize,
    pub duration_ms: u64,
    /// Buffer-line index of `lines[0]` (= `min(from_top, to_top)`).
    pub base_line: usize,
    /// `|to_top - from_top| + height` rows starting at `base_line`, "~"-padded
    /// past end of buffer.
    pub lines: Vec<String>,
    /// Selection spans aligned with `lines` (same length), covering the
    /// **maximal** extent the slide touches (anchor → the scroll endpoint
    /// furthest from the anchor), so the client can grow *and* shrink the
    /// highlight to the interpolated cursor. See [`sel_extends_down`].
    ///
    /// [`sel_extends_down`]: ScrollAnim::sel_extends_down
    pub selection: Vec<Option<(usize, usize)>>,
    /// Orientation of the visual selection sliding with the band, used by the
    /// client to clip the highlight's moving edge to the interpolated cursor:
    /// `Some(true)` when the anchor is at/above the cursor (selection extends
    /// downward, so rows below the cursor aren't selected yet), `Some(false)`
    /// when it extends upward, `None` when no visual selection is sliding.
    pub sel_extends_down: Option<bool>,
    /// 1-based buffer line number per row (aligned with `lines`), `None` for
    /// `~` filler rows, so the number column slides with the text during the
    /// animation.
    pub numbers: Vec<Option<usize>>,
    /// Per band row (aligned with `lines`), the half-open screen-column spans of
    /// every `hlsearch` match — so the search highlight rides the slide instead of
    /// vanishing until it settles. Empty inner vec for rows with no match.
    pub search: Vec<Vec<(usize, usize)>>,
    /// Per band row, the live `incsearch` preview match, or `None` — carried for
    /// the same reason as [`search`](ScrollAnim::search) (a scroll while the search
    /// prompt is open).
    pub incsearch: Vec<Option<(usize, usize)>>,
}

/// The renderable form of the bottom [`Panel`](crate::editor): a title, the
/// visible slice of its content, the cursor's row within that slice, and the
/// content height the client lays the panel out to. `None` in [`View::panel`]
/// when no panel is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelView {
    /// Label shown in the panel's title bar (e.g. `Messages`, `Buffers`).
    pub title: String,
    /// The visible content rows (already scrolled and **word-wrapped** to the
    /// panel width); never longer than `height`. The client pads shorter content
    /// with blank rows. A long logical entry occupies several consecutive rows.
    pub lines: Vec<String>,
    /// First display row (within the visible slice) of the selected logical
    /// entry. The client places the editing cursor here.
    pub cursor_row: usize,
    /// Number of consecutive display rows the selected entry occupies in the
    /// visible slice (≥ 1 — more than one when the entry wrapped). The client
    /// highlights `cursor_row .. cursor_row + cursor_span` as the focused line, so
    /// the whole wrapped entry reads as selected.
    pub cursor_span: usize,
    /// Content height in rows (excludes the title row). The client lays the
    /// whole panel out as `height + 1` rows; the editor sized it so the text
    /// window keeps at least one row.
    pub height: usize,
}

/// The renderable form of the floating selectable-list [`Menu`](crate::editor):
/// the choice labels, the highlighted index, and where it floats. The server
/// projects the on-screen geometry (anchor + size) from this plus the focused
/// window, the same way it places the completion popup. `None` in [`View::menu`]
/// when no menu is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuView {
    /// The choice labels, in order, each shown on its own row.
    pub items: Vec<String>,
    /// The highlighted index into `items` (0-based; always in range — a menu is
    /// never opened empty).
    pub selected: usize,
    /// Whether the menu floats under the cursor or centered over the editor.
    pub placement: MenuPlacement,
}

/// A rectangle in screen cells, relative to the **windows area** (the region the
/// window tree lays out into — the frame minus the global bottom panel and
/// command line). The client offsets it by that area's origin when painting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViewRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

/// Which screen region a window (or separator) belongs to: the main editor area,
/// or one of the four permanent docks. A window's `rect`/separator coordinates are
/// relative to its region's own origin (each region lays out at `(0, 0)`); the
/// client maps the region to its absolute screen origin using the [`View`]'s dock
/// band sizes. `Main` is the default and the only region when no dock is open, so
/// a dock-free session renders exactly as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowRegion {
    #[default]
    Main,
    DockLeft,
    DockRight,
    DockTop,
    DockBottom,
}

/// A split border between sibling windows: a vertical `│` run (between the
/// columns of a vertical split) or a horizontal `─` run (between the rows of a
/// horizontal split), anchored at `(x, y)` in its [`region`](Separator::region)'s
/// cells and `length` cells long. The core computes these from the layout tree;
/// the client only paints them (offsetting by the region origin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Separator {
    pub vertical: bool,
    pub x: usize,
    pub y: usize,
    pub length: usize,
    /// The region this separator's `(x, y)` is relative to.
    pub region: WindowRegion,
}

/// One window's renderable content: its screen `rect`, whether it holds focus,
/// and every field that is per-window (text, cursor, selection, search, gutter,
/// status-line data, and any scroll gesture). The client paints each window at
/// its `rect` with its own gutter, text body, and a status line on its bottom
/// row; the terminal cursor is drawn only in the `focused` window. With a single
/// window this is the whole text area — identical to the pre-windows view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowView {
    /// Where this window sits within its [`region`](WindowView::region) (origin
    /// `(0, 0)` per region; the client offsets by the region's screen origin).
    pub rect: ViewRect,
    /// Which screen region (main area or a dock) this window belongs to.
    pub region: WindowRegion,
    /// The buffer this window shows. The server projects this buffer's syntax /
    /// diagnostics slice for the window (two windows onto the same buffer share
    /// one `SyntaxState`, each slicing its own `(top, height)`).
    pub buffer: BufferId,
    /// Whether this window holds focus (the terminal cursor is drawn here).
    pub focused: bool,
    /// Visible text rows (the window's text body — `rect.height - 1` rows, the
    /// last row being its status line). Rows past the buffer are the literal
    /// `"~"`, as in vim.
    pub lines: Vec<String>,
    /// Cursor row relative to the top of this window's text body.
    pub cursor_row: usize,
    /// First visible screen column (horizontal scroll offset) under `nowrap`. `0`
    /// unless a long line scrolled the viewport right. The client drops this many
    /// leading screen cells from each row and shifts the cursor and every span
    /// left by it; the number gutter is *not* offset.
    pub leftcol: usize,
    /// Cursor byte/column offset within its line (for the ruler and
    /// `nvim_win_get_cursor`).
    pub cursor_col: usize,
    /// Cursor's screen-cell column on its line (wide-char and tab aware), for
    /// placing the terminal cursor.
    pub cursor_screen_col: usize,
    /// Secondary (multi-)cursors visible in this window, each as `(row, screen
    /// col)` relative to the top of the text body — the primary cursor is carried
    /// separately by [`cursor_row`]/[`cursor_screen_col`]. Empty for an
    /// unfocused window or when no multi-cursors are active.
    ///
    /// [`cursor_row`]: WindowView::cursor_row
    /// [`cursor_screen_col`]: WindowView::cursor_screen_col
    pub secondary_cursors: Vec<(usize, usize)>,
    /// File name for this window's status line (`"[No Name]"` when unset).
    pub file_name: String,
    /// The buffer's **effective** treesitter filetype/language — the override
    /// (`:set filetype=…`, `vim.treesitter.start`) when set, otherwise the
    /// extension-derived language, otherwise empty. The server highlights
    /// in-process so it ignores this, but the serverless web build picks its
    /// front-end grammar from it (preferring it over the file extension), which
    /// is how `:set ft=…` highlights a buffer the extension table misses.
    pub filetype: String,
    /// Whether this window's buffer has no file path yet (a fresh `[No Name]`
    /// buffer), so a write needs a target. Sent to clients as an explicit flag so
    /// a GUI can route a bare `:w` to its save dialog without matching `file_name`.
    pub unnamed: bool,
    pub modified: bool,
    /// 1-based cursor line, for this window's status-line ruler.
    pub cursor_line: usize,
    /// Per visible row (aligned with `lines`), the half-open screen-column span
    /// `[start, end)` to paint as the visual-mode selection, or `None`. `end` may
    /// exceed the row's text width to mark a selected newline (one extra cell) or
    /// to fill a linewise selection to the window's text edge.
    pub selection: Vec<Option<(usize, usize)>>,
    /// Per visible row, the half-open screen-column spans of every **secondary**
    /// multi-cursor's visual selection (the primary's lives in [`selection`]).
    /// Painted with the same `Visual` style; empty inner vecs for rows no
    /// secondary selection touches, and empty everywhere outside a visual mode.
    /// Mirrors the shape of [`search`] so a row can carry several disjoint
    /// selections (one cursor per).
    ///
    /// [`selection`]: WindowView::selection
    /// [`search`]: WindowView::search
    pub secondary_selection: Vec<Vec<(usize, usize)>>,
    /// Per visible row, the half-open screen-column spans of every search match
    /// (`Search`/`hlsearch`). Empty inner vecs for rows with no match.
    pub search: Vec<Vec<(usize, usize)>>,
    /// Per visible row, the single match the live `incsearch` preview rests on,
    /// or `None`.
    pub incsearch: Vec<Option<(usize, usize)>>,
    /// Present only on a redraw caused by a scroll command that moved this
    /// window's viewport; carries the data a client needs to animate the slide.
    pub scroll: Option<ScrollAnim>,
    /// 1-based buffer line number per visible row (aligned with `lines`), or
    /// `None` for `~` filler rows. The client formats the number column from
    /// these.
    pub numbers: Vec<Option<usize>>,
    /// `:set number` — show the absolute line number.
    pub number: bool,
    /// `:set relativenumber` — show numbers relative to the cursor line.
    pub relativenumber: bool,
    /// Width in cells of the number column (`0` when both options are off).
    pub number_width: usize,
    /// This window's buffer `tabstop`: the width the client must expand a `\t`
    /// to, so its tab rendering matches the server's [`cursor_screen_col`] (which
    /// is computed with this same value). A client that hard-codes a different
    /// width would misplace the cursor over tabbed lines.
    ///
    /// [`cursor_screen_col`]: WindowView::cursor_screen_col
    pub tabstop: usize,
    /// Whether this window is a **float**: drawn on top of the tiled windows at
    /// its absolute `rect`. The client paints floats in a second, on-top pass (in
    /// list order, which is z-order); a tiled window is `false`.
    pub floating: bool,
    /// The float's border style (`None` for a tiled window or a borderless
    /// float). When set, the client draws the border around `rect` and the inner
    /// content (this view's `lines`/gutter/status) sits one cell inside it — the
    /// projection already sized `lines` to that inset area.
    pub border: BorderStyle,
    /// The float's title, drawn on its top border. `None` when untitled.
    pub title: Option<String>,
    /// Pre-computed facts the `'statusline'` `%`-format engine reads to expand its
    /// built-in fields (`%f`, `%l`, `%y`, …). Built here in core — where the
    /// buffer and viewport live — so the server only has to run the (Lua-aware)
    /// engine over it. See [`crate::statusline`].
    pub status_ctx: StatuslineCtx,
    /// Whether this window paints its own status row, per `'laststatus'`
    /// ([`Editor::window_statusline_visible`]): false hides it (modes `0`/`3`, or
    /// `1` with a single window) and the freed bottom row becomes text. When
    /// false the window's `lines` already fill the extra row, so the client must
    /// not carve a status row off this window's rect.
    ///
    /// [`Editor::window_statusline_visible`]: crate::Editor::window_statusline_visible
    pub status_visible: bool,
}

/// One tab page's cell in the tabline. `label` is the tab's focused window's
/// buffer name (`[No Name]` when unset), `modified` flags unsaved changes, and
/// `window_count` is how many windows the tab holds. The client formats the cell
/// (vim's default `{count}{+} {label}`) and highlights the `current` one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabView {
    pub label: String,
    pub modified: bool,
    pub window_count: usize,
}

/// One region's tabline: its tab cells in tabline order plus the active cell
/// index. Empty `tabs` ⇒ that region draws no tabline (its `showtabline` gate hid
/// it, or — for a dock — it is closed). `current` is meaningful only when `tabs`
/// is non-empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionTabline {
    pub tabs: Vec<TabView>,
    pub current: usize,
    /// A fixed dock title shown at the start of the strip (the `nx.dock` `title`
    /// option), independent of the tab cells. Empty for the main region and for a
    /// dock with no title.
    pub title: String,
}

/// Every region's independent tabline (see [`RegionTabline`]): the main editor
/// area plus the four docks, indexed by `DockSide::idx` (`[left, right, top,
/// bottom]`). Each region carries its own tab pages, so the client draws a tabline
/// at the top of each region's band rather than one global bar.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionTablines {
    pub main: RegionTabline,
    pub docks: [RegionTabline; 4],
}

/// A snapshot of everything a client needs to draw a frame: the **global** chrome
/// (mode label, command line, message, panel) plus the list of [`WindowView`]s to
/// paint. With one window the list has a single entry spanning the whole text
/// area, so the rendered frame matches the pre-windows view exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// The windows to paint, in layout order. Always at least one.
    pub windows: Vec<WindowView>,
    /// The tab pages, in tabline order — **empty** when only one tab is open (the
    /// client then draws no tabline). When non-empty, `current_tab` indexes the
    /// active cell.
    pub tabline: Vec<TabView>,
    /// Index into `tabline` of the active tab. Meaningful only when `tabline` is
    /// non-empty.
    pub current_tab: usize,
    /// Per-region tablines — each region (main + each open dock) has its own
    /// independent tab pages. `region_tablines.main` carries the same cells as the
    /// legacy `tabline`/`current_tab` above (kept until clients migrate); the dock
    /// entries are the new per-dock tablines. See [`RegionTablines`].
    pub region_tablines: RegionTablines,
    /// The split borders between windows, each in its [`Separator::region`]'s
    /// cells. Empty with a single window and no dock.
    pub separators: Vec<Separator>,
    /// The left dock's content width in cells, `0` when closed. A client reserves
    /// `width + 1` columns on the left (the `+1` a separator) and renders
    /// `WindowRegion::DockLeft` windows there. See [`WindowRegion`].
    pub dock_left: usize,
    /// The right dock's content width in cells, `0` when closed.
    pub dock_right: usize,
    /// The top dock's content height in rows, `0` when closed. The top dock sits
    /// **above** the tabline.
    pub dock_top: usize,
    /// The bottom dock's content height in rows, `0` when closed (sits above the
    /// read-only panel).
    pub dock_bottom: usize,
    /// Uppercase mode name for the status line, e.g. `"NORMAL"`.
    pub mode_label: String,
    /// True while in command-line mode; the cursor then belongs to the command
    /// region, which the client owns.
    pub command_mode: bool,
    /// True while `r` waits for its replacement character (a one-shot replace
    /// that stays in normal mode). Clients show the replace cursor shape while it
    /// holds, mirroring vim's operator-pending feedback.
    pub pending_replace: bool,
    /// Command-line contents (text after the leading prompt char).
    pub cmdline: String,
    /// The command-line prompt character: `:` for an ex command, `/` / `?` for a
    /// forward / backward search. Only meaningful while `command_mode`.
    pub cmdline_prefix: char,
    /// The multi-char prompt label for a `vim.ui.input` prompt, shown ahead of
    /// the editable line in place of `cmdline_prefix`. Empty for `:`/`/`/`?`.
    pub cmdline_prompt: String,
    /// Cursor position within `cmdline` as a character count from its start, so
    /// the client can place the command cursor mid-line after `<Left>`/`<Right>`
    /// edits. Only meaningful while `command_mode`.
    pub cmdline_cursor: usize,
    /// Transient status message (shown on the command line when not typing one).
    pub message: String,
    /// The bottom panel (`:messages`, `:ls`), or `None` when none is open. When
    /// present it has input focus, so the client draws the editing cursor inside
    /// the panel rather than the text window. Global — one per editor, below all
    /// windows.
    pub panel: Option<PanelView>,
    /// The floating selectable-list widget (`nx.ui.select`; later the picker),
    /// or `None` when none is open. When present it has input focus and floats
    /// over the focused window's text area; the server projects its geometry.
    pub menu: Option<MenuView>,
    /// The single **global** status line's `%`-format context, present only at
    /// `'laststatus'`=3. It carries the *focused* window's facts (vim shows the
    /// current window's status line in the global bar); the server runs the engine
    /// over it across the full editor width and the client paints it docked one
    /// row above the command line. `None` for modes `0/1/2`, where status lines
    /// are per-window (or hidden) instead.
    pub global_statusline: Option<StatuslineCtx>,
}

impl View {
    pub(crate) fn from_editor(ed: &Editor) -> View {
        let bands = ed.dock_bands();
        let windows: Vec<WindowView> = ed
            .window_layouts()
            .iter()
            .map(|w| window_view(ed, w))
            .collect();
        // At `laststatus=3` the single global status line shows the *focused*
        // window's facts; capture that window's `%`-context for the server to run
        // the engine over (falling back to the first window if none is flagged).
        let global_statusline = ed.global_statusline_visible().then(|| {
            windows
                .iter()
                .find(|w| w.focused)
                .unwrap_or(&windows[0])
                .status_ctx
                .clone()
        });
        View {
            windows,
            tabline: ed.tab_labels().into_iter().map(tab_label_to_view).collect(),
            current_tab: ed.current_tab_index(),
            region_tablines: ed.region_tablines(),
            separators: ed.all_separators(),
            mode_label: ed.mode.label().to_string(),
            command_mode: ed.mode == Mode::Command,
            pending_replace: ed.pending_replace(),
            cmdline: ed.cmdline.clone(),
            cmdline_prefix: ed.cmdline_prefix(),
            cmdline_prompt: ed.cmdline_prompt().to_string(),
            cmdline_cursor: ed.cmdline_cursor(),
            // In terminal-job mode every key goes to the child, so surface the way
            // out where vim shows `-- INSERT --` (unless a real message is up).
            message: if ed.mode == Mode::Terminal && ed.message.is_empty() {
                "-- TERMINAL --  (<C-\\><C-n> or 3×<Esc> to exit)".to_string()
            } else {
                ed.message.clone()
            },
            panel: ed.panel_view(),
            menu: ed.menu_view(),
            global_statusline,
            dock_left: bands.left,
            dock_right: bands.right,
            dock_top: bands.top,
            dock_bottom: bands.bottom,
        }
    }

    /// The focused window — the one the terminal cursor is drawn in, and the
    /// reference point for global overlays (the completion popup, the
    /// under-cursor diagnostic). There is always at least one window; falls back
    /// to the first if somehow none is flagged.
    pub fn focused(&self) -> &WindowView {
        self.windows
            .iter()
            .find(|w| w.focused)
            .unwrap_or(&self.windows[0])
    }
}

/// Project a core [`TabLabel`] into the wire-facing [`TabView`].
pub(crate) fn tab_label_to_view(label: TabLabel) -> TabView {
    TabView {
        label: label.name,
        modified: label.modified,
        window_count: label.window_count,
    }
}

/// Project one window into a [`WindowView`], slicing *its* buffer at *its* view
/// position. The focused window renders the editor's live `cursor`/`top` and owns
/// the transient overlays — the visual selection, the live `incsearch` preview,
/// and the scroll-animation band; the rest render their stashed positions with
/// only the persistent `hlsearch` highlight.
fn window_view(ed: &Editor, w: &WindowLayout) -> WindowView {
    let buf = ed
        .buffer_of(w.buffer)
        .expect("a live window's buffer is always open");
    let line_count = buf.line_count();
    let number_width = ed.number_width_for(w.options, line_count);
    // A bordered float spends one cell on each side on its border, so its content
    // (gutter + text + status) lives in the rect inset by one cell. Tiled windows
    // and borderless floats use the whole rect. The client draws the border on the
    // outer `rect`, then paints this content into the inset area, so the two agree.
    let inset = if w.floating && w.border != BorderStyle::None {
        1
    } else {
        0
    };
    let content_height = w.rect.height.saturating_sub(2 * inset);
    let content_width = w.rect.width.saturating_sub(2 * inset);
    // Whether this window draws its own status row (per `'laststatus'`); when it
    // does not, the freed row becomes text, so the window shows one more line.
    let status_visible = ed.window_statusline_visible(w.floating);
    // The content's own rows minus its status line (when shown); selections fill
    // to the text width (the area past the number gutter).
    let height = content_height
        .saturating_sub(usize::from(status_visible))
        .max(1);
    let width = content_width.saturating_sub(number_width);
    let top = w.top;
    // A stashed cursor may sit past a buffer that shrank while this window was
    // inactive; clamp it for the rendered ruler / cursor row.
    let cur_line = w.cursor.line.min(line_count.saturating_sub(1));

    let lines = window_lines(buf, top, height, line_count);
    let numbers = window_numbers(top, height, line_count);
    // The selection and incsearch preview belong to the focused window only.
    let selection = if w.focused {
        selection_spans(ed, buf, width, line_count, top, height)
    } else {
        vec![None; height]
    };
    // Per-cursor visual selections of the secondary multi-cursors (focused only).
    let secondary_selection = if w.focused {
        secondary_selection_spans(ed, buf, width, line_count, top, height)
    } else {
        vec![Vec::new(); height]
    };
    let (search, incsearch) = ed.search_highlights_in(buf, w.cursor, w.focused, top, height);

    let scroll = if w.focused {
        ed.pending_scroll().map(|ps| {
            let base_line = ps.from_top.min(ps.to_top);
            let count = ps.from_top.abs_diff(ps.to_top) + height;
            // The band carries the selection over the *maximal* extent the slide
            // touches: anchor → whichever scroll endpoint is furthest from the
            // anchor. Computing it at the live (destination) cursor would carry the
            // *small* end when the selection is shrinking, so the rows the cursor is
            // still sweeping back across wouldn't be in the band and would flash.
            // The client reveals/hides them per the interpolated cursor (see
            // `sel_extends_down`). (A selection whose anchor sits *between* the two
            // scroll endpoints — the cursor crossing the anchor mid-slide — only
            // gets the far side's rows; the near side is a rare, minor under-show.)
            let (selection, sel_extends_down) = if ed.mode.is_visual() {
                let anchor_line = ed.visual_anchor().line;
                let far_line =
                    if anchor_line.abs_diff(ps.from_cursor) >= anchor_line.abs_diff(ps.to_cursor) {
                        ps.from_cursor
                    } else {
                        ps.to_cursor
                    };
                let mut head = ed.cursor;
                head.line = far_line;
                let spans =
                    selection_spans_with_head(ed, buf, width, line_count, base_line, count, head);
                (spans, Some(anchor_line <= far_line))
            } else {
                (vec![None; count], None)
            };
            // The hlsearch / incsearch matches over the *band's* rows, so the
            // highlight slides with the text instead of disappearing for the
            // duration of the animation and snapping back when it settles.
            let (band_search, band_incsearch) =
                ed.search_highlights_in(buf, w.cursor, w.focused, base_line, count);
            ScrollAnim {
                from_top: ps.from_top,
                to_top: ps.to_top,
                from_cursor: ps.from_cursor,
                to_cursor: ps.to_cursor,
                duration_ms: ps.duration_ms,
                base_line,
                lines: window_lines(buf, base_line, count, line_count),
                selection,
                sel_extends_down,
                numbers: window_numbers(base_line, count, line_count),
                search: band_search,
                incsearch: band_incsearch,
            }
        })
    } else {
        None
    };

    let file_name = buf
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[No Name]".to_string());

    // Effective filetype: the treesitter override (`:set ft=…`) if any, else the
    // extension-derived language. Drives `%y` and the web build's grammar choice.
    let filetype = ed.ts_language_for(w.buffer).unwrap_or_default();

    let cursor_screen_col = {
        let line = buf.line(cur_line);
        unicode::virtcol(&line, w.cursor.col, buf.options.effective_tabstop())
    };

    // Secondary multi-cursors visible in the focused window, as (row, screen
    // col). Off-screen cursors are dropped; the primary is carried separately.
    let secondary_cursors = if w.focused {
        ed.secondary_cursor_bytes()
            .into_iter()
            .filter_map(|byte| {
                let line = buf.byte_to_line(byte);
                if line < top || line >= top + height {
                    return None;
                }
                let col = byte - buf.line_start(line);
                let s = buf.line(line);
                let screen_col = unicode::virtcol(&s, col, buf.options.effective_tabstop());
                Some((line - top, screen_col))
            })
            .collect()
    } else {
        Vec::new()
    };

    let status_ctx = window_status_ctx(StatusCtxInputs {
        buf,
        w,
        file_name: &file_name,
        filetype: &filetype,
        cur_line,
        line_count,
        cursor_screen_col,
        top,
        text_height: height,
    });

    WindowView {
        rect: ViewRect {
            x: w.rect.x,
            y: w.rect.y,
            width: w.rect.width,
            height: w.rect.height,
        },
        region: w.region,
        buffer: w.buffer,
        focused: w.focused,
        lines,
        cursor_row: cur_line.saturating_sub(top).min(height.saturating_sub(1)),
        leftcol: w.leftcol,
        cursor_col: w.cursor.col,
        cursor_screen_col,
        secondary_cursors,
        file_name,
        filetype,
        unnamed: buf.path.is_none(),
        modified: buf.modified,
        cursor_line: cur_line + 1,
        selection,
        secondary_selection,
        search,
        incsearch,
        scroll,
        numbers,
        number: w.options.number,
        relativenumber: w.options.relativenumber,
        number_width,
        tabstop: buf.options.effective_tabstop(),
        floating: w.floating,
        border: w.border,
        title: w.title.clone(),
        status_ctx,
        status_visible,
    }
}

/// The window facts [`window_status_ctx`] reads — grouped into a struct so the
/// builder doesn't take a long positional argument list (and `clippy` stays
/// happy).
struct StatusCtxInputs<'a> {
    buf: &'a Buffer,
    w: &'a WindowLayout,
    file_name: &'a str,
    /// The buffer's effective treesitter filetype (override or extension), for
    /// `%y` — so `:set ft=…` shows in the status line, not just the extension.
    filetype: &'a str,
    /// Clamped cursor line (0-based) — the rendered line, matching the ruler.
    cur_line: usize,
    line_count: usize,
    /// Cursor screen-cell column (0-based, tab/wide aware) for `%v`.
    cursor_screen_col: usize,
    /// 0-based first visible buffer line, and visible text rows, for `%P`.
    top: usize,
    text_height: usize,
}

/// Build the [`StatuslineCtx`] the `%`-format engine expands its built-in fields
/// from. The facts come straight off the buffer and the window's viewport; the
/// flags nxvim does not model yet (`'modifiable'`, `'readonly'`, help buffers)
/// take their always-true / always-false defaults, so `%m`/`%r`/`%h` render
/// faithfully for what nxvim supports rather than faking a state it lacks.
fn window_status_ctx(inp: StatusCtxInputs) -> StatuslineCtx {
    let path = inp.buf.path.as_deref();
    let file_tail = path
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "[No Name]".to_string());
    StatuslineCtx {
        // `%f`/`%F`: nxvim keeps the path as opened; v1 uses it verbatim for both
        // (no `canonicalize`, which would be I/O — forbidden in core).
        file_rel: inp.file_name.to_string(),
        file_full: inp.file_name.to_string(),
        file_tail,
        modified: inp.buf.modified,
        modifiable: true,
        readonly: false,
        help: false,
        filetype: inp.filetype.to_string(),
        bufnr: inp.w.buffer.0 as usize,
        line: inp.cur_line + 1,
        line_count: inp.line_count,
        col: inp.w.cursor.col + 1,
        virtcol: inp.cursor_screen_col + 1,
        top_line: inp.top + 1,
        text_height: inp.text_height,
    }
}

/// 1-based buffer line number for each of the `count` rows starting at buffer
/// line `base`, `None` for rows past the end of the buffer (the `~` fillers).
fn window_numbers(base: usize, count: usize, line_count: usize) -> Vec<Option<usize>> {
    (0..count)
        .map(|row| {
            let idx = base + row;
            (idx < line_count).then_some(idx + 1)
        })
        .collect()
}

/// Build `count` rendered rows starting at buffer line `base`, padding rows past
/// the end of the buffer with `"~"` (as vim shows below the last line).
fn window_lines(buf: &Buffer, base: usize, count: usize, line_count: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(count);
    for row in 0..count {
        let idx = base + row;
        if idx < line_count {
            lines.push(buf.line(idx));
        } else {
            lines.push("~".to_string());
        }
    }
    lines
}

/// Compute, for each of the `count` rows starting at buffer line `base`, the
/// half-open screen-column span to highlight as the visual selection (or
/// `None`). Returns all-`None` outside visual modes. Called for the focused
/// window only, so the editor's live `mode`/`cursor`/anchor describe `buf`.
fn selection_spans(
    ed: &Editor,
    buf: &Buffer,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
) -> Vec<Option<(usize, usize)>> {
    selection_spans_with_head(ed, buf, width, line_count, base, count, ed.cursor)
}

/// [`selection_spans`] with an explicit selection `head` instead of the live
/// cursor — so the scroll band can project the selection at the slide's furthest
/// extent (rather than the destination cursor) and let the client clip it back to
/// the interpolated cursor.
fn selection_spans_with_head(
    ed: &Editor,
    buf: &Buffer,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
    head: Cursor,
) -> Vec<Option<(usize, usize)>> {
    let mut spans = vec![None; count];
    let Some(visual_mode) = ed.rendered_visual_mode() else {
        return spans;
    };

    let (start, end) = order_selection(ed.visual_anchor(), head);
    let linewise = visual_mode == Mode::VisualLine;
    let ts = ed.tabstop();
    for (row, span) in spans.iter_mut().enumerate() {
        let buf_line = base + row;
        if buf_line >= line_count {
            continue;
        }
        *span = selection_row_span(buf, width, start, end, linewise, ts, buf_line);
    }

    spans
}

/// Like [`selection_spans`] but for the **secondary** multi-cursors: per row, the
/// spans of every secondary cursor's selection (its anchor→head). Empty outside a
/// visual mode or with no secondary cursors. The primary's selection is projected
/// separately into [`WindowView::selection`].
fn secondary_selection_spans(
    ed: &Editor,
    buf: &Buffer,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
) -> Vec<Vec<(usize, usize)>> {
    let mut rows = vec![Vec::new(); count];
    if !ed.mode.is_visual() || !ed.has_secondary_cursors() {
        return rows;
    }
    let linewise = ed.mode == Mode::VisualLine;
    let ts = ed.tabstop();
    for (anchor, head) in ed.secondary_selections() {
        let (start, end) = order_selection(anchor, head);
        for (row, list) in rows.iter_mut().enumerate() {
            let buf_line = base + row;
            if buf_line >= line_count {
                continue;
            }
            if let Some(span) = selection_row_span(buf, width, start, end, linewise, ts, buf_line) {
                list.push(span);
            }
        }
    }
    rows
}

/// Order a selection's two ends (`anchor`, `cursor`) by buffer position into
/// `(start, end)`.
fn order_selection(a: Cursor, c: Cursor) -> (Cursor, Cursor) {
    if (a.line, a.col) <= (c.line, c.col) {
        (a, c)
    } else {
        (c, a)
    }
}

/// The half-open screen-column span to highlight on buffer line `buf_line` for a
/// single selection running from ordered `start` to `end`, or `None` if the line
/// lies outside it. Shared by the primary selection and every secondary cursor's.
fn selection_row_span(
    buf: &Buffer,
    width: usize,
    start: Cursor,
    end: Cursor,
    linewise: bool,
    ts: usize,
    buf_line: usize,
) -> Option<(usize, usize)> {
    if buf_line < start.line || buf_line > end.line {
        return None;
    }
    if linewise {
        // Whole line, filled to the viewport edge — as vim paints it.
        return Some((0, width));
    }
    let text = buf.line_cow(buf_line);
    let mut vc = unicode::LineVirtcol::new(&text, ts);
    // Charwise: clip the inclusive [start, end] region to this row.
    let lo = if buf_line == start.line { start.col } else { 0 };
    let start_col = vc.at(lo);
    let end_col = if buf_line == end.line {
        // Include the grapheme under the trailing cursor.
        let hi = unicode::next_grapheme(&text, end.col.min(text.len()));
        vc.at(hi)
    } else {
        // The selection continues onto the next line: highlight the text and one
        // extra cell standing in for the selected newline.
        vc.at(text.len()) + 1
    };
    Some((start_col, end_col))
}
