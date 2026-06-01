//! The nxvim server: a headless editor process that owns the core model and
//! Lua runtime and exposes them over msgpack-RPC.
//!
//! This is the rust-native analogue of neovim's `main.c` + `event/` + `api/`.
//! It runs on a single thread with an async runtime: the RPC reader/writer are
//! independent tasks, while the server loop processes one message at a time
//! against the (non-`Send`) editor and Lua state. Clients (the TUI today, a
//! native GUI later) attach over the same RPC channel and are never blocked by
//! the server's bookkeeping.

mod syntax;

use nxvim_core::{parse_keys, unicode, Editor};
use nxvim_lua::LuaRuntime;
use nxvim_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use std::collections::HashMap;
use syntax::{SyntaxClient, SyntaxEvent};
use tokio::io::{AsyncRead, AsyncWrite};

/// Startup options for the server.
#[derive(Debug, Default, Clone)]
pub struct ServerInit {
    /// File to open in the initial buffer, if any.
    pub file: Option<String>,
}

/// The single buffer id nxvim uses today (multiple buffers are on the roadmap).
const BUFFER_ID: u64 = 0;

/// A cached highlight span in buffer coordinates: a byte range within a line.
#[derive(Clone)]
struct ByteSpan {
    start: usize,
    end: usize,
    group: String,
}

/// Per-buffer treesitter sync bookkeeping (one buffer for now).
#[derive(Default)]
struct SyntaxState {
    /// Detected filetype/language, `None` when the buffer has no known grammar.
    language: Option<&'static str>,
    /// Has the worker been sent the full text (`ts_open`) for the current content?
    opened: bool,
    /// `changedtick` of the last `ts_open`/`ts_edit` we sent.
    last_tick: u64,
    /// A request is in flight; coalesce further edits until its reply lands.
    pending: bool,
    /// Last viewport `[first, last)` we requested, to detect scroll-only changes.
    last_view: (usize, usize),
    /// Latest spans from the worker, keyed by absolute buffer line.
    spans: HashMap<usize, Vec<ByteSpan>>,
}

struct Server {
    editor: Editor,
    lua: LuaRuntime,
    rpc: Rpc,
    /// Attached UI dimensions `(width, height)`, once a client has attached.
    ui: Option<(usize, usize)>,
    syntax: SyntaxClient,
    syntax_state: SyntaxState,
}

/// Run the server over a connected stream until the client disconnects or the
/// editor quits.
pub async fn run<S>(stream: S, init: ServerInit) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (rpc, mut incoming) = connect(reader, writer);

    let editor = match init.file {
        Some(path) => Editor::open(path).unwrap_or_else(|_| Editor::new()),
        None => Editor::new(),
    };
    let lua = LuaRuntime::new().map_err(|e| anyhow::anyhow!("lua init failed: {e}"))?;
    let (syntax, mut syntax_events) = SyntaxClient::new();

    let mut server = Server {
        editor,
        lua,
        rpc,
        ui: None,
        syntax,
        syntax_state: SyntaxState::default(),
    };

    loop {
        tokio::select! {
            // Editor input / API calls from the UI client.
            message = incoming.recv() => {
                let Some(message) = message else { break };
                server.handle(message).await;
                if server.editor.should_quit {
                    server.rpc.notify("nxvim_exit", vec![]);
                    break;
                }
            }
            // Highlight spans / restarts from the syntax process. Selecting here
            // (rather than blocking on it) is what keeps the editor responsive
            // regardless of the worker's speed or health.
            Some(event) = syntax_events.recv() => {
                server.on_syntax_event(event);
            }
        }
    }
    Ok(())
}

impl Server {
    async fn handle(&mut self, message: Incoming) {
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
    fn dispatch(&mut self, method: &str, params: &[Value]) -> Result<Value, String> {
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
            "nvim_command" => {
                let cmd = text(params.first());
                self.run_command(&cmd);
                Ok(Value::Nil)
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
            "nvim_get_api_info" => {
                // [channel_id, metadata]; metadata kept minimal for now.
                Ok(Value::Array(vec![Value::from(1u64), Value::Map(vec![])]))
            }
            other => Err(format!("Unknown method: {other}")),
        }
    }

    fn input(&mut self, keys: &str) {
        for key in parse_keys(keys) {
            self.editor.input(key);
        }
        self.drain_lua();
    }

    fn run_command(&mut self, cmd: &str) {
        self.editor.command(cmd);
        self.drain_lua();
    }

    /// Run any Lua chunks the editor queued (via `:lua`), then apply their
    /// effects (queued ex-commands, captured output) back to the editor.
    fn drain_lua(&mut self) {
        loop {
            let chunks = std::mem::take(&mut self.editor.lua_queue);
            if chunks.is_empty() {
                break;
            }
            for chunk in chunks {
                if let Err(e) = self.lua.exec(&chunk) {
                    self.editor.message = format!("E5108: Error executing lua: {e}");
                }
            }
            for cmd in self.lua.take_commands() {
                self.editor.command(&cmd);
            }
            if let Some(last) = self.lua.take_output().last() {
                self.editor.message = last.clone();
            }
        }
    }

    fn get_lines(&self, params: &[Value]) -> Value {
        let lines = self.editor.lines();
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
        Value::Array(
            lines[start..end.min(lines.len())]
                .iter()
                .map(|l| Value::from(l.as_str()))
                .collect(),
        )
    }

    /// Push the current view to the client as a single `redraw` notification
    /// carrying an nxvim-native view map (no neovim grid protocol). The client
    /// renders the regions with its own widgets.
    fn redraw(&mut self) {
        let (w, h) = match self.ui {
            Some(dims) => dims,
            None => return,
        };
        let view = self.editor.view(w, h);

        // Drive the syntax process from the freshly-settled viewport, then paint
        // with whatever spans it has returned so far (this never blocks on it).
        self.sync_syntax(h);
        let highlights = self.highlights_for(&view.numbers);

        let lines = Value::Array(view.lines.iter().map(|l| Value::from(l.as_str())).collect());
        let selection = spans_value(&view.selection);
        let numbers = numbers_value(&view.numbers);
        let scroll = match &view.scroll {
            Some(s) => {
                let scroll_lines =
                    Value::Array(s.lines.iter().map(|l| Value::from(l.as_str())).collect());
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
                    (Value::from("lines"), scroll_lines),
                    (Value::from("selection"), spans_value(&s.selection)),
                    (Value::from("numbers"), numbers_value(&s.numbers)),
                    (Value::from("highlights"), self.highlights_for(&s.numbers)),
                ])
            }
            None => Value::Nil,
        };
        let map = vec![
            (Value::from("lines"), lines),
            (
                Value::from("cursor_row"),
                Value::from(view.cursor_row as u64),
            ),
            (
                Value::from("cursor_col"),
                Value::from(view.cursor_col as u64),
            ),
            (
                Value::from("cursor_screen_col"),
                Value::from(view.cursor_screen_col as u64),
            ),
            (
                Value::from("mode_label"),
                Value::from(view.mode_label.as_str()),
            ),
            (Value::from("command_mode"), Value::from(view.command_mode)),
            (Value::from("cmdline"), Value::from(view.cmdline.as_str())),
            (Value::from("message"), Value::from(view.message.as_str())),
            (
                Value::from("file_name"),
                Value::from(view.file_name.as_str()),
            ),
            (Value::from("modified"), Value::from(view.modified)),
            (
                Value::from("cursor_line"),
                Value::from(view.cursor_line as u64),
            ),
            (Value::from("selection"), selection),
            (Value::from("scroll"), scroll),
            (Value::from("numbers"), numbers),
            (Value::from("number"), Value::from(view.number)),
            (
                Value::from("relativenumber"),
                Value::from(view.relativenumber),
            ),
            (
                Value::from("number_width"),
                Value::from(view.number_width as u64),
            ),
            (Value::from("highlights"), highlights),
        ];

        self.rpc.notify("redraw", vec![Value::Map(map)]);
    }

    // ----- treesitter syntax integration ------------------------------------

    /// Handle a message from the syntax process. A restart forces a re-`open`;
    /// `ts_highlights` updates the span cache and repaints.
    fn on_syntax_event(&mut self, event: SyntaxEvent) {
        match event {
            SyntaxEvent::Restarted => {
                // Fresh worker, empty state: re-sync from full text next redraw.
                self.syntax_state.opened = false;
                self.syntax_state.pending = false;
                self.syntax_state.spans.clear();
                self.redraw();
            }
            // `ts_highlights` updates the cache; any other notification (e.g.
            // `ts_error` — a grammar that wouldn't load/parse) is ignored, so the
            // buffer simply stays un-highlighted and editing is unaffected.
            SyntaxEvent::Notification { method, params } if method == "ts_highlights" => {
                self.store_spans(&params);
                self.redraw();
            }
            SyntaxEvent::Notification { .. } => {}
        }
    }

    /// Decide what (if anything) to send the syntax process this frame: an
    /// `open` (first sync / after a resync / language change), an `edit` (text
    /// deltas), or a `view` (scroll only). Coalesces while a request is pending.
    fn sync_syntax(&mut self, height: usize) {
        let language = filetype_of(self.editor.buffer.path.as_deref());
        // Language gone (no path / unknown extension): nothing to highlight.
        let Some(language) = language else {
            self.syntax_state.language = None;
            return;
        };
        self.syntax.ensure_started();

        let line_count = self.editor.buffer.line_count();
        // Highlight a one-screen overscan above and below the viewport, so the
        // lines a scroll reveals are already cached and colored — no white flash
        // during the smooth-scroll animation (whose band spans up to ~2 screens).
        let first = self.editor.top.saturating_sub(height).min(line_count);
        let last = (self.editor.top + 2 * height).min(line_count);
        let tick = self.editor.buffer.changedtick;
        let language_changed = self.syntax_state.language != Some(language);
        self.syntax_state.language = Some(language);

        // A fresh language or un-opened buffer needs a full open.
        let needs_open = language_changed || !self.syntax_state.opened;

        if needs_open {
            let batch = self.editor.buffer.take_edits();
            let _ = batch; // superseded by the full-text open
            let text = self.editor.buffer.text.to_string();
            self.syntax
                .open(BUFFER_ID, tick, language, &text, first, last);
            self.syntax_state.opened = true;
            self.syntax_state.last_tick = tick;
            self.syntax_state.last_view = (first, last);
            self.syntax_state.pending = true;
            return;
        }

        if tick != self.syntax_state.last_tick {
            // Text changed. Wait if a request is already in flight (the deltas
            // stay journaled and flush when its reply arrives).
            if self.syntax_state.pending {
                return;
            }
            let batch = self.editor.buffer.take_edits();
            if batch.resync {
                let text = self.editor.buffer.text.to_string();
                self.syntax
                    .open(BUFFER_ID, tick, language, &text, first, last);
            } else {
                self.syntax
                    .edit(BUFFER_ID, tick, edits_value(&batch.edits), first, last);
            }
            self.syntax_state.last_tick = tick;
            self.syntax_state.last_view = (first, last);
            self.syntax_state.pending = true;
            return;
        }

        // Text unchanged: re-query only if the viewport scrolled.
        if (first, last) != self.syntax_state.last_view && !self.syntax_state.pending {
            self.syntax.view(BUFFER_ID, first, last);
            self.syntax_state.last_view = (first, last);
            self.syntax_state.pending = true;
        }
    }

    /// Replace the span cache from a `ts_highlights` reply.
    fn store_spans(&mut self, params: &[Value]) {
        self.syntax_state.pending = false;
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        let spans = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("spans"))
            .and_then(|(_, v)| v.as_array());
        let mut cache: HashMap<usize, Vec<ByteSpan>> = HashMap::new();
        if let Some(spans) = spans {
            for span in spans {
                let Some(a) = span.as_array() else { continue };
                if a.len() != 4 {
                    continue;
                }
                let line = a[0].as_u64().unwrap_or(0) as usize;
                let start = a[1].as_u64().unwrap_or(0) as usize;
                let end = a[2].as_u64().unwrap_or(0) as usize;
                let group = a[3].as_str().unwrap_or("").to_string();
                cache
                    .entry(line)
                    .or_default()
                    .push(ByteSpan { start, end, group });
            }
        }
        self.syntax_state.spans = cache;
    }

    /// Build a per-row `highlights` payload from a row→buffer-line mapping
    /// (`numbers`, 1-based, `None` for filler): each row's cached byte spans
    /// converted to **screen columns** (tab- and wide-char aware, like the
    /// selection), as `[start_col, end_col, group]`. Used for both the static
    /// viewport and the scroll-animation band.
    fn highlights_for(&self, numbers: &[Option<usize>]) -> Value {
        let rows = numbers
            .iter()
            .map(|num| match num {
                Some(n) => {
                    let line_idx = n - 1;
                    let Some(spans) = self.syntax_state.spans.get(&line_idx) else {
                        return Value::Array(Vec::new());
                    };
                    let text = self.editor.buffer.line(line_idx);
                    let row = spans
                        .iter()
                        .map(|s| {
                            let start = unicode::virtcol(&text, s.start, unicode::TABSTOP);
                            let end = unicode::virtcol(&text, s.end, unicode::TABSTOP);
                            Value::Array(vec![
                                Value::from(start as u64),
                                Value::from(end as u64),
                                Value::from(s.group.as_str()),
                            ])
                        })
                        .collect();
                    Value::Array(row)
                }
                None => Value::Array(Vec::new()),
            })
            .collect();
        Value::Array(rows)
    }
}

/// Map a buffer's file extension to a treesitter language name. Unknown
/// extensions (and paths with none) yield `None` — no highlighting, and no
/// worker is spawned. This table is the seam where more languages plug in.
fn filetype_of(path: Option<&std::path::Path>) -> Option<&'static str> {
    let ext = path?.extension()?.to_str()?;
    // Test hook (debug builds only): a `.crash` file selects the reserved
    // `__crash` language, whose worker aborts on open — used to verify the editor
    // survives and respawns a crashed worker. Absent from release binaries.
    #[cfg(debug_assertions)]
    if ext == "crash" {
        return Some("__crash");
    }
    Some(match ext {
        "rs" => "rust",
        "py" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "json" => "json",
        "toml" => "toml",
        "md" | "markdown" => "markdown",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "go" => "go",
        "lua" => "lua",
        "html" => "html",
        "css" => "css",
        "sh" | "bash" => "bash",
        _ => return None,
    })
}

/// Encode buffer edit deltas for the `ts_edit` message: each is a 10-element
/// array `[start_byte, old_end_byte, new_end_byte, start_row, start_col,
/// old_end_row, old_end_col, new_end_row, new_end_col, text]`.
fn edits_value(edits: &[nxvim_core::BufferEdit]) -> Value {
    Value::Array(
        edits
            .iter()
            .map(|e| {
                Value::Array(vec![
                    Value::from(e.start_byte as u64),
                    Value::from(e.old_end_byte as u64),
                    Value::from(e.new_end_byte as u64),
                    Value::from(e.start_point.0 as u64),
                    Value::from(e.start_point.1 as u64),
                    Value::from(e.old_end_point.0 as u64),
                    Value::from(e.old_end_point.1 as u64),
                    Value::from(e.new_end_point.0 as u64),
                    Value::from(e.new_end_point.1 as u64),
                    Value::from(e.text.as_str()),
                ])
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

fn uint(v: Option<&Value>, default: usize) -> usize {
    v.and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(default)
}

fn text(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or("").to_string()
}
