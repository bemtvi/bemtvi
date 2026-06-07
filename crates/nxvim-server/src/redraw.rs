//! Projecting the editor `View` into the `redraw` notification map clients
//! render: lines, cursor, chrome styles, scroll band, panel, and the per-frame
//! deduped style palette ([`StyleTable`]).

use crate::Server;
use nxvim_core::highlight::Style;
use nxvim_core::view::{ScrollAnim, Separator, TabView, ViewRect, WindowView};
use nxvim_core::{BorderStyle, PanelView};
use rmpv::Value;
use std::collections::HashMap;

impl Server {
    /// Push the current view to the client as a single `redraw` notification
    /// carrying an nxvim-native view map (no neovim grid protocol). The map holds
    /// the **global** chrome (mode, command line, message, panel, popup) plus a
    /// `windows` array — one sub-map per window with its rect and per-window text,
    /// gutter, status, and highlight data — and a `separators` array for the
    /// inter-split borders. With one window the array has a single entry and the
    /// client paints exactly as before.
    pub(crate) fn redraw(&mut self) {
        let (w, h) = match self.ui {
            Some(dims) => dims,
            None => return,
        };
        let view = self.editor.view(w, h);

        // Refresh the current buffer's highlights from the in-process engine for
        // the freshly-settled viewport (same-frame, memoized per content+view).
        self.refresh_highlights(h);
        // Drive LSP document sync for the current buffer (non-blocking).
        self.sync_lsp();

        // Resolve every highlight span and chrome region to a concrete style here
        // on the server (the registry lives in the core). Spans carry an index
        // into a per-frame, deduped `styles` palette; the client paints the RGB.
        let mut styles = StyleTable::default();
        let chrome = self.chrome_styles(&mut styles);

        // The message line shows the diagnostic under the cursor, but only when
        // nothing more important (an error, command output) already holds it —
        // and never via `echo`, so the under-cursor text doesn't flood
        // `:messages` on every cursor move. A message *echoed after* `view()` ran
        // (which consumes and clears the transient line) — e.g. a grammar load
        // failure surfaced lazily when `refresh_highlights` first opened the
        // buffer in the engine — is read straight off the editor so it shows this
        // frame rather than waiting for the next keypress.
        let message = if !view.message.is_empty() {
            view.message.clone()
        } else if !self.editor.message.is_empty() {
            self.editor.message.clone()
        } else {
            self.diagnostic_under_cursor().unwrap_or_default()
        };

        // Project each window: its rect, per-window text/gutter/status data, and
        // its own buffer's syntax/diagnostic slice.
        let windows: Vec<Value> = view
            .windows
            .iter()
            .map(|win| self.window_value(win, &mut styles))
            .collect();

        // The split borders between windows.
        let separators = Value::Array(view.separators.iter().map(separator_value).collect());

        // The tabline cells (empty array when only one tab is open, so the client
        // draws no tabline) and the active cell index.
        let tabline = Value::Array(view.tabline.iter().map(tab_value).collect());

        // The insert-mode completion popup, `Nil` when none is open. The focused
        // window's text-area width (its width minus its number gutter) bounds the
        // overlay so it can't spill past the editable region.
        let text_width = view
            .focused()
            .rect
            .width
            .saturating_sub(view.focused().number_width);
        let pmenu = self.pmenu_value(&view, text_width);
        // The bottom panel (`:messages`, `:ls`), `Nil` when none is open.
        let panel = match &view.panel {
            Some(p) => project_panel(p),
            None => Value::Nil,
        };

        // Built last: every per-window/`chrome` style id above indexes into it.
        let styles_value = styles.into_value();
        let map = vec![
            (Value::from("windows"), Value::Array(windows)),
            (Value::from("separators"), separators),
            (Value::from("tabline"), tabline),
            (
                Value::from("current_tab"),
                Value::from(view.current_tab as u64),
            ),
            (
                Value::from("mode_label"),
                Value::from(view.mode_label.as_str()),
            ),
            (Value::from("command_mode"), Value::from(view.command_mode)),
            (
                Value::from("pending_replace"),
                Value::from(view.pending_replace),
            ),
            (Value::from("cmdline"), Value::from(view.cmdline.as_str())),
            (
                Value::from("cmdline_prefix"),
                Value::from(view.cmdline_prefix.to_string().as_str()),
            ),
            (
                Value::from("cmdline_prompt"),
                Value::from(view.cmdline_prompt.as_str()),
            ),
            (
                Value::from("cmdline_cursor"),
                Value::from(view.cmdline_cursor as u64),
            ),
            (Value::from("message"), Value::from(message.as_str())),
            (Value::from("styles"), styles_value),
            (Value::from("chrome"), chrome),
            (Value::from("panel"), panel),
            (Value::from("pmenu"), pmenu),
        ];

        self.rpc.notify("redraw", vec![Value::Map(map)]);
    }

    /// Project one window into its redraw sub-map: the rect and focus flag, the
    /// per-window text/cursor/gutter/status fields, and the window's own syntax
    /// highlights, diagnostic underlines, and scroll band (each resolving styles
    /// into the shared per-frame `styles` palette).
    fn window_value(&self, win: &WindowView, styles: &mut StyleTable) -> Value {
        let highlights = self.highlights_for(win.buffer, &win.numbers, styles);
        let diagnostics = self.diagnostics_for(win.buffer, &win.numbers, styles);
        let scroll = match &win.scroll {
            Some(s) => self.project_band(win.buffer, s, styles),
            None => Value::Nil,
        };
        Value::Map(vec![
            (Value::from("rect"), rect_value(&win.rect)),
            (Value::from("focused"), Value::from(win.focused)),
            (Value::from("lines"), lines_value(&win.lines)),
            (
                Value::from("cursor_row"),
                Value::from(win.cursor_row as u64),
            ),
            (
                Value::from("cursor_col"),
                Value::from(win.cursor_col as u64),
            ),
            (
                Value::from("cursor_screen_col"),
                Value::from(win.cursor_screen_col as u64),
            ),
            (Value::from("leftcol"), Value::from(win.leftcol as u64)),
            (
                Value::from("file_name"),
                Value::from(win.file_name.as_str()),
            ),
            (Value::from("modified"), Value::from(win.modified)),
            (
                Value::from("cursor_line"),
                Value::from(win.cursor_line as u64),
            ),
            (Value::from("selection"), spans_value(&win.selection)),
            (Value::from("search"), multi_spans_value(&win.search)),
            (Value::from("incsearch"), spans_value(&win.incsearch)),
            (Value::from("numbers"), numbers_value(&win.numbers)),
            (Value::from("number"), Value::from(win.number)),
            (
                Value::from("relativenumber"),
                Value::from(win.relativenumber),
            ),
            (
                Value::from("number_width"),
                Value::from(win.number_width as u64),
            ),
            (Value::from("tabstop"), Value::from(win.tabstop as u64)),
            (Value::from("highlights"), highlights),
            (Value::from("diagnostics"), diagnostics),
            (Value::from("scroll"), scroll),
            // Float overlay chrome. A tiled window is `floating: false` with no
            // border/title, so the client paints it exactly as before.
            (Value::from("floating"), Value::from(win.floating)),
            (Value::from("border"), Value::from(border_name(win.border))),
            (
                Value::from("title"),
                match &win.title {
                    Some(t) => Value::from(t.as_str()),
                    None => Value::Nil,
                },
            ),
        ])
    }

    /// Project a scroll-animation band into the `scroll` sub-map a client animates
    /// the slide from. Mirrors the main map's lines/selection/numbers/highlights
    /// projection over the (taller) animation window.
    pub(crate) fn project_band(
        &self,
        buffer: nxvim_core::BufferId,
        s: &ScrollAnim,
        styles: &mut StyleTable,
    ) -> Value {
        let highlights = self.highlights_for(buffer, &s.numbers, styles);
        Value::Map(vec![
            (Value::from("from_top"), Value::from(s.from_top as u64)),
            (Value::from("to_top"), Value::from(s.to_top as u64)),
            (
                Value::from("from_cursor"),
                Value::from(s.from_cursor as u64),
            ),
            (Value::from("to_cursor"), Value::from(s.to_cursor as u64)),
            (Value::from("duration_ms"), Value::from(s.duration_ms)),
            (Value::from("base_line"), Value::from(s.base_line as u64)),
            (Value::from("lines"), lines_value(&s.lines)),
            (Value::from("selection"), spans_value(&s.selection)),
            (Value::from("numbers"), numbers_value(&s.numbers)),
            (Value::from("highlights"), highlights),
        ])
    }

    /// Resolve the editor-chrome highlight groups (the background, gutter,
    /// selection, and status line) to style-palette indices for this frame. Each
    /// resolved group becomes a `name -> style_id` entry; groups the colorscheme
    /// leaves undefined are simply absent, so the client keeps its built-in look
    /// (e.g. reverse-video selection) for them. Empty map when no theme is loaded.
    pub(crate) fn chrome_styles(&self, styles: &mut StyleTable) -> Value {
        // Map redraw key -> highlight group. The keys mirror the View regions the
        // client themes; the groups are neovim's standard chrome groups.
        const CHROME: &[(&str, &str)] = &[
            ("normal", "Normal"),
            ("line_nr", "LineNr"),
            ("cursor_line_nr", "CursorLineNr"),
            ("visual", "Visual"),
            ("search", "Search"),
            ("incsearch", "IncSearch"),
            ("status_line", "StatusLine"),
            ("end_of_buffer", "EndOfBuffer"),
        ];
        let entries = CHROME
            .iter()
            .filter_map(|(key, group)| {
                let style = self.editor.highlights.resolve(group)?;
                Some((Value::from(*key), Value::from(styles.intern(style) as u64)))
            })
            .collect();
        Value::Map(entries)
    }
}

/// A per-redraw palette of distinct resolved [`Style`]s, deduped so identical
/// styles (common across a theme's many same-colored groups) cost one wire entry
/// and the spans/chrome just carry small integer ids into it.
#[derive(Default)]
pub(crate) struct StyleTable {
    list: Vec<Style>,
    index: HashMap<Style, usize>,
}

impl StyleTable {
    /// Return the index of `style` in the palette, appending it on first sight.
    pub(crate) fn intern(&mut self, style: Style) -> usize {
        if let Some(&i) = self.index.get(&style) {
            return i;
        }
        let i = self.list.len();
        self.index.insert(style.clone(), i);
        self.list.push(style);
        i
    }

    /// Encode the palette as the redraw's `styles` array (index = position),
    /// each entry the same `{ fg, bg, sp, <attrs> }` map `nvim_get_hl` returns.
    fn into_value(self) -> Value {
        Value::Array(self.list.iter().map(style_value).collect())
    }
}

/// Encode a tab page as a `{ label, modified, window_count }` map for the redraw
/// map's `tabline` array. The client formats the cell and highlights the active
/// one (carried separately as `current_tab`).
fn tab_value(tab: &TabView) -> Value {
    Value::Map(vec![
        (Value::from("label"), Value::from(tab.label.as_str())),
        (Value::from("modified"), Value::from(tab.modified)),
        (
            Value::from("window_count"),
            Value::from(tab.window_count as u64),
        ),
    ])
}

/// Encode a split border as a `{ vertical, x, y, length }` map (cells, relative
/// to the windows area) for the redraw map's `separators` array.
fn separator_value(sep: &Separator) -> Value {
    Value::Map(vec![
        (Value::from("vertical"), Value::from(sep.vertical)),
        (Value::from("x"), Value::from(sep.x as u64)),
        (Value::from("y"), Value::from(sep.y as u64)),
        (Value::from("length"), Value::from(sep.length as u64)),
    ])
}

/// The wire name for a float's [`BorderStyle`] — the same tokens
/// `nvim_win_get_config` returns, which the client maps to its border glyphs.
fn border_name(border: BorderStyle) -> &'static str {
    match border {
        BorderStyle::None => "none",
        BorderStyle::Single => "single",
        BorderStyle::Rounded => "rounded",
        BorderStyle::Double => "double",
        BorderStyle::Solid => "solid",
    }
}

/// Encode a window's screen rect as a `{ x, y, width, height }` map (cells,
/// relative to the windows area) for the redraw map.
fn rect_value(rect: &ViewRect) -> Value {
    Value::Map(vec![
        (Value::from("x"), Value::from(rect.x as u64)),
        (Value::from("y"), Value::from(rect.y as u64)),
        (Value::from("width"), Value::from(rect.width as u64)),
        (Value::from("height"), Value::from(rect.height as u64)),
    ])
}

/// Encode a slice of text rows as a msgpack array of strings for the redraw map.
pub(crate) fn lines_value(lines: &[String]) -> Value {
    Value::Array(lines.iter().map(|l| Value::from(l.as_str())).collect())
}

/// Project the bottom panel (`:messages`, `:ls`) into its redraw sub-map.
fn project_panel(p: &PanelView) -> Value {
    Value::Map(vec![
        (Value::from("title"), Value::from(p.title.as_str())),
        (Value::from("lines"), lines_value(&p.lines)),
        (Value::from("cursor_row"), Value::from(p.cursor_row as u64)),
        (
            Value::from("cursor_span"),
            Value::from(p.cursor_span as u64),
        ),
        (Value::from("height"), Value::from(p.height as u64)),
    ])
}

/// Encode per-row selection spans as an array of `[start, end]` pairs (`Nil`
/// for unselected rows) for the redraw map.
fn spans_value(spans: &[Option<(usize, usize)>]) -> Value {
    Value::Array(
        spans
            .iter()
            .map(|s| match s {
                Some((start, end)) => {
                    Value::Array(vec![Value::from(*start as u64), Value::from(*end as u64)])
                }
                None => Value::Nil,
            })
            .collect(),
    )
}

/// Encode per-row *multiple* spans (the search-match highlight) as an array with
/// one entry per visible row, each an array of `[start, end]` screen-column
/// pairs (empty for rows with no match).
fn multi_spans_value(rows: &[Vec<(usize, usize)>]) -> Value {
    Value::Array(
        rows.iter()
            .map(|row| {
                Value::Array(
                    row.iter()
                        .map(|(start, end)| {
                            Value::Array(vec![Value::from(*start as u64), Value::from(*end as u64)])
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

/// Encode per-row 1-based line numbers as an array (`Nil` for `~` filler rows)
/// for the redraw map.
fn numbers_value(numbers: &[Option<usize>]) -> Value {
    Value::Array(
        numbers
            .iter()
            .map(|n| match n {
                Some(n) => Value::from(*n as u64),
                None => Value::Nil,
            })
            .collect(),
    )
}

/// Encode a resolved [`Style`] as the RPC map the query methods return: colors
/// as `0xRRGGBB` integers (neovim's convention) under `fg`/`bg`/`sp`, and each
/// set boolean attribute as `true`. Absent fields are simply omitted.
pub(crate) fn style_value(style: &Style) -> Value {
    let mut map = Vec::new();
    let mut color = |key: &str, c: Option<nxvim_core::Rgb>| {
        if let Some(rgb) = c {
            map.push((Value::from(key), Value::from(rgb.to_u32())));
        }
    };
    color("fg", style.fg);
    color("bg", style.bg);
    color("sp", style.sp);
    for (key, on) in [
        ("bold", style.bold),
        ("italic", style.italic),
        ("underline", style.underline),
        ("undercurl", style.undercurl),
        ("strikethrough", style.strikethrough),
        ("reverse", style.reverse),
    ] {
        if on {
            map.push((Value::from(key), Value::from(true)));
        }
    }
    Value::Map(map)
}
