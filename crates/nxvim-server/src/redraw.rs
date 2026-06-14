//! Projecting the editor `View` into the `redraw` notification map clients
//! render: lines, cursor, chrome styles, scroll band, panel, and the per-frame
//! deduped style palette ([`StyleTable`]).

use crate::EditHost;
use nxvim_core::highlight::Style;
use nxvim_core::statusline::{self, ExprKind};
use nxvim_core::view::{
    MenuView, RegionTabline, RegionTablines, ScrollAnim, Separator, TabView, ViewRect,
    WindowRegion, WindowView,
};
use nxvim_core::{BorderStyle, MenuPlacement, PanelView};
use rmpv::Value;
use std::collections::HashMap;

impl EditHost {
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
        // Match the current terminal's PTY winsize to its window before projecting,
        // so a resized terminal reflows its mirrored screen this frame. Native only —
        // the wasm terminal resize rides the daemon leg (Phase 7).
        #[cfg(feature = "native")]
        self.sync_terminal_sizes();
        let view = self.editor.view(w, h);

        // Refresh the current buffer's highlights from the in-process engine for
        // the freshly-settled viewport (same-frame, memoized per content+view).
        // Native only — the browser highlights JS-side (`nxvim-edithost`), and LSP sync
        // needs a language server (Phase 6).
        #[cfg(feature = "native")]
        self.refresh_highlights(h);
        // Drive LSP document sync for the current buffer (non-blocking).
        #[cfg(feature = "native")]
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
            // The under-cursor diagnostic is LSP-sourced — empty on the browser build.
            #[cfg(feature = "native")]
            {
                self.diagnostic_under_cursor().unwrap_or_default()
            }
            #[cfg(not(feature = "native"))]
            {
                String::new()
            }
        };

        // The global `'statusline'` / `'tabline'` formats (empty ⇒ the built-in
        // look), read once and shared across the window status + tabline projection.
        let statusline_fmt = self.editor.global_options().statusline;
        let tabline_fmt = self.editor.global_options().tabline;
        // The `'guifont'` value, relayed verbatim for a GUI client to parse and
        // apply; empty (the default) leaves the client on its own font.
        let guifont = self.editor.global_options().guifont;

        // A `%{}`/`%!` statusline *or* tabline expression evaluates Lua that reads
        // live editor state through the `vim.fn.*` surface (mode/cursor/buffer/
        // window/tab). Refresh the Rust→Lua mirror so those reads reflect this
        // frame; skip the cost when neither format has expressions (the default
        // looks and pure-field formats compute entirely in Rust, never reaching Lua).
        let has_expr = |f: &str| f.contains("%{") || f.contains("%!");
        if has_expr(&statusline_fmt) || has_expr(&tabline_fmt) {
            self.push_buf_mirror();
        }

        // Project each window: its rect, per-window text/gutter/status data, and
        // its own buffer's syntax/diagnostic slice.
        let windows: Vec<Value> = view
            .windows
            .iter()
            .map(|win| self.window_value(win, &view.mode_label, &statusline_fmt, &mut styles))
            .collect();

        // The single global status line (`laststatus=3`), spanning the full editor
        // width and showing the focused window's facts; `Nil` for modes 0/1/2,
        // where status lines are per-window (the `status` array above) instead.
        let global_status = match &view.global_statusline {
            Some(ctx) => {
                let width = self.ui.map_or(0, |(w, _)| w);
                self.render_statusline(ctx, width, &view.mode_label, &statusline_fmt, &mut styles)
            }
            None => Value::Nil,
        };

        // The split borders between windows.
        let separators = Value::Array(view.separators.iter().map(separator_value).collect());

        // The tabline cells (empty array when only one tab is open, so the client
        // draws no tabline) and the active cell index.
        let tabline = Value::Array(view.tabline.iter().map(tab_value).collect());

        // Per-region tablines: each region (main + each open dock) carries its own
        // independent tab pages. A map keyed by region (`main`/`left`/`right`/
        // `top`/`bottom`), each value `{ tabs: [...], current: N }`. Clients draw a
        // tabline at the top of each region's band from this; the legacy `tabline`/
        // `current_tab` above mirror `main` until every client migrates.
        let region_tablines = region_tablines_value(&view.region_tablines);

        // A custom `'tabline'` ('tabline' option non-empty), rendered through the
        // same `%`-format engine as the statusline into one styled row spanning the
        // full editor width — the focused window supplies the `%`-item context, as
        // in neovim. `Nil` when the option is empty (the client formats the
        // structured `tabline` cells itself) or the tabline isn't shown this frame
        // (`view.tabline` empty ⇒ `showtabline` hides it). The mirror was already
        // refreshed above when the format has expressions.
        let tabline_segments = if tabline_fmt.is_empty() || view.tabline.is_empty() {
            Value::Nil
        } else {
            self.render_statusline(
                &view.focused().status_ctx,
                w,
                &view.mode_label,
                &tabline_fmt,
                &mut styles,
            )
        };

        // The insert-mode completion popup, `Nil` when none is open. The focused
        // window's text-area width (its width minus its number gutter) bounds the
        // overlay so it can't spill past the editable region.
        let text_width = view
            .focused()
            .rect
            .width
            .saturating_sub(view.focused().number_width);
        // The popup is LSP-completion-driven — always `Nil` on the browser build.
        #[cfg(feature = "native")]
        let pmenu = self.pmenu_value(&view, text_width);
        #[cfg(not(feature = "native"))]
        let pmenu = {
            let _ = text_width;
            Value::Nil
        };
        // The bottom panel (`:messages`, `:ls`), `Nil` when none is open.
        let panel = match &view.panel {
            Some(p) => project_panel(p),
            None => Value::Nil,
        };
        // The floating selectable-list menu (`nx.ui.select`; later the picker),
        // `Nil` when none is open. Geometry is computed here from the focused
        // window, the same way the completion popup is placed.
        let menu = match &view.menu {
            Some(m) => project_menu(m, &view, text_width),
            None => Value::Nil,
        };

        // Built last: every per-window/`chrome` style id above indexes into it.
        let styles_value = styles.into_value();
        let map = vec![
            (Value::from("windows"), Value::Array(windows)),
            (Value::from("global_status"), global_status),
            (Value::from("separators"), separators),
            (Value::from("tabline"), tabline),
            (Value::from("region_tablines"), region_tablines),
            (Value::from("tabline_segments"), tabline_segments),
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
            (Value::from("guifont"), Value::from(guifont.as_str())),
            (Value::from("styles"), styles_value),
            (Value::from("chrome"), chrome),
            (Value::from("panel"), panel),
            (Value::from("pmenu"), pmenu),
            (Value::from("dock_left"), Value::from(view.dock_left as u64)),
            (
                Value::from("dock_right"),
                Value::from(view.dock_right as u64),
            ),
            (Value::from("dock_top"), Value::from(view.dock_top as u64)),
            (
                Value::from("dock_bottom"),
                Value::from(view.dock_bottom as u64),
            ),
            (Value::from("menu"), menu),
        ];

        self.fx.notify("redraw", vec![Value::Map(map)]);
    }

    /// Project one window into its redraw sub-map: the rect and focus flag, the
    /// per-window text/cursor/gutter/status fields, and the window's own syntax
    /// highlights, diagnostic underlines, and scroll band (each resolving styles
    /// into the shared per-frame `styles` palette).
    fn window_value(
        &self,
        win: &WindowView,
        mode_label: &str,
        statusline_fmt: &str,
        styles: &mut StyleTable,
    ) -> Value {
        // Syntax highlights (treesitter) and the LSP overlays (diagnostics / signs /
        // inlay hints) are native-only projections; the browser build emits empty
        // arrays for them so the redraw map keeps a stable shape (JS-side highlighting
        // paints from the buffer text instead).
        #[cfg(feature = "native")]
        let highlights = self.highlights_for(win.buffer, &win.numbers, styles);
        #[cfg(not(feature = "native"))]
        let highlights = Value::Array(Vec::new());
        let status = self.status_value(win, mode_label, statusline_fmt, styles);
        #[cfg(feature = "native")]
        let (diagnostics, diagnostics_virt, diagnostics_signs, sign_column, inlay_hints) = (
            self.diagnostics_for(win.buffer, &win.numbers, styles),
            self.diagnostics_virt_text_for(win.buffer, &win.numbers, styles),
            self.diagnostics_signs_for(win.buffer, &win.numbers, styles),
            self.diagnostics_sign_column(win.buffer),
            self.inlay_hints_for(win.buffer, &win.numbers, styles),
        );
        #[cfg(not(feature = "native"))]
        let (diagnostics, diagnostics_virt, diagnostics_signs, sign_column, inlay_hints) = (
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            false,
            Value::Array(Vec::new()),
        );
        let scroll = match &win.scroll {
            Some(s) => self.project_band(win.buffer, s, styles),
            None => Value::Nil,
        };
        Value::Map(vec![
            (Value::from("rect"), rect_value(&win.rect)),
            (Value::from("region"), Value::from(region_str(win.region))),
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
            (
                Value::from("cursors"),
                Value::Array(
                    win.secondary_cursors
                        .iter()
                        .map(|&(row, col)| {
                            Value::Array(vec![Value::from(row as u64), Value::from(col as u64)])
                        })
                        .collect(),
                ),
            ),
            (Value::from("leftcol"), Value::from(win.leftcol as u64)),
            (
                Value::from("file_name"),
                Value::from(win.file_name.as_str()),
            ),
            // The buffer's effective treesitter filetype (override or extension),
            // so a client that highlights JS-side (the wasm edit-host) can pick the
            // grammar. Native clients ignore it (they paint server highlight spans).
            (Value::from("filetype"), Value::from(win.filetype.as_str())),
            (Value::from("unnamed"), Value::from(win.unnamed)),
            (Value::from("modified"), Value::from(win.modified)),
            (
                Value::from("cursor_line"),
                Value::from(win.cursor_line as u64),
            ),
            (Value::from("selection"), spans_value(&win.selection)),
            (
                Value::from("secondary_selection"),
                multi_spans_value(&win.secondary_selection),
            ),
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
            (Value::from("diagnostics_virt"), diagnostics_virt),
            (Value::from("diagnostics_signs"), diagnostics_signs),
            (Value::from("sign_column"), Value::from(sign_column)),
            (Value::from("inlay_hints"), inlay_hints),
            (Value::from("status"), status),
            // Whether this window paints its own status row (per `'laststatus'`).
            // False hides it (modes 0/3, or 1 with one window); the client then
            // gives the freed row to text rather than carving a status line.
            (
                Value::from("status_visible"),
                Value::from(win.status_visible),
            ),
            (Value::from("scroll"), scroll),
            // Float overlay chrome. A tiled window is `floating: false` with no
            // border/title, so the client paints it exactly as before.
            (Value::from("floating"), Value::from(win.floating)),
            (Value::from("border"), Value::from(win.border.as_str())),
            (
                Value::from("title"),
                match &win.title {
                    Some(t) => Value::from(t.as_str()),
                    None => Value::Nil,
                },
            ),
        ])
    }

    /// Project a window's status line as the `status` array — one `{ text, style }`
    /// segment per highlighted run. Runs the [`statusline`] `%`-format engine over
    /// the global `'statusline'` (or the built-in default when empty) against the
    /// window's pre-computed `status_ctx`, evaluating `%{}`/`%!` expressions via
    /// the (Lua-aware) [`EditHost::eval_statusline_expr`] callback, then resolves
    /// each segment's highlight group to a palette style id.
    ///
    /// `style` is `Nil` for a segment with no group, or one whose group the
    /// colorscheme leaves undefined — the client then paints it in the base
    /// `StatusLine` look (the `status_line` chrome style, or reverse-video out of
    /// the box), exactly as it did before per-segment styling existed.
    fn status_value(
        &self,
        win: &WindowView,
        mode_label: &str,
        statusline_fmt: &str,
        styles: &mut StyleTable,
    ) -> Value {
        // The status line spans the window's content width — its rect inset by the
        // float border, matching where the client paints it (and what `%=`/`%<`
        // resolve against).
        let inset = if win.floating && win.border != BorderStyle::None {
            1
        } else {
            0
        };
        let width = win.rect.width.saturating_sub(2 * inset);
        self.render_statusline(&win.status_ctx, width, mode_label, statusline_fmt, styles)
    }

    /// Run the `%`-format engine over one [`StatuslineCtx`] across `width` cells and
    /// project the result as a `status` segment array (`{ text, style }` per
    /// highlighted run). Shared by the per-window status line ([`Self::status_value`])
    /// and the single global one (`laststatus=3`); both differ only in their context
    /// and width. `statusline_fmt` empty ⇒ the built-in default look (rendered through
    /// the same engine). Each segment's highlight group resolves to a style-palette
    /// id, `Nil` when it has none / the colorscheme leaves it undefined.
    fn render_statusline(
        &self,
        ctx: &nxvim_core::statusline::StatuslineCtx,
        width: usize,
        mode_label: &str,
        statusline_fmt: &str,
        styles: &mut StyleTable,
    ) -> Value {
        let default;
        let fmt = if statusline_fmt.is_empty() {
            default = default_statusline(mode_label);
            &default
        } else {
            statusline_fmt
        };

        let items = match statusline::parse(fmt) {
            Ok(items) => items,
            // A malformed 'statusline' shows its own error text rather than a
            // blank line — loud, not silent (per CLAUDE.md).
            Err(err) => return Value::Array(vec![segment_value(&err, Value::Nil)]),
        };

        let mut eval = |_kind: ExprKind, raw: &str| self.eval_statusline_expr(raw);
        let pieces = statusline::expand(&items, ctx, &mut eval);
        let segments = statusline::layout(&pieces, width);

        Value::Array(
            segments
                .iter()
                .map(|seg| {
                    let style = seg
                        .group
                        .as_deref()
                        .and_then(|g| self.editor.highlights.resolve(g))
                        .map(|s| Value::from(styles.intern(s) as u64))
                        .unwrap_or(Value::Nil);
                    segment_value(&seg.text, style)
                })
                .collect(),
        )
    }

    /// Evaluate one `%{}`/`%!` statusline expression. nxvim has no Vimscript, so
    /// **only `v:lua.…` expressions are supported** — anything else returns a loud
    /// `E:…` marker naming the offending expression (rendered on the status line)
    /// rather than silently expanding to nothing. A `v:lua.` prefix is stripped to
    /// the bare Lua expression (`v:lua.require('m').f()` → `require('m').f()`),
    /// which the synchronous evaluator runs inline during redraw.
    fn eval_statusline_expr(&self, raw: &str) -> String {
        let expr = raw.trim();
        let Some(lua) = expr.strip_prefix("v:lua.") else {
            return format!(
                "E:statusline: unsupported expression {{{expr}}} (only v:lua.* is supported)"
            );
        };
        match self.lua.eval_to_value(lua) {
            Ok(value) => stringify_eval(&value),
            Err(err) => format!("E:{err}"),
        }
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
        // Native-only overlays (see `window_value`); the browser band carries empty
        // highlight/inlay arrays.
        #[cfg(feature = "native")]
        let highlights = self.highlights_for(buffer, &s.numbers, styles);
        #[cfg(not(feature = "native"))]
        let highlights = Value::Array(Vec::new());
        // Inlay hints ride the band too (keyed on `s.numbers` like highlights), so
        // they slide with the text instead of vanishing until the slide settles.
        #[cfg(feature = "native")]
        let inlay_hints = self.inlay_hints_for(buffer, &s.numbers, styles);
        #[cfg(not(feature = "native"))]
        let inlay_hints = Value::Array(Vec::new());
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
            (
                Value::from("sel_extends_down"),
                s.sel_extends_down.map_or(Value::Nil, Value::from),
            ),
            // hlsearch / incsearch matches for the band, so the highlight rides the
            // slide rather than vanishing until it settles (mirrors `window_value`).
            (Value::from("search"), multi_spans_value(&s.search)),
            (Value::from("incsearch"), spans_value(&s.incsearch)),
            (Value::from("numbers"), numbers_value(&s.numbers)),
            (Value::from("highlights"), highlights),
            (Value::from("inlay_hints"), inlay_hints),
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

/// Encode one status-line segment as `{ text, style }` for the `status` array.
/// `style` is a `u64` index into the frame's style palette, or `Nil` for the
/// base `StatusLine` look.
fn segment_value(text: &str, style: Value) -> Value {
    Value::Map(vec![
        (Value::from("text"), Value::from(text)),
        (Value::from("style"), style),
    ])
}

/// The built-in `'statusline'` look, expressed as a format string so the one
/// engine renders it too: ` MODE  file[+]  …  line,col `. The mode label is an
/// nxvim addition (neovim shows the mode elsewhere), spliced in as a literal —
/// escaped, though mode names never contain `%` — since it is not a `%`-item.
fn default_statusline(mode_label: &str) -> String {
    format!(" {}  %f%m%=%l,%c ", mode_label.replace('%', "%%"))
}

/// Coerce a `%{}`/`%!` Lua result to the text that goes on the status line.
/// Strings pass through; numbers stringify; `nil`/`false` (lualine's "nothing
/// here") and anything more exotic render empty — matching neovim's lenient
/// expression-to-text coercion for the cases a status line actually produces.
fn stringify_eval(value: &Value) -> String {
    match value {
        Value::String(s) => s.as_str().unwrap_or_default().to_string(),
        Value::Integer(n) => n.to_string(),
        Value::F64(n) => n.to_string(),
        Value::F32(n) => n.to_string(),
        _ => String::new(),
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

/// One region's tabline as `{ tabs: [cell, …], current: N }`. `tabs` is empty when
/// that region draws no tabline (hidden by `showtabline`, or a closed dock).
fn region_tabline_value(rt: &RegionTabline) -> Value {
    Value::Map(vec![
        (
            Value::from("tabs"),
            Value::Array(rt.tabs.iter().map(tab_value).collect()),
        ),
        (Value::from("current"), Value::from(rt.current as u64)),
        (Value::from("title"), Value::from(rt.title.as_str())),
    ])
}

/// The per-region tablines as a map keyed by region — `main` plus the four docks
/// (`left`/`right`/`top`/`bottom`, matching the `nx.dock` side keywords). Each
/// value is a [`region_tabline_value`].
fn region_tablines_value(rts: &RegionTablines) -> Value {
    Value::Map(vec![
        (Value::from("main"), region_tabline_value(&rts.main)),
        (Value::from("left"), region_tabline_value(&rts.docks[0])),
        (Value::from("right"), region_tabline_value(&rts.docks[1])),
        (Value::from("top"), region_tabline_value(&rts.docks[2])),
        (Value::from("bottom"), region_tabline_value(&rts.docks[3])),
    ])
}

/// Encode a split border as a `{ vertical, x, y, length, region }` map (cells,
/// relative to the separator's region origin) for the redraw map's `separators`
/// array.
fn separator_value(sep: &Separator) -> Value {
    Value::Map(vec![
        (Value::from("vertical"), Value::from(sep.vertical)),
        (Value::from("x"), Value::from(sep.x as u64)),
        (Value::from("y"), Value::from(sep.y as u64)),
        (Value::from("length"), Value::from(sep.length as u64)),
        (Value::from("region"), Value::from(region_str(sep.region))),
    ])
}

/// The wire string for a window/separator [`WindowRegion`] — `"main"` or one of
/// `"dock_left"`/`"dock_right"`/`"dock_top"`/`"dock_bottom"`. Clients map it to the
/// region's absolute screen origin using the redraw map's dock band sizes.
fn region_str(region: WindowRegion) -> &'static str {
    match region {
        WindowRegion::Main => "main",
        WindowRegion::DockLeft => "dock_left",
        WindowRegion::DockRight => "dock_right",
        WindowRegion::DockTop => "dock_top",
        WindowRegion::DockBottom => "dock_bottom",
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

/// Project the floating selectable-list [`MenuView`] into its redraw sub-map,
/// computing the bordered box's anchor and content size in **text-area cells**
/// (the client adds the gutter + text-area origin, then draws the border) — the
/// same convention and placement strategy as the completion popup. `Cursor`
/// placement anchors under the cursor and flips above when there's no room
/// below; `Editor` placement centers the box over the focused window's text area
/// (the picker refines this when it lands). `text_width` bounds the box to the
/// editable region. Mirrors `EditHost::pmenu_value`.
fn project_menu(m: &MenuView, view: &nxvim_core::View, text_width: usize) -> Value {
    const MAX_H: usize = 10;
    let focused = view.focused();
    let text_height = focused.lines.len();
    let count = m.items.len();
    let want = count.min(MAX_H);
    // Content width: the widest label (in cells), min 1.
    let content_w = m
        .items
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(1)
        .max(1);

    let (row, col, width, height) = match m.placement {
        MenuPlacement::Cursor => {
            let cursor_row = focused.cursor_row;
            let anchor_col = focused.cursor_screen_col.saturating_sub(focused.leftcol);
            let max_w = text_width.saturating_sub(anchor_col).max(1);
            let width = content_w.min(max_w);
            // Below if the bordered box fits, else above, else clamp to whichever
            // side has more room (the popup's four-tier fallback).
            let below = text_height.saturating_sub(cursor_row + 1);
            let above = cursor_row;
            let (row, height) = if want + 2 <= below {
                (cursor_row + 1, want)
            } else if want + 2 <= above {
                (cursor_row - (want + 2), want)
            } else if below >= above {
                (cursor_row + 1, below.saturating_sub(2).clamp(1, want))
            } else {
                let h = above.saturating_sub(2).clamp(1, want);
                (cursor_row.saturating_sub(h + 2), h)
            };
            (row, anchor_col, width, height)
        }
        MenuPlacement::Editor => {
            let height = want.min(text_height.saturating_sub(2).max(1));
            let width = content_w.min(text_width.saturating_sub(2).max(1));
            let row = text_height.saturating_sub(height + 2) / 2;
            let col = text_width.saturating_sub(width + 2) / 2;
            (row, col, width, height)
        }
    };

    let items: Vec<Value> = m.items.iter().map(|s| Value::from(s.as_str())).collect();
    Value::Map(vec![
        (Value::from("items"), Value::Array(items)),
        (Value::from("selected"), Value::from(m.selected as u64)),
        (Value::from("row"), Value::from(row as u64)),
        (Value::from("col"), Value::from(col as u64)),
        (Value::from("width"), Value::from(width as u64)),
        (Value::from("height"), Value::from(height as u64)),
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
