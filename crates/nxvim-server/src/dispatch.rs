//! The `nvim_*` / `nxvim_*` RPC surface: the request/notification handler and
//! the method dispatch table that defines the API clients call.

use crate::redraw::{lines_value, style_value};
use crate::Server;
use nxvim_core::BufferId;
use nxvim_rpc::Incoming;
use rmpv::Value;

impl Server {
    pub(crate) async fn handle(&mut self, message: Incoming) {
        match message {
            Incoming::Request { id, method, params } => {
                match self.dispatch(&method, &params) {
                    Ok(value) => self.rpc.respond(id, Ok(value)),
                    Err(err) => self.rpc.respond(id, Err(Value::from(err))),
                }
                self.redraw();
            }
            Incoming::Notification { method, params } => {
                let _ = self.dispatch(&method, &params);
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
                Ok(Value::Nil)
            }
            "nvim_ui_try_resize" => {
                let w = uint(params.first(), 80);
                let h = uint(params.get(1), 24);
                self.ui = Some((w, h));
                self.editor.resize(w, h);
                Ok(Value::Nil)
            }
            "nvim_input" => {
                let keys = text(params.first());
                self.input(&keys);
                Ok(Value::from(keys.len() as u64))
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
                // `eval_to_value_pumped` so a `vim.fn.input` / `vim.fn.confirm` in
                // the chunk can park on the command line (returning Nil now; the
                // chunk resumes when the user answers) rather than erroring.
                let value = match self.lua.eval_to_value_pumped(&code) {
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
            "nvim_win_get_cursor" => Ok(Value::Array(vec![
                // (1-based line, 0-based column) like neovim.
                Value::from((self.editor.cursor.line + 1) as u64),
                Value::from(self.editor.cursor.col as u64),
            ])),
            "nvim_buf_get_lines" => Ok(self.get_lines(params)),
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
                // (buffer): the buffer's file name, "" if unnamed; 0 = current.
                let handle = uint(params.first(), 0) as u64;
                let id = if handle == 0 {
                    self.editor.current_buffer_id()
                } else {
                    BufferId(handle)
                };
                Ok(Value::from(self.editor.buffer_name(id).unwrap_or_default()))
            }
            "nvim_get_hl" => {
                // (ns, { name = "<group>" }) -> the group resolved through its
                // link chain to concrete colors/attrs, or `{}` if unstyled.
                let name = params
                    .get(1)
                    .and_then(map_get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                Ok(match self.editor.highlights.resolve(name) {
                    Some(style) => style_value(&style),
                    None => Value::Map(vec![]),
                })
            }
            "nxvim_resolve_capture" => {
                // Debug hook: resolve a treesitter capture name through the
                // `@`-group fallback chain to a concrete style (Phase 5 will use
                // this in the redraw path). `Nil` when nothing matches.
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
                let want_select = params.get(2).and_then(Value::as_bool).unwrap_or(false);
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
                let want = params.first().and_then(Value::as_bool).unwrap_or(false);
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
            // ----- the insert-mode completion popup (mouse routing) ---------
            "nxvim_complete_select" => {
                // (index): highlight a visible completion item by absolute index
                // (clamped) — the mouse click / wheel counterpart to <C-n>/<C-p>.
                let idx = params.first().and_then(Value::as_u64).unwrap_or(0) as usize;
                self.lsp_menu_select(idx);
                Ok(Value::Nil)
            }
            "nxvim_complete_accept" => {
                // Accept the highlighted item — a click on the selected row, the
                // <C-y> equivalent. `run_pending` drains the edit's effects, as the
                // keyboard accept path does via `input`'s trailing drain.
                self.lsp_menu_accept();
                self.run_pending();
                Ok(Value::Nil)
            }
            other => Err(format!("Unknown method: {other}")),
        }
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

fn text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
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
