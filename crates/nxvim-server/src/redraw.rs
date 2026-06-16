//! Projecting the editor `View` into the `redraw` notification map clients
//! render: lines, cursor, chrome styles, scroll band, panel, and the per-frame
//! deduped style palette ([`StyleTable`]).

use crate::EditHost;
use nxvim_core::editor::expr::{self, OptVal};
use nxvim_core::highlight::Style;
use nxvim_core::statusline::{self, ExprKind};
use nxvim_core::unicode;
use nxvim_core::view::{
    MenuView, RegionTabline, RegionTablines, ScrollAnim, Separator, TabView, ViewRect,
    WindowRegion, WindowView,
};
use nxvim_core::{BorderStyle, ContentFloatView, MenuPlacement, PanelView};
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
        // so a resized terminal reflows its mirrored screen this frame. Both builds:
        // native resizes the local PTY, wasm forwards a `term_resize` to the daemon.
        self.sync_terminal_sizes();
        // Color the focused terminal's scrollback only while it's being browsed (never
        // on the live flood path); a no-op when output is live or already materialized.
        self.sync_terminal_styles();
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
        // The legacy `pmenu` key is retired (Phase 4-C): all completion — including
        // the `lsp` source — now renders through the unified `menu` widget below, so
        // this is always `Nil`. Kept as a key for client wire compatibility.
        let _ = text_width;
        let pmenu = Value::Nil;
        // The bottom panel (`:messages`, `:ls`), `Nil` when none is open.
        let panel = match &view.panel {
            Some(p) => project_panel(p),
            None => Value::Nil,
        };
        // The floating selectable-list menu (`nx.ui.select`; later the picker),
        // `Nil` when none is open. Geometry is computed here from the focused
        // window, the same way the completion popup is placed.
        let menu = match &view.menu {
            Some(m) => self.project_menu(m, &view, text_width, &mut styles),
            None => Value::Nil,
        };
        // The list-less content float (`nx.ui.float`; LSP hover / signature help),
        // `Nil` when none is open. A non-grabbing transient overlay — its geometry
        // is computed here from the cursor (or centered over the editor).
        let float = match &view.content_float {
            Some(cf) => self.project_content_float(cf, &view, text_width),
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
            // The current buffer's identity + edit version, so the browser Worker ships
            // the full buffer text to its JS highlighter only when the text actually
            // changed (not on cursor-only / terminal-driven redraws). `(bufnr,
            // changedtick)` distinguishes an edit *and* a buffer switch.
            (
                Value::from("bufnr"),
                Value::from(self.editor.current_buffer_id().0),
            ),
            (
                Value::from("changedtick"),
                Value::from(self.editor.buffer().changedtick),
            ),
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
            (
                Value::from("hidden_docks"),
                Value::Array(
                    view.hidden_docks
                        .iter()
                        .map(|l| Value::from(l.as_str()))
                        .collect(),
                ),
            ),
            (Value::from("menu"), menu),
            (Value::from("float"), float),
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
        // The browser build highlights code JS-side and leaves these empty — *except*
        // a terminal, whose per-cell colors live only in the wasm-side vt100 grid and
        // can't be recovered from the buffer text. Project those (and intern their
        // styles into the shared palette); `terminal_highlights` returns `None` for a
        // non-terminal window, so a code buffer keeps its empty array + JS highlighting.
        #[cfg(not(feature = "native"))]
        let highlights = self
            .terminal_highlights(win.buffer, &win.numbers, styles)
            .unwrap_or_else(|| Value::Array(Vec::new()));
        // Display columns of the `^X` / `<xx>` substitutions, for the wasm renderer
        // to colour as `SpecialKey`; the native client paints them from `highlights`,
        // so it gets an empty array (keeping the redraw map shape stable).
        #[cfg(feature = "native")]
        let special_key = Value::Array(Vec::new());
        #[cfg(not(feature = "native"))]
        let special_key = special_key_spans(&win.lines, win.tabstop);
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
            (Value::from("lines"), display_lines_value(&win.lines)),
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
                Value::from("cursor_width"),
                Value::from(win.cursor_width as u64),
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
            // Whether this window's buffer is a *live* terminal. Gates the live
            // terminal-only behaviors: the Worker skips shipping its (potentially huge
            // scrollback) text, since its colors come from the palette, not the JS
            // highlighter. Goes false the instant the child exits (the buffer becomes
            // plain, editable text whose lines ship normally again).
            (
                Value::from("terminal"),
                Value::from(self.terminals.contains_key(&win.buffer)),
            ),
            // Whether this window paints from the server vt100 color palette
            // (`highlights` + `styles`) rather than the JS highlighter — a live
            // terminal, *or* a closed one that kept its frozen colors. Distinct from
            // `terminal`: a dead terminal ships its text and stays navigable, yet its
            // final output keeps its highlighting (the colors are frozen at exit; see
            // `terminal_frozen`). The browser uses this for the per-window render path
            // and to keep terminal style contributions out of the global chrome mode.
            (
                Value::from("term_colors"),
                Value::from(
                    self.terminals.contains_key(&win.buffer)
                        || self.terminal_frozen.contains_key(&win.buffer),
                ),
            ),
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
            (Value::from("special_key"), special_key),
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
            default = default_statusline(mode_label, &ctx.fileencoding, ctx.bomb);
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

        let mut eval = |_kind: ExprKind, raw: &str| self.eval_statusline_expr(raw, ctx);
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

    /// Evaluate one `%{}`/`%!` statusline expression against the window's
    /// [`StatuslineCtx`]. Two expression flavours are supported:
    ///
    /// - **`v:lua.…`** — the `v:lua.` prefix is stripped to the bare Lua
    ///   expression (`v:lua.require('m').f()` → `require('m').f()`), which the
    ///   synchronous evaluator runs inline during redraw. (nxvim has no Vimscript;
    ///   `v:lua.` is the bridge to a config's own logic.)
    /// - **Pure Vim expressions** — literals, arithmetic, comparison, logical and
    ///   ternary operators, and `&option` references (`%{&fileencoding}`,
    ///   `%{&bomb?"[bom]":""}`). These run through the pure core evaluator
    ///   ([`nxvim_core::editor::expr::eval_expr`]); `&option` resolves against the
    ///   buffer-display options the `StatuslineCtx` carries.
    ///
    /// Anything else — a bare variable, an unknown option, a malformed expression —
    /// returns a loud `E:…` marker naming the offender (rendered on the status
    /// line) rather than silently expanding to nothing.
    fn eval_statusline_expr(
        &self,
        raw: &str,
        ctx: &nxvim_core::statusline::StatuslineCtx,
    ) -> String {
        let expr = raw.trim();
        if let Some(lua) = expr.strip_prefix("v:lua.") {
            return match self.lua.eval_to_value(lua) {
                Ok(value) => stringify_eval(&value),
                Err(err) => format!("E:{err}"),
            };
        }
        match expr::eval_expr(expr, &|name| statusline_option(ctx, name)) {
            Ok(text) => text,
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
            (Value::from("lines"), display_lines_value(&s.lines)),
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
/// engine renders it too: ` MODE  file[+]  …  enc  line,col `. The mode label and
/// the encoding are nxvim additions spliced in as literals — escaped, though
/// neither a mode name nor an encoding label ever contains `%` — since they are not
/// `%`-items (neovim has no encoding item; it's conventionally `%{&fenc}`). `enc`
/// carries the buffer's `'fileencoding'`, with a `[bom]` suffix when `'bomb'` is set.
///
/// The `%<` before `%f` is the truncation point (vim's `%<%f` idiom): when the line
/// is too narrow, the *path* is the thing that shrinks (keeping its tail), so the
/// right-aligned encoding + position stay visible. Without it the cut would default
/// to the `%=` marker, and a long path would overflow the prefix and drop the whole
/// right-aligned section — hiding the encoding behind a `>`.
fn default_statusline(mode_label: &str, fileencoding: &str, bomb: bool) -> String {
    let enc = if bomb {
        format!("{fileencoding}[bom]")
    } else {
        fileencoding.to_string()
    };
    format!(
        " {}  %<%f%m%={}  %l,%c ",
        mode_label.replace('%', "%%"),
        enc.replace('%', "%%"),
    )
}

/// Resolve an `&option` reference in a statusline `%{…}` against the buffer's
/// display state, as captured in [`StatuslineCtx`]. Only the options a status
/// line meaningfully shows — and that the projected context actually carries —
/// are known; anything else returns `None`, which the evaluator turns into a loud
/// `E518: Unknown option` (per CLAUDE.md's no-silent-stub rule). Boolean options
/// resolve to `0`/`1`, matching Vim's numeric view of `&bomb`, `&modified`, ….
fn statusline_option(ctx: &nxvim_core::statusline::StatuslineCtx, name: &str) -> Option<OptVal> {
    match name {
        "fileencoding" | "fenc" => Some(OptVal::Str(ctx.fileencoding.clone())),
        "filetype" | "ft" => Some(OptVal::Str(ctx.filetype.clone())),
        "bomb" => Some(OptVal::Int(ctx.bomb as i64)),
        "modified" | "mod" => Some(OptVal::Int(ctx.modified as i64)),
        "readonly" | "ro" => Some(OptVal::Int(ctx.readonly as i64)),
        "modifiable" | "ma" => Some(OptVal::Int(ctx.modifiable as i64)),
        _ => None,
    }
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

/// Encode a slice of text rows as a msgpack array of strings — verbatim, the raw
/// buffer bytes. This is the **content** encoding (`nvim_buf_get_lines` and the
/// like), so a plugin reading buffer text sees the real scalars. The redraw paint
/// path uses [`display_lines_value`] instead, which substitutes unprintable
/// control chars.
pub(crate) fn lines_value(lines: &[String]) -> Value {
    Value::Array(lines.iter().map(|l| Value::from(l.as_str())).collect())
}

/// Encode a slice of text rows for the **display** path (the redraw `lines` array
/// the client paints), passing each through [`unicode::display_line`]. An
/// unprintable control byte — a C1 control from the latin1 fallback, an embedded
/// C0 control — reaches the client as its vim-style `^X` / `<xx>` text instead of
/// a font tofu box. The substitution is display-only (the buffer keeps the
/// original bytes) and its widths match the server's column math (`grapheme_width`),
/// so the cursor and highlight spans still line up. Used for the window rows and
/// the scroll band; content reads stay on [`lines_value`].
pub(crate) fn display_lines_value(lines: &[String]) -> Value {
    Value::Array(
        lines
            .iter()
            .map(|l| Value::from(unicode::display_line(l).as_ref()))
            .collect(),
    )
}

/// Per-line **display-column** spans (`[start, end)`, half-open, same virtcol
/// space as `selection`/`search`) of the unprintable control chars that
/// [`display_lines_value`] substitutes as `^X` / `<xx>` tokens. The native build
/// colours these tokens by overlaying the `SpecialKey` highlight group in its
/// server-computed highlight spans (`treesitter.rs`); the wasm edit-host has no
/// server highlights and paints from the buffer text JS-side, so it needs the
/// token columns spelled out to give them the same colour. Emitted only on the
/// non-native build for that reason — over a daemon the web takes the
/// server-styled path and gets `SpecialKey` from the highlight spans instead.
#[cfg(not(feature = "native"))]
fn special_key_spans(lines: &[String], tabstop: usize) -> Value {
    Value::Array(
        lines
            .iter()
            .map(|l| {
                let positions = unicode::unprintable_positions(l);
                if positions.is_empty() {
                    return Value::Array(Vec::new());
                }
                // `LineVirtcol::at` walks forward; `unprintable_positions` returns
                // byte ranges in increasing order, so the (sb, eb, next sb, …) calls
                // are monotonic and stay on the cheap forward path.
                let mut vc = unicode::LineVirtcol::new(l, tabstop);
                Value::Array(
                    positions
                        .iter()
                        .map(|&(sb, eb)| {
                            let start = vc.at(sb) as u64;
                            let end = vc.at(eb) as u64;
                            Value::Array(vec![Value::from(start), Value::from(end)])
                        })
                        .collect(),
                )
            })
            .collect(),
    )
}

/// Project the bottom panel (`:messages`, `:ls`) into its redraw sub-map. Panel
/// rows go through [`display_lines_value`] so an unprintable control byte in a
/// message — a C1 control from the latin1 fallback, an embedded C0 control —
/// shows as its `^X` / `<xx>` token rather than a font tofu box, exactly as the
/// window text does. The panel paints plain text (no per-cell span overlay), so
/// the substitution needs no accompanying column math.
fn project_panel(p: &PanelView) -> Value {
    Value::Map(vec![
        (Value::from("title"), Value::from(p.title.as_str())),
        (Value::from("lines"), display_lines_value(&p.lines)),
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
impl EditHost {
    fn project_menu(
        &mut self,
        m: &MenuView,
        view: &nxvim_core::View,
        text_width: usize,
        styles: &mut StyleTable,
    ) -> Value {
        let editor = &self.editor;
        const MAX_H: usize = 10;
        let focused = view.focused();
        let text_height = focused.lines.len();
        // A picker carries a prompt line plus a separator row between it and the list;
        // `nx.ui.select` carries neither. Both count toward the box height (`chrome`),
        // the prompt's text toward the width.
        let prompt_rows = usize::from(m.query.is_some());
        let chrome = prompt_rows * 2;
        let query_w = m.query.as_ref().map_or(0, |q| q.chars().count() + 1);

        // The box height (content rows), the scroll offset of the first visible row,
        // and the windowed rows themselves — only the visible slice is materialized, so
        // a 100k-item picker costs the same per frame as a 10-item one.
        let (row, col, width, height, rows, selected) = match m.placement {
            MenuPlacement::Cursor => {
                // `select` is small — project the whole list (no scrolling subtlety) and
                // let the client place the cursor; keeps the four-tier flip exact.
                let rows = editor.menu_rows(0, m.total);
                let count = (rows.len() + prompt_rows).min(MAX_H);
                let content_w = rows
                    .iter()
                    .map(|(l, _)| l.chars().count())
                    .max()
                    .unwrap_or(1)
                    .max(query_w)
                    .max(1);
                let cursor_row = focused.cursor_row;
                // Anchor under the start of the word being completed (the caret
                // minus the typed prefix's display width), not under the caret —
                // so the list lines up with the text it will replace. `anchor_offset`
                // is `0` for a `select`, leaving it cursor-anchored as before. This
                // is the logical content anchor (the word start); each client offsets
                // the box left by its own left-border width so the *content* lands
                // here (a full cell in the TUI / GUI, ~nothing for the web's 1px rule).
                let anchor_col = focused
                    .cursor_screen_col
                    .saturating_sub(focused.leftcol)
                    .saturating_sub(m.anchor_offset);
                let max_w = text_width.saturating_sub(anchor_col).max(1);
                let width = content_w.min(max_w);
                // The vertical border chrome: 2 (top + bottom) normally, 1 for the
                // top-borderless completion popup. Drives both the fit test and the
                // above-placement origin.
                let vchrome = if m.completion { 1 } else { 2 };
                // Below if the bordered box fits, else above, else clamp to whichever
                // side has more room (the popup's four-tier fallback).
                let below = text_height.saturating_sub(cursor_row + 1);
                let above = cursor_row;
                let (row, height) = if count + vchrome <= below {
                    (cursor_row + 1, count)
                } else if count + vchrome <= above {
                    (cursor_row - (count + vchrome), count)
                } else if below >= above {
                    (
                        cursor_row + 1,
                        below.saturating_sub(vchrome).clamp(1, count),
                    )
                } else {
                    let h = above.saturating_sub(vchrome).clamp(1, count);
                    (cursor_row.saturating_sub(h + vchrome), h)
                };
                (row, anchor_col, width, height, rows, m.selected)
            }
            MenuPlacement::Editor => {
                // A picker is a FIXED box — never content-hugging (that looks ragged).
                // Resolve the configured extent against the viewport, default ~80% × 60%.
                const DEFAULT_W: f32 = 0.8;
                const DEFAULT_H: f32 = 0.6;
                let max_w = text_width.saturating_sub(2).max(1);
                let max_h = text_height.saturating_sub(2).max(1);
                let width = m
                    .width
                    .map_or((text_width as f32 * DEFAULT_W).round() as usize, |e| {
                        e.resolve(text_width)
                    })
                    .clamp(1, max_w);
                let height = m
                    .height
                    .map_or((text_height as f32 * DEFAULT_H).round() as usize, |e| {
                        e.resolve(text_height)
                    })
                    .clamp(chrome + 1, max_h);
                let row = text_height.saturating_sub(height + 2) / 2;
                let col = text_width.saturating_sub(width + 2) / 2;
                // Scroll the window so the selected row stays visible, clamped to the end,
                // and send `selected` rebased into that window (the client renders the
                // window directly). Only `list_rows` rows are cloned, never all `total`.
                // `chrome` reserves the prompt + separator rows.
                let list_rows = height.saturating_sub(chrome).max(1);
                let mut start = if m.selected >= list_rows {
                    m.selected + 1 - list_rows
                } else {
                    0
                };
                start = start.min(m.total.saturating_sub(list_rows));
                let rows = editor.menu_rows(start, list_rows);
                (row, col, width, height, rows, m.selected - start)
            }
        };

        // The preview pane (Phase 3): a column on the right of an editor-placement
        // picker rendering the selected row's file. `None` for a `select` / preview-less
        // picker (and for `Cursor` placement — the cursor float-beside is Phase 4).
        // Sized against the resolved box; the map carries its own `width` so the client
        // knows how many columns the list keeps (`box width − preview width − 1`).
        let preview = if matches!(m.placement, MenuPlacement::Editor) {
            self.project_preview(m, width, height, styles)
        } else {
            None
        };

        let items: Vec<Value> = rows
            .iter()
            .map(|(label, _)| Value::from(label.as_str()))
            .collect();
        // Matched-character spans per visible row (parallel to `items`): `[start, end]`
        // half-open **char** ranges the client bolds.
        let match_spans = Value::Array(
            rows.iter()
                .map(|(_, spans)| {
                    Value::Array(
                        spans
                            .iter()
                            .map(|r| {
                                Value::Array(vec![
                                    Value::from(r.start as u64),
                                    Value::from(r.end as u64),
                                ])
                            })
                            .collect(),
                    )
                })
                .collect(),
        );
        let mut map = vec![
            (Value::from("items"), Value::Array(items)),
            (Value::from("selected"), Value::from(selected as u64)),
            (
                Value::from("selected_active"),
                Value::from(m.selected_active),
            ),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(col as u64)),
            (Value::from("width"), Value::from(width as u64)),
            (Value::from("height"), Value::from(height as u64)),
            (Value::from("match_spans"), match_spans),
        ];
        // The completion popup omits its top border so it sits flush with the line
        // below the cursor. Absent ⇒ a full border (the `select` / picker default).
        if m.completion {
            map.push((Value::from("border_top"), Value::from(false)));
        }
        // The prompt query: present (even when empty) for a picker, absent for a
        // promptless `nx.ui.select`. Its presence tells the client to draw a prompt row,
        // a separator, and the caret; `query_cursor` is the caret's char column and
        // `prompt_pos` whether the prompt sits above or below the list.
        if let Some(query) = &m.query {
            map.push((Value::from("query"), Value::from(query.as_str())));
            map.push((
                Value::from("query_cursor"),
                Value::from(m.query_cursor as u64),
            ));
            map.push((
                Value::from("prompt_pos"),
                Value::from(match m.prompt_pos {
                    nxvim_core::PromptPos::Top => "top",
                    nxvim_core::PromptPos::Bottom => "bottom",
                }),
            ));
        }
        // The preview sub-map (`{ lines, first_line, title, loc, width, highlights }`),
        // present only when this picker carries a preview pane. Its presence tells the
        // client to split the box into a list column + this preview column.
        if let Some(preview) = preview {
            map.push((Value::from("preview"), preview));
        }
        // The completion docs sidebar (Phase 4-D): a separate float beside a
        // `Cursor`-placed completion popup rendering the selected `lsp` row's docs.
        // Native-only (the docs come from the server's LSP item cache; the wasm
        // edit-host has no language servers). Absent ⇒ the client draws no sidebar.
        #[cfg(feature = "native")]
        if matches!(m.placement, MenuPlacement::Cursor) {
            if let Some(docs) =
                self.project_complete_docs(m, row, col, width, text_width, text_height)
            {
                map.push((Value::from("docs"), docs));
            }
        }
        Value::Map(map)
    }

    /// The completion **docs sidebar** (Phase 4-D — the widget-spec `preview =
    /// "markdown"` kind in cursor placement): a float beside the popup rendering the
    /// selected row's documentation. `None` unless the menu opted into docs
    /// (`m.docs`), a row is actively selected (`m.selected_key`) and is an `lsp` row
    /// (`m.selected_source_accept` — the only source whose item cache the server
    /// holds), and that cached item actually carries docs. Rendered as plain lines
    /// (like hover), placed to the right of the box `(row, col, width)` and flipping to
    /// its left when the right has no room; `(text_width, text_height)` is the editor
    /// viewport the float is clamped to.
    #[cfg(feature = "native")]
    fn project_complete_docs(
        &self,
        m: &MenuView,
        row: usize,
        col: usize,
        width: usize,
        text_width: usize,
        text_height: usize,
    ) -> Option<Value> {
        if !m.docs {
            return None;
        }
        // Three docs sources feed this sidebar: a plugin async row carries its docs
        // **inline** (`selected_doc`, Phase 4-E), rendered verbatim; or it carries a
        // **resolve handle** (`selected_resolve`) whose docs the server fetched lazily
        // into `complete_resolve_docs`; or an `lsp` row (`selected_source_accept`) has
        // its docs in the server's LSP item cache. A `buffer` row has none.
        let lines: Vec<String> = if let Some(doc) = m.selected_doc.as_deref().or_else(|| {
            m.selected_resolve
                .and_then(|id| self.complete_resolve_docs.get(&id))
                .map(String::as_str)
        }) {
            doc.lines()
                .map(str::to_string)
                .skip_while(|l| l.trim().is_empty())
                .collect()
        } else if m.selected_source_accept {
            let key = m.selected_key?;
            let item = self.lsp_complete.as_ref()?.items.get(key)?;
            crate::lsp::complete_doc_lines(item)
        } else {
            return None;
        };
        if lines.is_empty() {
            return None;
        }
        /// Cap the docs float's content width — a long signature wraps off-screen
        /// otherwise; the body is windowed, not a hard limit.
        const MAX_DOCS_W: usize = 60;
        /// Cap its height — a huge docstring shouldn't fill the screen beside a popup.
        const MAX_DOCS_H: usize = 12;
        let content_w = lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_DOCS_W);
        // Place to the right of the box (its content spans `[col, col+width)`; each
        // client draws a 1-cell border, so the box's right border sits at `col+width`
        // and the docs float's own left border one cell past it → content at
        // `col+width+2`). Flip to the left when the right edge overruns the viewport.
        let right_start = col + width + 2;
        // `< text_width` keeps a 1-col margin past the float's right border (the
        // `+ 1 <= text_width` form clippy rejects).
        let (docs_col, docs_w) = if right_start + content_w < text_width {
            (right_start, content_w)
        } else {
            // Left of the box: the docs float's right border one cell left of the box's
            // left border (at `col-1`), so its content ends at `col-3`.
            let w = content_w.min(col.saturating_sub(3)).max(1);
            (col.saturating_sub(2 + w), w)
        };
        // Clamp the height to the rows available below the float's top (a full border
        // costs 2), then window the lines to it.
        let docs_h = lines
            .len()
            .min(MAX_DOCS_H)
            .min(text_height.saturating_sub(row).saturating_sub(2).max(1));
        let shown = &lines[..docs_h.min(lines.len())];
        Some(Value::Map(vec![
            (Value::from("lines"), display_lines_value(shown)),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(docs_col as u64)),
            (Value::from("width"), Value::from(docs_w as u64)),
            (Value::from("height"), Value::from(docs_h as u64)),
        ]))
    }

    /// Project the list-less **content float** (`nx.ui.float`; LSP hover /
    /// signature help) into its redraw sub-map. Geometry is computed here in
    /// text-area cells (the client adds the gutter + text-area origin, then draws
    /// the border): in `Cursor` placement it anchors at the cursor and prefers to
    /// sit **above** it (vim shows a hover above the symbol), flipping below — then
    /// clamping to the larger side — when there's no room; in `Editor` placement it
    /// centers. Content is windowed to the resolved height. Sibling of
    /// [`project_menu`](Self::project_menu); much simpler (no list, no scrolling).
    fn project_content_float(
        &self,
        cf: &ContentFloatView,
        view: &nxvim_core::View,
        text_width: usize,
    ) -> Value {
        /// Cap the float width — long markup wraps off-screen otherwise; the body is
        /// windowed, not a hard limit.
        const MAX_W: usize = 80;
        /// Cap the height — a huge docstring shouldn't fill the whole screen.
        const MAX_H: usize = 20;
        let focused = view.focused();
        let text_height = focused.lines.len();
        // Hug the content (title included), capped. A bordered float spends one cell
        // on each side, so the fit tests below reserve 2 rows/cols of chrome.
        let content_w = cf
            .lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(1)
            .max(cf.title.as_ref().map_or(0, |t| t.chars().count() + 2))
            .clamp(1, MAX_W);
        let count = cf.lines.len().min(MAX_H);
        const CHROME: usize = 2;

        let (row, col, width, height) = match cf.placement {
            MenuPlacement::Cursor => {
                let cursor_row = focused.cursor_row;
                let anchor_col = focused.cursor_screen_col.saturating_sub(focused.leftcol);
                // Anchor the box at the cursor, but shift it left when the right margin
                // has no room (a full box shifted left beats a squished one). The box
                // includes its border chrome; the content `width` is what's left.
                let box_w = (content_w + CHROME).min(text_width.max(1));
                let col = anchor_col.min(text_width.saturating_sub(box_w));
                let width = box_w.saturating_sub(CHROME).max(1);
                let above = cursor_row;
                let below = text_height.saturating_sub(cursor_row + 1);
                // Above if the bordered box fits (vim shows a hover above the symbol),
                // else below, else clamp to whichever side has more room.
                let (row, height) = if count + CHROME <= above {
                    (cursor_row - (count + CHROME), count)
                } else if count + CHROME <= below {
                    (cursor_row + 1, count)
                } else if above >= below {
                    let h = above.saturating_sub(CHROME).clamp(1, count);
                    (cursor_row.saturating_sub(h + CHROME), h)
                } else {
                    (cursor_row + 1, below.saturating_sub(CHROME).clamp(1, count))
                };
                (row, col, width, height)
            }
            MenuPlacement::Editor => {
                let width = content_w.min(text_width.saturating_sub(CHROME).max(1));
                let height = count.min(text_height.saturating_sub(CHROME).max(1));
                let row = text_height.saturating_sub(height + CHROME) / 2;
                let col = text_width.saturating_sub(width + CHROME) / 2;
                (row, col, width, height)
            }
        };
        let shown = &cf.lines[..height.min(cf.lines.len())];
        Value::Map(vec![
            (Value::from("lines"), display_lines_value(shown)),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(col as u64)),
            (Value::from("width"), Value::from(width as u64)),
            (Value::from("height"), Value::from(height as u64)),
            (Value::from("border"), Value::from(cf.border.as_str())),
            (
                Value::from("title"),
                cf.title
                    .as_deref()
                    .map_or(Value::Nil, |t| Value::from(t.to_string())),
            ),
        ])
    }

    /// Resolve the picker's preview pane into its redraw sub-map, or `None` for a
    /// preview-less picker. Reads the selected row's file through the host FS (cached by
    /// path), windows it to the pane height around the target location, and emits the
    /// pane's `width` so the client sizes the list column. `highlights` is empty here;
    /// Phase 3b fills it with native tree-sitter spans. A row with no target, an
    /// unreadable file, or an off-tick FS yields a visible placeholder, never a blank.
    fn project_preview(
        &mut self,
        m: &MenuView,
        box_w: usize,
        box_h: usize,
        styles: &mut StyleTable,
    ) -> Option<Value> {
        if !m.has_preview {
            return None;
        }
        // Reserve ~60% of the box for the preview, keeping a sane (≥1-col) list column
        // and a 1-col separator. The pane spans the full box content height.
        let preview_w = (((box_w as f32) * 0.6) as usize)
            .min(box_w.saturating_sub(2))
            .max(1);
        let pane_h = box_h.max(1);

        let (lines, first_line, loc, title, highlights) = match &m.preview {
            Some(target) => {
                self.ensure_preview(&target.path);
                let len = self.preview_cache.lines.len();
                // The manual scroll offset belongs to one target; reset it when the
                // selection moves to a different row/file so each selection re-centers.
                if self.preview_anchor.as_ref() != Some(target) {
                    self.preview_scroll = 0;
                    self.preview_anchor = Some(target.clone());
                }
                // Fold this frame's one-shot scroll gesture (`<C-d>`/`<C-u>` half page,
                // `<C-f>`/`<C-b>` full page) into the persistent offset. Full page keeps
                // a two-line overlap, matching the editor's normal `<C-f>`/`<C-b>`.
                if let Some(gesture) = m.preview_scroll {
                    let half = (pane_h / 2).max(1) as isize;
                    let page = pane_h.saturating_sub(2).max(1) as isize;
                    self.preview_scroll += match gesture {
                        nxvim_core::PreviewScroll::HalfDown => half,
                        nxvim_core::PreviewScroll::HalfUp => -half,
                        nxvim_core::PreviewScroll::PageDown => page,
                        nxvim_core::PreviewScroll::PageUp => -page,
                    };
                }
                // The auto window start (show a `location` match ~a third down), clamped
                // to the file; a file-kind target (no `loc`) starts at the top.
                let base = match target.loc {
                    Some((r, _)) if r >= pane_h => (r - pane_h / 3).min(len.saturating_sub(pane_h)),
                    _ => 0,
                } as isize;
                // Apply the manual offset and clamp the visible window to the file, then
                // fold the clamp back into the stored offset so reversing direction
                // (e.g. `<C-u>` after scrolling past the end) responds on the first key.
                let max_start = len.saturating_sub(pane_h) as isize;
                let start = (base + self.preview_scroll).clamp(0, max_start.max(0));
                self.preview_scroll = start - base;
                let start = start as usize;
                let cache = &self.preview_cache;
                let end = (start + pane_h).min(len);
                let win = cache.lines.get(start..end).unwrap_or(&[]);
                // Per windowed line, the cached tree-sitter spans mapped to char
                // columns + per-frame style ids — the same `[start, end, group,
                // style_id]` shape as a window's text highlights, so the clients reuse
                // their span renderer. Empty rows (no grammar / blank line) stay plain.
                let highlights = Value::Array(
                    win.iter()
                        .enumerate()
                        .map(|(i, text)| {
                            preview_line_spans(
                                text,
                                cache.highlights.get(&(start + i)),
                                &self.editor.highlights,
                                styles,
                            )
                        })
                        .collect(),
                );
                // The match position, rebased into the window — only when the read
                // succeeded (a placeholder has no meaningful location to highlight).
                let loc = match target.loc {
                    Some((r, c)) if cache.ok && r >= start && r < end => Some((r - start, c)),
                    _ => None,
                };
                (
                    win.to_vec(),
                    start + 1,
                    loc,
                    target.path.clone(),
                    highlights,
                )
            }
            // The picker has a preview pane, but this row carries no target.
            None => {
                self.preview_anchor = None;
                (
                    vec!["No preview".to_string()],
                    1,
                    None,
                    String::new(),
                    Value::Array(Vec::new()),
                )
            }
        };

        let loc_value = match loc {
            Some((r, c)) => Value::Array(vec![Value::from(r as u64), Value::from(c as u64)]),
            None => Value::Nil,
        };
        Some(Value::Map(vec![
            // The preview paints file content and its syntax spans key off the same
            // `grapheme_width`, so it substitutes control chars like the main text —
            // otherwise a control byte's spans would misalign with the painted row.
            (Value::from("lines"), display_lines_value(&lines)),
            (Value::from("first_line"), Value::from(first_line as u64)),
            (Value::from("title"), Value::from(title.as_str())),
            (Value::from("width"), Value::from(preview_w as u64)),
            (Value::from("loc"), loc_value),
            (Value::from("highlights"), highlights),
        ]))
    }

    /// Ensure [`preview_cache`](EditHost::preview_cache) holds the file for `path`,
    /// reading it through the editor's host FS on a path miss (a hit — the common case
    /// as the selection moves within one file's matches — does nothing). A read error or
    /// an off-tick FS fills a single visible placeholder line with `ok = false`.
    fn ensure_preview(&mut self, path: &str) {
        let p = std::path::Path::new(path);
        if self.preview_cache.path.as_deref() == Some(p) {
            return;
        }
        let (lines, ok) = read_preview_file(&self.editor, p);
        // Syntax-highlight the whole file once, here on the path miss (Phase 3b), so
        // moving the selection within one file's matches never re-parses. Keyed by
        // file line; empty when the read failed or no grammar is installed for the
        // path's language (the preview then renders plain).
        let highlights = if ok {
            nxvim_core::language_of_path(Some(p)).map_or_else(HashMap::new, |lang| {
                // Trailing newline to match the engine's buffer invariant (it treats
                // the last line as a phantom: `len_lines - 1`); without it a
                // single-line file parses to zero lines and drops every span.
                let text = lines.join("\n") + "\n";
                let mut by_line: HashMap<usize, Vec<nxvim_core::Span>> = HashMap::new();
                for span in self.editor.preview_highlights(lang, &text, 0, lines.len()) {
                    by_line.entry(span.line).or_default().push(span);
                }
                by_line
            })
        } else {
            HashMap::new()
        };
        self.preview_cache = PreviewCache {
            path: Some(p.to_path_buf()),
            lines,
            ok,
            highlights,
        };
    }
}

/// The picker preview pane's read cache: the file last read for the preview, so
/// moving the selection within the results re-reads only when the target path
/// changes. See [`EditHost::preview_cache`].
#[derive(Default)]
pub(crate) struct PreviewCache {
    /// The path whose contents `lines` holds; `None` before the first read.
    path: Option<std::path::PathBuf>,
    /// The file's lines (newline-split, trailing newline dropped). On a read failure
    /// or an off-tick FS this is a single placeholder line.
    lines: Vec<String>,
    /// `false` when `lines` is a placeholder (unreadable / loading) — suppresses the
    /// location range-highlight, which would be meaningless over the placeholder.
    ok: bool,
    /// Native tree-sitter highlight spans (Phase 3b), keyed by 0-based file line —
    /// the engine's raw byte-offset spans, computed **once** per file read (the whole
    /// file is parsed on a path miss). `project_preview` maps the windowed slice to
    /// char columns + per-frame style ids; an empty map ⇒ no grammar (plain preview).
    highlights: HashMap<usize, Vec<nxvim_core::Span>>,
}

/// Read a file's lines for the read-only preview pane through the editor's host FS,
/// capped at [`MAX_PREVIEW_BYTES`]. Returns `(lines, ok)`; `ok = false` (with a
/// single visible placeholder line) when the FS is off-tick (daemon/wasm — preview
/// rides the async seam later) or the read fails. Lossy-decodes non-UTF-8 so a
/// binary file previews as best-effort text rather than erroring.
fn read_preview_file(editor: &nxvim_core::Editor, path: &std::path::Path) -> (Vec<String>, bool) {
    use std::io::Read as _;
    /// Cap on the bytes pulled into a single preview read — a guard against a huge
    /// file stalling the frame, not a UI limit (the pane only shows a window anyway).
    const MAX_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;
    if editor.host_fs_offtick() {
        return (vec![format!("{}: loading…", path.display())], false);
    }
    let reader = match editor.host_fs().open_read(path) {
        Ok(r) => r,
        Err(e) => return (vec![format!("{}: {e}", path.display())], false),
    };
    let mut buf = Vec::new();
    if let Err(e) = reader.take(MAX_PREVIEW_BYTES).read_to_end(&mut buf) {
        return (vec![format!("{}: {e}", path.display())], false);
    }
    let text = String::from_utf8_lossy(&buf);
    (text.lines().map(|l| l.to_string()).collect(), true)
}

/// Map one preview line's cached tree-sitter `spans` (byte offsets within the line)
/// to the redraw highlight shape — `[start_char, end_char, group, style_id]` per
/// span, in **char** columns (the preview renders char-by-char, no tab expansion).
/// `style_id` interns the resolved [`Style`] into the frame palette, or `Nil` when
/// the capture has no colorscheme mapping (the client falls back to its own theme).
/// `None`/blank ⇒ an empty array (a plain row).
fn preview_line_spans(
    text: &str,
    spans: Option<&Vec<nxvim_core::Span>>,
    highlights: &nxvim_core::Highlights,
    styles: &mut StyleTable,
) -> Value {
    let Some(spans) = spans else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        spans
            .iter()
            .filter_map(|s| {
                // Byte → char column within the line; skip a span that doesn't land
                // on char boundaries (defensive — engine spans always should).
                let start = text.get(..s.start_byte)?.chars().count();
                let end = text.get(..s.end_byte)?.chars().count();
                let style_id = match highlights.resolve_capture(&s.group) {
                    Some(style) => Value::from(styles.intern(style) as u64),
                    None => Value::Nil,
                };
                Some(Value::Array(vec![
                    Value::from(start as u64),
                    Value::from(end as u64),
                    Value::from(s.group.as_str()),
                    style_id,
                ]))
            })
            .collect(),
    )
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
