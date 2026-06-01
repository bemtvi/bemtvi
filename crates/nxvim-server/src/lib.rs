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

use nxvim_core::highlight::{HlDef, Style};
use nxvim_core::{parse_color, parse_keys, unicode, Editor};
use nxvim_lua::{HlSet, LuaRuntime};
use nxvim_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use syntax::{SyntaxClient, SyntaxEvent};
use tokio::io::{AsyncRead, AsyncWrite};

/// Startup options for the server.
#[derive(Debug, Default, Clone)]
pub struct ServerInit {
    /// File to open in the initial buffer, if any.
    pub file: Option<String>,
    /// Config directory whose `init.lua` is sourced at startup (`None` to skip).
    pub config_dir: Option<PathBuf>,
    /// Directories Lua searches for modules and runtime files (the runtimepath).
    pub runtimepath: Vec<PathBuf>,
}

/// Resolve nxvim's config directory and runtimepath from the environment, the
/// way the real binary starts up. Tests bypass this and pass explicit paths in
/// [`ServerInit`] instead, so they never depend on the host's home directory.
///
/// - **Config dir:** `$NXVIM_CONFIG`, else `$XDG_CONFIG_HOME/nxvim`, else
///   `$HOME/.config/nxvim` (`None` if none resolve).
/// - **Runtimepath:** any `$NXVIM_RUNTIMEPATH` entries first (explicit override),
///   then the config dir, then every plugin discovered under
///   `<config>/pack/*/start/*` (neovim's package layout, so a plugin checkout is
///   drop-in).
pub fn default_runtime() -> (Option<PathBuf>, Vec<PathBuf>) {
    let config_dir = resolve_config_dir();
    let mut runtimepath: Vec<PathBuf> = Vec::new();
    if let Some(rtp) = std::env::var_os("NXVIM_RUNTIMEPATH") {
        runtimepath.extend(std::env::split_paths(&rtp));
    }
    if let Some(cfg) = &config_dir {
        runtimepath.push(cfg.clone());
        runtimepath.extend(discover_plugins(cfg));
    }
    (config_dir, runtimepath)
}

/// First of `$NXVIM_CONFIG`, `$XDG_CONFIG_HOME/nxvim`, `$HOME/.config/nxvim`.
fn resolve_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("NXVIM_CONFIG") {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("nxvim"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("nxvim"))
}

/// Every immediate `<config>/pack/*/start/*` directory — installed plugins, each
/// contributing its root to the runtimepath. Missing/unreadable dirs yield none.
fn discover_plugins(config_dir: &Path) -> Vec<PathBuf> {
    let mut plugins = Vec::new();
    let pack = config_dir.join("pack");
    let Ok(packages) = std::fs::read_dir(&pack) else {
        return plugins;
    };
    for package in packages.flatten() {
        let start = package.path().join("start");
        if let Ok(entries) = std::fs::read_dir(&start) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    plugins.push(entry.path());
                }
            }
        }
    }
    plugins
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
    let lua =
        LuaRuntime::new(init.runtimepath).map_err(|e| anyhow::anyhow!("lua init failed: {e}"))?;
    let (syntax, mut syntax_events) = SyntaxClient::new();

    let mut server = Server {
        editor,
        lua,
        rpc,
        ui: None,
        syntax,
        syntax_state: SyntaxState::default(),
    };

    // Source the user's `init.lua` (if any) before serving the client, exactly
    // as neovim runs config at startup: its options, mappings, and colorscheme
    // are in place by the time the first `redraw` goes out on UI attach.
    if let Some(config_dir) = &init.config_dir {
        server.source_init(&config_dir.join("init.lua"));
    }

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
            other => Err(format!("Unknown method: {other}")),
        }
    }

    fn input(&mut self, keys: &str) {
        for key in parse_keys(keys) {
            self.editor.input(key);
        }
        self.run_pending();
    }

    fn run_command(&mut self, cmd: &str) {
        self.editor.command(cmd);
        self.run_pending();
    }

    /// Source a startup Lua file (the user's `init.lua`). Missing files are
    /// skipped silently — having no config is normal. A Lua error surfaces on
    /// the message line; effects are drained through the same path as `:lua`.
    fn source_init(&mut self, path: &Path) {
        let src = match std::fs::read_to_string(path) {
            Ok(src) => src,
            Err(_) => return,
        };
        if let Err(e) = self.lua.exec(&src) {
            self.editor.message = format!("E5113: Error while sourcing init.lua: {e}");
        }
        self.apply_lua_effects();
        self.run_pending();
    }

    /// Apply the side effects the last Lua chunk left in the runtime: highlight
    /// definitions fold into the core registry, queued ex-commands run against
    /// the editor, and the final captured `print` / `nvim_echo` line becomes the
    /// message.
    fn apply_lua_effects(&mut self) {
        for hl in self.lua.take_highlights() {
            self.editor.highlights.set(&hl.name, hl_def(&hl));
        }
        for cmd in self.lua.take_commands() {
            self.editor.command(&cmd);
        }
        if let Some(last) = self.lua.take_output().last() {
            self.editor.message = last.clone();
        }
    }

    /// Drive queued work to convergence: run the `:lua` chunks the editor
    /// queued, resolve every ex-command the core deferred (a Lua user command,
    /// else the unknown-command error), and repeat until nothing new is queued.
    /// Both queues feed each other — a user command can `vim.cmd(...)`, a `:lua`
    /// can define a command — so a single fixpoint loop covers them.
    fn run_pending(&mut self) {
        loop {
            for chunk in std::mem::take(&mut self.editor.lua_queue) {
                if let Err(e) = self.lua.exec(&chunk) {
                    self.editor.message = format!("E5108: Error executing lua: {e}");
                }
                self.apply_lua_effects();
            }
            for cmd in std::mem::take(&mut self.editor.deferred_commands) {
                self.resolve_command(&cmd);
            }
            if self.editor.lua_queue.is_empty() && self.editor.deferred_commands.is_empty() {
                break;
            }
        }
    }

    /// Resolve an ex-command the core didn't recognize: load a colorscheme,
    /// dispatch a Lua user command if one is registered under that name, or
    /// report the standard unknown-command error. `cmd` is the trimmed line.
    fn resolve_command(&mut self, cmd: &str) {
        let name = cmd.split_whitespace().next().unwrap_or("");
        let args = cmd.get(name.len()..).unwrap_or("").trim_start();
        match name {
            "colorscheme" | "colo" => self.set_colorscheme(args.trim()),
            _ if self.lua.has_user_command(name) => {
                if let Err(e) = self.lua.run_user_command(name, args) {
                    self.editor.message = format!("E5108: Error executing command {name}: {e}");
                }
                self.apply_lua_effects();
            }
            _ => self.editor.message = format!("E492: Not an editor command: {name}"),
        }
    }

    /// Load a colorscheme by name: source `colors/<name>.lua` off the
    /// runtimepath (whose body populates the highlight registry via
    /// `nvim_set_hl`), record `g:colors_name`, and fire the `ColorScheme`
    /// autocmd. With no name, report the active colorscheme. The drain happens
    /// in the caller's `run_pending` fixpoint loop, so any `vim.cmd(...)` the
    /// theme queues is still resolved.
    fn set_colorscheme(&mut self, name: &str) {
        if name.is_empty() {
            return; // `:colorscheme` with no arg is a query we don't surface yet
        }
        let Some(path) = self.find_runtime_file(&format!("colors/{name}.lua")) else {
            self.editor.message = format!("E185: Cannot find color scheme '{name}'");
            return;
        };
        let src = match std::fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => {
                self.editor.message = format!("E185: Cannot read color scheme '{name}': {e}");
                return;
            }
        };
        if let Err(e) = self.lua.exec(&src) {
            self.editor.message = format!("E5108: Error loading colorscheme {name}: {e}");
        }
        self.apply_lua_effects();
        let _ = self.lua.set_global_var("colors_name", name);
        if let Err(e) = self.lua.fire_autocmd("ColorScheme", name) {
            self.editor.message = format!("E5108: Error in ColorScheme autocmd: {e}");
        }
        self.apply_lua_effects();
    }

    /// Find a runtime file (e.g. `colors/catppuccin.lua`) by searching each
    /// runtimepath entry in order; the first existing match wins. `None` if no
    /// entry holds it.
    fn find_runtime_file(&self, relative: &str) -> Option<PathBuf> {
        self.lua.runtimepath().iter().find_map(|rt| {
            let candidate = rt.join(relative);
            candidate.is_file().then_some(candidate)
        })
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

        // Resolve every highlight span and chrome region to a concrete style here
        // on the server (the registry lives in the core). Spans carry an index
        // into a per-frame, deduped `styles` palette; the client paints the RGB.
        let mut styles = StyleTable::default();
        let highlights = self.highlights_for(&view.numbers, &mut styles);
        let chrome = self.chrome_styles(&mut styles);

        let lines = Value::Array(view.lines.iter().map(|l| Value::from(l.as_str())).collect());
        let selection = spans_value(&view.selection);
        let numbers = numbers_value(&view.numbers);
        let scroll = match &view.scroll {
            Some(s) => {
                let scroll_lines =
                    Value::Array(s.lines.iter().map(|l| Value::from(l.as_str())).collect());
                let scroll_highlights = self.highlights_for(&s.numbers, &mut styles);
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
                    (Value::from("highlights"), scroll_highlights),
                ])
            }
            None => Value::Nil,
        };
        // Built last: every `highlights`/`chrome` style id above indexes into it.
        let styles_value = styles.into_value();
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
            (Value::from("styles"), styles_value),
            (Value::from("chrome"), chrome),
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
    /// selection), as `[start_col, end_col, group, style_id]`. `style_id` indexes
    /// into the per-frame `styles` palette when the span's capture resolves
    /// through the registry; it is `Nil` otherwise, so the client falls back to
    /// its built-in theme for that group. Used for both the static viewport and
    /// the scroll-animation band (which share `styles`).
    fn highlights_for(&self, numbers: &[Option<usize>], styles: &mut StyleTable) -> Value {
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
                            let style_id = match self.editor.highlights.resolve_capture(&s.group) {
                                Some(style) => Value::from(styles.intern(style) as u64),
                                None => Value::Nil,
                            };
                            Value::Array(vec![
                                Value::from(start as u64),
                                Value::from(end as u64),
                                Value::from(s.group.as_str()),
                                style_id,
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

    /// Resolve the editor-chrome highlight groups (the background, gutter,
    /// selection, and status line) to style-palette indices for this frame. Each
    /// resolved group becomes a `name -> style_id` entry; groups the colorscheme
    /// leaves undefined are simply absent, so the client keeps its built-in look
    /// (e.g. reverse-video selection) for them. Empty map when no theme is loaded.
    fn chrome_styles(&self, styles: &mut StyleTable) -> Value {
        // Map redraw key -> highlight group. The keys mirror the View regions the
        // client themes; the groups are neovim's standard chrome groups.
        const CHROME: &[(&str, &str)] = &[
            ("normal", "Normal"),
            ("line_nr", "LineNr"),
            ("cursor_line_nr", "CursorLineNr"),
            ("visual", "Visual"),
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
struct StyleTable {
    list: Vec<Style>,
    index: HashMap<Style, usize>,
}

impl StyleTable {
    /// Return the index of `style` in the palette, appending it on first sight.
    fn intern(&mut self, style: Style) -> usize {
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

/// Translate a Lua-side `nvim_set_hl` definition into the core registry's
/// `HlDef`, parsing the color strings (`#rrggbb` / named / `NONE`) here at the
/// boundary so `nxvim-lua` need not know about the color type.
fn hl_def(hl: &HlSet) -> HlDef {
    let color = |c: &Option<String>| c.as_deref().and_then(parse_color);
    HlDef {
        fg: color(&hl.fg),
        bg: color(&hl.bg),
        sp: color(&hl.sp),
        bold: hl.bold,
        italic: hl.italic,
        underline: hl.underline,
        undercurl: hl.undercurl,
        strikethrough: hl.strikethrough,
        reverse: hl.reverse,
        link: hl.link.clone(),
    }
}

/// Encode a resolved [`Style`] as the RPC map the query methods return: colors
/// as `0xRRGGBB` integers (neovim's convention) under `fg`/`bg`/`sp`, and each
/// set boolean attribute as `true`. Absent fields are simply omitted.
fn style_value(style: &Style) -> Value {
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
