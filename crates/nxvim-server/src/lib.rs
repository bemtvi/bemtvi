//! The nxvim server: a headless editor process that owns the core model and
//! Lua runtime and exposes them over msgpack-RPC.
//!
//! This is the rust-native analogue of neovim's `main.c` + `event/` + `api/`.
//! It runs on a single thread with an async runtime: the RPC reader/writer are
//! independent tasks, while the server loop processes one message at a time
//! against the (non-`Send`) editor and Lua state. Clients (the TUI today, a
//! native GUI later) attach over the same RPC channel and are never blocked by
//! the server's bookkeeping.

mod keymap;
mod lsp;
mod syntax;

use keymap::{BuiltinAction, Keymaps, MappingRhs, NativeDefault, Step};
use lsp::{
    CompletionMenu, LspDocState, LspReqKind, PendingLspReq, ServerRuntime, CODE_ACTION_PANEL_TITLE,
};
use nxvim_core::highlight::{HlDef, Style};
use nxvim_core::view::ScrollAnim;
use nxvim_core::{
    parse_color, parse_keys, unicode, BufferId, Editor, Key, KeyCode, Mode, PanelView,
};
use nxvim_lsp::{CodeActionData, LspManager, ServerKey};
use nxvim_lua::{HlSet, LuaRuntime, PanelOp};
use nxvim_rpc::syntax::{encode_edits, EditWire, SpanWire};
use nxvim_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use std::collections::{HashMap, HashSet};
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

/// A cached highlight span in buffer coordinates: a byte range within a line.
#[derive(Clone)]
struct ByteSpan {
    start: usize,
    end: usize,
    group: String,
}

/// Per-buffer treesitter sync bookkeeping. One of these per open buffer, keyed
/// by [`BufferId`] in [`Server::syntax_states`], so a buffer keeps its parse
/// state and span cache while another is in the window — switching back paints
/// instantly instead of re-parsing.
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
    /// Per-buffer syntax sync state, keyed by buffer id (entries created lazily
    /// on first sync, dropped when the buffer is deleted).
    syntax_states: HashMap<BufferId, SyntaxState>,
    /// Set when a syntax event changed something the client should see. Drained
    /// once per loop turn so a burst of worker notifications costs one `redraw`,
    /// not one per notification.
    syntax_dirty: bool,
    /// The in-process LSP client: spawns/supervises N language servers and
    /// bridges them to the [`LspEvent`] channel the main loop selects on.
    lsp: LspManager,
    /// Per-buffer LSP document-sync state, keyed by buffer id (the `syntax_states`
    /// analogue).
    lsp_states: HashMap<BufferId, LspDocState>,
    /// Negotiated runtime state (encoding, sync kind) per started server, learned
    /// from each `initialize` reply.
    lsp_servers: HashMap<ServerKey, ServerRuntime>,
    /// Server keys already handed to `ensure_server`, so a server is requested
    /// once rather than on every redraw (the `SyntaxClient::ensure_started` guard).
    lsp_ensured: HashSet<ServerKey>,
    /// Set when an LSP event changed something the client should see (e.g. a fresh
    /// `Initialized` that should trigger a `didOpen`). Coalesced like `syntax_dirty`.
    lsp_dirty: bool,
    /// Monotonic generation counter stamped onto each language-feature request,
    /// so a reply whose generation is behind the latest of its kind is dropped
    /// (Decision 3 — the go-to analogue of the syntax `tick`).
    lsp_req_gen: u64,
    /// The in-flight language-feature request per kind (definition, references,
    /// …), used to match a reply to its intent and drop stale ones.
    lsp_requests: HashMap<LspReqKind, PendingLspReq>,
    /// The open insert-mode completion popup (Phase 5), or `None`. Server-owned
    /// like the diagnostics cache; projected into the `pmenu` redraw key and
    /// driven by the popup-open key routing in [`Server::completion_menu_key`].
    completion: Option<CompletionMenu>,
    /// The code actions currently listed in the `:LspCodeAction` panel (Phase 6),
    /// indexed by panel select. A `<CR>` on row `i` applies `lsp_code_actions[i]`'s
    /// edit; cleared on apply. Empty when no code-action panel is active.
    lsp_code_actions: Vec<CodeActionData>,
    /// The buffer that was current the last time lifecycle events were emitted;
    /// `None` until the startup seed. A change here means a `BufEnter` (fired on
    /// every entry).
    last_buffer_id: Option<BufferId>,
    /// Buffers that have already had their fire-once events (`BufReadPost` /
    /// `FileType`) emitted, so re-entering them doesn't re-announce.
    announced: HashSet<BufferId>,
    /// The editor mode at the last lifecycle diff. A transition *into* insert
    /// (from a non-insert mode) fires `InsertEnter`; tracked here so the per-key
    /// diff can spot the edge without touching the core's insert chokepoints.
    last_mode: Mode,
    /// The user-mapping engine: per-mode tries + the withhold/replay buffer that
    /// `Server::input` runs every key through before `editor.input`. Rebuilt from
    /// `vim._keymaps` when its version advances (checked once per input batch).
    keymaps: Keymaps,
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
        Some(path) => Editor::open_or_named(path),
        None => Editor::new(),
    };
    let lua =
        LuaRuntime::new(init.runtimepath).map_err(|e| anyhow::anyhow!("lua init failed: {e}"))?;
    let (syntax, mut syntax_events) = SyntaxClient::new();
    let (lsp, mut lsp_events) = LspManager::new();

    let mut server = Server {
        editor,
        lua,
        rpc,
        ui: None,
        syntax,
        syntax_states: HashMap::new(),
        syntax_dirty: false,
        lsp,
        lsp_states: HashMap::new(),
        lsp_servers: HashMap::new(),
        lsp_ensured: HashSet::new(),
        lsp_dirty: false,
        lsp_req_gen: 0,
        lsp_requests: HashMap::new(),
        completion: None,
        lsp_code_actions: Vec::new(),
        last_buffer_id: None,
        announced: HashSet::new(),
        last_mode: Mode::Normal,
        keymaps: Keymaps::default(),
    };

    // Install the built-in LSP keymaps as overridable defaults (design B2/B3),
    // so a user `vim.keymap.set` for the same `(mode, lhs)` shadows them via the
    // user > default precedence rung. *All* the LSP keys ride the matcher now,
    // including the `g`-prefixed go-to trio (`gd`/`gD`/`gr`): the matcher can own
    // the `g` prefix without breaking core's `gg`/`ge`/`dgg`/… motions because the
    // `command_status` oracle (merged from main) releases a withheld `g`-run to the
    // editor the moment it completes a built-in, so `gg` fires whole instead of
    // being folded into `gd`. This retires the bespoke `lsp_pending_g` recognizer
    // (and, earlier, `lsp_pending_ctrl_x` — `<C-x><C-o>` is just a two-key map).
    server.keymaps.set_native_defaults(vec![
        NativeDefault {
            mode: "n",
            lhs: "gd",
            action: BuiltinAction::Lsp(LspReqKind::Definition),
        },
        NativeDefault {
            mode: "n",
            lhs: "gD",
            action: BuiltinAction::Lsp(LspReqKind::Declaration),
        },
        NativeDefault {
            mode: "n",
            lhs: "gr",
            action: BuiltinAction::Lsp(LspReqKind::References),
        },
        NativeDefault {
            mode: "n",
            lhs: "K",
            action: BuiltinAction::Lsp(LspReqKind::Hover),
        },
        NativeDefault {
            mode: "i",
            lhs: "<C-Space>",
            action: BuiltinAction::Lsp(LspReqKind::Completion),
        },
        NativeDefault {
            mode: "i",
            lhs: "<C-x><C-o>",
            action: BuiltinAction::Lsp(LspReqKind::Completion),
        },
        NativeDefault {
            mode: "i",
            lhs: "<C-k>",
            action: BuiltinAction::Lsp(LspReqKind::SignatureHelp),
        },
    ]);

    // Seed the current-buffer snapshot before sourcing config, so a buffer-local
    // map declared with `buffer = 0` (or `nvim_create_autocmd`'s `buffer = 0`)
    // resolves to the real startup buffer rather than the default `0` — the buffer
    // already exists at config time, matching neovim. Carrying the filetype too
    // lets a `vim.lsp.enable(...)` in `init.lua` start a server for it. Lifecycle
    // emission refreshes it again before each autocmd fires; this makes it valid
    // earlier.
    {
        let buf = server.editor.current_buffer_id();
        let name = server.editor.buffer_name(buf).unwrap_or_default();
        let ft = filetype_of(server.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = server.lua.set_buf_snapshot(buf.0, &name, ft);
    }

    // Source the user's `init.lua` (if any) before serving the client, exactly
    // as neovim runs config at startup: its options, mappings, and colorscheme
    // are in place by the time the first `redraw` goes out on UI attach.
    if let Some(config_dir) = &init.config_dir {
        server.source_init(&config_dir.join("init.lua"));
    }

    // Startup seed: the initial buffer and the config's autocmds both exist now,
    // so fire the first buffer's lifecycle events (`BufReadPost`→`FileType`→
    // `BufEnter` for a file arg, `BufEnter` alone for the bare `[No Name]`).
    server.emit_lifecycle_events();
    server.run_pending();

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
                // Coalesce a burst: drain everything queued right now, then redraw
                // at most once — a fast/flooding worker would otherwise force a
                // full view re-projection per notification.
                while let Ok(event) = syntax_events.try_recv() {
                    server.on_syntax_event(event);
                }
                if std::mem::take(&mut server.syntax_dirty) {
                    server.redraw();
                }
            }
            // Replies from the language servers (initialize handshakes, published
            // diagnostics, server exits, log messages). Selecting here keeps the
            // editor responsive regardless of any server's speed or health.
            Some(event) = lsp_events.recv() => {
                server.on_lsp_event(event);
                // Coalesce a burst into a single repaint, as for syntax events.
                while let Ok(event) = lsp_events.try_recv() {
                    server.on_lsp_event(event);
                }
                if std::mem::take(&mut server.lsp_dirty) {
                    server.redraw();
                }
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
            other => Err(format!("Unknown method: {other}")),
        }
    }

    fn input(&mut self, keys: &str) {
        // Rebuild the keymap tries if the registry changed since the last batch —
        // once per `nvim_input`, not per key, so each keystroke only walks the
        // cached trie (design §6). A map a callback sets mid-batch takes effect on
        // the next batch, an accepted ordering.
        self.refresh_keymaps();
        for key in parse_keys(keys) {
            // Insert-mode completion popup is modal, stateful UI routing: while it
            // is open it owns every key (navigate / accept / dismiss / live-refresh)
            // ahead of the mapping engine (design B5). A key the popup *doesn't*
            // claim dismisses it and returns `false`, so we fall through to the
            // matcher below — `<C-k>` then fires signature help, `<Esc>` then leaves
            // insert, etc. (`completion_menu_key` is only reached while open.)
            if self.editor.mode == Mode::Insert
                && self.completion_menu_open()
                && self.completion_menu_key(key)
            {
                continue;
            }
            // The mapping layer interposes here, ahead of `editor.input`: each key
            // is run through the withhold/replay matcher, which hands back the steps
            // to apply (raw editor keys and/or a fired mapping). The built-in LSP
            // keys — the `gd`/`gD`/`gr` go-to trio, `K` hover, and the insert-mode
            // completion triggers — all ride it as overridable native default
            // mappings (design B2/B3); the `command_status` oracle keeps core's
            // `g`-motions (`gg`/`dgg`/…) intact under the `g`-prefix collision.
            self.feed_matcher(key);
        }
        self.run_pending();
    }

    /// Run one key through the general mapping matcher and apply the steps it
    /// produces. The single path into [`Keymaps::feed`], driving the per-key
    /// [`input`](Self::input) loop.
    fn feed_matcher(&mut self, key: Key) {
        let mode = self.editor.mode;
        for step in self.keymaps.feed(mode, key) {
            self.apply_step(step);
        }
    }

    /// Handle one key while the completion popup is open. Returns `true` when the
    /// key is consumed (navigation, accept, refresh); `false` after **closing**
    /// the menu, so the caller lets the key take its normal effect (`<Esc>` also
    /// leaves insert, a non-word char is inserted, `<C-k>` fires signature help).
    /// A word character or backspace is applied to the editor first, then the menu
    /// re-ranks (or re-requests) against the new prefix in place.
    fn completion_menu_key(&mut self, key: Key) -> bool {
        if key.ctrl {
            return match key.code {
                KeyCode::Char('n') => {
                    self.lsp_menu_move(1);
                    true
                }
                KeyCode::Char('p') => {
                    self.lsp_menu_move(-1);
                    true
                }
                KeyCode::Char('y') => {
                    self.lsp_menu_accept();
                    true
                }
                KeyCode::Char('e') => {
                    self.lsp_menu_close();
                    true
                }
                // Any other ctrl key (e.g. `<C-k>`): dismiss, then let it act.
                _ => {
                    self.lsp_menu_close();
                    false
                }
            };
        }
        match key.code {
            KeyCode::Down => {
                self.lsp_menu_move(1);
                true
            }
            KeyCode::Up => {
                self.lsp_menu_move(-1);
                true
            }
            KeyCode::Enter | KeyCode::Tab => {
                self.lsp_menu_accept();
                true
            }
            // A word character or backspace edits the buffer, then refreshes the
            // menu against the new prefix (the editor inserts/deletes first).
            KeyCode::Backspace => {
                self.editor.input(key);
                self.lsp_menu_after_edit();
                true
            }
            KeyCode::Char(c) if c.is_ascii_alphanumeric() || c == '_' => {
                self.editor.input(key);
                self.lsp_menu_after_edit();
                true
            }
            // `<Esc>` and any other key dismiss the menu, then take normal effect.
            _ => {
                self.lsp_menu_close();
                false
            }
        }
    }

    /// Resolve a withheld key-prefix on input idle — the matcher's `timeoutlen`
    /// flush (design D4). Mirrors [`input`](Self::input)'s drive, but the steps come
    /// from [`Keymaps::flush`] (no incoming key) instead of `feed`. Refreshing the
    /// tries first keeps the flush consistent with a registry/buffer change since the
    /// last batch; with nothing pending the whole call is a no-op.
    fn input_flush(&mut self) {
        self.refresh_keymaps();
        let mode = self.editor.mode;
        for step in self.keymaps.flush(mode) {
            self.apply_step(step);
        }
        self.run_pending();
    }

    /// Bring the keymap tries up to date for the current buffer. Re-reads the
    /// registry only when `vim._keymaps_version` advanced (one integer read across
    /// the bridge on the common path), and rebuilds the per-mode tries when either
    /// the snapshot or the current buffer changed — the latter so a buffer-local
    /// map (design D6) is in force exactly in its own buffer. Both checks are
    /// cheap; a mapping set or a buffer switched *mid-batch* takes effect on the
    /// next batch, the same accepted ordering the version check already implies.
    fn refresh_keymaps(&mut self) {
        let version = self.lua.keymaps_version();
        if version != self.keymaps.version {
            let snapshot = self.lua.keymaps_snapshot();
            self.keymaps.set_snapshot(version, snapshot);
        }
        let buffer = self.editor.current_buffer_id().0;
        if self.keymaps.needs_build(buffer) {
            self.keymaps.build_for(buffer);
        }
    }

    /// Apply one matcher [`Step`]: a raw key goes to the editor (with the per-key
    /// lifecycle diff, exactly as the old bare loop did); a fired mapping runs its
    /// RHS.
    fn apply_step(&mut self, step: Step) {
        match step {
            Step::Editor(key) => {
                self.editor.input(key);
                // Per *key*, not per message: a batched `o…<Esc>` must still see
                // the transition into insert on the `o`, which a once-per-input
                // diff would miss (it'd see only the settled Normal end-state).
                self.emit_lifecycle_events();
            }
            Step::Fire { rhs, silent, expr } => self.fire_mapping(rhs, silent, expr),
        }
    }

    /// Execute a fired mapping's RHS (design D7 — a `match` over the enum from day
    /// one, so the LSP backport adds its native action as one more arm). A Lua
    /// function is invoked and its effects folded in (any deferred ex-commands
    /// converge in the batch's trailing `run_pending`, like the autocmd path); a
    /// `noremap` string RHS is fed key-by-key straight to the editor.
    ///
    /// `<silent>` (`silent`) suppresses the message line the mapping leaves: the
    /// line is snapshotted before the fire and restored after, so a `:cmd` echo or
    /// `print` the mapping triggers doesn't linger on the command line. The
    /// `:messages` history (appended by `echo`) is deliberately *not* rewound — the
    /// output is still logged, only its transient display is hidden, matching vim's
    /// "no messages on the command line while executing this mapping." (Effects a
    /// Lua RHS *defers* to the trailing `run_pending` fall outside this window — an
    /// accepted corner, the same ordering caveat the rest of the fire path carries.)
    ///
    /// `<expr>` (`expr`) routes a Lua RHS through [`fire_expr`](Self::fire_expr): the
    /// function is run for its *return value* (the keys to feed), under a textlock
    /// that stops it mutating the editor. (A non-Lua `expr` RHS falls through to the
    /// normal path — nxvim has no expression evaluator for a string RHS.)
    fn fire_mapping(&mut self, rhs: MappingRhs, silent: bool, expr: bool) {
        let restore = silent.then(|| self.editor.message.clone());
        match (expr, rhs) {
            (true, MappingRhs::Lua(id)) => self.fire_expr(id),
            (_, rhs) => self.fire_mapping_inner(rhs),
        }
        if let Some(message) = restore {
            self.editor.message = message;
        }
    }

    /// Run an `<expr>` Lua RHS and feed the keys it returns. The function computes
    /// keys rather than acting (vim's `<expr>`): it runs under the prelude's textlock
    /// (`vim._expr_lock`, which makes `vim.cmd` raise), and whatever effects it
    /// queued anyway are **discarded** here — only the returned keys take effect, fed
    /// straight to the editor (noremap; the computed keys are not themselves
    /// remapped, the common case for `<expr>`, which is noremap by default). An error
    /// (a throwing handler, or a textlock violation) is surfaced and nothing is fed.
    fn fire_expr(&mut self, id: u64) {
        match self.lua.run_keymap_expr(id) {
            Ok(keys) => {
                self.discard_lua_effects();
                for key in parse_keys(&keys) {
                    self.editor.input(key);
                    self.emit_lifecycle_events();
                }
            }
            Err(e) => {
                self.discard_lua_effects();
                self.editor
                    .echo(format!("E5108: Error executing keymap: {e}"));
            }
        }
    }

    /// Drop every side effect the last Lua chunk queued without applying any of them
    /// — the `<expr>` sandbox's safety net: an `<expr>` RHS that printed, set a
    /// highlight, or queued a panel op despite the textlock has those effects thrown
    /// away here, so only its returned keys ever reach the editor. Mirrors the drains
    /// in [`apply_lua_effects`](Self::apply_lua_effects), but discards each.
    fn discard_lua_effects(&mut self) {
        let _ = self.lua.take_highlights();
        let _ = self.lua.take_commands();
        let _ = self.lua.take_output();
        let _ = self.lua.take_panel_ops();
    }

    fn fire_mapping_inner(&mut self, rhs: MappingRhs) {
        match rhs {
            MappingRhs::Lua(id) => {
                if let Err(e) = self.lua.run_keymap(id) {
                    self.editor
                        .echo(format!("E5108: Error executing keymap: {e}"));
                }
                self.apply_lua_effects();
            }
            MappingRhs::Keys(keys, _noremap) => {
                // A string RHS that reaches the server is fed straight to the
                // editor, bypassing the trie. The matcher only hands these over
                // for the non-remapping cases: a `noremap` RHS, or a `remap` RHS
                // that exhausted its re-feed budget (recursive remap expansion
                // happens inside the matcher's `feed`, never here).
                for key in keys {
                    self.editor.input(key);
                    self.emit_lifecycle_events();
                }
            }
            // A built-in default (the LSP keys) runs natively — no key-feeding, so
            // the `<cmd>`/remap caveats never touch it (design B3). `request_lsp`
            // and `LspReqKind` already exist on this branch; the matcher only ever
            // hands us a `Native` RHS for the four normal-mode LSP defaults and the
            // insert-mode completion triggers installed at startup.
            MappingRhs::Native(BuiltinAction::Lsp(kind)) => self.request_lsp(kind),
        }
    }

    fn run_command(&mut self, cmd: &str) {
        self.editor.command(cmd);
        self.emit_lifecycle_events();
        self.run_pending();
    }

    /// Diff the editor's current buffer against what was last announced and fire
    /// the buffer-lifecycle autocmds the transition implies — the central,
    /// server-side emission point (design D1) that keeps `nxvim-core` free of
    /// event types. Called after each applied input (per key in [`Server::input`],
    /// after `:`-commands and `nvim_set_current_buf`) and once at startup.
    ///
    /// Ordering on first opening a file mirrors neovim: `BufReadPost` → `FileType`
    /// → `BufEnter`. `BufReadPost`/`FileType` fire **once** per buffer (gated by
    /// `announced`) and **only for file-backed buffers** — a `[No Name]` buffer was
    /// never read from a file. `BufEnter` fires on **every** entry. `InsertEnter`
    /// fires on a transition *into* insert (covering `i/a/o/C/cc/s/…` without
    /// touching the core insert chokepoints — the diff sees the result). A cheap
    /// no-op for the vast majority of keys, which change neither buffer nor mode.
    fn emit_lifecycle_events(&mut self) {
        let buf = self.editor.current_buffer_id();
        let mode = self.editor.mode;
        let unannounced = !self.announced.contains(&buf);
        let entered = self.last_buffer_id != Some(buf);
        // A transition *into* insert (or replace — neovim fires InsertEnter for
        // both), measured against the last diff so staying in insert won't re-fire.
        let entered_insert = mode.is_insert() && !self.last_mode.is_insert();
        // Track the mode every call — even the no-op fast path — so a later entry
        // is still seen after an insert→normal round trip that took the fast path.
        self.last_mode = mode;
        if !unannounced && !entered && !entered_insert {
            return; // fast path: nothing transitioned
        }

        let name = self.editor.buffer_name(buf).unwrap_or_default();
        let file_backed = !name.is_empty();

        // Fire-once per buffer, file-backed only: BufReadPost then FileType.
        if unannounced {
            self.announced.insert(buf);
            if file_backed {
                self.fire_lifecycle("BufReadPost", &name, buf, &name);
                // FileType's pattern is the filetype derived from the path; skip
                // it entirely when nothing is detected (matching neovim).
                if let Some(ft) = filetype_of(self.editor.buffer().path.as_deref()) {
                    self.fire_lifecycle("FileType", ft, buf, &name);
                }
            }
        }

        // Fire-every on entry: BufEnter, for both file-backed and [No Name].
        if entered {
            self.last_buffer_id = Some(buf);
            self.fire_lifecycle("BufEnter", &name, buf, &name);
        }

        // Mode event: InsertEnter, with the entered mode's code as the pattern.
        if entered_insert {
            self.fire_lifecycle("InsertEnter", mode.short_code(), buf, &name);
        }
    }

    /// Push the current-buffer snapshot into the VM, fire `event` for `pattern` /
    /// `file` with buffer context, surface any callback error, and fold in the Lua
    /// effects the callbacks left. Deferred ex-commands the callbacks queue are
    /// drained by the caller's `run_pending`.
    fn fire_lifecycle(&mut self, event: &str, pattern: &str, buf: BufferId, file: &str) {
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, file, ft);
        if let Err(e) = self.lua.fire_autocmd_buf(event, pattern, buf.0, file) {
            self.editor
                .echo(format!("E5108: Error in {event} autocmd: {e}"));
        }
        self.apply_lua_effects();
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
            self.editor
                .echo(format!("E5113: Error while sourcing init.lua: {e}"));
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
        // Each captured `print` / `nvim_echo` line becomes a message: the last
        // is shown on the message line, and every line lands in `:messages`.
        for line in self.lua.take_output() {
            self.editor.echo(line);
        }
        // Panel requests from `vim.panel.*` drive the core's panel state.
        for op in self.lua.take_panel_ops() {
            match op {
                PanelOp::Open {
                    title,
                    lines,
                    wants_select,
                    cursor,
                } => {
                    self.editor.open_panel(title, lines, wants_select, cursor);
                }
                PanelOp::SetLines(lines) => self.editor.set_panel_lines(lines),
                PanelOp::OnSelect(wants) => self.editor.set_panel_on_select(wants),
                PanelOp::SetCursor(line) => self.editor.set_panel_cursor(line),
                PanelOp::Close => self.editor.close_panel(),
            }
        }
        // Server-start requests from `vim.lsp.start` (the `vim.lsp.enable` FileType
        // dispatcher) bind a buffer to its language server and ensure it is spawned.
        for op in self.lua.take_lsp_ops() {
            self.apply_lsp_op(op);
        }
    }

    /// Drive queued work to convergence: run the `:lua` chunks the editor
    /// queued, resolve every ex-command the core deferred (a Lua user command,
    /// else the unknown-command error), and repeat until nothing new is queued.
    /// Both queues feed each other — a user command can `vim.cmd(...)`, a `:lua`
    /// can define a command — so a single fixpoint loop covers them.
    fn run_pending(&mut self) {
        // Cap on fixpoint rounds before we conclude the queued work is
        // self-perpetuating — a command or `on_select` callback that re-queues
        // itself every round (e.g. a user command whose body re-runs the same
        // command). Without this the single-threaded server spins forever and
        // stops servicing input. Generous enough that any legitimate finite
        // chain converges first; mirrors neovim's `maxfuncdepth` recursion guard.
        const MAX_ROUNDS: usize = 100;
        let mut rounds = 0;
        loop {
            for chunk in std::mem::take(&mut self.editor.lua_queue) {
                if let Err(e) = self.lua.exec(&chunk) {
                    self.editor.echo(format!("E5108: Error executing lua: {e}"));
                }
                self.apply_lua_effects();
            }
            for cmd in std::mem::take(&mut self.editor.deferred_commands) {
                self.resolve_command(&cmd);
            }
            // `<CR>` selections on a select-enabled panel: notify RPC clients and
            // fire the Lua `on_select` callback. The callback may itself queue
            // commands / lua / panel ops, so this is inside the fixpoint loop.
            for (index, line) in std::mem::take(&mut self.editor.panel_selects) {
                // The `:LspCodeAction` list (Phase 6) is a select-enabled panel:
                // a `<CR>` on row `index` applies that action's edit, keyed to the
                // currently-open code-action panel by title so a select on some
                // *other* select panel can't misroute here.
                if self.editor.panel_title() == Some(CODE_ACTION_PANEL_TITLE) {
                    self.apply_code_action(index);
                    continue;
                }
                // Navigable LSP location lists (diagnostics, references) jump in
                // the core itself when their target line is selected, so they
                // never reach here — only scripted/RPC select panels do.
                self.rpc.notify(
                    "nxvim_panel_select",
                    vec![Value::Map(vec![
                        (Value::from("index"), Value::from(index as u64 + 1)),
                        (Value::from("line"), Value::from(line.as_str())),
                    ])],
                );
                if let Err(e) = self.lua.run_panel_select(index, &line) {
                    self.editor
                        .echo(format!("E5108: Error in panel on_select: {e}"));
                }
                self.apply_lua_effects();
            }
            if self.editor.lua_queue.is_empty()
                && self.editor.deferred_commands.is_empty()
                && self.editor.panel_selects.is_empty()
            {
                break;
            }
            rounds += 1;
            if rounds >= MAX_ROUNDS {
                // Drop the still-growing work and report it, rather than loop
                // forever. The editor stays responsive to the next message.
                self.editor.lua_queue.clear();
                self.editor.deferred_commands.clear();
                self.editor.panel_selects.clear();
                self.editor
                    .echo("E132: command recursion limit exceeded".to_string());
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
            // Phase-1 LSP observability: dump server/document state into the panel.
            "LspInfo" => {
                let lines = self.lsp_info_lines();
                self.editor.open_panel("LSP info", lines, false, 0);
            }
            // Phase-2: list the current buffer's diagnostics as a navigable
            // location list; `<CR>` on a row jumps to it (handled in the core).
            "LspDiagnostics" => match self.diagnostics_location_list() {
                Some((lines, targets)) => {
                    self.editor.open_panel("LSP diagnostics", lines, false, 0);
                    self.editor.set_panel_targets(targets);
                }
                None => self.editor.echo("No diagnostics"),
            },
            // Phase-3: go-to / references as ex-commands (the keymap-free path;
            // the reply jumps the cursor or opens a panel location list).
            "LspDefinition" => self.request_lsp(LspReqKind::Definition),
            "LspDeclaration" => self.request_lsp(LspReqKind::Declaration),
            "LspTypeDefinition" => self.request_lsp(LspReqKind::TypeDefinition),
            "LspImplementation" => self.request_lsp(LspReqKind::Implementation),
            "LspReferences" => self.request_lsp(LspReqKind::References),
            // Phase-4: hover docs into the panel, signature help on the message
            // line (the keymap-free path for `K` / `<C-k>`).
            "LspHover" => self.request_lsp(LspReqKind::Hover),
            "LspSignatureHelp" => self.request_lsp(LspReqKind::SignatureHelp),
            // Phase-6: buffer-mutating features. Format/code-action take no
            // argument; rename reads the new name the dispatcher split off.
            "LspFormat" => self.request_lsp_format(),
            "LspRename" => self.request_lsp_rename(args),
            "LspCodeAction" => self.request_lsp_code_action(),
            _ if self.lua.has_user_command(name) => {
                if let Err(e) = self.lua.run_user_command(name, args) {
                    self.editor
                        .echo(format!("E5108: Error executing command {name}: {e}"));
                }
                self.apply_lua_effects();
            }
            _ => self
                .editor
                .echo(format!("E492: Not an editor command: {name}")),
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
            self.editor
                .echo(format!("E185: Cannot find color scheme '{name}'"));
            return;
        };
        let src = match std::fs::read_to_string(&path) {
            Ok(src) => src,
            Err(e) => {
                self.editor
                    .echo(format!("E185: Cannot read color scheme '{name}': {e}"));
                return;
            }
        };
        if let Err(e) = self.lua.exec(&src) {
            self.editor
                .echo(format!("E5108: Error loading colorscheme {name}: {e}"));
        }
        self.apply_lua_effects();
        let _ = self.lua.set_global_var("colors_name", name);
        if let Err(e) = self.lua.fire_autocmd("ColorScheme", name) {
            self.editor
                .echo(format!("E5108: Error in ColorScheme autocmd: {e}"));
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
        // Drive LSP document sync for the current buffer (also non-blocking).
        self.sync_lsp();

        // Resolve every highlight span and chrome region to a concrete style here
        // on the server (the registry lives in the core). Spans carry an index
        // into a per-frame, deduped `styles` palette; the client paints the RGB.
        let mut styles = StyleTable::default();
        let highlights = self.highlights_for(&view.numbers, &mut styles);
        let diagnostics = self.diagnostics_for(&view.numbers, &mut styles);
        let chrome = self.chrome_styles(&mut styles);

        // The message line shows the diagnostic under the cursor, but only when
        // nothing more important (an error, command output) already holds it —
        // and never via `echo`, so the under-cursor text doesn't flood
        // `:messages` on every cursor move.
        let message = if view.message.is_empty() {
            self.diagnostic_under_cursor().unwrap_or_default()
        } else {
            view.message.clone()
        };

        let lines = lines_value(&view.lines);
        let selection = spans_value(&view.selection);
        let search = multi_spans_value(&view.search);
        let incsearch = spans_value(&view.incsearch);
        let numbers = numbers_value(&view.numbers);
        let scroll = match &view.scroll {
            Some(s) => self.project_band(s, &mut styles),
            None => Value::Nil,
        };
        // The bottom panel (`:messages`, `:ls`), `Nil` when none is open.
        let panel = match &view.panel {
            Some(p) => project_panel(p),
            None => Value::Nil,
        };
        // The insert-mode completion popup, `Nil` when none is open. The text
        // area width (frame minus the number gutter) bounds the overlay so it
        // can't spill past the editable region.
        let pmenu = self.pmenu_value(&view, w.saturating_sub(view.number_width));

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
                Value::from("cmdline_cursor"),
                Value::from(view.cmdline_cursor as u64),
            ),
            (Value::from("message"), Value::from(message.as_str())),
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
            (Value::from("search"), search),
            (Value::from("incsearch"), incsearch),
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
            (Value::from("diagnostics"), diagnostics),
            (Value::from("styles"), styles_value),
            (Value::from("chrome"), chrome),
            (Value::from("panel"), panel),
            (Value::from("pmenu"), pmenu),
        ];

        self.rpc.notify("redraw", vec![Value::Map(map)]);
    }

    /// Project a scroll-animation band into the `scroll` sub-map a client animates
    /// the slide from. Mirrors the main map's lines/selection/numbers/highlights
    /// projection over the (taller) animation window.
    fn project_band(&self, s: &ScrollAnim, styles: &mut StyleTable) -> Value {
        let highlights = self.highlights_for(&s.numbers, styles);
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

    // ----- treesitter syntax integration ------------------------------------

    /// Handle a message from the syntax process. A restart forces a re-`open`;
    /// `ts_highlights` updates the span cache and repaints.
    fn on_syntax_event(&mut self, event: SyntaxEvent) {
        match event {
            SyntaxEvent::Restarted => {
                // A fresh worker holds no buffers, so every cached state is moot:
                // drop them all and let the next sync re-`open` the current buffer
                // (others re-open when next switched to).
                self.syntax_states.clear();
                self.syntax_dirty = true;
            }
            SyntaxEvent::Disabled => {
                // The supervisor gave up (worker won't spawn or keeps crashing).
                // Tell the user once — buffers stay editable, just un-highlighted.
                self.editor
                    .echo("treesitter: syntax worker unavailable, highlighting disabled");
                self.syntax_dirty = true;
            }
            // `ts_highlights` updates the cache; any other notification (e.g.
            // `ts_error` — a grammar that wouldn't load/parse) is ignored, so the
            // buffer simply stays un-highlighted and editing is unaffected.
            SyntaxEvent::Notification { method, params } if method == "ts_highlights" => {
                self.store_spans(&params);
                self.syntax_dirty = true;
            }
            SyntaxEvent::Notification { .. } => {}
        }
    }

    /// Decide what (if anything) to send the syntax process this frame for the
    /// *current* buffer: an `open` (first sync / resync / language change), an
    /// `edit` (text deltas), or a `view` (scroll only). Coalesces while a request
    /// is pending. Each buffer's state is keyed independently, so switching back
    /// to a buffer reuses its cached parse rather than re-opening.
    fn sync_syntax(&mut self, height: usize) {
        // Forget any buffers the editor has since deleted (frees worker memory).
        self.reap_closed_buffers();

        let buffer = self.editor.current_buffer_id();
        let language = filetype_of(self.editor.buffer().path.as_deref());
        // Language gone (no path / unknown extension): nothing to highlight.
        let Some(language) = language else {
            if let Some(state) = self.syntax_states.get_mut(&buffer) {
                state.language = None;
            }
            return;
        };
        self.syntax.ensure_started();

        // Work on this buffer's state as an owned local (so we can freely borrow
        // `self.editor` / `self.syntax` meanwhile), then put it back.
        let mut state = self.syntax_states.remove(&buffer).unwrap_or_default();
        let id = buffer.0;

        let line_count = self.editor.buffer().line_count();
        // Highlight a one-screen overscan above and below the viewport, so the
        // lines a scroll reveals are already cached and colored — no white flash
        // during the smooth-scroll animation (whose band spans up to ~2 screens).
        let first = self.editor.top.saturating_sub(height).min(line_count);
        let last = (self.editor.top + 2 * height).min(line_count);
        let tick = self.editor.buffer().changedtick;
        let language_changed = state.language != Some(language);
        state.language = Some(language);

        // A fresh language or un-opened buffer needs a full open.
        if language_changed || !state.opened {
            let _ = self.editor.buffer_mut().take_edits(); // superseded by full open
            let text = self.editor.buffer().text.to_string();
            self.syntax.open(id, tick, language, &text, first, last);
            state.opened = true;
            state.last_tick = tick;
            state.last_view = (first, last);
            state.pending = true;
        } else if tick != state.last_tick {
            // Text changed. Skip if a request is already in flight (the deltas
            // stay journaled and flush when its reply arrives).
            if !state.pending {
                let batch = self.editor.buffer_mut().take_edits();
                if batch.resync {
                    let text = self.editor.buffer().text.to_string();
                    self.syntax.open(id, tick, language, &text, first, last);
                } else {
                    self.syntax
                        .edit(id, tick, edits_value(&batch.edits), first, last);
                }
                state.last_tick = tick;
                state.last_view = (first, last);
                state.pending = true;
            }
        } else if (first, last) != state.last_view && !state.pending {
            // Text unchanged: re-query only if the viewport scrolled.
            self.syntax.view(id, first, last);
            state.last_view = (first, last);
            state.pending = true;
        }

        self.syntax_states.insert(buffer, state);
    }

    /// Send `ts_close` for, and drop the state of, every buffer the worker still
    /// tracks that the editor no longer has open (deleted via `:bdelete`).
    fn reap_closed_buffers(&mut self) {
        let live = self.editor.buffer_ids();
        let dead: Vec<BufferId> = self
            .syntax_states
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in dead {
            self.syntax_states.remove(&id);
            self.syntax.close(id.0);
        }
    }

    /// Replace a buffer's span cache from its `ts_highlights` reply, routing by
    /// the reply's `buffer` id. A reply for an unknown buffer (e.g. one closed
    /// while the request was in flight) is dropped.
    fn store_spans(&mut self, params: &[Value]) {
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        let buffer = BufferId(u64_at(map, "buffer", 0));
        // The buffer the reply is for must still be open; its line count bounds
        // which line keys we accept, so a bogus `line` (e.g. `u64::MAX` from a
        // buggy/hostile worker) can't seed a junk entry that lives forever.
        let Some(line_count) = self.editor.line_count_of(buffer) else {
            return;
        };
        let Some(state) = self.syntax_states.get_mut(&buffer) else {
            return;
        };
        state.pending = false;
        let spans = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("spans"))
            .and_then(|(_, v)| v.as_array());
        let mut cache: HashMap<usize, Vec<ByteSpan>> = HashMap::new();
        if let Some(spans) = spans {
            // Decode through the shared `SpanWire` so the wire tuple shape stays
            // in lockstep with the worker's encoder.
            for span in spans.iter().filter_map(SpanWire::decode) {
                if span.line >= line_count {
                    continue; // out-of-range line: never displayed, don't cache it
                }
                cache.entry(span.line).or_default().push(ByteSpan {
                    start: span.start_byte,
                    end: span.end_byte,
                    group: span.group,
                });
            }
        }
        state.spans = cache;
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
        // Spans for the buffer currently in the window (absent until its first
        // `ts_highlights` reply lands, or for a buffer with no grammar).
        let spans_by_line = self
            .syntax_states
            .get(&self.editor.current_buffer_id())
            .map(|state| &state.spans);
        let rows = numbers
            .iter()
            .map(|num| match num {
                Some(n) => {
                    let line_idx = n - 1;
                    let Some(spans) = spans_by_line.and_then(|m| m.get(&line_idx)) else {
                        return Value::Array(Vec::new());
                    };
                    let text = self.editor.buffer().line(line_idx);
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

/// Map a buffer's file extension to a treesitter language name. Unknown
/// extensions (and paths with none) yield `None` — no highlighting, and no
/// worker is spawned. This table is the seam where more languages plug in.
pub(crate) fn filetype_of(path: Option<&std::path::Path>) -> Option<&'static str> {
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
    // Go through the shared `EditWire` so the wire tuple shape is defined once,
    // in `nxvim-rpc`, and can't drift from the worker's decoder.
    let wire: Vec<EditWire> = edits
        .iter()
        .map(|e| EditWire {
            start_byte: e.start_byte,
            old_end_byte: e.old_end_byte,
            new_end_byte: e.new_end_byte,
            start_point: e.start_point,
            old_end_point: e.old_end_point,
            new_end_point: e.new_end_point,
            text: e.text.clone(),
        })
        .collect();
    encode_edits(&wire)
}

/// Encode a slice of text rows as a msgpack array of strings for the redraw map.
fn lines_value(lines: &[String]) -> Value {
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

/// Read a `u64` field from a msgpack map slice, falling back to `default` when
/// the key is absent or not an integer.
fn u64_at(map: &[(Value, Value)], key: &str, default: u64) -> u64 {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or(default)
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
