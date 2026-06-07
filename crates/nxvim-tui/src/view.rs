//! The server's view, mirrored client-side for rendering, and the `redraw`
//! notification parsing that fills it in.

use ratatui::style::Style;
use ratatui::widgets::BorderType;
use rmpv::Value;
use std::time::Duration;

use crate::anim::ScrollData;
use crate::parse::{
    chrome_style, map_get, map_str, map_str_array, map_u16, map_u64, parse_diagnostics,
    parse_highlights, parse_multi_spans, parse_numbers, parse_pmenu_items, parse_spans,
    parse_styles, DiagSpan, HlSpan, IncSearchSpans, PmenuItem, SearchSpans,
};

/// One window's content mirrored from a redraw `windows[i]` sub-map: its screen
/// rect, focus flag, and all the per-window fields (text, cursor, selection,
/// search, syntax/diagnostics, gutter, and status-line data). The client paints
/// each of these at its `rect` with its own gutter, text body, and status line.
#[derive(Default)]
pub(crate) struct WindowView {
    /// The window's rect in **windows-area** cells, or `None` for the legacy flat
    /// redraw (a single window that fills the whole windows area — the synthetic
    /// paint fixtures). The renderer offsets a `Some` rect by the windows-area
    /// origin; a `None` rect takes the whole area.
    pub(crate) rect: Option<WinRect>,
    pub(crate) focused: bool,
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) cursor_screen_col: u16,
    pub(crate) file_name: String,
    pub(crate) modified: bool,
    pub(crate) cursor_line: usize,
    /// Per visible row, the half-open screen-column span `[start, end)` to paint
    /// as the visual selection, or `None`.
    pub(crate) selection: Vec<Option<(u16, u16)>>,
    /// Per visible row, the half-open screen-column spans of every search match
    /// (`hlsearch`). Empty inner vecs for rows with no match.
    pub(crate) search: SearchSpans,
    /// Per visible row, the single span the live `incsearch` preview rests on.
    pub(crate) incsearch: IncSearchSpans,
    /// Per visible row, the treesitter highlight spans `(start_col, end_col,
    /// group, style_id)` in screen columns. `style_id` indexes [`View::styles`]
    /// (the global palette) when resolved through a colorscheme; `None` falls
    /// back to the client's built-in [`group_style`](crate::render::group_style).
    pub(crate) highlights: Vec<Vec<HlSpan>>,
    /// Per visible row, the LSP diagnostic underline spans `(start_col, end_col,
    /// severity, style_id)` in screen columns.
    pub(crate) diagnostics: Vec<Vec<DiagSpan>>,
    /// A scroll gesture for this window, when its viewport just moved.
    pub(crate) scroll: Option<ScrollData>,
    /// Per visible row, the 1-based buffer line number (`None` for `~` fillers).
    pub(crate) numbers: Vec<Option<usize>>,
    pub(crate) number: bool,
    pub(crate) relativenumber: bool,
    pub(crate) number_width: u16,
    /// This window's buffer `tabstop`: how many cells to expand a `\t` to when
    /// painting, mirrored from the server so the text lines up with the server's
    /// `cursor_screen_col` (computed with the same value). Defaults to 8.
    pub(crate) tabstop: u16,
    /// Whether this window is a **float**: the renderer paints it in a second,
    /// on-top pass (over the tiled windows) rather than tiling it. Tiled windows
    /// and the legacy flat redraw are `false`.
    pub(crate) floating: bool,
    /// The float's border type, or `None` for a borderless float / tiled window.
    /// When set the renderer draws a bordered box around the window's rect and
    /// paints the content one cell inside it.
    pub(crate) border: Option<BorderType>,
    /// The float's title, drawn on the top border. `None` when untitled.
    pub(crate) title: Option<String>,
}

/// A window's rect in windows-area cells (mirrors `nxvim_core::ViewRect`).
#[derive(Default, Clone, Copy)]
pub(crate) struct WinRect {
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

/// A split separator the client draws between windows: a vertical `│` or
/// horizontal `─` run of `length` cells anchored at `(x, y)` in windows-area
/// cells. Empty with a single window.
#[derive(Clone, Copy)]
pub(crate) struct Separator {
    pub(crate) vertical: bool,
    pub(crate) x: u16,
    pub(crate) y: u16,
    pub(crate) length: u16,
}

/// The server's view, mirrored client-side for rendering: the **global** chrome
/// (mode, command line, message, panel, popup, style palette) plus the list of
/// [`WindowView`]s to paint and the split [`Separator`]s between them.
#[derive(Default)]
pub struct View {
    /// The windows to paint, in layout order. Always at least one.
    pub(crate) windows: Vec<WindowView>,
    /// The split separators between windows (empty with one window).
    pub(crate) separators: Vec<Separator>,
    pub(crate) mode_label: String,
    pub(crate) command_mode: bool,
    /// True while `r` waits for its replacement character (a one-shot replace
    /// that stays in normal mode). Drives the replace cursor shape.
    pub(crate) pending_replace: bool,
    pub(crate) cmdline: String,
    /// The command-line prompt char (`:` ex, `/` / `?` search). Defaults to `:`.
    pub(crate) cmdline_prefix: char,
    /// The multi-char `vim.ui.input` prompt label, rendered ahead of the editable
    /// line in place of `cmdline_prefix`. Empty for `:`/`/`/`?`.
    pub(crate) cmdline_prompt: String,
    /// Command cursor position as a character offset into `cmdline`, for placing
    /// the terminal cursor mid-line after `<Left>`/`<Right>` edits.
    pub(crate) cmdline_cursor: usize,
    pub(crate) message: String,
    /// The per-frame style palette the server resolved from the active
    /// colorscheme; per-window `highlights`/chrome ids index into it. Global.
    pub(crate) styles: Vec<Style>,
    /// Resolved editor-chrome styles (`None` when the theme leaves the group
    /// undefined — the client then keeps its built-in look for that region).
    pub(crate) normal: Option<Style>,
    pub(crate) line_nr: Option<Style>,
    pub(crate) cursor_line_nr: Option<Style>,
    pub(crate) visual: Option<Style>,
    pub(crate) search_style: Option<Style>,
    pub(crate) incsearch_style: Option<Style>,
    pub(crate) status_line: Option<Style>,
    pub(crate) end_of_buffer: Option<Style>,
    /// The bottom panel (`:messages`, `:ls`), or `None` when none is open. When
    /// present it has input focus: the editing cursor is drawn inside it. Global.
    pub(crate) panel: Option<PanelData>,
    /// The insert-mode completion popup, or `None` when none is open. Drawn last,
    /// over the focused window's text area. Global.
    pub(crate) pmenu: Option<PmenuData>,
}

/// The insert-mode completion popup mirrored from the server's redraw: the ranked
/// items, the selected index (`None` until the user navigates), and the overlay's
/// anchor and content size in **text-area cells** (the client adds the gutter and
/// text-area origin, then draws a bordered box around the content).
#[derive(Clone)]
pub(crate) struct PmenuData {
    pub(crate) items: Vec<PmenuItem>,
    pub(crate) selected: Option<usize>,
    pub(crate) row: u16,
    pub(crate) col: u16,
    pub(crate) width: u16,
    pub(crate) height: u16,
    /// The selected item's documentation lines, drawn in a preview box beside the
    /// popup. Empty ⇒ no preview (nothing selected, or the item has no docs).
    pub(crate) doc: Vec<String>,
}

/// The bottom panel mirrored from the server's redraw: a title, the visible
/// content slice, the cursor row within it, and the content height to lay out.
#[derive(Clone)]
pub(crate) struct PanelData {
    pub(crate) title: String,
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: u16,
    /// Display rows the selected (possibly wrapped) entry spans; the whole span
    /// is drawn as the focused line. Defaults to 1 (an unwrapped entry).
    pub(crate) cursor_span: u16,
    pub(crate) height: u16,
}

impl View {
    pub(crate) fn update(&mut self, params: &[Value]) {
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
    pub(crate) fn focused(&self) -> Option<&WindowView> {
        self.windows
            .iter()
            .find(|w| w.focused)
            .or_else(|| self.windows.first())
    }

    /// Whether the editor is in insert mode, mirrored from the server's
    /// `mode_label`. Drives the thin-bar "edit cursor" shape.
    pub(crate) fn is_insert(&self) -> bool {
        self.mode_label == "INSERT"
    }

    /// Whether the editor is replacing: either `R` replace mode (`mode_label`)
    /// or `r` waiting for its one replacement char. Both drive the underline
    /// cursor shape, matching vim's replace/operator-pending feedback.
    pub(crate) fn is_replace(&self) -> bool {
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
        cursor_col: map_u16(m, "cursor_col"),
        cursor_screen_col: map_u16(m, "cursor_screen_col"),
        file_name: map_str(m, "file_name"),
        modified: map_get(m, "modified")
            .and_then(Value::as_bool)
            .unwrap_or(false),
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

/// Map a float's wire border name (matching `nvim_win_get_config`) to the ratatui
/// [`BorderType`] used to draw it. `"none"`, a missing value, or an unknown name
/// yields `None` (no border). `"solid"` (neovim's space border) renders as the
/// nearest line style, `QuadrantInside`.
fn parse_border(value: Option<&Value>) -> Option<BorderType> {
    match value.and_then(Value::as_str) {
        Some("single") => Some(BorderType::Plain),
        Some("rounded") => Some(BorderType::Rounded),
        Some("double") => Some(BorderType::Double),
        Some("solid") => Some(BorderType::QuadrantInside),
        _ => None,
    }
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
