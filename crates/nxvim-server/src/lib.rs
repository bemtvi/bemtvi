//! The nxvim server: a headless editor process that owns the core model and
//! Lua runtime and exposes them over msgpack-RPC.
//!
//! This is the rust-native analogue of neovim's `main.c` + `event/` + `api/`.
//! It runs on a single thread with an async runtime: the RPC reader/writer are
//! independent tasks, while the server loop processes one message at a time
//! against the (non-`Send`) editor and Lua state. Clients (the TUI today, a
//! native GUI later) attach over the same RPC channel and are never blocked by
//! the server's bookkeeping.
//!
//! [`run`] hosts the `select!` loop; the [`Server`] state and its behavior are
//! split across focused sibling modules: [`dispatch`] (the RPC surface),
//! [`input`] (keystrokes/mappings), [`excmd`] (ex-commands), [`lifecycle`]
//! (autocmd emission), [`effects`] (draining queued Lua side effects),
//! [`redraw`] (View→wire projection), [`treesitter`] (highlight projection), and
//! [`lsp`] (language-server integration).

mod clipboard;
mod dispatch;
mod effects;
mod evloop;
mod excmd;
mod extmarks;
mod input;
mod keymap;
mod lifecycle;
mod lsp;
mod redraw;
mod treesitter;

use evloop::EventLoop;
use keymap::{BuiltinAction, Keymaps, NativeDefault};
use lsp::{CompletionMenu, LspDocState, LspReqKind, PendingLspReq, ServerRuntime};
use nxvim_core::{BufferId, Editor, Key, Mode, TabId, WindowId};
use nxvim_lsp::{CodeActionData, LspManager, ServerKey};
use nxvim_lua::LuaRuntime;
use nxvim_rpc::{connect, Rpc};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use treesitter::SyntaxState;

/// Startup options for the server.
///
/// Not `Clone`/`Debug`: [`ClipboardProvider::Custom`] holds a trait object that
/// is neither. No caller needs those — every construction site builds a fresh
/// value (`..Default::default()`) and moves it straight into [`run`].
#[derive(Default)]
pub struct ServerInit {
    /// File to open in the initial buffer, if any.
    pub file: Option<String>,
    /// Config directory whose `init.lua` is sourced at startup (`None` to skip).
    pub config_dir: Option<PathBuf>,
    /// Directories Lua searches for modules and runtime files (the runtimepath).
    pub runtimepath: Vec<PathBuf>,
    /// What backs the system-clipboard registers `"+` / `"*`. Defaults to
    /// [`ClipboardProvider::Disabled`] so tests are deterministic (no host
    /// clipboard); the real binary sets [`ClipboardProvider::System`].
    pub clipboard: ClipboardProvider,
    /// A fake millisecond clock for the mouse multi-click timestamp. `None` (the
    /// default) uses the real monotonic clock; a test injects a shared counter here
    /// and advances it between clicks to drive `'mousetime'` deterministically,
    /// without depending on wall-clock timing. See [`Server::mouse_stamp_ms`].
    pub mouse_clock: Option<Arc<AtomicU64>>,
}

/// How the server provides the `"+` / `"*` clipboard registers.
#[derive(Default)]
pub enum ClipboardProvider {
    /// Best-effort real host clipboard (the binary's choice). If no clipboard
    /// tool is found on this platform, the registers stay unavailable and error
    /// loudly on use rather than silently falling back to the unnamed register.
    System,
    /// No provider — `"+` / `"*` error loudly. The default, so bare-server tests
    /// never touch the host clipboard unless they opt in.
    #[default]
    Disabled,
    /// A caller-supplied provider; tests inject an in-memory fake here.
    Custom(Box<dyn nxvim_core::Clipboard>),
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

/// A window's `(id, (x, y, width, height))` rect snapshot, the unit of the
/// [`Server::last_window_rects`] diff that fires `WinResized`.
type WindowRect = (WindowId, (usize, usize, usize, usize));

struct Server {
    editor: Editor,
    lua: LuaRuntime,
    rpc: Rpc,
    /// Attached UI dimensions `(width, height)`, once a client has attached.
    ui: Option<(usize, usize)>,
    /// Per-buffer highlight memo, keyed by buffer id (created lazily on first
    /// redraw of a buffer, dropped when the buffer is deleted). The parse tree
    /// itself lives in the editor's [`nxvim_core::SyntaxEngine`]; this is only the
    /// slim span cache the redraw projects.
    syntax_states: HashMap<BufferId, SyntaxState>,
    /// Languages whose *on-disk* treesitter queries have already been resolved
    /// through the Lua runtimepath and offered to the engine (the buffer-open half
    /// of the query bridge). Guards the resolve to once per language — a pure
    /// `after/queries` / `;extends` overlay is merged by Lua the first time a buffer
    /// of that language is about to be highlighted, never re-resolved per redraw.
    ts_resolved_langs: HashSet<String>,
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
    /// once rather than on every redraw (a lazy-start guard).
    lsp_ensured: HashSet<ServerKey>,
    /// The next LSP client id to assign. Each `(name, root)` server gets one,
    /// stable across respawns (reused when its runtime is replaced), and it is
    /// the handle `LspAttach`'s `data.client_id` carries to Lua (Slice 3).
    next_lsp_client_id: u64,
    /// Set when an LSP event changed something the client should see (e.g. a fresh
    /// `Initialized` that should trigger a `didOpen`). Coalesced per loop turn so a
    /// burst of replies costs one repaint.
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
    /// Whether diagnostic underline spans are painted, toggled by
    /// `vim.diagnostic.config({ underline = … })` (Slice 2). Default `true`; the
    /// one diagnostic-config key with a backing surface in nxvim.
    diagnostics_underline: bool,
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
    /// The focused window at the last lifecycle diff; `None` until the startup
    /// seed. A change fires `WinLeave`(old) → `WinEnter`(new), bracketing the
    /// buffer events a window switch causes (Phase 5).
    last_window_id: Option<WindowId>,
    /// Every window id seen at the last diff, in layout order. Ids added since
    /// fire `WinNew`; ids gone fire `WinClosed`.
    known_windows: Vec<WindowId>,
    /// Each window's `(id, x, y, w, h)` rect at the last diff; a change fires
    /// `WinResized` (splits, `<C-w>`-resizes, terminal resizes). `None` until the
    /// seed so the first emit doesn't spuriously fire it.
    last_window_rects: Option<Vec<WindowRect>>,
    /// The active tab at the last lifecycle diff; `None` until the startup seed. A
    /// change fires `TabLeave`(old) → … → `TabEnter`(new), bracketing the window
    /// events the switch causes (`TabLeave → WinLeave → … → WinEnter → TabEnter`).
    last_tab_id: Option<TabId>,
    /// Every tab id seen at the last diff, in tabline order. Ids added since fire
    /// `TabNew`; ids gone fire `TabClosed`.
    known_tabs: Vec<TabId>,
    /// The user-mapping engine: per-mode tries + the withhold/replay buffer that
    /// `Server::input` runs every key through before `editor.input`. Rebuilt from
    /// `vim._keymaps` when its version advances (checked once per input batch).
    keymaps: Keymaps,
    /// The async Lua runtime's background actor (timers + child processes). Cheap
    /// to hold; its task spawns lazily on the first timer/`vim.system`. Commands go
    /// out fire-and-forget; completions return as [`LoopEvent`]s on a `select!` arm.
    evloop: EventLoop,
    /// Callback ids queued by `vim.schedule`, drained inside `run_pending` so a
    /// scheduled fn runs at the end of the current convergence (not nested in its
    /// caller). A scheduled fn may schedule more, so this feeds the fixpoint loop.
    scheduled: VecDeque<u64>,
    /// Per-buffer `changedtick` last copied into the `vim._bufs` Lua mirror
    /// ([`Server::push_buf_mirror`]), so an unchanged buffer's line array isn't
    /// re-serialized on every Lua entry — only the cheap cursor/window fields
    /// refresh each time (Phase 6).
    buf_mirror_ticks: HashMap<BufferId, u64>,
    /// Per-buffer line count last mirrored, so [`Server::push_buf_mirror`] can pass
    /// the old line count as `on_lines`' `lastline` when an attached buffer changes
    /// (`nvim_buf_attach`). Tracked only to fire faithful buffer-change callbacks —
    /// telescope drives its prompt filtering off `on_lines`.
    buf_mirror_lines: HashMap<BufferId, usize>,
    /// Per-buffer undo fingerprint last serialized into the `vim._undotree` Lua
    /// mirror ([`Server::push_undotree_mirror`]), so an unchanged tree isn't
    /// re-projected on every Lua entry — only edits/undo/redo rebuild it.
    undo_mirror_versions: HashMap<BufferId, (u64, usize, u64, bool)>,
    /// Monotonic base for the editor's time: `start.elapsed()` seconds are stamped
    /// onto undo nodes and handed to `vim.fn.localtime()`. Monotonic so elapsed
    /// labels survive wall-clock jumps; see [`Editor::set_now_mono`].
    start: std::time::Instant,
    /// Optional fake clock for the mouse multi-click timestamp ([`ServerInit::mouse_clock`]);
    /// when set, [`Server::mouse_stamp_ms`] reads it instead of `start.elapsed()`.
    mouse_clock: Option<Arc<AtomicU64>>,
    /// The highlight-registry [`generation`](nxvim_core::highlight::Highlights::generation)
    /// last folded into the `vim._hl_defs` Lua mirror ([`Server::push_buf_mirror`]).
    /// The mirror (potentially hundreds of groups) is re-pushed only when this
    /// changes — a colorscheme load, a `:hi`/`nvim_set_hl` — so the common chunk
    /// pays nothing for `nvim_get_hl` support. `None` until the first push.
    hl_mirror_gen: Option<u64>,
    /// The `vim._cb_fns` id of the `vim.ui.input` callback awaiting the open
    /// command-line prompt's result, or `None` when no scripted prompt is open
    /// (Phase 8). Set when a prompt opens; taken when the user submits/cancels.
    pending_ui_input: Option<u64>,
    /// The `vim._cb_fns` id of a coroutine parked on `vim.fn.getcharstr()`, or
    /// `None`. While set, the next key the server processes is delivered to this
    /// callback (resuming the coroutine) instead of being routed to the matcher —
    /// nxvim's stand-in for vim's blocking `getchar()` reading the typeahead.
    pending_getchar: Option<u64>,
    /// Keys queued by `nvim_feedkeys`, drained after the input batch / off-tick
    /// settle. Each carries whether it should be remapped (the `m` flag) or fed
    /// straight to the editor (the `n` flag). `nvim_feedkeys` with the `i` flag
    /// pushes to the front; otherwise to the back.
    feed_buffer: VecDeque<(Key, bool)>,
}

/// Run the server over a connected stream until the client disconnects or the
/// editor quits.
pub async fn run<S>(stream: S, init: ServerInit) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    let (rpc, mut incoming) = connect(reader, writer);

    let mut editor = match init.file {
        Some(path) => Editor::open_or_named(path),
        None => Editor::new(),
    };
    // The editor owns the in-process treesitter engine and queries it
    // synchronously for highlights (and, later, indentation). It loads
    // installable grammars from the data dir at runtime; a buffer with no grammar
    // simply isn't highlighted.
    editor.set_syntax_engine(Box::new(nxvim_ts::Engine::new(nxvim_ts::data_dir())));
    // The `"+` / `"*` registers route through an injected clipboard provider.
    // `System` resolves a real host clipboard tool (best effort); `Custom` is a
    // caller-supplied fake (tests); `Disabled` installs nothing and lets `"+`
    // error loudly.
    match init.clipboard {
        ClipboardProvider::System => {
            if let Some(cb) = clipboard::SystemClipboard::detect() {
                editor.set_clipboard(Box::new(cb));
            }
        }
        ClipboardProvider::Custom(cb) => editor.set_clipboard(cb),
        ClipboardProvider::Disabled => {}
    }
    let lua =
        LuaRuntime::new(init.runtimepath).map_err(|e| anyhow::anyhow!("lua init failed: {e}"))?;
    let (lsp, mut lsp_events) = LspManager::new();
    let (evloop, mut loop_events) = EventLoop::new();

    let mut server = Server {
        editor,
        lua,
        rpc,
        ui: None,
        syntax_states: HashMap::new(),
        ts_resolved_langs: HashSet::new(),
        lsp,
        lsp_states: HashMap::new(),
        lsp_servers: HashMap::new(),
        lsp_ensured: HashSet::new(),
        next_lsp_client_id: 1,
        lsp_dirty: false,
        lsp_req_gen: 0,
        lsp_requests: HashMap::new(),
        completion: None,
        lsp_code_actions: Vec::new(),
        diagnostics_underline: true,
        last_buffer_id: None,
        announced: HashSet::new(),
        last_mode: Mode::Normal,
        last_window_id: None,
        known_windows: Vec::new(),
        last_window_rects: None,
        last_tab_id: None,
        known_tabs: Vec::new(),
        keymaps: Keymaps::default(),
        evloop,
        scheduled: VecDeque::new(),
        buf_mirror_ticks: HashMap::new(),
        buf_mirror_lines: HashMap::new(),
        undo_mirror_versions: HashMap::new(),
        start: std::time::Instant::now(),
        mouse_clock: init.mouse_clock,
        hl_mirror_gen: None,
        pending_ui_input: None,
        pending_getchar: None,
        feed_buffer: VecDeque::new(),
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
    // Seed the buffer mirror too, so `init.lua` can read buffer lines / the cursor.
    server.push_buf_mirror();

    // Source the user's `init.lua` (if any) before serving the client, exactly
    // as neovim runs config at startup: its options, mappings, and colorscheme
    // are in place by the time the first `redraw` goes out on UI attach.
    if let Some(config_dir) = &init.config_dir {
        server.source_init(&config_dir.join("init.lua"));
    }

    // Startup seed: the initial buffer and the config's autocmds both exist now,
    // so fire the first buffer's lifecycle events (`BufReadPost`→`FileType`→
    // `BufEnter` for a file arg, `BufEnter` alone for the bare `[No Name]`).
    // Pre-seed the window set so the first window doesn't fire `WinNew` (neovim
    // skips it for the initial window); `last_window_id` stays `None` so the
    // first `WinEnter` still fires alongside `BufEnter`, the window analogue.
    server.known_windows = server.editor.window_ids();
    server.last_window_rects = Some(server.window_rects_snapshot());
    // Pre-seed the tab set so the initial tab doesn't fire `TabNew` (neovim, like
    // for the first window, doesn't); `last_tab_id` stays `None` so a later switch
    // still fires the first `TabEnter`/`TabLeave` pair.
    server.known_tabs = server.editor.tab_ids();
    server.emit_lifecycle_events();
    server.run_pending();
    // The startup VimEnter point has passed: `v:vim_did_enter` is now 1, so a
    // plugin that gates "the editor has finished starting" reads it as true.
    let _ = server.lua.set_vim_did_enter(true);

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
            // Replies from the language servers (initialize handshakes, published
            // diagnostics, server exits, log messages). Selecting here keeps the
            // editor responsive regardless of any server's speed or health.
            Some(event) = lsp_events.recv() => {
                server.on_lsp_event(event);
                // Coalesce a burst into a single repaint, as for syntax events.
                while let Ok(event) = lsp_events.try_recv() {
                    server.on_lsp_event(event);
                }
                // A reply handled by a Lua callback (Phase 4 seam) may defer work
                // via `vim.cmd` / `vim.schedule`; drive it to convergence and
                // repaint. Closes the latent gap where an `on_init`/`LspAttach`
                // callback's deferred work wasn't driven off-tick.
                let dirty = std::mem::take(&mut server.lsp_dirty);
                server.settle_events(dirty);
            }
            // Timers and child-process completions from the event-loop actor — the
            // first thing that wakes the server on wall-clock time rather than RPC.
            // The matching Lua callback runs here, on the one server thread.
            Some(event) = loop_events.recv() => {
                server.on_loop_event(event);
                // Coalesce a burst (a flurry of timers, several processes exiting)
                // into one settle + repaint, like the syntax/LSP arms.
                while let Ok(event) = loop_events.try_recv() {
                    server.on_loop_event(event);
                }
                server.settle_events(true);
            }
        }
    }
    Ok(())
}

/// Map a buffer's file extension to a treesitter language / filetype name (the
/// FileType autocmd and LSP server selection use this too). Delegates to
/// [`nxvim_core::language_of_path`] so the table lives in exactly one place — the
/// editor needs the same mapping to drive its in-process treesitter engine.
pub(crate) fn filetype_of(path: Option<&std::path::Path>) -> Option<&'static str> {
    nxvim_core::language_of_path(path)
}
