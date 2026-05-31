//! The nxvim server: a headless editor process that owns the core model and
//! Lua runtime and exposes them over msgpack-RPC.
//!
//! This is the rust-native analogue of neovim's `main.c` + `event/` + `api/`.
//! It runs on a single thread with an async runtime: the RPC reader/writer are
//! independent tasks, while the server loop processes one message at a time
//! against the (non-`Send`) editor and Lua state. Clients (the TUI today, a
//! native GUI later) attach over the same RPC channel and are never blocked by
//! the server's bookkeeping.

use nxvim_core::{parse_keys, Editor};
use nxvim_lua::LuaRuntime;
use nxvim_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};

/// Startup options for the server.
#[derive(Debug, Default, Clone)]
pub struct ServerInit {
    /// File to open in the initial buffer, if any.
    pub file: Option<String>,
}

struct Server {
    editor: Editor,
    lua: LuaRuntime,
    rpc: Rpc,
    /// Attached UI dimensions `(width, height)`, once a client has attached.
    ui: Option<(usize, usize)>,
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

    let mut server = Server {
        editor,
        lua,
        rpc,
        ui: None,
    };

    while let Some(message) = incoming.recv().await {
        server.handle(message);
        if server.editor.should_quit {
            server.rpc.notify("nxvim_exit", vec![]);
            break;
        }
    }
    Ok(())
}

impl Server {
    fn handle(&mut self, message: Incoming) {
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

        let lines = Value::Array(view.lines.iter().map(|l| Value::from(l.as_str())).collect());
        let selection = Value::Array(
            view.selection
                .iter()
                .map(|s| match s {
                    Some((start, end)) => {
                        Value::Array(vec![Value::from(*start as u64), Value::from(*end as u64)])
                    }
                    None => Value::Nil,
                })
                .collect(),
        );
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
        ];

        self.rpc.notify("redraw", vec![Value::Map(map)]);
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
