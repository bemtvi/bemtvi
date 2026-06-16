//! The `nvim_*` / `nxvim_*` RPC surface: the request/notification handler and
//! the method dispatch table that defines the API clients call.

use crate::redraw::{lines_value, style_value};
use crate::EditHost;
use nxvim_core::{
    BorderStyle, BufferId, FloatAnchor, FloatConfig, FloatRelative, MouseEvent, TabId,
    WindowConfigSpec, WindowId,
};
use nxvim_rpc::Incoming;
use rmpv::Value;

impl EditHost {
    /// The current time in milliseconds for stamping a mouse event. Reads the
    /// injected fake clock ([`ServerInit::mouse_clock`](crate::ServerInit)) when a
    /// test supplies one — so `'mousetime'`-based multi-click detection is driven
    /// deterministically — otherwise the real monotonic clock since startup.
    fn mouse_stamp_ms(&self) -> u64 {
        match &self.mouse_clock {
            Some(c) => c.load(std::sync::atomic::Ordering::SeqCst),
            None => self.start.elapsed().as_millis() as u64,
        }
    }

    pub(crate) async fn handle(&mut self, message: Incoming) {
        // Stamp the editor's monotonic clock once per message: undo-node commits
        // during this message read it, and `vim.fn.localtime()` mirrors it.
        let now = self.start.elapsed().as_secs() as i64;
        self.editor.set_now_mono(now);
        let _ = self.lua.set_mono_secs(now);
        // Millisecond clock for sub-second timing (the terminal triple-`<Esc>` chord),
        // from the same source as the mouse multi-click clock so a test's fake clock
        // drives both deterministically.
        self.editor.set_now_ms(self.mouse_stamp_ms());
        match message {
            Incoming::Request { id, method, params } => {
                match self.dispatch(&method, &params) {
                    Ok(value) => self.fx.respond(id, Ok(value)),
                    Err(err) => self.fx.respond(id, Err(Value::from(err))),
                }
                // Typeahead a dispatched method queued via `nvim_feedkeys` (e.g. an
                // `nvim_exec_lua` / `nvim_command` chunk that fed keys) is processed
                // before the repaint, so the frame reflects the fed keys' effects.
                // `nvim_input` already drained its own, so this is a no-op there.
                self.drain_feedkeys();
                self.redraw();
            }
            Incoming::Notification { method, params } => {
                let _ = self.dispatch(&method, &params);
                self.drain_feedkeys();
                self.redraw();
            }
        }

        // A `:sleep` parks the editor for the requested span. Awaiting (not
        // blocking) keeps the RPC reader/writer tasks alive, so input typed
        // during the sleep is buffered and applied once we wake.
        if let Some(ms) = self.editor.take_sleep() {
            tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
        }
    }

    /// Dispatch an API method. This is the (small, growing) `nvim_*` surface.
    pub(crate) fn dispatch(&mut self, method: &str, params: &[Value]) -> Result<Value, String> {
        match method {
            "nvim_ui_attach" => {
                let w = uint(params.first(), 80);
                let h = uint(params.get(1), 24);
                self.ui = Some((w, h));
                self.editor.resize(w, h);
                // The resize assigns the window its first rect, so a `nx.decor`
                // provider's viewport is only now known. Drive `run_pending` to
                // dispatch it (and any other off-tick work the size change queued)
                // before `handle` paints the first frame — otherwise the marks
                // wouldn't appear until the first keystroke.
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_ui_try_resize" => {
                let w = uint(params.first(), 80);
                let h = uint(params.get(1), 24);
                self.ui = Some((w, h));
                self.editor.resize(w, h);
                // A resize moves every window's visible range; redispatch decor
                // providers (and drain any queued work) before the repaint.
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_input" => {
                let keys = text(params.first());
                self.input(&keys);
                Ok(Value::from(keys.len() as u64))
            }
            // `nvim_input_mouse(button, action, modifier, grid, row, col)`: a mouse
            // gesture at a global screen cell. nxvim is single-grid, so `grid`
            // (param 3) is ignored and the editor hit-tests the cell itself. A
            // malformed button/action/modifier is a loud error at the boundary.
            "nvim_input_mouse" => {
                let button = text(params.first());
                let action = text(params.get(1));
                let modifier = text(params.get(2));
                let row = uint(params.get(4), 0);
                let col = uint(params.get(5), 0);
                let mut ev = MouseEvent::parse(&button, &action, &modifier, row, col)?;
                // Stamp the receive time from the server's clock; the editor's
                // multi-click detection compares these deltas against `'mousetime'`.
                ev.stamp_ms = self.mouse_stamp_ms();
                self.editor.mouse(ev);
                // A status-line click on a `%@…%X` region fires its Lua handler (and
                // settles the handler's effects); a no-op for every other gesture.
                self.dispatch_statusline_clicks();
                Ok(Value::Nil)
            }
            "nxvim_input_flush" => {
                // The TUI's synthetic `timeoutlen` idle flush (design D4): resolve a
                // trailing live-prefix withheld in the matcher without waiting for
                // the next keystroke. A no-op when nothing is pending.
                self.input_flush();
                Ok(Value::Nil)
            }
            "nvim_command" => {
                let cmd = text(params.first());
                self.run_command(&cmd);
                Ok(Value::Nil)
            }
            // `nvim_exec_lua(code[, args])`: evaluate a Lua chunk and return its
            // value over RPC — the entry point for synchronous getters like
            // `vim.diagnostic.get`. Effects the chunk queued (LSP ops, panel,
            // commands) drain afterward, exactly like a `:lua` chunk. `args` is
            // accepted for call-compatibility but not yet threaded into the chunk.
            "nvim_exec_lua" => {
                let code = text(params.first());
                // Refresh the buffer mirror before the eval: this is the one
                // synchronous-getter entry that reads buffer state *before* its
                // trailing `run_pending`, so a `nvim_buf_get_lines` in the chunk
                // must see fresh lines (Phase 6).
                self.push_buf_mirror();
                let value = match self.lua.eval_to_value(&code) {
                    Ok(value) => value,
                    Err(e) => {
                        self.editor.echo(format!("E5108: Error executing lua: {e}"));
                        Value::Nil
                    }
                };
                self.apply_lua_effects();
                self.run_pending();
                Ok(value)
            }
            "nvim_get_mode" => Ok(Value::Map(vec![(
                Value::from("mode"),
                Value::from(self.editor.mode.short_code()),
            )])),
            // ----- windows --------------------------------------------------
            "nvim_list_wins" => Ok(Value::Array(
                // Spans every tabpage, matching neovim; the current tab's windows
                // come first (tab order, then in-tab layout order).
                self.editor
                    .all_window_ids()
                    .into_iter()
                    .map(|id| Value::from(id.0))
                    .collect(),
            )),
            "nvim_get_current_win" => Ok(Value::from(self.editor.current_window_id().0)),
            "nvim_set_current_win" => {
                let id = WindowId(uint(params.first(), 0) as u64);
                self.editor.set_current_window(id);
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_win_get_buf" => {
                let win = self.resolve_win(params.first());
                Ok(match self.editor.window_buffer(win) {
                    Some(b) => Value::from(b.0),
                    None => Value::from(0u64),
                })
            }
            "nvim_win_set_buf" => {
                let win = self.resolve_win(params.first());
                let buf = BufferId(uint(params.get(1), 0) as u64);
                self.editor.set_window_buffer(win, buf);
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_win_get_cursor" => {
                let win = self.resolve_win(params.first());
                let (line, col) = self.editor.window_cursor(win).unwrap_or((0, 0));
                // (1-based line, 0-based column) like neovim.
                Ok(Value::Array(vec![
                    Value::from((line + 1) as u64),
                    Value::from(col as u64),
                ]))
            }
            "nvim_win_set_cursor" => {
                let win = self.resolve_win(params.first());
                // [row (1-based), col (0-based)].
                let pos = params.get(1).and_then(Value::as_array);
                let row = pos
                    .and_then(|a| a.first())
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as usize;
                let col = pos
                    .and_then(|a| a.get(1))
                    .and_then(Value::as_u64)
                    .unwrap_or(0) as usize;
                self.editor.set_window_cursor(win, row - 1, col);
                Ok(Value::Nil)
            }
            "nvim_win_get_width" => {
                let win = self.resolve_win(params.first());
                let w = self.editor.window_rect(win).map(|r| r.2).unwrap_or(0);
                Ok(Value::from(w as u64))
            }
            "nvim_win_get_height" => {
                let win = self.resolve_win(params.first());
                // Text rows = rect height minus the status line.
                let h = self
                    .editor
                    .window_rect(win)
                    .map(|r| r.3.saturating_sub(1))
                    .unwrap_or(0);
                Ok(Value::from(h as u64))
            }
            "nvim_win_set_width" => {
                let win = self.resolve_win(params.first());
                self.editor.set_window_width(win, uint(params.get(1), 0));
                Ok(Value::Nil)
            }
            "nvim_win_set_height" => {
                let win = self.resolve_win(params.first());
                self.editor.set_window_height(win, uint(params.get(1), 0));
                Ok(Value::Nil)
            }
            "nvim_win_close" => {
                let win = self.resolve_win(params.first());
                let force = flag(params.get(1), false);
                self.editor.close_window_by_id(win, force);
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_open_win" => {
                // (buffer, enter, config). A non-empty `config.relative` opens a
                // float (positioned absolutely on top of the tiled layout); else
                // it is the split form — `config.vertical` (or `config.split ==
                // "left"/"right"`) makes a vsplit. The new window is created bound
                // to `buffer` and focused; `enter == false` refocuses the previous
                // window.
                let buf = BufferId(uint(params.first(), 0) as u64);
                let enter = flag(params.get(1), true);
                let config = params.get(2);
                let relative = config
                    .and_then(map_get("relative"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let new = if relative.is_empty() {
                    let vertical = config
                        .and_then(map_get("vertical"))
                        .and_then(Value::as_bool)
                        .unwrap_or_else(|| {
                            matches!(
                                config.and_then(map_get("split")).and_then(Value::as_str),
                                Some("left" | "right")
                            )
                        });
                    let prev = self.editor.current_window_id();
                    let new = self.editor.open_split_window(buf, vertical);
                    if !enter {
                        self.editor.set_current_window(prev);
                    }
                    new
                } else {
                    // Float form. `buffer == 0` means the current buffer.
                    let buf = if buf.0 == 0 {
                        self.editor.current_buffer_id()
                    } else {
                        buf
                    };
                    // Reject an unknown buffer handle loudly (neovim's "Invalid
                    // buffer id"). A float binds the window to `buf` directly, so an
                    // unvalidated handle would make a later `buffers.get` panic and
                    // crash the server — a single-request DoS from the client.
                    if !self.editor.buffer_is_valid(buf) {
                        return Err(format!("nvim_open_win: Invalid buffer id: {}", buf.0));
                    }
                    let cfg = self.parse_float_config(config)?;
                    self.editor.open_float_window(buf, cfg, enter)
                };
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::from(new.0))
            }
            "nvim_win_get_config" => {
                let win = self.resolve_win(params.first());
                Ok(self.win_config_value(win))
            }
            "nvim_win_set_config" => {
                // (win, config): move/resize/restyle a float, or convert between a
                // float and a tiled split. `config` is a *partial* — absent keys
                // are unchanged (neovim's merge); `relative = ""` re-tiles a float.
                let win = self.resolve_win(params.first());
                let spec = self.parse_window_config(params.get(1))?;
                self.editor.set_window_config(win, spec);
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_win_get_position" => {
                // [row, col] of the window's top-left in windows-area cells.
                let win = self.resolve_win(params.first());
                let (x, y) = self
                    .editor
                    .window_rect(win)
                    .map(|r| (r.0, r.1))
                    .unwrap_or((0, 0));
                Ok(Value::Array(vec![
                    Value::from(y as u64),
                    Value::from(x as u64),
                ]))
            }
            // ----- tab pages (read-only) -----------------------------------
            "nvim_list_tabpages" => Ok(Value::Array(
                self.editor
                    .tab_ids()
                    .into_iter()
                    .map(|id| Value::from(id.0))
                    .collect(),
            )),
            "nvim_get_current_tabpage" => Ok(Value::from(self.editor.current_tab_id().0)),
            "nvim_set_current_tabpage" => {
                let tab = self.resolve_tabpage(params.first());
                self.editor.set_current_tabpage(tab);
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_tabpage_is_valid" => {
                let tab = self.resolve_tabpage(params.first());
                Ok(Value::from(self.editor.tab_is_valid(tab)))
            }
            "nvim_tabpage_get_number" => {
                let tab = self.resolve_tabpage(params.first());
                Ok(match self.editor.tab_number(tab) {
                    Some(n) => Value::from(n as u64),
                    None => Value::from(0u64),
                })
            }
            "nvim_tabpage_list_wins" => {
                let tab = self.resolve_tabpage(params.first());
                Ok(Value::Array(
                    self.editor
                        .tab_window_ids(tab)
                        .unwrap_or_default()
                        .into_iter()
                        .map(|id| Value::from(id.0))
                        .collect(),
                ))
            }
            "nvim_tabpage_get_win" => {
                let tab = self.resolve_tabpage(params.first());
                Ok(match self.editor.tab_current_window(tab) {
                    Some(win) => Value::from(win.0),
                    None => Value::from(0u64),
                })
            }
            "nvim_buf_get_lines" => Ok(self.get_lines(params)),
            "nvim_buf_line_count" => {
                // params[0] is the buffer handle: 0 = current, else a specific buffer.
                let handle = params.first().and_then(Value::as_u64).unwrap_or(0);
                let id = if handle == 0 {
                    self.editor.current_buffer_id()
                } else {
                    BufferId(handle)
                };
                Ok(Value::from(
                    self.editor.line_count_of(id).unwrap_or(0) as u64
                ))
            }
            "nvim_list_bufs" => Ok(Value::Array(
                self.editor
                    .buffer_ids()
                    .into_iter()
                    .map(|id| Value::from(id.0))
                    .collect(),
            )),
            "nvim_get_current_buf" => Ok(Value::from(self.editor.current_buffer_id().0)),
            "nvim_set_current_buf" => {
                let id = BufferId(uint(params.first(), 0) as u64);
                self.editor.set_current_buffer(id);
                self.emit_lifecycle_events();
                self.run_pending();
                Ok(Value::Nil)
            }
            "nvim_create_buf" => Ok(Value::from(self.editor.create_buffer().0)),
            "nvim_buf_get_name" => {
                // (buffer): the buffer's file name, "" if unnamed; 0 = current. A
                // terminal-job buffer has no path, so it reports its window title (the
                // child's OSC title) as its name — like neovim's `term://…` name.
                let handle = uint(params.first(), 0) as u64;
                let id = if handle == 0 {
                    self.editor.current_buffer_id()
                } else {
                    BufferId(handle)
                };
                let name = self
                    .editor
                    .terminal_title(id)
                    .or_else(|| self.editor.buffer_name(id))
                    .unwrap_or_default();
                Ok(Value::from(name))
            }
            "nvim_get_hl" => {
                // (ns, { name = "<group>" }) -> the group resolved through its
                // link chain to concrete colors/attrs, or `{}` if unstyled. A
                // non-zero `ns` reads that namespace's own table (a group not
                // defined there is `{}`, not the global fallback).
                let ns = params.first().and_then(Value::as_u64).unwrap_or(0) as u32;
                let name = params
                    .get(1)
                    .and_then(map_get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Ok(match self.editor.highlights.resolve_ns(ns, name) {
                    Some(style) => style_value(&style),
                    None => Value::Map(vec![]),
                })
            }
            "nxvim_resolve_capture" => {
                // Debug hook: resolve a treesitter capture name through the
                // `@`-group fallback chain to a concrete style (the same
                // resolution the redraw path uses). `Nil` when nothing matches.
                let capture = text(params.first());
                Ok(match self.editor.highlights.resolve_capture(&capture) {
                    Some(style) => style_value(&style),
                    None => Value::Nil,
                })
            }
            "nvim_get_api_info" => {
                // [channel_id, metadata]; metadata kept minimal for now.
                Ok(Value::Array(vec![Value::from(1u64), Value::Map(vec![])]))
            }
            // ----- the bottom message panel (nxvim-native) -----------------
            "nxvim_panel_open" => {
                // (title, lines, want_select?, cursor?): open (or replace) and
                // focus the panel. `want_select` (default false) makes `<CR>`
                // emit an `nxvim_panel_select` notification for the client to act
                // on. `cursor` (default 0) is the initially selected line
                // (0-based); the panel scrolls to keep it visible.
                let title = text(params.first());
                let lines = str_array(params.get(1));
                let want_select = flag(params.get(2), false);
                let cursor = params.get(3).and_then(Value::as_u64).unwrap_or(0) as usize;
                self.editor.open_panel(title, lines, want_select, cursor);
                Ok(Value::Nil)
            }
            "nxvim_panel_set_lines" => {
                // (lines): replace the open panel's content (no-op if none open).
                let lines = str_array(params.first());
                self.editor.set_panel_lines(lines);
                Ok(Value::Nil)
            }
            "nxvim_panel_set_select" => {
                // (bool): toggle `<CR>` select events on the open panel.
                let want = flag(params.first(), false);
                self.editor.set_panel_on_select(want);
                Ok(Value::Nil)
            }
            "nxvim_panel_set_cursor" => {
                // (line): move the open panel's selection (0-based) and scroll it
                // into view (no-op if none open).
                let line = params.first().and_then(Value::as_u64).unwrap_or(0) as usize;
                self.editor.set_panel_cursor(line);
                Ok(Value::Nil)
            }
            "nxvim_panel_close" => {
                self.editor.close_panel();
                Ok(Value::Nil)
            }
            "nxvim_panel_is_open" => Ok(Value::from(self.editor.panel_is_open())),
            "nxvim_panel_click" => {
                // (row): move the panel selection to the logical entry at visible
                // display `row` — the mouse-click counterpart to j/k. The client
                // sends <CR> itself to activate an already-selected row.
                let row = params.first().and_then(Value::as_u64).unwrap_or(0) as usize;
                self.editor.set_panel_cursor_by_row(row);
                Ok(Value::Nil)
            }
            // ----- the unified completion popup (mouse routing) -------------
            "nxvim_complete_select" => {
                // (index): highlight a visible completion item by absolute index
                // (clamped) — the mouse click / wheel counterpart to <C-n>/<C-p>.
                let idx = params.first().and_then(Value::as_u64).unwrap_or(0) as usize;
                self.editor.complete_select_index(idx);
                Ok(Value::Nil)
            }
            "nxvim_complete_accept" => {
                // Accept the highlighted item — a click on the selected row, the
                // <C-y> equivalent. A delegated (`lsp`) accept is applied by
                // `run_pending`'s settle, exactly as the keyboard accept path is.
                self.editor.complete_accept();
                self.run_pending();
                Ok(Value::Nil)
            }
            other => Err(format!("Unknown method: {other}")),
        }
    }

    /// Resolve a window-handle RPC argument to a [`WindowId`], mapping neovim's
    /// `0` (and a missing argument) to the current window.
    fn resolve_win(&self, v: Option<&Value>) -> WindowId {
        match v.and_then(Value::as_u64) {
            Some(0) | None => self.editor.current_window_id(),
            Some(n) => WindowId(n),
        }
    }

    /// Resolve a tabpage handle the way neovim does: `0` (or absent) is the
    /// current tab, anything else is that handle verbatim.
    fn resolve_tabpage(&self, v: Option<&Value>) -> TabId {
        match v.and_then(Value::as_u64) {
            Some(0) | None => self.editor.current_tab_id(),
            Some(n) => TabId(n),
        }
    }

    /// Parse a `nvim_open_win` float `config` map into a [`FloatConfig`]. The
    /// caller has already established `relative` is non-empty (the float form).
    /// Per the no-silent-stub rule, a `relative`/`anchor`/`border` value nxvim
    /// cannot position yet is an error naming what is unsupported, never a silent
    /// fallback. `width`/`height` are required and must be positive (as neovim).
    fn parse_float_config(&self, config: Option<&Value>) -> Result<FloatConfig, String> {
        let get = |k: &'static str| config.and_then(map_get(k));
        let relative = match get("relative").and_then(Value::as_str).unwrap_or("") {
            "editor" => FloatRelative::Editor,
            "cursor" => FloatRelative::Cursor,
            "win" => {
                let w = get("win").and_then(Value::as_u64).unwrap_or(0);
                let id = if w == 0 {
                    self.editor.current_window_id()
                } else {
                    WindowId(w)
                };
                FloatRelative::Win(id)
            }
            other => {
                return Err(format!(
                    "nvim_open_win: 'relative' value '{other}' is not supported yet"
                ))
            }
        };
        let anchor_kw = get("anchor").and_then(Value::as_str).unwrap_or("NW");
        let anchor = FloatAnchor::from_keyword(anchor_kw)
            .ok_or_else(|| format!("nvim_open_win: invalid 'anchor': '{anchor_kw}'"))?;
        let width = uint(get("width"), 0);
        let height = uint(get("height"), 0);
        if width == 0 || height == 0 {
            return Err("nvim_open_win: 'width' and 'height' must be positive".to_string());
        }
        let border = match get("border") {
            None => BorderStyle::None,
            // A non-string `border` (neovim's per-edge glyph array) is not rendered
            // yet; treat it as the default `none` rather than erroring, as before.
            Some(v) => {
                let kw = v.as_str().unwrap_or("none");
                BorderStyle::from_keyword(kw).ok_or_else(|| {
                    format!("nvim_open_win: 'border' style '{kw}' is not supported yet")
                })?
            }
        };
        Ok(FloatConfig {
            relative,
            anchor,
            row: get("row").and_then(as_int).unwrap_or(0),
            col: get("col").and_then(as_int).unwrap_or(0),
            width,
            height,
            zindex: get("zindex")
                .and_then(Value::as_u64)
                .map(|z| z as u32)
                .unwrap_or(50),
            focusable: flag(get("focusable"), true),
            border,
            title: parse_title(get("title")),
        })
    }

    /// Parse a `nvim_win_set_config` `config` map into a partial
    /// [`WindowConfigSpec`]: only the keys present become `Some`, so the core's
    /// merge leaves the rest unchanged (neovim's behavior). `relative = ""` is the
    /// re-tile form (`make_tiled`). Enumerated values (`relative`/`anchor`/
    /// `border`) are validated loudly, like [`Self::parse_float_config`].
    fn parse_window_config(&self, config: Option<&Value>) -> Result<WindowConfigSpec, String> {
        let get = |k: &'static str| config.and_then(map_get(k));
        let mut spec = WindowConfigSpec::default();
        match get("relative").and_then(Value::as_str) {
            None => {}
            Some("") => spec.make_tiled = true,
            Some("editor") => spec.relative = Some(FloatRelative::Editor),
            Some("cursor") => spec.relative = Some(FloatRelative::Cursor),
            Some("win") => {
                let w = get("win").and_then(Value::as_u64).unwrap_or(0);
                let id = if w == 0 {
                    self.editor.current_window_id()
                } else {
                    WindowId(w)
                };
                spec.relative = Some(FloatRelative::Win(id));
            }
            Some(other) => {
                return Err(format!(
                    "nvim_win_set_config: 'relative' value '{other}' is not supported yet"
                ))
            }
        }
        if let Some(a) = get("anchor").and_then(Value::as_str) {
            spec.anchor = Some(
                FloatAnchor::from_keyword(a)
                    .ok_or_else(|| format!("nvim_win_set_config: invalid 'anchor': '{a}'"))?,
            );
        }
        if let Some(b) = get("border") {
            // As in `parse_float_config`, a non-string `border` falls back to `none`.
            let kw = b.as_str().unwrap_or("none");
            spec.border = Some(BorderStyle::from_keyword(kw).ok_or_else(|| {
                format!("nvim_win_set_config: 'border' style '{kw}' is not supported yet")
            })?);
        }
        spec.row = get("row").and_then(as_int);
        spec.col = get("col").and_then(as_int);
        spec.width = get("width").and_then(Value::as_u64).map(|n| n as usize);
        spec.height = get("height").and_then(Value::as_u64).map(|n| n as usize);
        spec.zindex = get("zindex").and_then(Value::as_u64).map(|z| z as u32);
        spec.focusable = get("focusable").and_then(Value::as_bool);
        // A present `title` key sets (or, with an empty value, clears) the title;
        // an absent one leaves it unchanged.
        spec.title = get("title").map(|t| parse_title(Some(t)));
        Ok(spec)
    }

    /// The `nvim_win_get_config` value for window `win`: neovim's config map for a
    /// float (`relative`/`anchor`/`row`/`col`/`width`/`height`/`zindex`/
    /// `focusable`/`border`), or `{ relative = "" }` for a tiled window.
    fn win_config_value(&self, win: WindowId) -> Value {
        let Some(cfg) = self.editor.window_float_config(win) else {
            return Value::Map(vec![(Value::from("relative"), Value::from(""))]);
        };
        let relative = match cfg.relative {
            FloatRelative::Editor => "editor",
            FloatRelative::Win(_) => "win",
            FloatRelative::Cursor => "cursor",
        };
        let mut entries = vec![
            (Value::from("relative"), Value::from(relative)),
            (Value::from("anchor"), Value::from(cfg.anchor.as_str())),
            (Value::from("row"), Value::from(cfg.row as i64)),
            (Value::from("col"), Value::from(cfg.col as i64)),
            (Value::from("width"), Value::from(cfg.width as u64)),
            (Value::from("height"), Value::from(cfg.height as u64)),
            (Value::from("zindex"), Value::from(cfg.zindex as u64)),
            (Value::from("focusable"), Value::from(cfg.focusable)),
            (Value::from("border"), Value::from(cfg.border.as_str())),
        ];
        if let FloatRelative::Win(id) = cfg.relative {
            entries.push((Value::from("win"), Value::from(id.0)));
        }
        if let Some(title) = &cfg.title {
            entries.push((Value::from("title"), Value::from(title.as_str())));
        }
        Value::Map(entries)
    }

    pub(crate) fn get_lines(&self, params: &[Value]) -> Value {
        // params[0] is the buffer handle: 0 = current, else a specific buffer.
        // An unknown handle yields an empty list rather than erroring.
        let handle = params.first().and_then(Value::as_u64).unwrap_or(0);
        let lines = if handle == 0 {
            self.editor.lines()
        } else {
            match self.editor.lines_of(BufferId(handle)) {
                Some(lines) => lines,
                None => return Value::Array(Vec::new()),
            }
        };
        let n = lines.len() as i64;
        let norm = |i: i64| -> i64 {
            if i < 0 {
                (n + i + 1).max(0)
            } else {
                i.min(n)
            }
        };
        let start = norm(params.get(1).and_then(Value::as_i64).unwrap_or(0));
        let end = norm(params.get(2).and_then(Value::as_i64).unwrap_or(-1));
        let (start, end) = (start as usize, end.max(start) as usize);
        lines_value(&lines[start..end.min(lines.len())])
    }
}

/// A closure that looks up `key` in a msgpack map value (for reading RPC opts
/// tables like `nvim_get_hl`'s `{ name = … }`).
fn map_get(key: &'static str) -> impl Fn(&Value) -> Option<&Value> {
    move |v| match v {
        Value::Map(entries) => entries
            .iter()
            .find(|(k, _)| k.as_str() == Some(key))
            .map(|(_, v)| v),
        _ => None,
    }
}

fn uint(v: Option<&Value>, default: usize) -> usize {
    v.and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(default)
}

/// Read a signed integer RPC value, accepting a float (neovim's `nvim_open_win`
/// `row`/`col` may be fractional — we truncate) as well as an integer.
fn as_int(v: &Value) -> Option<isize> {
    v.as_i64()
        .map(|n| n as isize)
        .or_else(|| v.as_f64().map(|f| f as isize))
}

fn text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}

fn flag(v: Option<&Value>, default: bool) -> bool {
    v.and_then(Value::as_bool).unwrap_or(default)
}

/// Parse a `nvim_open_win` `title`: either a plain string, or neovim's
/// `[[text, hl], …]` chunk list (we keep the text, drop the per-chunk highlight
/// — Phase 2 paints the title in the border style, not per-chunk). `None` for an
/// absent or empty title.
fn parse_title(v: Option<&Value>) -> Option<String> {
    let title = match v {
        Some(Value::String(s)) => s.as_str().unwrap_or("").to_string(),
        Some(Value::Array(chunks)) => chunks
            .iter()
            .filter_map(|chunk| match chunk {
                // A chunk is `[text]` or `[text, hl]`; the first element is text.
                Value::Array(parts) => parts.first().and_then(Value::as_str),
                Value::String(s) => s.as_str(),
                _ => None,
            })
            .collect(),
        _ => return None,
    };
    (!title.is_empty()).then_some(title)
}

/// Read an RPC array-of-strings argument (the panel methods' `lines`). Non-array
/// values and non-string elements are dropped, yielding an empty list.
fn str_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}
