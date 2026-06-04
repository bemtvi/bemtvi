//! The server's view, mirrored client-side for rendering, and the `redraw`
//! notification parsing that fills it in.

use ratatui::style::Style;
use rmpv::Value;
use std::time::Duration;

use crate::anim::ScrollData;
use crate::parse::{
    chrome_style, map_get, map_str, map_str_array, map_u16, map_u64, parse_highlights,
    parse_multi_spans, parse_numbers, parse_spans, parse_styles, HlSpan, IncSearchSpans,
    SearchSpans,
};

/// The server's view, mirrored client-side for rendering.
#[derive(Default)]
pub struct View {
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: u16,
    pub(crate) cursor_col: u16,
    pub(crate) cursor_screen_col: u16,
    pub(crate) mode_label: String,
    pub(crate) command_mode: bool,
    pub(crate) cmdline: String,
    /// The command-line prompt char (`:` ex, `/` / `?` search). Defaults to `:`.
    pub(crate) cmdline_prefix: char,
    /// Command cursor position as a character offset into `cmdline`, for placing
    /// the terminal cursor mid-line after `<Left>`/`<Right>` edits.
    pub(crate) cmdline_cursor: usize,
    pub(crate) message: String,
    pub(crate) file_name: String,
    pub(crate) modified: bool,
    pub(crate) cursor_line: usize,
    /// Per visible row, the half-open screen-column span `[start, end)` to paint
    /// as the visual selection, or `None`. Mirrors the server's `View::selection`.
    pub(crate) selection: Vec<Option<(u16, u16)>>,
    /// Per visible row, the half-open screen-column spans of every search match
    /// (`hlsearch`). Empty inner vecs for rows with no match. Mirrors the
    /// server's `View::search`.
    pub(crate) search: SearchSpans,
    /// Per visible row, the single span the live `incsearch` preview rests on, or
    /// `None`. Mirrors the server's `View::incsearch`.
    pub(crate) incsearch: IncSearchSpans,
    /// Per visible row, the treesitter highlight spans `(start_col, end_col,
    /// group, style_id)` in screen columns. `style_id` indexes [`View::styles`]
    /// when the server resolved the span through a loaded colorscheme; `None`
    /// means fall back to the client's built-in
    /// [`group_style`](crate::render::group_style) theme.
    pub(crate) highlights: Vec<Vec<HlSpan>>,
    /// The per-frame style palette the server resolved from the active
    /// colorscheme; `highlights`/chrome ids index into it. Empty with no theme.
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
    pub(crate) scroll: Option<ScrollData>,
    /// Per visible row, the 1-based buffer line number (`None` for `~` fillers),
    /// from which the client formats the number column.
    pub(crate) numbers: Vec<Option<usize>>,
    /// `:set number` / `:set relativenumber` flags and the gutter width in cells
    /// (`0` when both are off), mirrored from the server.
    pub(crate) number: bool,
    pub(crate) relativenumber: bool,
    pub(crate) number_width: u16,
    /// The bottom panel (`:messages`, `:ls`), or `None` when none is open. When
    /// present it has input focus: the editing cursor is drawn inside it.
    pub(crate) panel: Option<PanelData>,
}

/// The bottom panel mirrored from the server's redraw: a title, the visible
/// content slice, the cursor row within it, and the content height to lay out.
#[derive(Clone)]
pub(crate) struct PanelData {
    pub(crate) title: String,
    pub(crate) lines: Vec<String>,
    pub(crate) cursor_row: u16,
    pub(crate) height: u16,
}

impl View {
    pub(crate) fn update(&mut self, params: &[Value]) {
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        self.lines = map_str_array(map, "lines");
        self.cursor_row = map_u16(map, "cursor_row");
        self.cursor_col = map_u16(map, "cursor_col");
        self.cursor_screen_col = map_u16(map, "cursor_screen_col");
        self.mode_label = map_str(map, "mode_label");
        self.command_mode = map_get(map, "command_mode")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.cmdline = map_str(map, "cmdline");
        self.cmdline_prefix = map_str(map, "cmdline_prefix").chars().next().unwrap_or(':');
        self.cmdline_cursor = map_u64(map, "cmdline_cursor") as usize;
        self.message = map_str(map, "message");
        self.file_name = map_str(map, "file_name");
        self.modified = map_get(map, "modified")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.cursor_line = map_u64(map, "cursor_line") as usize;
        self.selection = parse_spans(map_get(map, "selection"));
        self.search = parse_multi_spans(map_get(map, "search"));
        self.incsearch = parse_spans(map_get(map, "incsearch"));
        self.highlights = parse_highlights(map_get(map, "highlights"));
        // The style palette must land before chrome, which indexes into it.
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
        self.numbers = parse_numbers(map_get(map, "numbers"));
        self.number = map_get(map, "number")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.relativenumber = map_get(map, "relativenumber")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.number_width = map_u16(map, "number_width");
        self.panel = match map_get(map, "panel") {
            Some(Value::Map(p)) => Some(PanelData {
                title: map_str(p, "title"),
                lines: map_str_array(p, "lines"),
                cursor_row: map_u16(p, "cursor_row"),
                height: map_u16(p, "height"),
            }),
            _ => None,
        };
        self.scroll = match map_get(map, "scroll") {
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
                // The band's ids index this redraw's palette — snapshot it now,
                // since a later redraw will replace `self.styles`.
                styles: self.styles.clone(),
            }),
            _ => None,
        };
    }

    /// Build a view from a `redraw` notification's params — the client's own
    /// parsing path — so tests and tools can paint a known view.
    pub fn from_redraw(params: &[Value]) -> Self {
        let mut view = View::default();
        view.update(params);
        view
    }
}
