//! Projecting the editor `View` into the `redraw` notification map clients
//! render: lines, cursor, chrome styles, scroll band, panel, and the per-frame
//! deduped style palette ([`StyleTable`]).

use crate::EditHost;
use nxvim_core::editor::expr::{self, OptVal};
use nxvim_core::highlight::Style;
use nxvim_core::statusline::{self, ExprKind};
use nxvim_core::unicode;
use nxvim_core::view::{
    MenuView, RegionTabline, RegionTablines, RenderRow, ScrollAnim, Separator, TabView, ViewRect,
    WindowRegion, WindowView,
};
use nxvim_core::{BorderStyle, ContentFloatView, MenuPlacement, VirtChunk};
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

        // Refresh every visible buffer's highlights from the in-process engine for
        // the freshly-settled viewports (same-frame, memoized per content+view) — not
        // just the focused window's, so a grabbing float doesn't leave the buffer
        // behind it dark. Native only — the browser highlights JS-side
        // (`nxvim-edithost`), and LSP sync needs a language server (Phase 6).
        #[cfg(feature = "native")]
        self.refresh_highlights(&view.windows);
        // A large file's treesitter parse is resumed across frames; if it's still in
        // flight after this frame's `refresh_highlights`, wake again shortly to paint
        // the next budget's progress (and re-arm until it converges).
        #[cfg(feature = "native")]
        self.arm_parse_resume_if_pending();
        // Drive LSP document sync for the current buffer (non-blocking) — on BOTH builds:
        // native runs the server locally / over the daemon, wasm over the daemon's `lsp_*`
        // wire (Phase 6e). This is what sends the pending `didOpen` after a server's
        // `Initialized` (which the consumer defers to "the next sync"), so diagnostics and
        // `didChange` flow without waiting for an explicit request path to sync first.
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
                // The global bar shows the focused window's facts, so it resolves
                // (and reads the segment cache for) that window.
                let fw = view.focused().id.0;
                self.render_statusline(
                    fw,
                    self.resolve_window_layout(fw),
                    ctx,
                    width,
                    &view.mode_label,
                    &statusline_fmt,
                    &mut styles,
                )
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
            // The tabline always uses the `'tabline'` `%`-format, never a segment
            // layout (`None`) — segment layouts are a status-line surface only.
            self.render_statusline(
                view.focused().id.0,
                None,
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
            Some(cf) => self.project_content_float(cf, &view, text_width, &mut styles),
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
            // Whether the client must highlight *code* itself (JS-side treesitter) and
            // treat any per-window `highlights` spans as an overlay rather than the full
            // styling. True only on the browser (wasm) build, where the editor runs
            // locally and ships only extmark / semantic / terminal spans — so the client
            // must NOT flip into server-styled mode when those spans appear. The native
            // builds bake every highlight source into `highlights`, so it is false there.
            (
                Value::from("js_highlight"),
                Value::from(cfg!(not(feature = "native"))),
            ),
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
        // The text body arrives as one self-describing `RenderRow` per screen row
        // (`win.rows`) — the single source of truth core projects from. The wire
        // still carries the parallel per-row arrays clients decode, so unbundle
        // them once here; every projection below keys on these exactly as it did
        // when they were `WindowView` fields, so the bytes are unchanged. The scroll
        // band reuses the same unbundling over its (taller) row set.
        let RowArrays {
            lines,
            numbers,
            segments,
            continuation,
            selection,
            secondary_selection,
            search,
            incsearch,
            virt_lines,
        } = unbundle_rows(&win.rows);
        // Syntax highlights (treesitter) and the LSP overlays (diagnostics / signs /
        // inlay hints) are native-only projections; the browser build emits empty
        // arrays for them so the redraw map keeps a stable shape (JS-side highlighting
        // paints from the buffer text instead).
        #[cfg(feature = "native")]
        let highlights = self.highlights_for(win.buffer, &segments, styles);
        // The browser build highlights *code* JS-side, so it skips the treesitter
        // spans — but extmark highlights (the `nx.decor` / `nx.buf.set_extmark` layer)
        // and LSP semantic tokens are genuinely server-sourced and can't be
        // reproduced JS-side, so it still projects those (plus a terminal's vt100
        // colors) as an *overlay* the renderer paints on top of its JS colors. A code
        // buffer with no extmarks/semantic tokens gets empty rows + pure JS
        // highlighting; the `js_highlight` frame flag (below) keeps these overlay
        // spans from flipping the client into full server-styled mode.
        #[cfg(not(feature = "native"))]
        let highlights = self.overlay_highlights_for(win.buffer, &segments, styles);
        // Display columns of the `^X` / `<xx>` substitutions, for the wasm renderer
        // to colour as `SpecialKey`; the native client paints them from `highlights`,
        // so it gets an empty array (keeping the redraw map shape stable).
        #[cfg(feature = "native")]
        let special_key = Value::Array(Vec::new());
        #[cfg(not(feature = "native"))]
        let special_key = special_key_spans(&lines, win.tabstop);
        let status = self.status_value(win, mode_label, statusline_fmt, styles);
        // Extmark virtual text. The extmark store lives in core (shared with the wasm
        // edit-host, which runs the same `nx.buf.set_extmark` Lua and the same `nx.decor`
        // publish loop), so this projects on **both** builds — unlike the treesitter /
        // LSP overlays above, which are genuinely native-only. The wire shape is the
        // same on either build; only the transport differs.
        let virt_text = self.virt_text_for(win.buffer, &segments, &selection, styles);
        // Extmark `virt_lines` (whole virtual rows). Core already interleaved them into
        // the window's rows (the `RowKind::VirtLine` rows, unbundled into `virt_lines`
        // above); the server only resolves each chunk's `hl_group` to a frame style id.
        // Shared like `virt_text`.
        let virt_lines = self.virt_lines_value(&virt_lines, styles);
        #[cfg(feature = "native")]
        let (diagnostics, diagnostics_virt, diagnostics_signs, sign_width, inlay_hints) = (
            self.diagnostics_for(win.buffer, &segments, styles),
            self.diagnostics_virt_text_for(win.buffer, &segments, styles),
            self.diagnostics_signs_for(win.buffer, &segments, styles),
            self.sign_width_for(win.buffer, &numbers, win.signcolumn),
            self.inlay_hints_for(win.buffer, &segments, styles),
        );
        // The browser build has no diagnostics, so signs never appear; the sign
        // width still honors a fixed `yes` policy (its `floor`) so the layout matches
        // what core reserved.
        #[cfg(not(feature = "native"))]
        let (diagnostics, diagnostics_virt, diagnostics_signs, sign_width, inlay_hints) = (
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            win.signcolumn.floor_cells() as u16,
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
            (Value::from("lines"), display_lines_value(&lines)),
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
            // When this window's buffer is an image opened for preview
            // (`'imagepreview'`), the path to render. A reference, never the bytes —
            // the client reads/decodes once and caches (the bytes must not ride the
            // redraw frame). `Nil` for an ordinary buffer; clients ignore it.
            (
                Value::from("image"),
                match &win.image {
                    Some(img) => Value::Map(vec![
                        (Value::from("path"), Value::from(img.path.as_str())),
                        // The file's version (size + mtime-ms), so the client
                        // re-decodes when the file changed on disk.
                        (Value::from("size"), Value::from(img.size)),
                        (Value::from("mtime_ms"), Value::from(img.mtime_ms)),
                        // Whether the bytes live on a remote daemon. In a daemon
                        // (`:connect`) session the editor — and so this path — is
                        // local, but the file is on the daemon's disk, which the
                        // client can't open: it must fetch the bytes over the editor
                        // RPC (`nxvim_image_read`) instead. An embedded session shares
                        // the filesystem, so the client decodes `path` directly.
                        (Value::from("remote"), Value::from(self.fx.has_remote_fs())),
                    ]),
                    None => Value::Nil,
                },
            ),
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
            (Value::from("selection"), spans_value(&selection)),
            (
                Value::from("secondary_selection"),
                multi_spans_value(&secondary_selection),
            ),
            (Value::from("search"), multi_spans_value(&search)),
            (Value::from("incsearch"), spans_value(&incsearch)),
            (Value::from("numbers"), numbers_value(&numbers)),
            (Value::from("continuation"), bools_value(&continuation)),
            (Value::from("number"), Value::from(win.number)),
            (
                Value::from("relativenumber"),
                Value::from(win.relativenumber),
            ),
            (Value::from("cursorline"), Value::from(win.cursorline)),
            (
                Value::from("number_width"),
                Value::from(win.number_width as u64),
            ),
            (Value::from("tabstop"), Value::from(win.tabstop as u64)),
            (Value::from("special_key"), special_key),
            (Value::from("highlights"), highlights),
            (Value::from("diagnostics"), diagnostics),
            (Value::from("diagnostics_virt"), diagnostics_virt),
            (Value::from("virt_text"), virt_text),
            (Value::from("virt_lines"), virt_lines),
            (Value::from("diagnostics_signs"), diagnostics_signs),
            (Value::from("sign_width"), Value::from(sign_width as u64)),
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
        self.render_statusline(
            win.id.0,
            self.resolve_window_layout(win.id.0),
            &win.status_ctx,
            width,
            mode_label,
            statusline_fmt,
            styles,
        )
    }

    /// Run the `%`-format engine over one [`StatuslineCtx`] across `width` cells and
    /// project the result as a `status` segment array (`{ text, style }` per
    /// highlighted run). Shared by the per-window status line ([`Self::status_value`])
    /// and the single global one (`laststatus=3`); both differ only in their context
    /// and width. `statusline_fmt` empty ⇒ the built-in default look (rendered through
    /// the same engine). Each segment's highlight group resolves to a style-palette
    /// id, `Nil` when it has none / the colorscheme leaves it undefined.
    /// Resolve the effective `nx.statusline` segment layout for a window: its
    /// window-local override ([`WindowStatusline`](crate::WindowStatusline)) when
    /// set — `Segments` shows that layout, `Format` opts back to the `%`-format
    /// (returns `None`) — otherwise the global layout. `None` ⇒ the window uses the
    /// `'statusline'` `%`-format. The `setlocal 'statusline'` analogue.
    fn resolve_window_layout(&self, win_id: u64) -> Option<&nxvim_core::statusline::SegmentLayout> {
        match self.statusline_window.get(&nxvim_core::WindowId(win_id)) {
            Some(crate::WindowStatusline::Segments(layout)) => Some(layout),
            Some(crate::WindowStatusline::Format) => None,
            None => self.statusline_layout.as_ref(),
        }
    }

    #[allow(clippy::too_many_arguments)] // status-line render facts; bundling them
                                         // (window, layout, ctx, width, format, styles) would just hide the data flow.
    fn render_statusline(
        &self,
        win_id: u64,
        layout: Option<&nxvim_core::statusline::SegmentLayout>,
        ctx: &nxvim_core::statusline::StatuslineCtx,
        width: usize,
        mode_label: &str,
        statusline_fmt: &str,
        styles: &mut StyleTable,
    ) -> Value {
        // A resolved segment layout (the window's override or the global one) takes
        // precedence over the `'statusline'` `%`-format. Built-in segments resolve
        // here from `ctx` (with diagnostics filled in); custom segments come from the
        // per-`(window, name)` cache the Lua re-renders populate — keyed by `win_id`
        // so each window shows its own cell — no per-frame Lua (ADR 0002 rule 4).
        // The tabline caller passes `None` (it never uses a segment layout).
        if let Some(spec) = layout {
            let mut seg_ctx = ctx.clone();
            seg_ctx.diag_counts =
                self.statusline_diag_counts(nxvim_core::BufferId(ctx.bufnr as u64));
            let cache = &self.statusline_cache;
            let custom = |name: &str| cache.get(&(win_id, name.to_string())).cloned();
            let segments = statusline::compose_segments(spec, &seg_ctx, mode_label, width, &custom);
            return self.project_status_segments(&segments, styles);
        }

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
        self.project_status_segments(&segments, styles)
    }

    /// Resolve a status/tabline click in window `win_id` at display column `col` to
    /// the [`ClickAction`](nxvim_core::statusline::ClickAction) of the region
    /// covering it, or `None` if the column is in no region.
    ///
    /// Recomputed on demand (a click is rare) rather than cached during redraw: it
    /// rebuilds the relevant context at the painted width and finds the region
    /// covering `col`. The [`surface`](nxvim_core::ClickSurface) picks the format and
    /// width, mirroring [`Self::render_statusline`]'s paths so the recomputed spans
    /// match the painted line:
    /// - **Window** — the window's `'statusline'` (`%@…%X`) or its `nx.statusline`
    ///   segment layout (a cell `on_click`), at the window's content width.
    /// - **Global** — the focused window's `'statusline'` at the full editor width.
    /// - **Tabline** — the focused window's `'tabline'` `%`-format (`%nT` tab-select,
    ///   `%@…%X`) at the full editor width; never a segment layout.
    pub(crate) fn statusline_click_at(
        &mut self,
        win_id: u64,
        col: usize,
        surface: nxvim_core::ClickSurface,
    ) -> Option<nxvim_core::statusline::ClickAction> {
        use nxvim_core::ClickSurface;
        let (w, h) = self.ui?;
        let view = self.editor.view(w, h);
        let win = view.windows.iter().find(|win| win.id.0 == win_id)?;
        let ctx = &win.status_ctx;

        let clicks = match surface {
            // The main custom `'tabline'` — full width, focused window's context,
            // never a segment layout. Empty / malformed ⇒ no regions.
            ClickSurface::Tabline => {
                let tabline_fmt = self.editor.global_options().tabline;
                if tabline_fmt.is_empty() {
                    return None;
                }
                let items = statusline::parse(&tabline_fmt).ok()?;
                let mut eval = |_kind: ExprKind, raw: &str| self.eval_statusline_expr(raw, ctx);
                let pieces = statusline::expand(&items, ctx, &mut eval);
                let (_segments, clicks) = statusline::layout_with_clicks(&pieces, w);
                clicks
            }
            // A per-window status line (its content width) or the global bar (full
            // width); both honour the window's segment layout, else its `%`-format.
            ClickSurface::Window | ClickSurface::Global => {
                let width = if matches!(surface, ClickSurface::Global) {
                    w
                } else {
                    let inset = if win.floating && win.border != BorderStyle::None {
                        1
                    } else {
                        0
                    };
                    win.rect.width.saturating_sub(2 * inset)
                };
                if let Some(layout) = self.resolve_window_layout(win_id) {
                    // Segment layout: a clickable cell carries an `on_click` handler,
                    // which `compose_segments_with_clicks` turns into a column span.
                    let mut seg_ctx = ctx.clone();
                    seg_ctx.diag_counts =
                        self.statusline_diag_counts(nxvim_core::BufferId(ctx.bufnr as u64));
                    let cache = &self.statusline_cache;
                    let custom = |name: &str| cache.get(&(win_id, name.to_string())).cloned();
                    let (_segments, clicks) = statusline::compose_segments_with_clicks(
                        layout,
                        &seg_ctx,
                        &view.mode_label,
                        width,
                        &custom,
                    );
                    clicks
                } else {
                    let statusline_fmt = self.editor.global_options().statusline;
                    let default;
                    let fmt = if statusline_fmt.is_empty() {
                        default = default_statusline(&view.mode_label, &ctx.fileencoding, ctx.bomb);
                        &default
                    } else {
                        &statusline_fmt
                    };
                    // A malformed format renders as its error text (no click regions).
                    let items = statusline::parse(fmt).ok()?;
                    let mut eval = |_kind: ExprKind, raw: &str| self.eval_statusline_expr(raw, ctx);
                    let pieces = statusline::expand(&items, ctx, &mut eval);
                    let (_segments, clicks) = statusline::layout_with_clicks(&pieces, width);
                    clicks
                }
            }
        };
        clicks
            .into_iter()
            .find(|r| col >= r.start_col && col < r.end_col)
            .map(|r| r.action)
    }

    /// Project resolved [`StatusSegment`](nxvim_core::statusline::StatusSegment)s
    /// into the `status` array clients paint: `{ text, style }` per run, the
    /// highlight group resolved to a style-palette id (`Nil` when it has none /
    /// the colorscheme leaves it undefined). Shared by the `%`-format and the
    /// `nx.statusline` segment paths.
    fn project_status_segments(
        &self,
        segments: &[nxvim_core::statusline::StatusSegment],
        styles: &mut StyleTable,
    ) -> Value {
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

    /// Diagnostic counts `[error, warn, info, hint]` for the `diagnostics`
    /// statusline segment. Native delegates to the LSP store; the wasm edit-host
    /// has no language servers, so it is always zero there.
    #[cfg(feature = "native")]
    fn statusline_diag_counts(&self, buffer: nxvim_core::BufferId) -> [usize; 4] {
        self.diag_counts_for(buffer)
    }
    #[cfg(not(feature = "native"))]
    fn statusline_diag_counts(&self, _buffer: nxvim_core::BufferId) -> [usize; 4] {
        [0; 4]
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
    /// the slide from. The band is now **screen-row based** and projected exactly
    /// like a window: the same `unbundle_rows` + overlay projection over `s.rows`
    /// (a taller `RenderRow` set), so it carries everything a window does —
    /// including the interleaved `virt_lines` rows, secondary selections, and
    /// diagnostic virtual text that the old buffer-line band could not. The slide
    /// is expressed as the screen-row offsets `from_row`/`to_row` into the band.
    pub(crate) fn project_band(
        &self,
        buffer: nxvim_core::BufferId,
        s: &ScrollAnim,
        styles: &mut StyleTable,
    ) -> Value {
        let RowArrays {
            lines,
            numbers,
            segments,
            continuation,
            selection,
            secondary_selection,
            search,
            incsearch,
            virt_lines,
        } = unbundle_rows(&s.rows);
        // Native-only overlays (see `window_value`); the browser band carries empty
        // highlight/inlay/diagnostic arrays. Diagnostic underlines and signs ride the
        // band too (keyed on the per-row wrap `segments`, the same as the settled
        // window), so they slide with the text instead of blanking out for the slide.
        #[cfg(feature = "native")]
        let (highlights, inlay_hints, diagnostics_virt, diagnostics, diagnostics_signs) = (
            self.highlights_for(buffer, &segments, styles),
            self.inlay_hints_for(buffer, &segments, styles),
            self.diagnostics_virt_text_for(buffer, &segments, styles),
            self.diagnostics_for(buffer, &segments, styles),
            self.diagnostics_signs_for(buffer, &segments, styles),
        );
        #[cfg(not(feature = "native"))]
        let (highlights, inlay_hints, diagnostics_virt, diagnostics, diagnostics_signs) = (
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
        );
        // Extmark `virt_text` + `virt_lines` ride the band (pure projections, like
        // `window_value`), so they slide with the text instead of flashing on settle.
        let virt_text = self.virt_text_for(buffer, &segments, &selection, styles);
        let virt_lines = self.virt_lines_value(&virt_lines, styles);
        Value::Map(vec![
            (Value::from("from_row"), Value::from(s.from_row as u64)),
            (Value::from("to_row"), Value::from(s.to_row as u64)),
            (
                Value::from("from_cursor_row"),
                Value::from(s.from_cursor_row as u64),
            ),
            (
                Value::from("to_cursor_row"),
                Value::from(s.to_cursor_row as u64),
            ),
            (Value::from("duration_ms"), Value::from(s.duration_ms)),
            (Value::from("lines"), display_lines_value(&lines)),
            (Value::from("selection"), spans_value(&selection)),
            (
                Value::from("secondary_selection"),
                multi_spans_value(&secondary_selection),
            ),
            (
                Value::from("sel_extends_down"),
                s.sel_extends_down.map_or(Value::Nil, Value::from),
            ),
            // hlsearch / incsearch matches for the band, so the highlight rides the
            // slide rather than vanishing until it settles (mirrors `window_value`).
            (Value::from("search"), multi_spans_value(&search)),
            (Value::from("incsearch"), spans_value(&incsearch)),
            (Value::from("numbers"), numbers_value(&numbers)),
            (Value::from("continuation"), bools_value(&continuation)),
            (Value::from("highlights"), highlights),
            (Value::from("inlay_hints"), inlay_hints),
            (Value::from("virt_text"), virt_text),
            (Value::from("virt_lines"), virt_lines),
            (Value::from("diagnostics_virt"), diagnostics_virt),
            (Value::from("diagnostics"), diagnostics),
            (Value::from("diagnostics_signs"), diagnostics_signs),
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
            ("cursorline", "CursorLine"),
            ("visual", "Visual"),
            ("search", "Search"),
            ("incsearch", "IncSearch"),
            ("status_line", "StatusLine"),
            ("end_of_buffer", "EndOfBuffer"),
            ("float_border", "FloatBorder"),
            ("normal_float", "NormalFloat"),
            ("float_title", "FloatTitle"),
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

/// The parallel per-row wire arrays a client decodes, unbundled from core's
/// self-describing [`RenderRow`] layout. Shared by the settled window
/// (`window_value`) and the scroll band (`project_band`) — the band is just a
/// taller row set, so projecting it is identical work.
/// One visible row's soft-wrap **segment**, for the per-row server overlay
/// projections (treesitter highlights, diagnostics, inlay hints, extmark
/// `virt_text`). They compute spans in **full-line** screen-column space, then
/// [`clip`](RowSeg::clip) them to this segment and rebase to row-local columns — so
/// on a soft-wrapped (and `'breakindent'`/`'showbreak'`-indented) line each row only
/// paints the slice of the line it actually shows, at the right column. Built from
/// the core [`RenderRow`] layout, which already decided the wrap.
#[derive(Clone, Copy)]
pub(crate) struct RowSeg {
    /// 1-based buffer line this row shows (`None` for a `~` filler / virtual row, so
    /// the projection emits nothing).
    pub line: Option<usize>,
    /// Segment start column in full-line screen-column space.
    pub start: usize,
    /// Exclusive segment end column (`usize::MAX` for the last/only segment, which
    /// runs to end-of-line).
    pub end: usize,
    /// Row-local prefix width (`'breakindent'`/`'showbreak'`) added when rebasing.
    pub indent: usize,
}

impl RowSeg {
    /// Clip a full-line screen-column span `[a, b)` to this segment and rebase to
    /// row-local columns (adding the baked-prefix `indent`); `None` if it misses.
    pub(crate) fn clip(&self, a: usize, b: usize) -> Option<(usize, usize)> {
        let lo = a.max(self.start);
        let hi = b.min(self.end);
        (lo < hi).then(|| (lo - self.start + self.indent, hi - self.start + self.indent))
    }

    /// Clip a single full-line screen column (an inlay-hint / inline-`virt_text`
    /// anchor) to this segment, rebased; `None` if it falls outside.
    pub(crate) fn clip_col(&self, c: usize) -> Option<usize> {
        (c >= self.start && c < self.end).then(|| c - self.start + self.indent)
    }

    /// Whether this is the **last** display row of its line (runs to end-of-line), so
    /// end-of-line decorations (eol `virt_text`, the diagnostic message) belong here.
    pub(crate) fn is_last(&self) -> bool {
        self.end == usize::MAX
    }

    /// Whether this is the **first** display row of its line, so the gutter sign
    /// (like the number) shows here and not on continuation rows.
    pub(crate) fn is_first(&self) -> bool {
        self.start == 0
    }
}

struct RowArrays {
    lines: Vec<String>,
    numbers: Vec<Option<usize>>,
    /// Per-row wrap segments for the overlay projections (see [`RowSeg`]).
    segments: Vec<RowSeg>,
    /// Per row: `true` on a soft-wrap continuation row (a line's 2nd+ display row).
    /// `numbers` still carries the line number on these rows — it stays the row→line
    /// mapping for highlights / diagnostics — so this is the separate signal the
    /// client uses to blank the number column on continuations (vim shows the number
    /// on a wrapped line's first row only). It also disambiguates a continuation
    /// (real text, blank gutter) from a `~` filler, which both carry no displayed
    /// number.
    continuation: Vec<bool>,
    selection: Vec<Option<(usize, usize)>>,
    secondary_selection: Vec<Vec<(usize, usize)>>,
    search: Vec<Vec<(usize, usize)>>,
    incsearch: Vec<Option<(usize, usize)>>,
    virt_lines: Vec<Option<Vec<VirtChunk>>>,
}

/// Unbundle a [`RenderRow`] slice into the parallel per-row arrays the wire
/// carries (one entry per screen row, in order). The native highlight / inlay /
/// diagnostic projections then key on `numbers` exactly as before.
fn unbundle_rows(rows: &[RenderRow]) -> RowArrays {
    RowArrays {
        lines: rows.iter().map(|r| r.text.clone()).collect(),
        numbers: rows.iter().map(|r| r.number()).collect(),
        segments: rows
            .iter()
            .map(|r| RowSeg {
                line: r.number(),
                start: r.start_col(),
                end: r.seg_end_col,
                indent: r.indent,
            })
            .collect(),
        continuation: rows.iter().map(|r| r.is_continuation()).collect(),
        selection: rows.iter().map(|r| r.selection).collect(),
        secondary_selection: rows.iter().map(|r| r.secondary_selection.clone()).collect(),
        search: rows.iter().map(|r| r.search.clone()).collect(),
        incsearch: rows.iter().map(|r| r.incsearch).collect(),
        virt_lines: rows.iter().map(|r| r.virt_line.clone()).collect(),
    }
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

/// The completion docs sidebar's resolved geometry (text-area content cells) plus
/// the selected row's total doc-line count — returned by
/// [`EditHost::project_complete_docs`] so [`EditHost::project_menu`] can convert it
/// to a global box and stash it in core for the wheel hit-test.
#[cfg(feature = "native")]
struct CompleteDocsMeta {
    row: usize,
    col: usize,
    w: usize,
    h: usize,
    total: usize,
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
        let focused = view.focused();
        let text_height = focused.rows.len();
        // The box rect, the scroll offset, and the windowed rows — the placement
        // math lives in core (`Editor::menu_geom`), shared with the mouse hit-test so
        // a click lands on the row painted here. The metrics are the focused window's
        // cursor-screen position + text-area size; the server fills the content
        // (styling, preview, docs) around the box.
        let geom = self.editor.menu_geom(
            m,
            nxvim_core::MenuMetrics {
                cursor_row: focused.cursor_row,
                cursor_screen_col: focused.cursor_screen_col,
                leftcol: focused.leftcol,
                text_width,
                text_height,
            },
        );
        let nxvim_core::MenuGeom {
            row,
            col,
            width,
            height,
            rows,
            selected,
            ..
        } = geom;

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
        // The command-line wildmenu (`nx.cmdline_complete`) floats above the command
        // line, not the focused window — so the client anchors it to the command-line
        // area (frame-bottom, no number gutter) instead of the window's text inner.
        if matches!(m.placement, MenuPlacement::Cmdline) {
            map.push((Value::from("cmdline"), Value::from(true)));
            // Its docs sidebar (Phase 3): the highlighted command's synopsis + help,
            // a float beside the box. Feature-agnostic (the catalog candidates carry
            // their docs inline — `selected_doc`), so unlike the insert-completion
            // docs sidebar below this is NOT native-gated.
            if let Some(docs) = self.project_cmdline_docs(m, row, col, width, height, text_width) {
                map.push((Value::from("docs"), docs));
            }
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
            if let Some((docs, meta)) =
                self.project_complete_docs(m, row, col, width, text_width, text_height)
            {
                map.push((Value::from("docs"), docs));
                // Stash the docs float's box in GLOBAL cells so a wheel over it scrolls
                // the docs. The placement math is the server's (the content is too), so
                // core can't recompute it — it's fed back here for the hit-test. The
                // docs content sits at `(inner_x + col, win_y + row)`; the bordered
                // outer box is one cell out on every side.
                let inner_x = focused.rect.x + focused.number_width;
                let gx = (inner_x + meta.col).saturating_sub(1);
                let gy = (focused.rect.y + meta.row).saturating_sub(1);
                self.editor
                    .stash_complete_docs_hit(Some(nxvim_core::CompleteDocsHit {
                        x: gx,
                        y: gy,
                        w: meta.w + 2,
                        h: meta.h + 2,
                        total: meta.total,
                        view_h: meta.h,
                    }));
            } else {
                self.editor.stash_complete_docs_hit(None);
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
    ) -> Option<(Value, CompleteDocsMeta)> {
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
        // costs 2).
        let total = lines.len();
        let docs_h = total
            .min(MAX_DOCS_H)
            .min(text_height.saturating_sub(row).saturating_sub(2).max(1));
        // Window the lines from the core-owned scroll offset — a wheel over the
        // sidebar advances it (`Editor::scroll_complete_docs`). Clamp so a short tail
        // can't scroll past the end (the offset is also reset to 0 on a selection
        // change, so a new row's docs start at the top).
        let scroll = self.editor.complete_docs_scroll().min(total - docs_h);
        let shown = &lines[scroll..(scroll + docs_h).min(total)];
        let value = Value::Map(vec![
            (Value::from("lines"), display_lines_value(shown)),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(docs_col as u64)),
            (Value::from("width"), Value::from(docs_w as u64)),
            (Value::from("height"), Value::from(docs_h as u64)),
        ]);
        Some((
            value,
            CompleteDocsMeta {
                row,
                col: docs_col,
                w: docs_w,
                h: docs_h,
                total,
            },
        ))
    }

    /// The command-line wildmenu's **docs sidebar** (Phase 3): the highlighted
    /// command's synopsis + description, a bordered float beside the menu box. The
    /// catalog candidates carry their docs **inline** ([`MenuView::selected_doc`]),
    /// so — unlike [`project_complete_docs`](Self::project_complete_docs), fed by the
    /// native LSP item cache — this needs no language server and renders on the wasm
    /// edit-host too (hence no `#[cfg(feature = "native")]`).
    ///
    /// `(row, col, width, height)` is the wildmenu box (text-area cells). The float
    /// sits to the **right** of the box (`col + width + 2`), flipping to its left when
    /// the right edge overruns the viewport, and **bottom-aligns** to the box so it
    /// abuts the command line alongside it: its bottom border lands on the box's
    /// content bottom (`row + height`), placing its top at `row + height − docs_h − 1`.
    /// `None` unless the menu opted into docs, a row is actively selected, and that
    /// row carries doc text.
    fn project_cmdline_docs(
        &self,
        m: &MenuView,
        row: usize,
        col: usize,
        width: usize,
        height: usize,
        text_width: usize,
    ) -> Option<Value> {
        if !m.docs {
            return None;
        }
        // `selected_doc` is `Some` only when a row is actively selected (the popup is
        // noselect until the user navigates) and that catalog row carries a `doc`.
        let lines: Vec<String> = m
            .selected_doc
            .as_deref()?
            .lines()
            .map(str::to_string)
            .skip_while(|l| l.trim().is_empty())
            .collect();
        if lines.is_empty() {
            return None;
        }
        /// Cap the docs float's content width (a long line is windowed, not hard-cut).
        const MAX_DOCS_W: usize = 60;
        /// Cap its height so a long help text can't tower over the wildmenu.
        const MAX_DOCS_H: usize = 12;
        let content_w = lines
            .iter()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(1)
            .clamp(1, MAX_DOCS_W);
        // Right of the box, flipping left when the right edge has no room — the same
        // placement the insert-completion docs sidebar uses (`< text_width` keeps a
        // one-column margin past the float's right border).
        let right_start = col + width + 2;
        let (docs_col, docs_w) = if right_start + content_w < text_width {
            (right_start, content_w)
        } else {
            let w = content_w.min(col.saturating_sub(3)).max(1);
            (col.saturating_sub(2 + w), w)
        };
        // Bottom-align to the box: cap the height to the rows above the box's content
        // bottom (`row + height`), reserving one for the float's own bottom border.
        let docs_h = lines
            .len()
            .min(MAX_DOCS_H)
            .min((row + height).saturating_sub(1).max(1));
        let docs_row = (row + height).saturating_sub(docs_h + 1);
        let shown = &lines[..docs_h.min(lines.len())];
        Some(Value::Map(vec![
            (Value::from("lines"), display_lines_value(shown)),
            (Value::from("row"), Value::from(docs_row as u64)),
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
        styles: &mut StyleTable,
    ) -> Value {
        /// Display width of one chunked line: the sum of its chunks' char counts.
        fn line_width(line: &[nxvim_core::VirtChunk]) -> usize {
            line.iter().map(|c| c.text.chars().count()).sum()
        }
        /// Cap the float width — long markup wraps off-screen otherwise; the body is
        /// windowed, not a hard limit.
        const MAX_W: usize = 80;
        /// Cap the height — a huge docstring shouldn't fill the whole screen.
        const MAX_H: usize = 20;
        let focused = view.focused();
        let text_height = focused.rows.len();
        // Hug the content (title included), capped. A bordered float spends one cell
        // on each side, so the fit tests below reserve 2 rows/cols of chrome.
        let content_w = cf
            .lines
            .iter()
            .map(|l| line_width(l))
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
            // Content floats are never cmdline-placed (only the `nx.cmdline_complete`
            // menu is); a stray one falls back to editor-centered.
            MenuPlacement::Editor | MenuPlacement::Cmdline => {
                let width = content_w.min(text_width.saturating_sub(CHROME).max(1));
                let height = count.min(text_height.saturating_sub(CHROME).max(1));
                let row = text_height.saturating_sub(height + CHROME) / 2;
                let col = text_width.saturating_sub(width + CHROME) / 2;
                (row, col, width, height)
            }
            MenuPlacement::Bottom => {
                // Pinned to the editor's bottom-RIGHT corner (the which-key shape):
                // content-hugging like `Editor`, but the box (content + border chrome)
                // sits flush against both the last text row and the right edge.
                let width = content_w.min(text_width.saturating_sub(CHROME).max(1));
                let height = count.min(text_height.saturating_sub(CHROME).max(1));
                let row = text_height.saturating_sub(height + CHROME);
                let col = text_width.saturating_sub(width + CHROME);
                (row, col, width, height)
            }
        };
        let shown = &cf.lines[..height.min(cf.lines.len())];
        // Each line ships as a chunk run `[[text, style_id], …]` (the `virt_lines`
        // wire form), so a styled caller (which-key) can colour keys vs.
        // descriptions and dim unavailable rows. A plain caller is one unstyled
        // chunk per line, which resolves to a `Nil` style id (normal colors).
        let lines = Value::Array(
            shown
                .iter()
                .map(|line| self.virt_chunks_value(line, styles))
                .collect(),
        );
        Value::Map(vec![
            (Value::from("lines"), lines),
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

/// Encode a per-row boolean flag array for the redraw map (e.g. the soft-wrap
/// `continuation` signal the client reads to blank the number gutter).
fn bools_value(flags: &[bool]) -> Value {
    Value::Array(flags.iter().map(|&b| Value::from(b)).collect())
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
