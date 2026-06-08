//! The server's view, mirrored client-side for rendering, and the `redraw`
//! notification parsing that fills it in. Frontend-agnostic: styles are the
//! neutral [`Style`], so a TUI and a GUI share this model and each converts to
//! its own toolkit at paint time.

use std::time::Duration;

use rmpv::Value;

use crate::parse::{
    chrome_style, map_get, map_str, map_str_array, map_u16, map_u64, parse_border,
    parse_diagnostics, parse_highlights, parse_multi_spans, parse_numbers, parse_pmenu_items,
    parse_spans, parse_status, parse_styles, DiagSpan, HlSpan, IncSearchSpans, PmenuItem,
    SearchSpans, StatusSegment,
};
use crate::style::{Border, Style};

/// The scroll gesture mirrored from the server's redraw, ready to animate.
/// Line/cursor positions are kept as `f32` for interpolation; `lines`/`selection`
/// are the band covering the slide, anchored at `base_line`. A client that
/// animates scrolling (the TUI) drives this; one that doesn't can ignore it.
#[derive(Clone)]
pub struct ScrollData {
    pub from_top: f32,
    pub to_top: f32,
    pub from_cursor: f32,
    pub to_cursor: f32,
    pub duration: Duration,
    pub base_line: usize,
    pub lines: Vec<String>,
    pub selection: Vec<Option<(u16, u16)>>,
    pub numbers: Vec<Option<usize>>,
    /// Syntax highlights for the band (aligned with `lines`), so the slide is
    /// colored frame by frame instead of flashing white until it settles. Style
    /// ids index `styles` below.
    pub highlights: Vec<Vec<HlSpan>>,
    /// The style palette captured with this gesture. Snapshotted (not read live
    /// from [`View::styles`]) because a delayed highlight redraw arriving
    /// mid-slide replaces the live palette, which would leave the band's frozen
    /// style ids pointing at the wrong entries.
    pub styles: Vec<Style>,
}

/// One window's content mirrored from a redraw `windows[i]` sub-map: its screen
/// rect, focus flag, and all the per-window fields (text, cursor, selection,
/// search, syntax/diagnostics, gutter, and status-line data). The client paints
/// each of these at its `rect` with its own gutter, text body, and status line.
#[derive(Default)]
pub struct WindowView {
    /// The window's rect in **windows-area** cells, or `None` for the legacy flat
    /// redraw (a single window that fills the whole windows area — the synthetic
    /// paint fixtures). The renderer offsets a `Some` rect by the windows-area
    /// origin; a `None` rect takes the whole area.
    pub rect: Option<WinRect>,
    pub focused: bool,
    pub lines: Vec<String>,
    pub cursor_row: u16,
    pub cursor_screen_col: u16,
    /// First visible screen column (horizontal scroll offset) under `nowrap`. The
    /// renderer drops this many leading screen cells from each text row and shifts
    /// the cursor and every span left by it; the number gutter is not offset.
    pub leftcol: u16,
    /// This window's status line as rendered segments (`text` + resolved
    /// `style`), projected by the server's `%`-format engine. The client paints
    /// them left-to-right; an empty vec (an older server) falls back to a bare
    /// reverse-video status row.
    pub status: Vec<StatusSegment>,
    /// Whether this window paints its own status row (per `'laststatus'`). False
    /// (modes 0/3, or 1 with one window) gives the freed bottom row to text. An
    /// older server omits the flag; defaults to `true` (the historical
    /// every-window-has-a-status look).
    pub status_visible: bool,
    pub cursor_line: usize,
    /// Per visible row, the half-open screen-column span `[start, end)` to paint
    /// as the visual selection, or `None`.
    pub selection: Vec<Option<(u16, u16)>>,
    /// Per visible row, the half-open screen-column spans of every search match
    /// (`hlsearch`). Empty inner vecs for rows with no match.
    pub search: SearchSpans,
    /// Per visible row, the single span the live `incsearch` preview rests on.
    pub incsearch: IncSearchSpans,
    /// Per visible row, the treesitter highlight spans `(start_col, end_col,
    /// group, style_id)` in screen columns. `style_id` indexes [`View::styles`]
    /// (the global palette) when resolved through a colorscheme; `None` leaves
    /// the client to fall back to its own per-group theme.
    pub highlights: Vec<Vec<HlSpan>>,
    /// Per visible row, the LSP diagnostic underline spans `(start_col, end_col,
    /// severity, style_id)` in screen columns.
    pub diagnostics: Vec<Vec<DiagSpan>>,
    /// A scroll gesture for this window, when its viewport just moved.
    pub scroll: Option<ScrollData>,
    /// Per visible row, the 1-based buffer line number (`None` for `~` fillers).
    pub numbers: Vec<Option<usize>>,
    pub number: bool,
    pub relativenumber: bool,
    pub number_width: u16,
    /// This window's buffer `tabstop`: how many cells to expand a `\t` to when
    /// painting, mirrored from the server so the text lines up with the server's
    /// `cursor_screen_col` (computed with the same value). Defaults to 8.
    pub tabstop: u16,
    /// Whether this window is a **float**: the renderer paints it in a second,
    /// on-top pass (over the tiled windows) rather than tiling it. Tiled windows
    /// and the legacy flat redraw are `false`.
    pub floating: bool,
    /// The float's border type, or `None` for a borderless float / tiled window.
    /// When set the renderer draws a bordered box around the window's rect and
    /// paints the content one cell inside it.
    pub border: Option<Border>,
    /// The float's title, drawn on the top border. `None` when untitled.
    pub title: Option<String>,
}

/// A window's rect in windows-area cells (mirrors `nxvim_core::ViewRect`).
#[derive(Default, Clone, Copy)]
pub struct WinRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

/// A split separator the client draws between windows: a vertical `│` or
/// horizontal `─` run of `length` cells anchored at `(x, y)` in windows-area
/// cells. Empty with a single window.
#[derive(Clone, Copy)]
pub struct Separator {
    pub vertical: bool,
    pub x: u16,
    pub y: u16,
    pub length: u16,
}

/// The server's view, mirrored client-side for rendering: the **global** chrome
/// (mode, command line, message, panel, popup, style palette) plus the list of
/// [`WindowView`]s to paint and the split [`Separator`]s between them.
#[derive(Default)]
pub struct View {
    /// The windows to paint, in layout order. Always at least one.
    pub windows: Vec<WindowView>,
    /// The split separators between windows (empty with one window).
    pub separators: Vec<Separator>,
    /// The tab pages, in tabline order — empty when only one tab is open (no
    /// tabline is drawn). When non-empty, `current_tab` indexes the active cell.
    pub tabline: Vec<TabData>,
    /// A custom `'tabline'` rendered by the server's `%`-format engine into styled
    /// segments spanning the editor width. Empty when no `'tabline'` is set (the
    /// client then formats the [`TabData`] cells itself); when non-empty it is
    /// painted in place of those cells on the same reserved top row.
    pub tabline_segments: Vec<StatusSegment>,
    /// Index into `tabline` of the active tab (meaningful only when `tabline` is
    /// non-empty).
    pub current_tab: usize,
    pub mode_label: String,
    pub command_mode: bool,
    /// True while `r` waits for its replacement character (a one-shot replace
    /// that stays in normal mode). Drives the replace cursor shape.
    pub pending_replace: bool,
    pub cmdline: String,
    /// The command-line prompt char (`:` ex, `/` / `?` search). Defaults to `:`.
    pub cmdline_prefix: char,
    /// The multi-char `vim.ui.input` prompt label, rendered ahead of the editable
    /// line in place of `cmdline_prefix`. Empty for `:`/`/`/`?`.
    pub cmdline_prompt: String,
    /// Command cursor position as a character offset into `cmdline`, for placing
    /// the terminal cursor mid-line after `<Left>`/`<Right>` edits.
    pub cmdline_cursor: usize,
    pub message: String,
    /// The per-frame style palette the server resolved from the active
    /// colorscheme; per-window `highlights`/chrome ids index into it. Global.
    pub styles: Vec<Style>,
    /// Resolved editor-chrome styles (`None` when the theme leaves the group
    /// undefined — the client then keeps its built-in look for that region).
    pub normal: Option<Style>,
    pub line_nr: Option<Style>,
    pub cursor_line_nr: Option<Style>,
    pub visual: Option<Style>,
    pub search_style: Option<Style>,
    pub incsearch_style: Option<Style>,
    pub status_line: Option<Style>,
    pub end_of_buffer: Option<Style>,
    /// The single global status line (`laststatus=3`) as rendered segments,
    /// spanning the full editor width and showing the focused window's facts.
    /// Empty for modes 0/1/2 (status lines are per-window, or hidden); when
    /// non-empty the renderer docks it on one row just above the command line and
    /// no window paints its own status row. Global — one per editor.
    pub global_status: Vec<StatusSegment>,
    /// The bottom panel (`:messages`, `:ls`), or `None` when none is open. When
    /// present it has input focus: the editing cursor is drawn inside it. Global.
    pub panel: Option<PanelData>,
    /// The insert-mode completion popup, or `None` when none is open. Drawn last,
    /// over the focused window's text area. Global.
    pub pmenu: Option<PmenuData>,
}

/// The insert-mode completion popup mirrored from the server's redraw: the ranked
/// items, the selected index (`None` until the user navigates), and the overlay's
/// anchor and content size in **text-area cells** (the client adds the gutter and
/// text-area origin, then draws a bordered box around the content).
#[derive(Clone)]
pub struct PmenuData {
    pub items: Vec<PmenuItem>,
    pub selected: Option<usize>,
    pub row: u16,
    pub col: u16,
    pub width: u16,
    pub height: u16,
    /// The selected item's documentation lines, drawn in a preview box beside the
    /// popup. Empty ⇒ no preview (nothing selected, or the item has no docs).
    pub doc: Vec<String>,
}

/// One tabline cell mirrored from the server's redraw: the buffer label, its
/// modified flag, and the tab's window count. The client formats the rendered
/// text (vim's default `{count}{+} {label}`).
#[derive(Clone)]
pub struct TabData {
    pub label: String,
    pub modified: bool,
    pub window_count: usize,
}

/// The bottom panel mirrored from the server's redraw: a title, the visible
/// content slice, the cursor row within it, and the content height to lay out.
#[derive(Clone)]
pub struct PanelData {
    pub title: String,
    pub lines: Vec<String>,
    pub cursor_row: u16,
    /// Display rows the selected (possibly wrapped) entry spans; the whole span
    /// is drawn as the focused line. Defaults to 1 (an unwrapped entry).
    pub cursor_span: u16,
    pub height: u16,
}

impl View {
    pub fn update(&mut self, params: &[Value]) {
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        self.mode_label = map_str(map, "mode_label");
        self.command_mode = map_get(map, "command_mode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.pending_replace = map_get(map, "pending_replace")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.cmdline = map_str(map, "cmdline");
        self.cmdline_prefix = map_str(map, "cmdline_prefix").chars().next().unwrap_or(':');
        self.cmdline_prompt = map_str(map, "cmdline_prompt");
        self.cmdline_cursor = map_u64(map, "cmdline_cursor") as usize;
        self.message = map_str(map, "message");
        // The style palette must land before windows (their scroll bands snapshot
        // it) and chrome (which indexes into it).
        self.styles = parse_styles(map_get(map, "styles"));
        let chrome = |key| chrome_style(map_get(map, "chrome"), key, &self.styles);
        self.normal = chrome("normal");
        self.line_nr = chrome("line_nr");
        self.cursor_line_nr = chrome("cursor_line_nr");
        self.visual = chrome("visual");
        self.search_style = chrome("search");
        self.incsearch_style = chrome("incsearch");
        self.status_line = chrome("status_line");
        self.end_of_buffer = chrome("end_of_buffer");
        // The window list (the multi-window form), or a single window built from
        // the legacy flat top-level fields (the synthetic paint fixtures).
        self.windows = match map_get(map, "windows") {
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|w| match w {
                    Value::Map(wm) => Some(parse_window(wm, &self.styles)),
                    _ => None,
                })
                .collect(),
            _ => vec![parse_window(map, &self.styles)],
        };
        self.separators = parse_separators(map_get(map, "separators"));
        // The global status line (`laststatus=3`); empty/absent for per-window modes.
        self.global_status = parse_status(map_get(map, "global_status"), &self.styles);
        self.tabline = parse_tabline(map_get(map, "tabline"));
        self.tabline_segments = parse_status(map_get(map, "tabline_segments"), &self.styles);
        self.current_tab = map_u64(map, "current_tab") as usize;
        self.panel = match map_get(map, "panel") {
            Some(Value::Map(p)) => Some(PanelData {
                title: map_str(p, "title"),
                lines: map_str_array(p, "lines"),
                cursor_row: map_u16(p, "cursor_row"),
                // Older redraws (and the panel test fixtures) omit the span; an
                // unwrapped entry occupies exactly one row.
                cursor_span: map_get(p, "cursor_span")
                    .and_then(Value::as_u64)
                    .map_or(1, |n| n as u16),
                height: map_u16(p, "height"),
            }),
            _ => None,
        };
        self.pmenu = match map_get(map, "pmenu") {
            Some(Value::Map(p)) => Some(PmenuData {
                items: parse_pmenu_items(map_get(p, "items")),
                selected: map_get(p, "selected")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize),
                row: map_u16(p, "row"),
                col: map_u16(p, "col"),
                width: map_u16(p, "width"),
                height: map_u16(p, "height"),
                doc: map_str_array(p, "doc"),
            }),
            _ => None,
        };
    }

    /// The focused window — where the terminal cursor is drawn and the reference
    /// point for the completion popup — or `None` before the first redraw (a
    /// default `View` has no windows). A real redraw always carries at least one
    /// window, with one flagged focused; falls back to the first if none is.
    pub fn focused(&self) -> Option<&WindowView> {
        self.windows
            .iter()
            .find(|w| w.focused)
            .or_else(|| self.windows.first())
    }

    /// Whether the editor is in insert mode, mirrored from the server's
    /// `mode_label`. Drives the thin-bar "edit cursor" shape.
    pub fn is_insert(&self) -> bool {
        self.mode_label == "INSERT"
    }

    /// Whether the editor is replacing: either `R` replace mode (`mode_label`)
    /// or `r` waiting for its one replacement char. Both drive the underline
    /// cursor shape, matching vim's replace/operator-pending feedback.
    pub fn is_replace(&self) -> bool {
        self.mode_label == "REPLACE" || self.pending_replace
    }

    /// Build a view from a `redraw` notification's params — the client's own
    /// parsing path — so tests and tools can paint a known view.
    pub fn from_redraw(params: &[Value]) -> Self {
        let mut view = View::default();
        view.update(params);
        view
    }
}

/// Parse one window from a map slice — either a `windows[i]` sub-map (with a
/// `rect`) or the legacy flat top-level redraw (no `rect`, so the renderer gives
/// the single window the whole windows area). `styles` is the global palette the
/// window's scroll band snapshots.
fn parse_window(m: &[(Value, Value)], styles: &[Style]) -> WindowView {
    let rect = match map_get(m, "rect") {
        Some(Value::Map(r)) => Some(WinRect {
            x: map_u16(r, "x"),
            y: map_u16(r, "y"),
            width: map_u16(r, "width"),
            height: map_u16(r, "height"),
        }),
        _ => None,
    };
    let scroll = match map_get(m, "scroll") {
        Some(Value::Map(s)) => Some(ScrollData {
            from_top: map_u64(s, "from_top") as f32,
            to_top: map_u64(s, "to_top") as f32,
            from_cursor: map_u64(s, "from_cursor") as f32,
            to_cursor: map_u64(s, "to_cursor") as f32,
            duration: Duration::from_millis(map_u64(s, "duration_ms")),
            base_line: map_u64(s, "base_line") as usize,
            lines: map_str_array(s, "lines"),
            selection: parse_spans(map_get(s, "selection")),
            numbers: parse_numbers(map_get(s, "numbers")),
            highlights: parse_highlights(map_get(s, "highlights")),
            // The band's ids index this redraw's palette — snapshot it now, since
            // a later redraw will replace the live `styles`.
            styles: styles.to_vec(),
        }),
        _ => None,
    };
    WindowView {
        rect,
        // A flat redraw has no `focused` flag; its sole window is always focused.
        focused: map_get(m, "focused")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        lines: map_str_array(m, "lines"),
        cursor_row: map_u16(m, "cursor_row"),
        cursor_screen_col: map_u16(m, "cursor_screen_col"),
        leftcol: map_u16(m, "leftcol"),
        status: parse_status(map_get(m, "status"), styles),
        // Default true so an older server (no flag) keeps the per-window status.
        status_visible: map_get(m, "status_visible")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        cursor_line: map_u64(m, "cursor_line") as usize,
        selection: parse_spans(map_get(m, "selection")),
        search: parse_multi_spans(map_get(m, "search")),
        incsearch: parse_spans(map_get(m, "incsearch")),
        highlights: parse_highlights(map_get(m, "highlights")),
        diagnostics: parse_diagnostics(map_get(m, "diagnostics")),
        scroll,
        numbers: parse_numbers(map_get(m, "numbers")),
        number: map_get(m, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        relativenumber: map_get(m, "relativenumber")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        number_width: map_u16(m, "number_width"),
        // The server always sends a tabstop ≥ 1; treat a missing/0 value as the
        // historical default of 8 so an older server still renders sanely.
        tabstop: match map_u16(m, "tabstop") {
            0 => 8,
            ts => ts,
        },
        floating: map_get(m, "floating")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        border: parse_border(map_get(m, "border")),
        title: {
            let t = map_str(m, "title");
            (!t.is_empty()).then_some(t)
        },
    }
}

/// Parse the `tabline` array: each entry a `{ label, modified, window_count }`
/// map. Empty (so no tabline drawn) when only one tab is open.
fn parse_tabline(value: Option<&Value>) -> Vec<TabData> {
    let Some(Value::Array(arr)) = value else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| match v {
            Value::Map(t) => Some(TabData {
                label: map_str(t, "label"),
                modified: map_get(t, "modified")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                window_count: map_u64(t, "window_count") as usize,
            }),
            _ => None,
        })
        .collect()
}

/// Parse the `separators` array: each entry a `{ vertical, x, y, length }` map.
fn parse_separators(value: Option<&Value>) -> Vec<Separator> {
    let Some(Value::Array(arr)) = value else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| match v {
            Value::Map(s) => Some(Separator {
                vertical: map_get(s, "vertical")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                x: map_u16(s, "x"),
                y: map_u16(s, "y"),
                length: map_u16(s, "length"),
            }),
            _ => None,
        })
        .collect()
}
