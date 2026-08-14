//! Projecting the editor `View` into the `redraw` notification map clients
//! render: lines, cursor, chrome styles, scroll band, panel, and the per-frame
//! deduped style palette ([`StyleTable`]).

use crate::EditHost;
use bemtvi_core::editor::expr::{self, OptVal};
use bemtvi_core::highlight::Style;
use bemtvi_core::statusline::{self, ExprKind};
use bemtvi_core::unicode;
use bemtvi_core::view::{
    MenuView, RegionTabline, RegionTablines, RenderRow, ScrollAnim, Separator, TabView, ViewRect,
    WindowRegion, WindowView,
};
use bemtvi_core::{BorderStyle, ContentFloatView, MenuPlacement, VirtChunk, WinHl};
use rmpv::Value;
use std::collections::HashMap;

impl EditHost {
    /// Push the current view to the client as a single `redraw` notification
    /// carrying an bemtvi-native view map (no neovim grid protocol). The map holds
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
        // Evaluate a generic Lua `'foldexpr'` for the focused buffer (vim's per-line
        // model) and push the values into the fold engine *before* projecting, so
        // `foldmethod=expr` with a non-native foldexpr collapses this frame. Native
        // only — bemtvi-core can't run Lua, and the browser edit-host folds JS-side.
        // Cached by `changedtick`, so it costs nothing on a frame with no edit.
        #[cfg(feature = "native")]
        self.refresh_expr_folds();
        let view = self.editor.view(w, h);

        // Refresh every visible buffer's highlights from the in-process engine for
        // the freshly-settled viewports (same-frame, memoized per content+view) — not
        // just the focused window's, so a grabbing float doesn't leave the buffer
        // behind it dark. Native only — the browser highlights JS-side
        // (`bemtvi-edithost`), and LSP sync needs a language server (Phase 6).
        #[cfg(feature = "native")]
        self.refresh_highlights(&view.windows);
        // A large file's treesitter parse is resumed across frames; if it's still in
        // flight after this frame's `refresh_highlights`, wake again shortly to paint
        // the next budget's progress (and re-arm until it converges).
        #[cfg(feature = "native")]
        self.arm_parse_resume_if_pending();
        // Hand off any grammar the frame's work asked for (a buffer's language, one it
        // injects, a fold query) to be loaded off this thread — compiling a language's
        // queries is hundreds of ms and would otherwise stall the frame that needed it.
        // The load returns on the run loop and repaints.
        #[cfg(feature = "native")]
        self.dispatch_grammar_requests();
        // Drive LSP document sync for the current buffer (non-blocking) — on BOTH builds:
        // native runs the server locally / over the daemon, wasm over the daemon's `lsp_*`
        // wire (Phase 6e). This is what sends the pending `didOpen` after a server's
        // `Initialized` (which the consumer defers to "the next sync"), so diagnostics and
        // `didChange` flow without waiting for an explicit request path to sync first.
        self.sync_lsp();
        // Issue a `textDocument/foldingRange` request when the current buffer uses the
        // LSP fold source and lacks a fresh result (after `sync_lsp` flushed any
        // `didChange`, so the server folds against current text). The async reply
        // pushes the ranges into the fold engine and triggers a repaint; this only
        // fires for a buffer whose `foldmethod=expr` resolves to `btv.lsp.foldexpr`.
        self.maybe_request_folding_range();

        // Resolve every highlight span and chrome region to a concrete style here
        // on the server (the registry lives in the core). Spans carry an index
        // into a per-frame, deduped `styles` palette; the client paints the RGB.
        let mut styles = StyleTable::default();
        let chrome = self.chrome_styles(&mut styles);
        // The browser build highlights code JS-side, so the colorscheme's syntax
        // groups can't ride per-window `highlights` spans (those stay empty on wasm).
        // Resolve them into the same per-frame palette and ship them as `theme`, with
        // `theme_gen` (the registry generation) so the client rebuilds its JS color
        // map only when the colorscheme changes. Native builds bake syntax into the
        // spans, so neither key is emitted there.
        #[cfg(not(feature = "native"))]
        let theme = self.syntax_theme(&mut styles);

        // The message line shows the diagnostic under the cursor, but only when
        // nothing more important (an error, command output) already holds it —
        // and never via `echo`, so the under-cursor text doesn't flood
        // `:messages` on every cursor move. A message *echoed after* `view()` ran
        // (which consumes and clears the transient line) — e.g. a grammar load
        // failure surfaced lazily when `refresh_highlights` first opened the
        // buffer in the engine — is read straight off the editor so it shows this
        // frame rather than waiting for the next keypress.
        let (message, message_error) = if !view.message.is_empty() {
            (view.message.clone(), view.message_error)
        } else if !self.editor.message.is_empty() {
            (self.editor.message.clone(), self.editor.message_error)
        } else {
            // The under-cursor diagnostic is LSP-sourced — empty on the browser build.
            // It's informational on the message line, so it isn't painted as an error.
            #[cfg(feature = "native")]
            {
                (self.diagnostic_under_cursor().unwrap_or_default(), false)
            }
            #[cfg(not(feature = "native"))]
            {
                (String::new(), false)
            }
        };

        // The global `'statusline'` / `'tabline'` formats (empty ⇒ the built-in
        // look), read once and shared across the window status + tabline projection.
        // `global_options()` returns an owned `Options` (a multi-`String` clone), so
        // snapshot it once and move the three fields out rather than cloning it thrice.
        let global_opts = self.editor.global_options();
        let statusline_fmt = global_opts.statusline;
        let tabline_fmt = global_opts.tabline;
        // The `'guifont'` value, relayed verbatim for a GUI client to parse and
        // apply; empty (the default) leaves the client on its own font.
        let guifont = global_opts.guifont;
        // The `'timeout'` / `'timeoutlen'` mapping-timeout config, relayed so each
        // client runs its own idle-flush timer to match: skip arming when `timeout`
        // is off (`notimeout` → a withheld mapped prefix waits forever, which is how
        // a which-key popup stays up), else fire the flush after `timeoutlen` ms.
        let timeout = self.editor.timeout_enabled();
        let timeoutlen = self.editor.timeoutlen_ms();

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
        // its own buffer's syntax/diagnostic slice. Each projection also yields the
        // sign-column width it reserved this frame (a dynamic column grows to fit
        // signs core can't see alone) — stashed below so the next mouse hit-test
        // skips the same gutter the client is about to draw.
        let mut windows: Vec<Value> = Vec::with_capacity(view.windows.len());
        let mut sign_widths: Vec<(bemtvi_core::WindowId, usize)> =
            Vec::with_capacity(view.windows.len());
        for win in &view.windows {
            let (value, sign_width) =
                self.window_value(win, &view.mode_label, &statusline_fmt, &mut styles);
            windows.push(value);
            sign_widths.push((win.id, sign_width as usize));
        }
        // Push each window's rendered sign width back into core, where the mouse
        // hit-test reads it (`Editor::window_textoff`). `view` is an owned snapshot,
        // so mutating the editor here doesn't disturb this frame.
        for (id, cells) in sign_widths {
            self.editor.set_window_sign_width(id, cells);
        }

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
        // window's text-area width bounds the overlay so it can't spill past the
        // editable region. `text_width` is the text area minus *every* left gutter
        // (fold + sign + number) plus the padding/float-border insets, exactly the
        // width the clients paint the text inner at.
        let text_width = view.focused().text_width;
        // The legacy `pmenu` key is retired (Phase 4-C): all completion — including
        // the `lsp` source — now renders through the unified `menu` widget below, so
        // this is always `Nil`. Kept as a key for client wire compatibility.
        let pmenu = Value::Nil;
        // The floating selectable-list menu (`btv.ui.select`; later the picker),
        // `Nil` when none is open. Geometry is computed here from the focused
        // window, the same way the completion popup is placed.
        let menu = match &view.menu {
            Some(m) => self.project_menu(m, &view, text_width, w, h, &mut styles),
            None => Value::Nil,
        };
        // The list-less content float (`btv.ui.float`; LSP hover / signature help),
        // `Nil` when none is open. A non-grabbing transient overlay — its geometry
        // is computed here from the cursor (or centered over the editor).
        let float = match &view.content_float {
            Some(cf) => self.project_content_float(cf, &view, text_width, w, h, &mut styles),
            None => Value::Nil,
        };

        // Mirror the projected UI into `btv._ui` for the plugin test framework
        // (`t:float()` / `t:message()` / `t:cmdline()` / `t:statusline()`), before
        // `map` takes ownership of `float` / `global_status`. The status text is
        // pulled out of its chunk runs; the float is mirrored as-is (its lines carry
        // per-frame style ids, so tests assert on text). Only under `--test-plugin`
        // (`test_mode`), so a normal session pays nothing.
        if self.test_mode {
            let statusline_text = chunk_runs_text(&global_status);
            let clipboard = self.editor.clipboard_contents();
            let _ = self.lua.set_ui_mirror(
                &float,
                &message,
                view.cmdline.as_str(),
                &statusline_text,
                clipboard.as_ref().map(|(t, lw)| (t.as_str(), *lw)),
            );
        }

        // Built last: every per-window/`chrome` style id above indexes into it.
        let styles_value = styles.into_value();
        #[allow(unused_mut)]
        let mut map = vec![
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
            // Every string payload is display-scrubbed (`unicode::display_line`)
            // before it reaches the wire: the client paints these verbatim into
            // the terminal, so an ESC / control byte smuggled in via a file name,
            // an LSP diagnostic, or completion text must not become a terminal
            // escape sequence (OSC 52 clipboard exfil, keystroke injection).
            // The window `lines` array is scrubbed at `display_lines_value`;
            // these are the rest. `cmdline_cursor` is a char offset into the raw
            // cmdline, so it shifts with the substitution — translate it.
            (
                Value::from("cmdline"),
                Value::from(unicode::display_line(&view.cmdline).as_ref()),
            ),
            (
                Value::from("cmdline_prefix"),
                Value::from(view.cmdline_prefix.to_string().as_str()),
            ),
            (
                Value::from("cmdline_prompt"),
                Value::from(unicode::display_line(&view.cmdline_prompt).as_ref()),
            ),
            (
                Value::from("cmdline_cursor"),
                Value::from(
                    unicode::display_char_offset(&view.cmdline, view.cmdline_cursor) as u64,
                ),
            ),
            (
                Value::from("message"),
                Value::from(unicode::display_line(message.as_str()).as_ref()),
            ),
            (Value::from("message_error"), Value::from(message_error)),
            (Value::from("guifont"), Value::from(guifont.as_str())),
            (Value::from("timeout"), Value::from(timeout)),
            (Value::from("timeoutlen"), Value::from(timeoutlen)),
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
                        .map(|l| Value::from(unicode::display_line(l.as_str()).as_ref()))
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

        // The colorscheme → JS-highlighter bridge (wasm only; see `syntax_theme`).
        // `theme_gen` lets the client rebuild its color map only on a colorscheme change.
        #[cfg(not(feature = "native"))]
        {
            map.push((Value::from("theme"), theme));
            map.push((
                Value::from("theme_gen"),
                Value::from(self.editor.highlights.generation()),
            ));
        }

        // Terminal writes the tick queued (an OSC 52 clipboard write from a `"+`
        // yank) leave just ahead of the frame that tick produced. The editor is
        // synchronous and holds no transport, so this is where an escape bound for
        // the client's terminal — rather than for its renderer — is handed over.
        #[cfg(feature = "native")]
        self.flush_ui_sends();
        self.fx.notify("redraw", vec![Value::Map(map)]);
    }

    /// Project one window into its redraw sub-map: the rect and focus flag, the
    /// per-window text/cursor/gutter/status fields, and the window's own syntax
    /// highlights, diagnostic underlines, and scroll band (each resolving styles
    /// into the shared per-frame `styles` palette). Returned paired with the
    /// sign-column width (in cells) it reserved — the caller stashes that width
    /// back into core so the mouse hit-test skips the same gutter.
    fn window_value(
        &self,
        win: &WindowView,
        mode_label: &str,
        statusline_fmt: &str,
        styles: &mut StyleTable,
    ) -> (Value, u16) {
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
        let highlights = self.highlights_for(win.buffer, &win.winhl, &segments, styles);
        // The browser build highlights *code* JS-side, so it skips the treesitter
        // spans — but extmark highlights (the `btv.decor` / `btv.buf.set_extmark` layer)
        // and LSP semantic tokens are genuinely server-sourced and can't be
        // reproduced JS-side, so it still projects those (plus a terminal's vt100
        // colors) as an *overlay* the renderer paints on top of its JS colors. A code
        // buffer with no extmarks/semantic tokens gets empty rows + pure JS
        // highlighting; the `js_highlight` frame flag (below) keeps these overlay
        // spans from flipping the client into full server-styled mode.
        #[cfg(not(feature = "native"))]
        let highlights = self.overlay_highlights_for(win.buffer, &win.winhl, &segments, styles);
        // Display columns of the `^X` / `<xx>` substitutions, for the wasm renderer
        // to colour as `SpecialKey`; the native client paints them from `highlights`,
        // so it gets an empty array (keeping the redraw map shape stable).
        #[cfg(feature = "native")]
        let special_key = Value::Array(Vec::new());
        #[cfg(not(feature = "native"))]
        let special_key = special_key_spans(&lines, win.tabstop);
        let status = self.status_value(win, mode_label, statusline_fmt, styles);
        // Extmark virtual text. The extmark store lives in core (shared with the wasm
        // edit-host, which runs the same `btv.buf.set_extmark` Lua and the same `btv.decor`
        // publish loop), so this projects on **both** builds — unlike the treesitter /
        // LSP overlays above, which are genuinely native-only. The wire shape is the
        // same on either build; only the transport differs.
        let virt_text = self.virt_text_for(win.buffer, &win.winhl, &segments, &selection, styles);
        // Whole-line `line_fill` overlays (an btv-native blank-row rule). Appended to
        // the virt_text payload as full-width Overlay placements — over-provisioned to
        // the window width, the client clips them to the text body — so no client
        // change. Like virt_text, shared by both builds.
        let virt_text = self.apply_line_fill(
            virt_text,
            win.buffer,
            &segments,
            win.rect.width,
            win.tabstop,
            &win.winhl,
            styles,
        );
        // Extmark `virt_lines` (whole virtual rows). Core already interleaved them into
        // the window's rows (the `RowKind::VirtLine` rows, unbundled into `virt_lines`
        // above); the server only resolves each chunk's `hl_group` to a frame style id.
        // Shared like `virt_text`.
        let virt_lines = self.virt_lines_value(&virt_lines, &win.winhl, styles);
        // The line-background layer (`line_hl_group`): per screen row whose buffer
        // line carries one, `[row, style_id]`. Painted under the text like
        // `'cursorline'`, so the doc-float code blocks read as full-width code regions
        // with syntax composed on top. Core-/tick-shared (a core extmark), so BOTH
        // builds project it.
        let line_bg = self.line_bg_for(win.buffer, &win.winhl, &segments, styles);
        // The gutter signs (extmark `sign_text` merged with the LSP diagnostic signs)
        // and the resulting column width. Both sign sources are core-/tick-shared, so
        // this projects on BOTH builds; the width then follows the same `'signcolumn'`
        // policy on either build.
        let sign_cells = self.merged_sign_cells(win.buffer, &win.winhl, &segments, styles);
        let diagnostics_signs = crate::extmarks::signs_value(&sign_cells);
        let sign_width = crate::extmarks::sign_width_from_cells(&sign_cells, win.signcolumn);
        // Diagnostic underline spans + inline virtual text. Like the gutter signs
        // above, both read the core-/tick-shared `diagnostics_merged` store (the LSP
        // set plus the client-set `btv.diagnostic.set` set, the latter having no server
        // at all), so they project on BOTH builds — the browser edit-host paints the
        // squiggles and the trailing message from these payloads. Only `inlay_hints`
        // stays native-only: it has no client-set source and rides a live LSP.
        let diagnostics = self.diagnostics_for(win.buffer, &win.winhl, &segments, styles);
        let diagnostics_virt =
            self.diagnostics_virt_text_for(win.buffer, &win.winhl, &segments, styles);
        #[cfg(feature = "native")]
        let inlay_hints = self.inlay_hints_for(win.buffer, &win.winhl, &segments, styles);
        #[cfg(not(feature = "native"))]
        let inlay_hints = Value::Array(Vec::new());
        let scroll = match &win.scroll {
            Some(s) => self.project_band(win.buffer, &win.winhl, s, styles),
            None => Value::Nil,
        };
        let value = Value::Map(vec![
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
                // A hostile *name* is a first-class injection source (statusline,
                // tabline, E484 "cannot open" messages) — scrub it like any other
                // display text.
                Value::from(unicode::display_line(&win.file_name).as_ref()),
            ),
            // The buffer's effective treesitter filetype (override or extension),
            // so a client that highlights JS-side (the wasm edit-host) can pick the
            // grammar. Native clients ignore it (they paint server highlight spans).
            (Value::from("filetype"), Value::from(win.filetype.as_str())),
            // A rendered-markdown doc float's fenced code blocks, as `{ first_line, len,
            // lang }` row spans into this window's lines. The float's fences are stripped
            // server-side and its buffer is left untyped, so `filetype` cannot answer
            // "what language are these rows?" — this can. Native clients ignore it (they
            // paint server highlight spans); the serverless web build uses it to colour a
            // hover's signature. Absent (empty) for every ordinary window.
            (
                Value::from("code_blocks"),
                Value::Array(
                    win.code_blocks
                        .iter()
                        .map(|c| {
                            Value::Map(vec![
                                (Value::from("first_line"), Value::from(c.first_line as u64)),
                                (Value::from("len"), Value::from(c.len as u64)),
                                (
                                    Value::from("lang"),
                                    match &c.lang {
                                        Some(l) => Value::from(l.as_str()),
                                        None => Value::Nil,
                                    },
                                ),
                            ])
                        })
                        .collect(),
                ),
            ),
            // When this window's buffer is an image opened for preview
            // (`'imagepreview'`), the path to render. A reference, never the bytes —
            // the client reads/decodes once and caches (the bytes must not ride the
            // redraw frame). `Nil` for an ordinary buffer; clients ignore it.
            (
                Value::from("image"),
                match &win.image {
                    Some(img) => Value::Map(vec![
                        (
                            Value::from("path"),
                            Value::from(unicode::display_line(&img.path).as_ref()),
                        ),
                        // The file's version (size + mtime-ms), so the client
                        // re-decodes when the file changed on disk.
                        (Value::from("size"), Value::from(img.size)),
                        (Value::from("mtime_ms"), Value::from(img.mtime_ms)),
                        // Whether the bytes live on a remote daemon. In a daemon
                        // (`:connect`) session the editor — and so this path — is
                        // local, but the file is on the daemon's disk, which the
                        // client can't open: it must fetch the bytes over the editor
                        // RPC (`bemtvi_image_read`) instead. An embedded session shares
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
            // `'colorcolumn'`: the 1-based text columns painted with the `ColorColumn`
            // group (a vertical ruler). Empty array unless the option is set.
            (
                Value::from("colorcolumn"),
                Value::Array(
                    win.colorcolumn
                        .iter()
                        .map(|&c| Value::from(c as u64))
                        .collect(),
                ),
            ),
            (
                Value::from("number_width"),
                Value::from(win.number_width as u64),
            ),
            // The fold-marker gutter: its width and the per-row marker strings the
            // client paints to the left of the sign / number columns. Omitted (width
            // `0`, empty array) unless `'foldcolumn'` is set.
            (
                Value::from("foldcolumn_width"),
                Value::from(win.foldcolumn_width as u64),
            ),
            (
                Value::from("foldcolumn"),
                Value::Array(
                    win.foldcolumn
                        .iter()
                        .map(|s| Value::from(s.as_str()))
                        .collect(),
                ),
            ),
            // `'padding'` as `[top, right, bottom, left]` cells (CSS order). The
            // client insets this window's content box by it (the same way it
            // re-derives the float-border inset); the projection's row width/height
            // already account for it. Omitted-as-zero by default.
            (
                Value::from("padding"),
                Value::Array(vec![
                    Value::from(win.padding.top as u64),
                    Value::from(win.padding.right as u64),
                    Value::from(win.padding.bottom as u64),
                    Value::from(win.padding.left as u64),
                ]),
            ),
            (Value::from("tabstop"), Value::from(win.tabstop as u64)),
            // This window's `winhighlight` chrome overrides (a `key -> style_id` map
            // for the chrome groups it renames, e.g. `Normal:NormalSB`). Empty for a
            // window with no remap; the client merges it over the global `chrome` map.
            (
                Value::from("chrome"),
                self.window_chrome_overrides(&win.winhl, styles),
            ),
            (Value::from("special_key"), special_key),
            (Value::from("highlights"), highlights),
            (Value::from("diagnostics"), diagnostics),
            (Value::from("diagnostics_virt"), diagnostics_virt),
            (Value::from("virt_text"), virt_text),
            (Value::from("virt_lines"), virt_lines),
            (Value::from("line_bg"), line_bg),
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
        ]);
        (value, sign_width)
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
        // float border and any `'padding'` (left + right), matching where the client
        // paints it (and what `%=`/`%<` resolve against).
        let inset = if win.floating && win.border != BorderStyle::None {
            1
        } else {
            0
        };
        let width = win
            .rect
            .width
            .saturating_sub(2 * inset)
            .saturating_sub(win.padding.horizontal());
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

    /// Resolve the effective `btv.statusline` segment layout for a window: its
    /// window-local override ([`WindowStatusline`](crate::WindowStatusline)) when
    /// set — `Segments` shows that layout, `Format` opts back to the `%`-format
    /// (returns `None`) — otherwise the global layout. `None` ⇒ the window uses the
    /// `'statusline'` `%`-format. The `setlocal 'statusline'` analogue.
    fn resolve_window_layout(
        &self,
        win_id: u64,
    ) -> Option<&bemtvi_core::statusline::SegmentLayout> {
        match self.statusline_window.get(&bemtvi_core::WindowId(win_id)) {
            Some(crate::WindowStatusline::Segments(layout)) => Some(layout),
            Some(crate::WindowStatusline::Format) => None,
            None => self.statusline_layout.as_ref(),
        }
    }

    /// Run the `%`-format engine over one [`StatuslineCtx`] across `width` cells and
    /// project the result as a `status` segment array (`{ text, style }` per
    /// highlighted run). Shared by the per-window status line ([`Self::status_value`])
    /// and the single global one (`laststatus=3`); both differ only in their context
    /// and width. `statusline_fmt` empty ⇒ the built-in default look (rendered through
    /// the same engine). Each segment's highlight group resolves to a style-palette
    /// id, `Nil` when it has none / the colorscheme leaves it undefined.
    #[allow(clippy::too_many_arguments)] // status-line render facts; bundling them
                                         // (window, layout, ctx, width, format, styles) would just hide the data flow.
    fn render_statusline(
        &self,
        win_id: u64,
        layout: Option<&bemtvi_core::statusline::SegmentLayout>,
        ctx: &bemtvi_core::statusline::StatuslineCtx,
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
            let (seg_ctx, custom) = self.segment_render_inputs(win_id, spec, ctx);
            let segments = statusline::compose_segments(spec, &seg_ctx, mode_label, width, &custom);
            return self.project_status_segments(&segments, styles);
        }

        let pieces = match self.expand_statusline_fmt(statusline_fmt, mode_label, ctx) {
            Ok(pieces) => pieces,
            // A malformed 'statusline' shows its own error text rather than a
            // blank line — loud, not silent (per CLAUDE.md).
            Err(err) => return Value::Array(vec![segment_value(&err, Value::Nil)]),
        };
        let segments = statusline::layout(&pieces, width);
        self.project_status_segments(&segments, styles)
    }

    /// Resolve the effective `'statusline'` `%`-format (empty ⇒ the built-in
    /// default look) and expand it into layout-ready [`Piece`](statusline::Piece)s
    /// for `ctx` — the exact resolution shared by the render path
    /// ([`render_statusline`](Self::render_statusline)) and the click-resolution
    /// path ([`statusline_click_at`](Self::statusline_click_at)), so the painted
    /// line and the recomputed click spans always agree. `Err` carries a malformed
    /// format's parse-error text.
    fn expand_statusline_fmt(
        &self,
        statusline_fmt: &str,
        mode_label: &str,
        ctx: &bemtvi_core::statusline::StatuslineCtx,
    ) -> Result<Vec<statusline::Piece>, String> {
        let default;
        let fmt = if statusline_fmt.is_empty() {
            default = default_statusline(mode_label, ctx);
            &default
        } else {
            statusline_fmt
        };
        let items = statusline::parse(fmt)?;
        let mut eval = |_kind: ExprKind, raw: &str| self.eval_statusline_expr(raw, ctx);
        Ok(statusline::expand(&items, ctx, &mut eval))
    }

    /// The two inputs a segment-layout render needs, built identically on the
    /// render and click paths: `ctx` cloned with its diagnostic counts filled in,
    /// and the custom-segment lookup over the per-`(window, name)` cache the Lua
    /// re-renders populate.
    fn segment_render_inputs<'a>(
        &'a self,
        win_id: u64,
        spec: &bemtvi_core::statusline::SegmentLayout,
        ctx: &bemtvi_core::statusline::StatuslineCtx,
    ) -> (
        bemtvi_core::statusline::StatuslineCtx,
        impl Fn(&str) -> Option<Vec<bemtvi_core::statusline::StatusSegment>> + 'a,
    ) {
        let mut seg_ctx = ctx.clone();
        // The diagnostic counts are the only per-frame input with a collection
        // cost (they walk the whole merged set), and only the `diagnostics`
        // built-in reads them — skip the walk for a layout that never shows it.
        if spec.uses_builtin("diagnostics") {
            seg_ctx.diag_counts =
                self.statusline_diag_counts(bemtvi_core::BufferId(ctx.bufnr as u64));
        }
        let cache = &self.statusline_cache;
        let custom = move |name: &str| cache.get(&(win_id, name.to_string())).cloned();
        (seg_ctx, custom)
    }

    /// Resolve a status/tabline click in window `win_id` at display column `col` to
    /// the [`ClickAction`](bemtvi_core::statusline::ClickAction) of the region
    /// covering it, or `None` if the column is in no region.
    ///
    /// Recomputed on demand (a click is rare) rather than cached during redraw: it
    /// rebuilds the relevant context at the painted width and finds the region
    /// covering `col`. The [`surface`](bemtvi_core::ClickSurface) picks the format and
    /// width, mirroring [`Self::render_statusline`]'s paths so the recomputed spans
    /// match the painted line:
    /// - **Window** — the window's `'statusline'` (`%@…%X`) or its `btv.statusline`
    ///   segment layout (a cell `on_click`), at the window's content width.
    /// - **Global** — the focused window's `'statusline'` at the full editor width.
    /// - **Tabline** — the focused window's `'tabline'` `%`-format (`%nT` tab-select,
    ///   `%@…%X`) at the full editor width; never a segment layout.
    pub(crate) fn statusline_click_at(
        &mut self,
        win_id: u64,
        col: usize,
        surface: bemtvi_core::ClickSurface,
    ) -> Option<bemtvi_core::statusline::ClickAction> {
        use bemtvi_core::ClickSurface;
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
                    win.rect
                        .width
                        .saturating_sub(2 * inset)
                        .saturating_sub(win.padding.horizontal())
                };
                if let Some(layout) = self.resolve_window_layout(win_id) {
                    // Segment layout: a clickable cell carries an `on_click` handler,
                    // which `compose_segments_with_clicks` turns into a column span.
                    let (seg_ctx, custom) = self.segment_render_inputs(win_id, layout, ctx);
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
                    // A malformed format renders as its error text (no click regions).
                    let pieces = self
                        .expand_statusline_fmt(&statusline_fmt, &view.mode_label, ctx)
                        .ok()?;
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

    /// Project resolved [`StatusSegment`](bemtvi_core::statusline::StatusSegment)s
    /// into the `status` array clients paint: `{ text, style }` per run, the
    /// highlight group resolved to a style-palette id (`Nil` when it has none /
    /// the colorscheme leaves it undefined). Shared by the `%`-format and the
    /// `btv.statusline` segment paths.
    fn project_status_segments(
        &self,
        segments: &[bemtvi_core::statusline::StatusSegment],
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
    fn statusline_diag_counts(&self, buffer: bemtvi_core::BufferId) -> [usize; 4] {
        self.diag_counts_for(buffer)
    }
    #[cfg(not(feature = "native"))]
    fn statusline_diag_counts(&self, _buffer: bemtvi_core::BufferId) -> [usize; 4] {
        [0; 4]
    }

    /// Evaluate one `%{}`/`%!` statusline expression against the window's
    /// [`StatuslineCtx`]. Two expression flavours are supported:
    ///
    /// - **`v:lua.…`** — the `v:lua.` prefix is stripped to the bare Lua
    ///   expression (`v:lua.require('m').f()` → `require('m').f()`), which the
    ///   synchronous evaluator runs inline during redraw. (bemtvi has no Vimscript;
    ///   `v:lua.` is the bridge to a config's own logic.)
    /// - **Pure Vim expressions** — literals, arithmetic, comparison, logical and
    ///   ternary operators, and `&option` references (`%{&fileencoding}`,
    ///   `%{&bomb?"[bom]":""}`). These run through the pure core evaluator
    ///   ([`bemtvi_core::editor::expr::eval_expr`]); `&option` resolves against the
    ///   buffer-display options the `StatuslineCtx` carries.
    ///
    /// Anything else — a bare variable, an unknown option, a malformed expression —
    /// returns a loud `E:…` marker naming the offender (rendered on the status
    /// line) rather than silently expanding to nothing.
    fn eval_statusline_expr(
        &self,
        raw: &str,
        ctx: &bemtvi_core::statusline::StatuslineCtx,
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
        buffer: bemtvi_core::BufferId,
        winhl: &WinHl,
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
        // Gutter signs ride the band too (extmark + diagnostic, merged), keyed on the
        // band's per-row `segments` so they slide with the text — like `window_value`.
        let diagnostics_signs =
            crate::extmarks::signs_value(&self.merged_sign_cells(buffer, winhl, &segments, styles));
        #[cfg(feature = "native")]
        let (highlights, inlay_hints, diagnostics_virt, diagnostics) = (
            self.highlights_for(buffer, winhl, &segments, styles),
            self.inlay_hints_for(buffer, winhl, &segments, styles),
            self.diagnostics_virt_text_for(buffer, winhl, &segments, styles),
            self.diagnostics_for(buffer, winhl, &segments, styles),
        );
        #[cfg(not(feature = "native"))]
        let (highlights, inlay_hints, diagnostics_virt, diagnostics) = (
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
            Value::Array(Vec::new()),
        );
        // Extmark `virt_text` + `virt_lines` ride the band (pure projections, like
        // `window_value`), so they slide with the text instead of flashing on settle.
        let virt_text = self.virt_text_for(buffer, winhl, &segments, &selection, styles);
        let virt_lines = self.virt_lines_value(&virt_lines, winhl, styles);
        // The line-background layer rides the band too, so a code block's tint slides
        // with the text (mirrors `window_value`).
        let line_bg = self.line_bg_for(buffer, winhl, &segments, styles);
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
            (Value::from("line_bg"), line_bg),
            (Value::from("diagnostics_virt"), diagnostics_virt),
            (Value::from("diagnostics"), diagnostics),
            (Value::from("diagnostics_signs"), diagnostics_signs),
        ])
    }

    /// Resolve a highlight `group` for a window, honoring its `winhighlight` remap
    /// (`winhl`) — `group` is renamed once (see [`WinHl::remap`]) before the normal
    /// link-following resolution. With an empty remap (the common case) this is
    /// exactly `highlights.resolve(group)`, so non-`winhighlight` windows are
    /// unchanged.
    pub(crate) fn resolve_winhl(&self, winhl: &WinHl, group: &str) -> Option<Style> {
        self.editor.highlights.resolve(winhl.remap(group))
    }

    /// Like [`resolve_winhl`](Self::resolve_winhl) but for a treesitter capture: the
    /// capture name is remapped once, then resolved through the `@`-group fallback
    /// chain. (`winhighlight` keys on the named group, so `Comment:Foo` remaps a
    /// span tagged `Comment`/`@comment` literally — not the final resolved style.)
    pub(crate) fn resolve_capture_winhl(&self, winhl: &WinHl, group: &str) -> Option<Style> {
        self.editor.highlights.resolve_capture(winhl.remap(group))
    }

    /// Resolve the editor-chrome highlight groups (the background, gutter,
    /// selection, and status line) to style-palette indices for this frame. Each
    /// resolved group becomes a `name -> style_id` entry; groups the colorscheme
    /// leaves undefined are simply absent, so the client keeps its built-in look
    /// (e.g. reverse-video selection) for them. Empty map when no theme is loaded.
    pub(crate) fn chrome_styles(&self, styles: &mut StyleTable) -> Value {
        let entries = CHROME
            .iter()
            .filter_map(|(key, group)| {
                let style = self.editor.highlights.resolve(group)?;
                Some((Value::from(*key), Value::from(styles.intern(style) as u64)))
            })
            .collect();
        Value::Map(entries)
    }

    /// Resolve the tree-sitter capture groups the browser highlighter colors to
    /// style-palette ids — the wasm build's bridge from the loaded colorscheme to
    /// JS-side syntax highlighting (the native builds bake these into per-window
    /// `highlights` spans instead, so this is wasm-only). Each entry is a capture
    /// name keyed exactly as `web/highlight.js`'s static `FG` table, resolved via
    /// the standard capture fallback (`@function.builtin` → `@function` →
    /// `Function`); the client walks the same dotted-fallback chain over the result,
    /// so it themes from the colorscheme's `@`-groups and falls back to the static
    /// table only for captures the scheme leaves undefined. Absent groups are simply
    /// omitted (the client keeps its built-in color for them).
    #[cfg(not(feature = "native"))]
    pub(crate) fn syntax_theme(&self, styles: &mut StyleTable) -> Value {
        let entries = SYNTAX_CAPTURES
            .iter()
            .filter_map(|capture| {
                let style = self.editor.highlights.resolve_capture(capture)?;
                Some((
                    Value::from(*capture),
                    Value::from(styles.intern(style) as u64),
                ))
            })
            .collect();
        Value::Map(entries)
    }

    /// Resolve the first defined highlight group in `groups` to a palette id
    /// (interning it into `styles`), or `None` when none of them is themed. Gives a
    /// widget region a fallback chain — e.g. `TelescopeSelection` → `PmenuSel` →
    /// `Visual` — so it themes under a plugin's groups when present and a sensible
    /// built-in group otherwise.
    fn resolve_region(&self, groups: &[&str], styles: &mut StyleTable) -> Option<u64> {
        groups
            .iter()
            .find_map(|g| self.editor.highlights.resolve(g))
            .map(|s| styles.intern(s) as u64)
    }

    /// The themeable colors of the menu/picker widget, resolved from the well-known
    /// plugin highlight groups so a colorscheme themes the popup automatically: the
    /// insert-completion popup follows **nvim-cmp** (`Pmenu` / `PmenuSel` /
    /// `CmpItemAbbrMatch`), a picker / `select` list follows **telescope**
    /// (`Telescope*`). Each region falls through a chain ending in a group the
    /// built-in scheme defines, and is emitted only when something resolves; the
    /// client keeps its built-in look for an absent region. Returned as a
    /// `region -> style_id` map (empty ⇒ no key emitted by the caller).
    fn menu_styles(&self, completion: bool, styles: &mut StyleTable) -> Value {
        // `(wire key, group fallback chain)`. Completion ↔ cmp groups, picker ↔
        // telescope groups; both bottom out at the core chrome the built-in scheme
        // ships so the popup is themed even under `:colorscheme bemtvi`.
        let chains: &[(&str, &[&str])] = if completion {
            &[
                ("bg", &["Pmenu", "NormalFloat"]),
                ("sel", &["PmenuSel", "Visual"]),
                ("match", &["CmpItemAbbrMatch", "Special"]),
                ("border", &["FloatBorder"]),
                // The docs sidebar beside the popup (LSP documentation / cmdline help):
                // nvim-cmp's dedicated documentation-window groups, else the popup look.
                ("doc", &["CmpDocumentation", "Pmenu", "NormalFloat"]),
                ("doc_border", &["CmpDocumentationBorder", "FloatBorder"]),
            ]
        } else {
            &[
                ("bg", &["TelescopeNormal", "NormalFloat", "Pmenu"]),
                ("sel", &["TelescopeSelection", "PmenuSel", "Visual"]),
                ("match", &["TelescopeMatching", "Special"]),
                ("border", &["TelescopeBorder", "FloatBorder"]),
                ("prompt", &["TelescopePromptPrefix", "Special"]),
                ("title", &["TelescopeTitle", "FloatTitle"]),
                ("doc", &["TelescopePreviewNormal", "NormalFloat", "Pmenu"]),
                ("doc_border", &["TelescopePreviewBorder", "FloatBorder"]),
            ]
        };
        let entries = chains
            .iter()
            .filter_map(|(key, groups)| {
                self.resolve_region(groups, styles)
                    .map(|id| (Value::from(*key), Value::from(id)))
            })
            .collect();
        Value::Map(entries)
    }

    /// A window's `winhighlight` overrides to the global chrome map: for each chrome
    /// key whose group this window *renames* (e.g. `Normal:NormalSB` renames the
    /// `normal` key's `Normal`), the remapped group's resolved style id — so the
    /// client paints this window's background / gutter / EOB with the renamed group.
    /// Only the keys the remap actually touches are emitted; every other key (and
    /// every non-`winhighlight` window) falls back to the global [`chrome_styles`]
    /// map. Empty for almost every window, so the wire is unchanged for them.
    ///
    /// [`chrome_styles`]: Self::chrome_styles
    fn window_chrome_overrides(&self, winhl: &WinHl, styles: &mut StyleTable) -> Value {
        if winhl.is_empty() {
            return Value::Map(Vec::new());
        }
        let entries = CHROME
            .iter()
            .filter_map(|(key, group)| {
                let remapped = winhl.remap(group);
                // Unchanged keys fall back to the global chrome map — emit nothing.
                if remapped == *group {
                    return None;
                }
                let style = self.editor.highlights.resolve(remapped)?;
                Some((Value::from(*key), Value::from(styles.intern(style) as u64)))
            })
            .collect();
        Value::Map(entries)
    }
}

/// The redraw-key → highlight-group map for editor chrome (background, gutter,
/// selection, status line, float chrome). The keys mirror the View regions the
/// client themes; the groups are neovim's standard chrome groups. Shared by the
/// global [`EditHost::chrome_styles`] map and the per-window `winhighlight`
/// overrides ([`EditHost::window_chrome_overrides`]).
const CHROME: &[(&str, &str)] = &[
    ("normal", "Normal"),
    ("cursor", "Cursor"),
    ("line_nr", "LineNr"),
    ("cursor_line_nr", "CursorLineNr"),
    ("cursorline", "CursorLine"),
    ("colorcolumn", "ColorColumn"),
    ("visual", "Visual"),
    ("search", "Search"),
    ("incsearch", "IncSearch"),
    ("status_line", "StatusLine"),
    // The UNFOCUSED window's status bar. vim paints only the focused window's bar
    // with `StatusLine`; every other one takes `StatusLineNC`, which is the cue
    // telling you which split has focus. A client falls back to `status_line` when
    // the colorscheme leaves it undefined.
    ("status_line_nc", "StatusLineNC"),
    ("win_separator", "WinSeparator"),
    ("tabline", "TabLine"),
    ("tabline_sel", "TabLineSel"),
    ("tabline_fill", "TabLineFill"),
    ("error_msg", "ErrorMsg"),
    ("msg_area", "MsgArea"),
    ("end_of_buffer", "EndOfBuffer"),
    ("float_border", "FloatBorder"),
    ("normal_float", "NormalFloat"),
    ("float_title", "FloatTitle"),
];

/// The tree-sitter capture names the browser highlighter colors, keyed exactly as
/// the static `FG` table in `web/highlight.js`. The wasm redraw resolves each
/// through the colorscheme's capture fallback ([`Highlights::resolve_capture`]) so
/// the client themes code from the active colorscheme; the client walks the same
/// dotted-fallback chain (`function.call` → `function`) over the result, so only the
/// distinctive sub-cases need their own entry. Wasm-only (the native builds resolve
/// syntax into per-window `highlights` spans directly).
#[cfg(not(feature = "native"))]
const SYNTAX_CAPTURES: &[&str] = &[
    "comment",
    "keyword",
    "conditional",
    "repeat",
    "exception",
    "include",
    "operator",
    "keyword.operator",
    "string",
    "character",
    "escape",
    "string.escape",
    "string.special",
    "number",
    "float",
    "boolean",
    "constant",
    "constant.builtin",
    "function",
    "function.builtin",
    "function.macro",
    "constructor",
    "type",
    "type.builtin",
    "property",
    "field",
    "variable.member",
    "variable",
    "variable.parameter",
    "parameter",
    "variable.builtin",
    "label",
    "punctuation",
    "punctuation.bracket",
    "punctuation.delimiter",
    "punctuation.special",
    "attribute",
    "annotation",
    "namespace",
    "module",
    "tag",
    "tag.attribute",
    "tag.delimiter",
    "keyword.directive",
    // Markup captures — a markdown buffer highlighted by the browser's bundled markdown
    // grammar (block) plus the markdown_inline it injects. Only a colorscheme's *fg*
    // crosses the wire (the client paints per-column colors), so the bold/italic the
    // built-in scheme gives `@markup.strong` / `@markup.italic` doesn't come through and
    // the client falls back to its own hue for those. `markup.raw.block` is deliberately
    // absent: it is a full-line background, which this build has no layer for, and the
    // client drops the capture rather than repaint the code block's foreground.
    "markup.heading",
    "markup.heading.1",
    "markup.heading.2",
    "markup.heading.3",
    "markup.heading.4",
    "markup.heading.5",
    "markup.heading.6",
    "markup.strong",
    "markup.italic",
    "markup.strikethrough",
    "markup.raw",
    "markup.link",
    "markup.link.label",
    "markup.link.url",
    "markup.list",
    "markup.list.checked",
    "markup.list.unchecked",
    "markup.quote",
];

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
/// base `StatusLine` look. The text is display-scrubbed here — the statusline
/// renders the file name and LSP/diagnostic-derived text, so a control byte in
/// either must not reach the terminal as an escape sequence. Segments carry no
/// char offsets, so the substitution shifts nothing.
fn segment_value(text: &str, style: Value) -> Value {
    Value::Map(vec![
        (
            Value::from("text"),
            Value::from(unicode::display_line(text).as_ref()),
        ),
        (Value::from("style"), style),
    ])
}

/// The built-in `'statusline'` look, expressed as a format string so the one
/// engine renders it too: ` MODE  file[+]  …  enc  line,col `. The mode label and
/// the encoding are bemtvi additions spliced in as literals — escaped, though
/// neither a mode name nor an encoding label ever contains `%` — since they are not
/// `%`-items (neovim has no encoding item; it's conventionally `%{&fenc}`). `enc`
/// carries the buffer's `'fileencoding'`, with a `[bom]` suffix when `'bomb'` is set
/// and a `[noeol]` suffix when the buffer holds an **unterminated file** — the only cue
/// that the file's last line lacks a line break (and that saving under the default
/// `'fixendofline'` will supply one).
///
/// `[noeol]` is narrower than `!endofline`. The buffer must hold an unterminated *file*
/// ([`Buffer::is_unterminated_document`](bemtvi_core::Buffer::is_unterminated_document),
/// which excludes the empty document a brand-new or 0-byte file has — vim shows `[New]`
/// there, never `[noeol]`), and it must be a document rather than editor chrome
/// (`buftype` `""`: a panel, `btv.view`, quickfix list or terminal is never written to
/// disk). Without those gates the marker would sit on `[No Name]`, on every listing and
/// on every new file — noise on exactly the buffers it says nothing about.
///
/// The `%<` before `%f` is the truncation point (vim's `%<%f` idiom): when the line
/// is too narrow, the *path* is the thing that shrinks (keeping its tail), so the
/// right-aligned encoding + position stay visible. Without it the cut would default
/// to the `%=` marker, and a long path would overflow the prefix and drop the whole
/// right-aligned section — hiding the encoding behind a `>`.
fn default_statusline(mode_label: &str, ctx: &bemtvi_core::statusline::StatuslineCtx) -> String {
    let mut enc = ctx.fileencoding.clone();
    if ctx.bomb {
        enc.push_str("[bom]");
    }
    if ctx.unterminated_file && ctx.buftype.is_empty() {
        enc.push_str("[noeol]");
    }
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
fn statusline_option(ctx: &bemtvi_core::statusline::StatuslineCtx, name: &str) -> Option<OptVal> {
    match name {
        "fileencoding" | "fenc" => Some(OptVal::Str(ctx.fileencoding.clone())),
        "filetype" | "ft" => Some(OptVal::Str(ctx.filetype.clone())),
        "buftype" | "bt" => Some(OptVal::Str(ctx.buftype.clone())),
        "bomb" => Some(OptVal::Int(ctx.bomb as i64)),
        "endofline" | "eol" => Some(OptVal::Int(ctx.endofline as i64)),
        "fixendofline" | "fixeol" => Some(OptVal::Int(ctx.fixendofline as i64)),
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

/// Flatten a status-line / chunk-run `Value` to its visible text — used to mirror
/// the status line into `btv._ui.statusline` for the plugin test framework. A chunk
/// is `[text, style]`; this collects each chunk's leading text in order, recursing
/// through the surrounding arrays/maps so it works whatever the exact nesting is.
/// Only chunk-pair text is taken (not bare scalar fields), so labels in a map don't
/// leak in. `Nil` (per-window status modes / no global bar) flattens to "".
fn chunk_runs_text(value: &Value) -> String {
    fn walk(v: &Value, out: &mut String) {
        match v {
            Value::Array(items) => {
                // A chunk pair `[text, style, …]`: take the leading string, don't
                // descend further into it.
                if items.len() >= 2 {
                    if let Some(Value::String(s)) = items.first() {
                        out.push_str(s.as_str().unwrap_or_default());
                        return;
                    }
                }
                for it in items {
                    walk(it, out);
                }
            }
            Value::Map(entries) => {
                // A statusline segment / cell map `{ text: "…", style: … }` (what
                // `segment_value` builds): take its `text` and don't descend into the
                // style. This is the shape `global_status` actually carries — without
                // this arm the mirror would drop every segment's text (the values are
                // bare strings/ints the `_` arm ignores).
                if let Some((_, Value::String(s))) = entries
                    .iter()
                    .find(|(k, _)| matches!(k, Value::String(ks) if ks.as_str() == Some("text")))
                {
                    out.push_str(s.as_str().unwrap_or_default());
                    return;
                }
                for (_, val) in entries {
                    walk(val, out);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(value, &mut out);
    out
}

/// Word-wrap each of `lines` to at most `width` columns, breaking on whitespace and
/// hard-breaking a single word longer than `width`. Blank lines are preserved (so a
/// synopsis / blank / body layout keeps its paragraph break). Used by the cmdline
/// wildmenu docs float so a long help line flows onto several rows instead of being
/// cut off at the box's right border. Column = `char` count (the help text is ASCII).
pub(crate) fn wrap_doc_lines(lines: &[String], width: usize) -> Vec<String> {
    if width == 0 {
        return lines.to_vec();
    }
    let mut out = Vec::with_capacity(lines.len());
    // Push `word` onto the wrap output, hard-breaking it into `width`-sized chunks when
    // it alone overflows the box (a URL / long identifier). Returns the trailing
    // remainder that becomes the current in-progress row.
    let break_long = |out: &mut Vec<String>, word: &str| -> String {
        let mut chars: Vec<char> = word.chars().collect();
        while chars.len() > width {
            out.push(chars[..width].iter().collect());
            chars.drain(..width);
        }
        chars.iter().collect()
    };
    for line in lines {
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in line.split_whitespace() {
            let ww = word.chars().count();
            if cur.is_empty() {
                cur = if ww <= width {
                    word.to_string()
                } else {
                    break_long(&mut out, word)
                };
            } else if cur.chars().count() + 1 + ww <= width {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(std::mem::take(&mut cur));
                cur = if ww <= width {
                    word.to_string()
                } else {
                    break_long(&mut out, word)
                };
            }
        }
        out.push(cur);
    }
    out
}

/// Place a docs sidebar of `content_w` columns beside a popup box whose content
/// starts at `box_col` and is `box_width` wide, within a `bound_w`-column area.
/// Prefers the right of the box, flipping left when that side has more room, and
/// returns `(docs_col, docs_w)` — the float's **content** top-left column and its
/// width, in the bound area's own cells. `None` when neither side fits a readable
/// width, so a caller shows no sidebar rather than a one-column sliver. Shared by
/// both docs surfaces (insert-completion + the cmdline wildmenu).
pub(crate) fn place_docs_beside(
    box_col: usize,
    box_width: usize,
    content_w: usize,
    bound_w: usize,
) -> Option<(usize, usize)> {
    /// Below this, a sidebar is a useless sliver — better none than a 1-col float.
    const MIN_DOCS_W: usize = 10;
    // Right of the box: its content spans `[box_col, box_col+box_width)`; each client
    // draws a 1-cell border, so the box's right border sits at `box_col+box_width` and
    // the docs float's own left border one cell past it → content at `+2`. A trailing
    // 1-col margin keeps it off the bound's right edge. Left of the box: the docs
    // float's right border one cell left of the box's left border (`box_col-1`), so its
    // content ends at `box_col-3`, starting from the bound's left.
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

/// Encode a tab page as a `{ label, modified, window_count }` map for the redraw
/// map's `tabline` array. The client formats the cell and highlights the active
/// one (carried separately as `current_tab`).
fn tab_value(tab: &TabView) -> Value {
    Value::Map(vec![
        // Tab labels carry file names — display-scrub (terminal clients paint
        // them verbatim; a control byte in a name is an injection source).
        (
            Value::from("label"),
            Value::from(unicode::display_line(&tab.label).as_ref()),
        ),
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
        (
            Value::from("title"),
            Value::from(unicode::display_line(&rt.title).as_ref()),
        ),
    ])
}

/// The per-region tablines as a map keyed by region — `main` plus the four docks
/// (`left`/`right`/`top`/`bottom`, matching the `btv.dock` side keywords). Each
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

/// The wire string for a window/separator [`WindowRegion`] — `"main"`, one of
/// `"dock_left"`/`"dock_right"`/`"dock_top"`/`"dock_bottom"`, or `"screen"`.
/// Clients map it to the region's absolute screen origin using the redraw map's
/// dock band sizes; `"screen"` (only ever an `editor`-relative float) is already in
/// windows-area cells, so its origin is the windows area's own.
fn region_str(region: WindowRegion) -> &'static str {
    match region {
        WindowRegion::Main => "main",
        WindowRegion::Screen => "screen",
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

/// The parallel per-row wire arrays a client decodes, unbundled from core's
/// self-describing [`RenderRow`] layout. Shared by the settled window
/// (`window_value`) and the scroll band (`project_band`) — the band is just a
/// taller row set, so projecting it is identical work.
struct RowArrays<'a> {
    /// The per-row display text, borrowed from the source [`RenderRow`]s (the rows
    /// outlive this projection): the only consumers — [`display_lines_value`] and
    /// `special_key_spans` — read it as `&str`, so there's nothing to own.
    lines: Vec<&'a str>,
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
    /// Per-row span lists / virt-line chunk runs, borrowed like `lines` — these
    /// are projected every frame, so cloning them per row would be pure waste.
    secondary_selection: Vec<&'a [(usize, usize)]>,
    search: Vec<&'a [(usize, usize)]>,
    incsearch: Vec<Option<(usize, usize)>>,
    virt_lines: Vec<Option<&'a [VirtChunk]>>,
}

/// Unbundle a [`RenderRow`] slice into the parallel per-row arrays the wire
/// carries (one entry per screen row, in order). The native highlight / inlay /
/// diagnostic projections then key on `numbers` exactly as before.
fn unbundle_rows(rows: &[RenderRow]) -> RowArrays<'_> {
    RowArrays {
        lines: rows.iter().map(|r| r.text.as_str()).collect(),
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
        secondary_selection: rows
            .iter()
            .map(|r| r.secondary_selection.as_slice())
            .collect(),
        search: rows.iter().map(|r| r.search.as_slice()).collect(),
        incsearch: rows.iter().map(|r| r.incsearch).collect(),
        virt_lines: rows.iter().map(|r| r.virt_line.as_deref()).collect(),
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
pub(crate) fn display_lines_value<S: AsRef<str>>(lines: &[S]) -> Value {
    Value::Array(
        lines
            .iter()
            .map(|l| Value::from(unicode::display_line(l.as_ref()).as_ref()))
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
fn special_key_spans<S: AsRef<str>>(lines: &[S], tabstop: usize) -> Value {
    Value::Array(
        lines
            .iter()
            .map(|l| {
                let l = l.as_ref();
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

/// Project the floating selectable-list [`MenuView`] into its redraw sub-map,
/// computing the bordered box's anchor and content size. A `Cursor` box is in
/// **text-area cells** (the client adds the gutter + text-area origin, then draws
/// the border) — the same convention and placement strategy as the completion
/// popup — anchored under the cursor and flipping above when there's no room below.
/// The `Editor` / `Bottom` picker overlay is instead sized and centered over the
/// WHOLE editor (`editor_w`/`editor_h`) and carries the `editor_relative` flag, so
/// the client floats it over the windows area rather than the focused split.
/// `text_width` bounds a `Cursor` box to the editable region. Mirrors
/// `EditHost::pmenu_value`.
impl EditHost {
    fn project_menu(
        &mut self,
        m: &MenuView,
        view: &bemtvi_core::View,
        text_width: usize,
        editor_w: usize,
        editor_h: usize,
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
            bemtvi_core::MenuMetrics {
                cursor_row: focused.cursor_row,
                cursor_screen_col: focused.cursor_screen_col,
                leftcol: focused.leftcol,
                text_width,
                text_height,
                editor_w,
                editor_h,
            },
        );
        let bemtvi_core::MenuGeom {
            row,
            col,
            width,
            height,
            rows,
            selected,
            start,
            kind_col,
            ..
        } = geom;

        // Items are display-scrubbed like every other payload, so a control byte
        // in a completion label (an LSP word, a file name) reaches the client as
        // its `^X` token. The per-row `match`/`layout` offsets below are char
        // offsets into the RAW labels — translate them through the same
        // substitution or they index a string the client never saw.
        let items: Vec<Value> = rows
            .iter()
            .map(|(label, _)| Value::from(unicode::display_line(label).as_ref()))
            .collect();
        // Per-row **kind** labels (parallel to `items`): the short category the client
        // right-aligns on each completion row (`"Snippet"`, `"Function"`, …). `Nil` for
        // a kind-less row (a `buffer` word) and for every non-completion menu. Omitted
        // entirely when no row carries a kind, so a `select` / picker map is unchanged.
        let row_kinds = self.editor.menu_kinds_window(start, rows.len());
        let any_kind = row_kinds.iter().any(Option::is_some);
        let kinds = Value::Array(
            row_kinds
                .into_iter()
                .map(|k| k.map_or(Value::Nil, Value::from))
                .collect(),
        );
        // Per-row two-column **layout** (parallel to `items`): the `[head, match start,
        // match end]` char offsets of a `path:line:col: <line>` row (live_grep), so the
        // client can fit the head and the body as separate columns — and highlight the
        // source's own match — instead of head-cutting one string. `Nil` for a plain
        // row; the key is omitted entirely when no row declares one, so every other
        // menu's map is byte-for-byte unchanged.
        let row_layouts = self.editor.menu_layout_window(start, rows.len());
        let any_layout = row_layouts.iter().any(Option::is_some);
        let layouts = Value::Array(
            row_layouts
                .into_iter()
                .zip(rows.iter())
                .map(|(l, (label, _))| {
                    l.map_or(Value::Nil, |l| {
                        Value::Array(vec![
                            Value::from(unicode::display_char_offset(label, l.head.into()) as u64),
                            Value::from(
                                unicode::display_char_offset(label, l.match_start.into()) as u64
                            ),
                            Value::from(
                                unicode::display_char_offset(label, l.match_end.into()) as u64
                            ),
                        ])
                    })
                })
                .collect(),
        );
        // Multi-select: a bool per visible row (parallel to `items`) flagging the
        // user-marked rows (`<Tab>`), so the client can mark them. Always present;
        // all-false when nothing is marked.
        let marked = Value::Array(
            self.editor
                .menu_marked_window(start, rows.len())
                .into_iter()
                .map(Value::from)
                .collect(),
        );
        // Matched-character spans per visible row (parallel to `items`): `[start, end]`
        // half-open **char** ranges the client bolds — translated through the
        // display substitution, like the layouts above.
        let match_spans = Value::Array(
            rows.iter()
                .map(|(label, spans)| {
                    Value::Array(
                            spans
                                .iter()
                                .map(|r| {
                                    Value::Array(vec![
                                        Value::from(
                                            unicode::display_char_offset(label, r.start) as u64
                                        ),
                                        Value::from(
                                            unicode::display_char_offset(label, r.end) as u64
                                        ),
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
            (Value::from("marked"), marked),
        ];
        // Only grep-shaped picker rows carry a layout; omit the key otherwise.
        if any_layout {
            map.push((Value::from("layouts"), layouts));
        }
        // Only completion popups carry kinds; omit the key when every row is kind-less
        // so `select` / picker / cmdline maps are byte-for-byte unchanged.
        if any_kind {
            map.push((Value::from("kinds"), kinds));
            // The aligned kind column's start (just past the widest label): every row's
            // kind renders here so they line up. Absent ⇒ the client falls back to
            // right-aligning per row.
            if let Some(kc) = kind_col {
                map.push((Value::from("kind_col"), Value::from(kc as u64)));
            }
        }
        // Themeable colors for the widget (bg / selection / matched chars / border,
        // plus prompt + title for a picker), resolved from nvim-cmp / telescope
        // groups. The completion popup and the command-line wildmenu follow cmp; a
        // picker / `select` list follows telescope.
        let completion = m.completion || matches!(m.placement, MenuPlacement::Cmdline);
        let menu_styles = self.menu_styles(completion, styles);
        if matches!(&menu_styles, Value::Map(m) if !m.is_empty()) {
            map.push((Value::from("styles"), menu_styles));
        }
        // The completion popup omits its top border so it sits flush with the line
        // below the cursor. Absent ⇒ a full border (the `select` / picker default).
        if m.completion {
            map.push((Value::from("border_top"), Value::from(false)));
        }
        // The picker box's optional title (`btv.picker.open{ title = … }`), rendered
        // on the top border. Absent ⇒ no title.
        if let Some(title) = &m.title {
            map.push((Value::from("title"), Value::from(title.as_str())));
        }
        // The `Editor` / `Bottom` picker overlay floats over the WHOLE editor: its
        // `row`/`col` are editor-absolute (windows-area cells, computed by
        // `menu_geom` against `editor_w`/`editor_h`), so the client anchors the box to
        // the windows-area origin instead of the focused window's text inner — a split
        // can't squeeze it into the active pane. Absent ⇒ window-relative (the
        // cursor-anchored completion popup / `select`).
        if matches!(m.placement, MenuPlacement::Editor | MenuPlacement::Bottom) {
            map.push((Value::from("editor_relative"), Value::from(true)));
        }
        // The command-line wildmenu (`btv.cmdline_complete`) floats above the command
        // line, not the focused window — so the client anchors it to the command-line
        // area (frame-bottom, no number gutter) instead of the window's text inner.
        if matches!(m.placement, MenuPlacement::Cmdline) {
            map.push((Value::from("cmdline"), Value::from(true)));
            // Its docs are no longer a `menu.docs` overlay — they render as a real
            // doc-float window beside/below the box, opened during input handling
            // (`EditHost::sync_cmdline_docs_float`). See the plan doc.
        }
        // The prompt query: present (even when empty) for a picker, absent for a
        // promptless `btv.ui.select`. Its presence tells the client to draw a prompt row,
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
                    bemtvi_core::PromptPos::Top => "top",
                    bemtvi_core::PromptPos::Bottom => "bottom",
                }),
            ));
        }
        // The include/exclude filter boxes (`{ include, exclude, focus, expanded,
        // badge }`), present only for a picker whose source declared `filter = true`.
        // Absent ⇒ the client draws exactly the single-prompt box it always has, so
        // every non-filterable picker's map is byte-for-byte unchanged.
        //
        // `expanded` says whether to draw the two rows between the prompt and the
        // separator (`filter_rows` in the geometry — 2 when set); `focus` says which of
        // the three lines `query_cursor` belongs to; `badge` is the already-composed
        // collapsed-state indicator, so no client counts patterns itself.
        if let Some(f) = &m.filters {
            let mut fm: Vec<(Value, Value)> = vec![
                (Value::from("include"), Value::from(f.include.as_str())),
                (Value::from("exclude"), Value::from(f.exclude.as_str())),
                (
                    Value::from("focus"),
                    Value::from(match f.focus {
                        bemtvi_core::PromptField::Query => "query",
                        bemtvi_core::PromptField::Include => "include",
                        bemtvi_core::PromptField::Exclude => "exclude",
                    }),
                ),
                (Value::from("expanded"), Value::from(f.expanded)),
            ];
            if let Some(badge) = &f.badge {
                fm.push((Value::from("badge"), Value::from(badge.as_str())));
            }
            map.push((Value::from("filters"), Value::Map(fm)));
        }
        // The preview pane (Phase 3): a column on the right of an editor-placement
        // picker rendering the selected row's file. `None` for a `select` / preview-less
        // picker (and for `Cursor` placement — the cursor float-beside is Phase 4).
        // Sized against the resolved box; the map carries its own `width` so the client
        // knows how many columns the list keeps (`box width − preview width − 1`).
        // Runs after the row conversions above so the rows borrow (which pins
        // `self.editor`) is released before this `&mut self` call.
        let preview = if matches!(m.placement, MenuPlacement::Editor) {
            self.project_preview(m, width, height, styles)
        } else {
            None
        };
        // The preview sub-map (`{ lines, first_line, title, loc, width, highlights }`),
        // present only when this picker carries a preview pane. Its presence tells the
        // client to split the box into a list column + this preview column.
        if let Some(preview) = preview {
            map.push((Value::from("preview"), preview));
        }
        // The completion docs are no longer a `menu.docs` overlay projected here — they
        // render as a **real doc-float window** beside the popup, opened during input
        // handling (`EditHost::sync_complete_docs_float`) via `Editor::open_completion_docs_float`,
        // which owns the placement and gets syntax highlighting + native wheel scroll
        // for free (the hover model). See docs/plans/2026-07-05-completion-docs-real-window.md.
        Value::Map(map)
    }

    /// Project the list-less **content float** (`btv.ui.float`; LSP hover /
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
        view: &bemtvi_core::View,
        text_width: usize,
        editor_w: usize,
        editor_h: usize,
        styles: &mut StyleTable,
    ) -> Value {
        /// Display width of one chunked line: the sum of its chunks' char counts.
        fn line_width(line: &[bemtvi_core::VirtChunk]) -> usize {
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
            // Content floats are never cmdline-placed (only the `btv.cmdline_complete`
            // menu is); a stray one falls back to editor-centered. Anchored to the
            // whole editor (`editor_w`/`editor_h`), not the focused window — a split
            // must not drag an editor-relative float into the active pane.
            MenuPlacement::Editor | MenuPlacement::Cmdline => {
                let width = content_w.min(editor_w.saturating_sub(CHROME).max(1));
                let height = count.min(editor_h.saturating_sub(CHROME).max(1));
                let row = editor_h.saturating_sub(height + CHROME) / 2;
                let col = editor_w.saturating_sub(width + CHROME) / 2;
                (row, col, width, height)
            }
            MenuPlacement::Bottom => {
                // Pinned to the editor's bottom-RIGHT corner (the which-key shape):
                // content-hugging like `Editor`, but the box (content + border chrome)
                // sits flush against both the last text row and the right edge — of the
                // whole editor (`editor_w`/`editor_h`), so a split leaves it in place.
                let width = content_w.min(editor_w.saturating_sub(CHROME).max(1));
                let height = count.min(editor_h.saturating_sub(CHROME).max(1));
                let row = editor_h.saturating_sub(height + CHROME);
                let col = editor_w.saturating_sub(width + CHROME);
                (row, col, width, height)
            }
        };
        // Which base the geometry above is relative to, so the client offsets it by
        // the matching origin: `Cursor` floats over the focused window's text area
        // (cursor-anchored hover / signature), while `Editor`/`Bottom` floats anchor
        // to the whole editor's windows area (the which-key surface).
        let editor_relative = !matches!(cf.placement, MenuPlacement::Cursor);
        let shown = &cf.lines[..height.min(cf.lines.len())];
        // Each line ships as a chunk run `[[text, style_id], …]` (the `virt_lines`
        // wire form), so a styled caller (which-key) can colour keys vs.
        // descriptions and dim unavailable rows. A plain caller is one unstyled
        // chunk per line, which resolves to a `Nil` style id (normal colors).
        // A content float (which-key &c.) is an overlay, not a dock window, so no
        // `winhighlight` remap applies — resolve its chunks against the global theme.
        let lines = Value::Array(
            shown
                .iter()
                .map(|line| self.virt_chunks_value(line, &WinHl::default(), styles))
                .collect(),
        );
        Value::Map(vec![
            (Value::from("lines"), lines),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(col as u64)),
            (Value::from("width"), Value::from(width as u64)),
            (Value::from("height"), Value::from(height as u64)),
            (Value::from("border"), Value::from(cf.border.as_str())),
            (Value::from("editor_relative"), Value::from(editor_relative)),
            (
                Value::from("title"),
                cf.title.as_deref().map_or(Value::Nil, |t| {
                    Value::from(unicode::display_line(t).into_owned())
                }),
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

        let (lines, first_line, loc, title, highlights, filetype, line_bg) = match &m.preview {
            Some(target) => {
                self.ensure_preview(&target.path);
                let len = self.preview_cache.lines.len();
                // The manual scroll offset belongs to one target; reset it when the
                // selection moves to a different row/file so each selection re-centers.
                if self.preview_anchor.as_ref() != Some(target) {
                    self.preview_scroll = 0;
                    self.preview_hscroll = 0;
                    self.preview_anchor = Some(target.clone());
                }
                // Fold this frame's one-shot scroll gesture (`<C-d>`/`<C-u>` half page,
                // `<C-f>`/`<C-b>` full page) into the persistent offset. Full page keeps
                // a two-line overlap, matching the editor's normal `<C-f>`/`<C-b>`. A
                // horizontal gesture (`<S-ScrollWheel>` / horizontal wheel) advances the
                // column offset instead, a few columns per notch.
                if let Some(gesture) = m.preview_scroll {
                    /// Columns per horizontal preview notch (vim's default `'mousescroll'`).
                    const HSTEP: usize = 6;
                    let half = (pane_h / 2).max(1) as isize;
                    let page = pane_h.saturating_sub(2).max(1) as isize;
                    match gesture {
                        bemtvi_core::PreviewScroll::HalfDown => self.preview_scroll += half,
                        bemtvi_core::PreviewScroll::HalfUp => self.preview_scroll -= half,
                        bemtvi_core::PreviewScroll::PageDown => self.preview_scroll += page,
                        bemtvi_core::PreviewScroll::PageUp => self.preview_scroll -= page,
                        bemtvi_core::PreviewScroll::Right => self.preview_hscroll += HSTEP,
                        bemtvi_core::PreviewScroll::Left => {
                            self.preview_hscroll = self.preview_hscroll.saturating_sub(HSTEP)
                        }
                    }
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
                // Expand each visible line's tabs to spaces up front. The preview
                // renders char-by-char with each cell one column, but a raw `\t` handed
                // to the client paints as nothing (a zero-width control), so tab-indented
                // content — help code blocks, aligned source — would collapse leftward.
                // Expanding here (the window text does the same via `expand_tabs`) keeps
                // the sliced lines, the `max_w` clamp, and the byte→column span mapping
                // all in one consistent expanded-column space for every client.
                let exp: Vec<std::borrow::Cow<'_, str>> =
                    win.iter().map(|l| expand_preview_tabs(l)).collect();
                // Horizontal scroll: clamp the manual column offset to the widest visible
                // line (so a short window can't stay scrolled past its longest row), fold
                // the clamp back like the vertical offset, then slice each line + the
                // match column from there. `preview_w` is the visible column count.
                let max_w = exp.iter().map(|l| l.chars().count()).max().unwrap_or(0);
                let hscroll = self.preview_hscroll.min(max_w.saturating_sub(preview_w));
                self.preview_hscroll = hscroll;
                // Per windowed line, the cached tree-sitter spans mapped to char
                // columns + per-frame style ids — the same `[start, end, group,
                // style_id]` shape as a window's text highlights, so the clients reuse
                // their span renderer. Empty rows (no grammar / blank line) stay plain.
                // The raw line is passed so the byte→column mapping expands tabs to match
                // `exp`; spans rebase by `hscroll` to match the sliced line.
                let highlights = Value::Array(
                    win.iter()
                        .enumerate()
                        .map(|(i, text)| {
                            preview_line_spans(
                                text,
                                cache.highlights.get(&(start + i)),
                                &self.editor.highlights,
                                styles,
                                hscroll,
                            )
                        })
                        .collect(),
                );
                // Slice each visible (tab-expanded) line to the horizontal window (the
                // client renders from column 0 and truncates to the pane width — so at
                // `hscroll == 0` this is the whole line).
                let shown: Vec<String> = exp
                    .iter()
                    .map(|l| l.chars().skip(hscroll).collect())
                    .collect();
                // The match position, rebased into the window — only when the read
                // succeeded (a placeholder has no meaningful location to highlight).
                let loc = match target.loc {
                    Some((r, c)) if cache.ok && r >= start && r < end => {
                        Some((r - start, c.saturating_sub(hscroll)))
                    }
                    _ => None,
                };
                let filetype = cache.lang.clone();
                // The line-background layer: each visible row whose file line carries a
                // full-line-background capture (`@markup.raw.block` — a fenced code
                // block) as `[row, style_id]`, the group resolved into this frame's
                // palette. The client paints it under the text so the block background
                // survives the token spans instead of showing only between them.
                // Resolved once (one group for every row); empty when the colorscheme
                // leaves `@markup.raw.block` undefined.
                let line_bg = match self.editor.highlights.resolve("@markup.raw.block") {
                    Some(style) => {
                        let style_id = styles.intern(style) as u64;
                        Value::Array(
                            (start..end)
                                .filter(|line| cache.block_bg_lines.contains(line))
                                .map(|line| {
                                    Value::Array(vec![
                                        Value::from((line - start) as u64),
                                        Value::from(style_id),
                                    ])
                                })
                                .collect(),
                        )
                    }
                    None => Value::Array(Vec::new()),
                };
                (
                    shown,
                    start + 1,
                    loc,
                    target.path.clone(),
                    highlights,
                    filetype,
                    line_bg,
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
            // The treesitter language the pane was highlighted as (empty ⇒ no grammar),
            // mirroring a window's `filetype`. Lets a help `doc/*.txt` report `vimdoc`.
            (Value::from("filetype"), Value::from(filetype.as_str())),
            // The line-background layer: `[row, style_id]` per visible row backing a
            // fenced code block (`@markup.raw.block`), painted under the text so its
            // background isn't overwritten by the (injected) token spans. Empty when
            // the colorscheme leaves the group undefined.
            (Value::from("line_bg"), line_bg),
        ]))
    }

    /// Ensure [`preview_cache`](EditHost::preview_cache) holds the file for `path`,
    /// reading it through the editor's host FS on a path miss (a hit — the common case
    /// as the selection moves within one file's matches — does nothing). A read error
    /// fills a single visible placeholder line with `ok = false`.
    ///
    /// Three sources, in order: an already-loaded buffer's in-memory lines (works
    /// off-tick + shows unsaved edits); an **async fetch** over the off-tick fs seam
    /// (daemon / web — placeholder until it lands, see [`apply_preview`]); a synchronous
    /// local read (native, on-tick).
    fn ensure_preview(&mut self, path: &str) {
        // Resolve a relative target against the effective cwd up front, so the cache key,
        // the in-memory lookup, the sync read, AND the async fetch/landing all agree on
        // ONE absolute path. `rg`/`grep` (and the btv.fs fallback) emit cwd-relative paths,
        // but the daemon/OPFS read carries no session cwd — so an unresolved relative path
        // would read against the wrong root and the preview would never land.
        let abs = self.resolve_preview_path(path);
        let p = abs.as_path();
        if self.preview_cache.path.as_deref() == Some(p) {
            return;
        }
        // An already-loaded buffer (the buffers picker previews open buffers; a file
        // item that's open benefits too): read its in-memory lines — no host FS, so it
        // works off-tick and reflects live unsaved edits.
        if let Some(id) = self.editor.find_buffer_by_path(p) {
            let lines = self.editor.lines_of(id).unwrap_or_default();
            self.store_preview(p, lines, true);
            return;
        }
        // Off-tick fs (daemon / web): the file isn't open and can't be read
        // synchronously, so kick a fetch over the same `fs_fetch` seam `:edit` uses
        // (tagged with the reserved [`PREVIEW_FETCH_BUF`] so its landing routes here, not
        // to a buffer) and show a placeholder until it arrives. Caching the placeholder
        // under `p` stops the next frame from re-issuing the fetch.
        if self.editor.host_fs_offtick() {
            self.fx
                .fs_fetch(PREVIEW_FETCH_BUF, p.to_string_lossy().into_owned());
            self.store_preview(p, vec![format!("{}: loading…", p.display())], false);
            return;
        }
        // On-tick local read.
        let (lines, ok) = read_preview_file(&self.editor, p);
        self.store_preview(p, lines, ok);
    }

    /// Resolve a preview target `path` to an absolute path against the effective working
    /// dir (window-local → tab-local → global, like `:edit`). An already-absolute path is
    /// returned unchanged. Keeps `rg`/`grep`/btv.fs cwd-relative results readable through
    /// the off-tick fs seam, which has no session cwd of its own.
    fn resolve_preview_path(&self, path: &str) -> std::path::PathBuf {
        let p = std::path::Path::new(path);
        if p.is_absolute() {
            return p.to_path_buf();
        }
        // Through [`session_cwd`](EditHost::session_cwd) — the one place that answers
        // "where the user is" — so a preview resolves against the same base as `:edit`
        // and every LSP path, on a daemon session as much as locally.
        self.session_cwd().join(p)
    }

    /// Store the preview `lines` for `path` into [`preview_cache`](EditHost::preview_cache),
    /// syntax-highlighting the whole file once here (so moving the selection within one
    /// file's matches never re-parses). Highlights are keyed by 0-based file line; empty
    /// when the read failed (`ok = false`) or no grammar is installed for the path.
    /// Recompute the cached preview's highlights in place — its language's grammar
    /// just landed, and the preview was stored while the load was still in flight (so
    /// it painted as plain text). The file is not re-read: only the spans change.
    pub(crate) fn rehighlight_preview(&mut self) {
        let Some(path) = self.preview_cache.path.clone() else {
            return;
        };
        let lines = std::mem::take(&mut self.preview_cache.lines);
        let ok = self.preview_cache.ok;
        self.store_preview(&path, lines, ok);
    }

    fn store_preview(&mut self, p: &std::path::Path, lines: Vec<String>, ok: bool) {
        // The highlight language: the extension's grammar, or — for a vim help
        // `doc/*.txt`, which no extension rule catches — `vimdoc`, decided from the
        // file's last non-blank line (where the `ft=help` modeline lives).
        let lang = if ok {
            bemtvi_core::language_of_path(Some(p)).or_else(|| {
                let last = lines
                    .iter()
                    .rfind(|l| !l.trim().is_empty())
                    .map_or("", |l| l.as_str());
                bemtvi_core::language_of_help_doc(p, last)
            })
        } else {
            None
        };
        let mut highlights: HashMap<usize, Vec<bemtvi_core::Span>> = HashMap::new();
        let mut block_bg_lines = std::collections::HashSet::new();
        if let Some(lang) = lang {
            // Trailing newline to match the engine's buffer invariant (it treats
            // the last line as a phantom: `len_lines - 1`); without it a
            // single-line file parses to zero lines and drops every span.
            let text = lines.join("\n") + "\n";
            let (spans, bg) = self.resolved_preview_highlights(lang, &text, 0, lines.len());
            for span in spans {
                highlights.entry(span.line).or_default().push(span);
            }
            block_bg_lines = bg.into_iter().collect();
        }
        self.preview_cache = PreviewCache {
            path: Some(p.to_path_buf()),
            lines,
            ok,
            highlights,
            block_bg_lines,
            lang: lang.unwrap_or_default().to_string(),
        };
    }

    /// Land an async preview fetch ([`ensure_preview`]'s off-tick branch): replace the
    /// `"loading…"` placeholder with the fetched `lines`. A no-op when the selection has
    /// moved to a different target since the fetch was issued (the cache now holds another
    /// path) — that stale result is dropped, and the new target's own fetch is in flight.
    /// The caller repaints (native via `settle_events`, wasm via `complete_fs_read`).
    pub(crate) fn apply_preview(&mut self, path: String, lines: Vec<String>, ok: bool) {
        if self.preview_cache.path.as_deref() != Some(std::path::Path::new(&path)) {
            return;
        }
        self.store_preview(std::path::Path::new(&path), lines, ok);
    }
}

/// The reserved [`BufferId`] an async **preview** fetch ([`EditHost::ensure_preview`])
/// tags its `fs_fetch` with, so the shared open-landing (`apply_open` /
/// `complete_fs_read`) routes the bytes to [`EditHost::apply_preview`] instead of into a
/// buffer. Far above any real bufnr (allocated incrementally from 1) so it never
/// collides, and below 2^53 so it round-trips exactly through the wasm FFI's `f64`
/// buffer id. See `docs/plans/2026-06-24-remote-web-pickers.md`.
pub(crate) const PREVIEW_FETCH_BUF: bemtvi_core::BufferId = bemtvi_core::BufferId(1 << 48);

/// Decode fetched preview bytes to lines, lossily (a binary file previews as best-effort
/// text), capped at [`MAX_PREVIEW_BYTES`] so a huge file can't stall the frame.
pub(crate) fn bytes_to_preview_lines(bytes: &[u8]) -> Vec<String> {
    let capped = &bytes[..bytes.len().min(MAX_PREVIEW_BYTES as usize)];
    String::from_utf8_lossy(capped)
        .lines()
        .map(str::to_string)
        .collect()
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
    highlights: HashMap<usize, Vec<bemtvi_core::Span>>,
    /// 0-based file lines a full-line-background capture (`@markup.raw.block` — a
    /// fenced code block) touches. Projected as the preview's `line_bg` layer so the
    /// block background is painted *under* the text rather than left in the per-cell
    /// spans, where the winner-takes-cell merge (and a `>lua` block's injected token
    /// spans) would overwrite it on every non-blank cell.
    block_bg_lines: std::collections::HashSet<usize>,
    /// The treesitter language the preview was highlighted as (`"rust"`, `"vimdoc"`,
    /// …), or empty when the path has no known grammar. Surfaced to clients as the
    /// preview's `filetype`, mirroring a window's — and the seam that lets a help
    /// `doc/*.txt` resolve to `vimdoc` even though its extension alone wouldn't.
    lang: String,
}

/// Cap on the bytes pulled into a single preview read / fetch — a guard against a huge
/// file stalling the frame, not a UI limit (the pane only shows a window anyway).
const MAX_PREVIEW_BYTES: u64 = 2 * 1024 * 1024;

/// Tab width the preview pane expands tabs at. The pane previews an *unopened* file,
/// so there's no buffer `'tabstop'` to honour; 8 is vim's global default (and what
/// help `doc/*.txt` — the common tab-indented preview — declares via `ts=8`).
const PREVIEW_TABSTOP: usize = 8;

/// Expand a preview line's tabs to spaces at [`PREVIEW_TABSTOP`], counting each
/// non-tab char as one display column. The preview renders char-by-char (each cell
/// one column — wide chars included, a pre-existing limitation), so column tracking
/// here is char-based to match; a raw `\t` would otherwise paint as a zero-width
/// control and collapse tab indentation. Borrows the (common) tab-free line untouched.
fn expand_preview_tabs(line: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !line.contains('\t') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len() + PREVIEW_TABSTOP);
    let mut col = 0;
    for ch in line.chars() {
        if ch == '\t' {
            let n = PREVIEW_TABSTOP - (col % PREVIEW_TABSTOP);
            for _ in 0..n {
                out.push(' ');
            }
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    Cow::Owned(out)
}

/// Char column of original byte offset `byte` in the [`expand_preview_tabs`]
/// rendering of `line` — the mapping that rebases the tree-sitter spans (raw byte
/// offsets) onto the expanded, char-rendered preview line. `byte` past end-of-line
/// clamps to the expanded width.
fn expanded_col_at(line: &str, byte: usize) -> usize {
    let mut col = 0;
    for (i, ch) in line.char_indices() {
        if i >= byte {
            break;
        }
        if ch == '\t' {
            col += PREVIEW_TABSTOP - (col % PREVIEW_TABSTOP);
        } else {
            col += 1;
        }
    }
    col
}

/// Read a file's lines for the read-only preview pane through the editor's host FS,
/// capped at [`MAX_PREVIEW_BYTES`]. Returns `(lines, ok)`; `ok = false` (with a
/// single visible placeholder line) when the FS is off-tick (daemon/wasm — preview
/// rides the async seam later) or the read fails. Lossy-decodes non-UTF-8 so a
/// binary file previews as best-effort text rather than erroring.
fn read_preview_file(editor: &bemtvi_core::Editor, path: &std::path::Path) -> (Vec<String>, bool) {
    use std::io::Read as _;
    if editor.host_fs_offtick() {
        return (vec![format!("{}: loading…", path.display())], false);
    }
    // A directory can't be read as a file — list its entries instead of showing the
    // error (the file picker previews a focused directory this way, e.g. `:e src/<Tab>`
    // highlighting a sub-directory). On Linux a directory OPENS fine and only fails at
    // READ time (`EISDIR`), so both the open and the read errors fall back to the
    // listing; a genuine non-directory error (permission, missing) still surfaces.
    let reader = match editor.host_fs().open_read(path) {
        Ok(r) => r,
        Err(e) => return read_preview_dir(editor, path, &e),
    };
    let mut buf = Vec::new();
    if let Err(e) = reader.take(MAX_PREVIEW_BYTES).read_to_end(&mut buf) {
        return read_preview_dir(editor, path, &e);
    }
    let text = String::from_utf8_lossy(&buf);
    (text.lines().map(|l| l.to_string()).collect(), true)
}

/// The preview lines for a *directory* target: its entries, directories first (each
/// with a trailing `/`), then files, both alphabetical. Falls back to the original
/// open error `open_err` when `path` is not a readable directory either (so a real
/// permission/missing error still shows). Returns `ok = true` for a listing so the
/// pane renders it as content (an empty directory shows a hint, still `ok`).
fn read_preview_dir(
    editor: &bemtvi_core::Editor,
    path: &std::path::Path,
    open_err: &std::io::Error,
) -> (Vec<String>, bool) {
    let mut entries = match editor.host_fs().read_dir(path) {
        Ok(entries) => entries,
        Err(_) => return (vec![format!("{}: {open_err}", path.display())], false),
    };
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    if entries.is_empty() {
        return (vec!["(empty directory)".to_string()], true);
    }
    let lines = entries
        .into_iter()
        .map(|e| {
            if e.is_dir {
                format!("{}/", e.name)
            } else {
                e.name
            }
        })
        .collect();
    (lines, true)
}

/// Map one preview line's cached tree-sitter `spans` (byte offsets within the line)
/// to the redraw highlight shape — `[start_char, end_char, group, style_id]` per
/// span, in **char** columns (the preview renders char-by-char, no tab expansion).
/// `style_id` interns the resolved [`Style`] into the frame palette, or `Nil` when
/// the capture has no colorscheme mapping (the client falls back to its own theme).
/// `None`/blank ⇒ an empty array (a plain row).
fn preview_line_spans(
    text: &str,
    spans: Option<&Vec<bemtvi_core::Span>>,
    highlights: &bemtvi_core::Highlights,
    styles: &mut StyleTable,
    hscroll: usize,
) -> Value {
    let Some(spans) = spans else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        spans
            .iter()
            .filter_map(|s| {
                // Byte → char column within the tab-expanded line (matching the
                // `expand_preview_tabs` rendering the client paints); skip a span that
                // doesn't land on char boundaries (defensive — engine spans always should).
                text.get(..s.start_byte)?;
                text.get(..s.end_byte)?;
                let start = expanded_col_at(text, s.start_byte);
                let end = expanded_col_at(text, s.end_byte);
                // Rebase into the horizontally-scrolled window: a span fully left of
                // the first visible column is dropped, the rest shift left by `hscroll`
                // (the client renders the sliced line from column 0).
                if end <= hscroll {
                    return None;
                }
                let start = start.saturating_sub(hscroll);
                let end = end - hscroll;
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
fn multi_spans_value(rows: &[&[(usize, usize)]]) -> Value {
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
    let mut color = |key: &str, c: Option<bemtvi_core::Rgb>| {
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
