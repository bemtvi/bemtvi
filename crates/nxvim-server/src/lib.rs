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
mod daemon;
mod decoration;
mod dispatch;
mod edithost;
mod effects;
mod evloop;
mod excmd;
mod extmarks;
mod host;
mod inbound;
mod input;
mod keymap;
mod lifecycle;
mod lsp;
mod quic;
mod redraw;
mod save;
mod shada;
mod treesitter;

/// The process-spawning seam (`vim.system` / `jobstart` / `:!`) and its types,
/// re-exported for [`ServerInit::host_proc`] — the edit-host split injects a
/// daemon-backed [`HostProc`] here (the process-side companion to
/// [`nxvim_core::HostFs`]).
pub use host::{HostProc, ProcEvents, ProcSpec, StdHostProc};
/// The persistence (shada) seam and its native redb backend. The store sits
/// behind [`ShadaStore`] so the platform layer injects it through
/// [`ServerInit::shada`] — native binaries pass [`default_shada`] (redb over a
/// file at [`shada_dir`]); the wasm Worker build will pass a redb-over-OPFS store;
/// tests pass a [`RedbFileStore`] over a temp dir, or `None` to disable.
pub use shada::{default_shada, is_store_file, shada_dir, RedbFileStore, ShadaStore};

/// The daemon wire protocol for the edit-host split: the daemon-side servers
/// ([`serve_daemon`] for child processes, [`serve_fs_daemon`] for file reads,
/// [`serve_sys_daemon`] for the blocking `vim.system` shell-out) and the edit-host-side
/// clients ([`RemoteHostProc`], [`RemoteHostFs`], [`RemoteBlockingSystem`]) that forward
/// to them over any [`AsyncRead`](tokio::io::AsyncRead)/[`AsyncWrite`](tokio::io::AsyncWrite)
/// wire (a duplex, or ssh stdio to `nxvim --daemon`). [`HostFsAsync`] is the async fs
/// seam the server fetches buffer contents through off the editor tick; [`FsRead`] is
/// what one fetch resolves to.
pub use daemon::{
    connect_daemon, serve_daemon, serve_fs_daemon, serve_fs_daemon_on, serve_lsp_daemon,
    serve_lsp_daemon_on, serve_luafs_daemon, serve_luafs_daemon_on, serve_proc_daemon_on,
    serve_sys_daemon, serve_sys_daemon_on, DaemonClient, FsRead, HostFsAsync, RemoteBlockingSystem,
    RemoteHostFs, RemoteHostProc, RemoteLspTransport, RemoteLuaFs, WatchEvent,
};

/// The native daemon transport (Open Decision #2): a WebTransport/QUIC listener that
/// runs the [`run_daemon_io`] multiplexer over one bidi stream ([`serve_quic`], the
/// `--daemon --listen` role), and the edit-host-side [`connect_quic`] that pins the
/// daemon's self-signed cert TOFU + presents the launch-minted bearer token and returns
/// the same [`DaemonClient`] `connect_daemon` does over stdio. [`bind_quic_listener`]
/// mints the identity/token and resolves the bound address (for an ephemeral `:0` port).
pub use quic::{bind_quic_listener, connect_quic, mint_token, serve_quic, ListenerInfo};

use edithost::{HostEffects, NativeEffects};
use evloop::EventLoop;
use keymap::{BuiltinAction, Keymaps, NativeDefault};
use lsp::{
    CompletionMenu, DiagnosticConfig, InlayResolveTarget, LspDocState, LspReqKind, PendingLspReq,
    ServerRuntime,
};
use nxvim_core::{
    BufferId, Editor, FileStat, HostFs, Key, Mode, PendingSave, StdHostFs, TabId, WindowId,
};
use nxvim_lsp::{CodeActionData, LspManager, ServerKey};
use nxvim_lua::LuaRuntime;
use nxvim_rpc::{connect, Incoming};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
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
    /// The persistence (shada) store. `None` (the default) disables persistence
    /// entirely — so tests that don't opt in never touch the real state dir and
    /// stay hermetic. The native binaries inject [`default_shada`] (redb under
    /// `stdpath("state")/shada`); the wasm Worker build injects a redb-over-OPFS
    /// store; a test injects a [`RedbFileStore`] over a temp dir. `Send` (boxed) so
    /// it rides [`ServerInit`] onto the server's own thread. See
    /// `docs/plans/2026-06-11-shada-persistence.md`.
    pub shada: Option<Box<dyn ShadaStore + Send>>,
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
    /// The filesystem backend the editor reads and writes buffers through. `None`
    /// (the default) uses the local disk ([`StdHostFs`]); the edit-host split will
    /// inject a daemon-backed [`HostFs`] here so buffer I/O — including the initial
    /// file — crosses the wire while editing stays local
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). It is
    /// `Send` (boxed) because [`ServerInit`] is moved onto the server's own thread;
    /// it is rebuilt into the editor's single-threaded `Rc<dyn HostFs>` there.
    pub host_fs: Option<Box<dyn HostFs + Send>>,
    /// The seam child processes (`vim.system` / `jobstart` / `:!`) are spawned
    /// through. `None` (the default) spawns real local processes
    /// ([`StdHostProc`](host::StdHostProc)); the edit-host split will inject a
    /// daemon-backed [`HostProc`](host::HostProc) here so processes run on the
    /// remote while editing stays local
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). `Send`
    /// (boxed) so it rides [`ServerInit`] onto the server's own thread; it is
    /// rebuilt into the shared `Arc<dyn HostProc>` the event-loop actor holds
    /// there.
    pub host_proc: Option<Box<dyn HostProc + Send>>,
    /// The **async** filesystem the *initial buffer* is fetched through, off the
    /// editor tick — the daemon-backed analog of the sync [`host_fs`](Self::host_fs)
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). `None` (the
    /// default) opens the startup file synchronously through `host_fs` as before;
    /// when set, the editor starts empty and the server fetches [`file`](Self::file)'s
    /// bytes over the wire *after* the loop begins, then loads them into a replica
    /// buffer — so a slow remote read never freezes startup. `Send` (boxed) to ride
    /// onto the server thread, where it is rebuilt into an `Arc<dyn HostFsAsync>`.
    /// (When set, the initial open, `:edit` (Phase 3f), and `:write` (Phase 3e) all
    /// cross this seam off-tick; the explorer and `:tabnew`/LSP go-to still use the
    /// sync `host_fs`.)
    pub host_fs_async: Option<Box<dyn HostFsAsync + Send>>,
    /// The backend the **blocking** `vim.system(...):wait()` shell-out runs through.
    /// `None` (the default) spawns the process locally
    /// ([`StdBlockingSystem`](nxvim_lua::StdBlockingSystem)); the edit-host split injects
    /// a daemon-backed [`RemoteBlockingSystem`] here so a synchronous `root_dir`
    /// shell-out (`cargo metadata`) runs on the remote where the project files are
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3, Open Decision
    /// #5's blocking-bridge note). `Send` (boxed) so it rides [`ServerInit`] onto the
    /// server's own thread, where it is rebuilt into the Lua runtime's
    /// `Rc<dyn BlockingSystem>`. Unlike the off-tick fs/process seams, this one parks the
    /// editor thread on the reply (the call is synchronous) — its wire's RPC tasks live
    /// on their own thread so that park can't deadlock.
    pub blocking_system: Option<Box<dyn nxvim_lua::BlockingSystem + Send>>,
    /// The transport language servers are spawned through. `None` (the default) runs
    /// them as real local children ([`LocalLspTransport`](nxvim_lsp::LocalLspTransport));
    /// the edit-host split injects a daemon-backed [`RemoteLspTransport`] here so a
    /// language server runs on the remote where the project files are, tunneling its
    /// long-lived stdio over the wire while editing stays local
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). `Send` (boxed)
    /// so it rides [`ServerInit`] onto the server's own thread, where it is rebuilt into
    /// the shared `Arc<dyn LspTransport>` the [`LspManager`] holds.
    pub lsp_transport: Option<Box<dyn nxvim_lsp::LspTransport + Send>>,
    /// The backend the **project-facing** Lua filesystem surface (`vim.uv.fs_*`,
    /// `vim.fn.readblob`/`glob`/`filereadable`/`executable`/…) runs through. `None`
    /// (the default) hits the local disk via the persistent
    /// [`StdLuaFs`](nxvim_lua::StdLuaFs); the edit-host split injects a daemon-backed
    /// [`RemoteLuaFs`] here so a plugin reads the *remote* project (telescope previews,
    /// LSP `root_dir` detection, gitsigns) instead of the local machine
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → *The full split*,
    /// *Lua-visible filesystem semantics*). Like [`blocking_system`](Self::blocking_system)
    /// it is a synchronous blocking bridge: each call parks the editor thread on the
    /// daemon reply, its wire's RPC tasks on their own thread. `Send` (boxed) so it rides
    /// [`ServerInit`] onto the server thread, where it is rebuilt into the Lua runtime's
    /// `Rc<dyn LuaFs>`.
    pub lua_fs: Option<Box<dyn nxvim_lua::LuaFs + Send>>,
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
    /// The outbound async-effect seam (Phase 4, Open Decision #6 (a)): the editor
    /// tick pushes redraws / notifications / responses to the client and hands
    /// timer / process / watch commands to the event-loop actor *through* this,
    /// never touching the [`Rpc`] or [`EventLoop`] directly. [`NativeEffects`] is
    /// today's behavior verbatim; the wasm build swaps in a JS-interop + daemon-link
    /// implementor. See [`edithost`].
    fx: Box<dyn HostEffects>,
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
    /// In-flight `inlayHint/resolve`s, keyed by the `cb_id` their token carries.
    /// Unlike the single-slot `lsp_requests`, many lazy hints can resolve at once,
    /// so each gets a distinct `cb_id` (from `inlay_resolve_seq`) and routes back
    /// by it — the [`InlayResolveTarget`] records which placeholder span to fill.
    inlay_resolves: HashMap<u64, InlayResolveTarget>,
    /// Monotonic source of `cb_id`s for `inlay_resolves` (never reused, so a stale
    /// reply for a superseded resolve finds no target and is dropped).
    inlay_resolve_seq: u64,
    /// The open insert-mode completion popup (Phase 5), or `None`. Server-owned
    /// like the diagnostics cache; projected into the `pmenu` redraw key and
    /// driven by the popup-open key routing in [`Server::completion_menu_key`].
    completion: Option<CompletionMenu>,
    /// The code actions currently listed in the `:LspCodeAction` panel (Phase 6),
    /// indexed by panel select. A `<CR>` on row `i` applies `lsp_code_actions[i]`'s
    /// edit; cleared on apply. Empty when no code-action panel is active.
    lsp_code_actions: Vec<CodeActionData>,
    /// The `vim.diagnostic.config` keys with a backing surface — the underline
    /// spans and the inline virtual text — toggled by `vim.diagnostic.config`.
    diag_config: DiagnosticConfig,
    /// The editor-wide semantic-tokens gate (Phase 3), toggled by
    /// `vim.lsp.semantic_tokens.enable`. Default on; `false` hides the semantic
    /// paint everywhere and stops the refresh requests (the per-buffer
    /// `LspDocState::semantic_enabled` is the narrower override).
    semantic_tokens_enabled: bool,
    /// The buffer that was current the last time lifecycle events were emitted;
    /// `None` until the startup seed. A change here means a `BufEnter` (fired on
    /// every entry).
    last_buffer_id: Option<BufferId>,
    /// Buffers that have already had their fire-once events (`BufReadPost` /
    /// `FileType`) emitted, so re-entering them doesn't re-announce.
    announced: HashSet<BufferId>,
    /// Every buffer id present at the last lifecycle diff. Ids gone since (a
    /// `:bdelete` / `nvim_buf_delete`) have their Lua-side buffer-local state
    /// (commands, keymaps) purged so a reused bufnr can't inherit it.
    known_buffers: Vec<BufferId>,
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
    /// Sender half handed to each `:TSInstall` background job (a `spawn_blocking`
    /// that fetches + compiles a grammar off the editor thread). Completions return
    /// on the matching `select!` arm, where the editor reloads the grammar and
    /// echoes the result — see [`Server::on_install_done`].
    install_tx: UnboundedSender<InstallOutcome>,
    /// Per-frame **ephemeral** extmarks placed by decoration providers, keyed by
    /// buffer. Rebuilt from scratch every redraw: cleared at the start of the
    /// provider drive ([`Server::run_decoration_providers`]), populated as each
    /// provider's `on_win` / `on_line` emits `ephemeral = true` marks, read by
    /// [`Server::extmark_intervals`] while projecting, and left until the next
    /// frame clears it. Separate from each buffer's persistent
    /// [`ExtmarkStore`](nxvim_core::ExtmarkStore) so single-frame decorations never
    /// touch undo/redo or the `nvim_buf_get_extmarks` mirror.
    ephemeral_extmarks: HashMap<BufferId, nxvim_core::ExtmarkStore>,
    /// Monotonic redraw counter handed to decoration providers as the frame `tick`
    /// (neovim's display tick). Incremented once per [`Server::run_decoration_providers`].
    decor_tick: u64,
    /// Buffers with an off-tick write currently on the wire — at most one per buffer,
    /// so a buffer's overlapping `:w`s serialize (snapshot order = wire order) rather
    /// than racing. Cleared when the write acks.
    saves_inflight: HashSet<BufferId>,
    /// Off-tick writes waiting their turn behind an in-flight write to the *same*
    /// buffer, dispatched in order as each ack frees the slot. A failed write fails
    /// (and drops) the rest of its buffer's queue loudly.
    saves_queued: HashMap<BufferId, VecDeque<PendingSave>>,
    /// A `:wqa` / `:xa` quit deferred until every write of its `:wall` batch has acked
    /// (the multi-buffer save slice). `None` outside a pending batch-quit; while set, the
    /// save ack handler removes each seq as it lands and replays `:qa` once the set
    /// empties — and **cancels** the gate (drops it) if any write in the batch fails, so
    /// a failed multi-buffer save keeps the editor up exactly as a failed `:wq` does.
    quit_all_gate: Option<save::QuitAllGate>,
    /// The native file watch armed for each file-backed buffer, keyed by buffer and
    /// holding the `(path, disk-stat)` it was armed against. [`Server::sync_buffer_watches`]
    /// reconciles this against the live buffers every tick: a new file-backed buffer
    /// arms a watch, a closed one disarms, and a changed key (a reload/save gave the
    /// file a new identity) re-arms — so the watch follows the file across atomic
    /// replaces. Each watch's loop id is [`INTERNAL_WATCH_BASE`]` + buffer.0`, which
    /// the [`LoopEvent::FsEvent`] arm uses to route a change back to `checktime` for
    /// that buffer (vs. running a Lua `vim.uv.fs_event` callback). Local sessions
    /// only — a daemon session uses [`Server::remote_watches`] instead.
    buf_watches: HashMap<BufferId, (PathBuf, Option<FileStat>)>,
    /// The paths watched on the **daemon** (`HostWatch` leg) in a daemon session — the
    /// remote analogue of [`Server::buf_watches`]. [`Server::sync_buffer_watches`] arms a
    /// watch (`HostFsAsync::watch`) for each file-backed buffer's path and disarms a
    /// closed one; a `fs_changed` push for one reconciles off-tick. Empty in a local
    /// session (it arms `buf_watches` instead). The daemon owns change detection, so this
    /// holds only paths — no stat snapshot (unlike `buf_watches`).
    remote_watches: HashSet<String>,
    /// Buffers whose off-tick reload (the remote watch leg) is in flight, awaiting a
    /// `FileChangedShellPost` once the re-fetch lands in [`Server::apply_open`]. The
    /// remote reload can't be synchronous (it crosses the wire), so the post event is
    /// deferred to the fetch's completion rather than fired inline like the local path.
    reload_posts: HashSet<BufferId>,
}

/// Base for the loop ids of the server's **internal** per-buffer file watches, set
/// far above any Lua-allocated `vim.uv.fs_event` callback id so a [`LoopEvent::FsEvent`]
/// can be classified by `id >= INTERNAL_WATCH_BASE` alone. Buffer `b`'s watch id is
/// `INTERNAL_WATCH_BASE + b.0`, so the change routes straight back to the buffer with
/// no side table. (Lua callback ids are monotonic from 1 and never approach `1 << 48`.)
pub(crate) const INTERNAL_WATCH_BASE: u64 = 1 << 48;

/// A finished `:TSInstall` job: the requested language and the install result
/// (the report, or a loud error). Delivered from the blocking worker to the
/// server's `select!` loop.
type InstallOutcome = (String, anyhow::Result<nxvim_ts::install::InstallReport>);

/// Run the server over a connected stream until the client disconnects or the
/// editor quits.
pub async fn run<S>(stream: S, init: ServerInit) -> anyhow::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (reader, writer) = tokio::io::split(stream);
    run_io(reader, writer, init).await
}

/// Run the server over **separate** read/write halves. [`run`] (the public,
/// single-stream entry every front end uses) splits its stream and delegates here;
/// the two-half shape is kept so a transport whose directions are distinct objects
/// needn't `join` them only to be `split` straight back apart.
async fn run_io<R, W>(reader: R, writer: W, init: ServerInit) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, mut incoming) = connect(reader, writer);

    // The editor reads/writes buffers through this fs — the local disk by default,
    // or an injected (eventually daemon-backed) backend. Rebuilt here, on the
    // server thread, into the single-threaded `Rc<dyn HostFs>` the editor holds
    // (`ServerInit` carried it `Send` across the thread boundary).
    let host_fs: Rc<dyn HostFs> = match init.host_fs {
        // `Rc::from` yields `Rc<dyn HostFs + Send>`; returning it into the
        // `Rc<dyn HostFs>` binding drops the `Send` bound by unsize coercion.
        Some(fs) => {
            let fs: Rc<dyn HostFs + Send> = Rc::from(fs);
            fs
        }
        None => Rc::new(StdHostFs),
    };
    // The async (daemon) fs the *initial* buffer is fetched through, off the editor
    // tick. Rebuilt here into a shared `Arc<dyn HostFsAsync>` (Send dropped by unsize
    // coercion), mirroring the `host_proc` rebuild below. `None` = no daemon fs.
    let host_fs_async: Option<Arc<dyn HostFsAsync>> = init.host_fs_async.map(|fs| {
        // `Arc::from` yields `Arc<dyn HostFsAsync + Send>`; rebinding to the
        // `Arc<dyn HostFsAsync>` type drops the `Send` bound by unsize coercion.
        let fs: Arc<dyn HostFsAsync + Send> = Arc::from(fs);
        let fs: Arc<dyn HostFsAsync> = fs;
        fs
    });
    // When a daemon fs is present, defer the startup file: fetch its bytes *after*
    // the loop begins (so a slow remote read never freezes startup) and start with an
    // empty buffer. Otherwise open it synchronously through `host_fs` exactly as
    // before — the first buffer fetched the same way every later `:edit` is; a bare
    // session still installs the fs so a later `:edit` / `:write` routes through it.
    let deferred_open = host_fs_async.as_ref().and(init.file.clone());
    let mut editor = match (&host_fs_async, init.file) {
        // Daemon fs: start empty regardless of `file`; the fetch task below loads it.
        // A daemon session also does buffer I/O off-tick — `:w` snapshots and enqueues
        // a `PendingSave` (the save path, `save.rs`) and `:edit` enqueues a
        // `PendingOpen` fetch — instead of blocking the editor thread on the network,
        // so turn on off-tick filesystem mode here.
        (Some(_), _) => {
            let mut editor = Editor::new();
            editor.set_host_fs(host_fs);
            editor.set_host_fs_offtick(true);
            editor
        }
        (None, Some(path)) => Editor::open_or_named_with(path, host_fs),
        (None, None) => {
            let mut editor = Editor::new();
            editor.set_host_fs(host_fs);
            editor
        }
    };
    // The off-tick fetch of the deferred startup file: read its bytes over the wire
    // and deliver them (or a read error) into the loop, where they load into a
    // replica buffer. The same channel carries later `:edit` opens (each tagged with
    // the buffer to fill); a bare/local session leaves it idle. The startup file fills
    // the editor's initial `[No Name]` buffer, so tag it with that buffer's id.
    let (open_tx, mut open_rx) = unbounded_channel::<(BufferId, String, std::io::Result<FsRead>)>();
    if let (Some(fs), Some(path)) = (host_fs_async.as_ref(), deferred_open) {
        let fs = fs.clone();
        let startup_buf = editor.current_buffer_id();
        let open_tx = open_tx.clone();
        tokio::spawn(async move {
            let result = fs.read(path.clone()).await;
            let _ = open_tx.send((startup_buf, path, result));
        });
    }
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
    // The blocking `vim.system(...):wait()` shell-out runs through this seam — a local
    // spawn by default, or an injected daemon bridge so a `root_dir` shell-out runs on
    // the remote where the project files live. Rebuilt here, on the server thread, into
    // the Lua runtime's `Rc<dyn BlockingSystem>` (`ServerInit` carried it `Send` across
    // the thread boundary; the two-step drops `Send` by unsize coercion, as `host_fs`
    // does). `None` leaves the default local spawn in place — a bare/local session is
    // unchanged.
    if let Some(sys) = init.blocking_system {
        let sys: Rc<dyn nxvim_lua::BlockingSystem + Send> = Rc::from(sys);
        let sys: Rc<dyn nxvim_lua::BlockingSystem> = sys;
        lua.set_blocking_system(sys);
    }
    // The project-facing Lua filesystem surface (`vim.uv.fs_*` / `vim.fn` fs builtins)
    // runs through this seam — the local disk by default, or an injected daemon bridge
    // so a plugin sees the *remote* project. Rebuilt here, on the server thread, into the
    // Lua runtime's `Rc<dyn LuaFs>` (the same `Send`-dropping two-step). `None` leaves the
    // default persistent local `StdLuaFs` in place — a bare/local session is unchanged.
    if let Some(fs) = init.lua_fs {
        let fs: Rc<dyn nxvim_lua::LuaFs + Send> = Rc::from(fs);
        let fs: Rc<dyn nxvim_lua::LuaFs> = fs;
        lua.set_lua_fs(fs);
    }
    // Language servers are spawned through this transport — real local children by
    // default, or an injected daemon-backed tunnel. Rebuilt here, on the server thread,
    // into the shared `Arc<dyn LspTransport>` the manager holds (`ServerInit` carried it
    // `Send` across the thread boundary; the two-step drops `Send` by unsize coercion, as
    // the `host_proc` rebuild does). `None` keeps the default local spawn.
    let (lsp, mut lsp_events) = match init.lsp_transport {
        Some(transport) => {
            let transport: Arc<dyn nxvim_lsp::LspTransport + Send> = Arc::from(transport);
            LspManager::with_transport(transport)
        }
        None => LspManager::new(),
    };
    // Child processes are spawned through this seam — real local processes by
    // default, or an injected (eventually daemon-backed) backend. Rebuilt here,
    // on the server thread, into the shared `Arc<dyn HostProc>` the event-loop
    // actor holds (`ServerInit` carried it `Send` across the thread boundary).
    let host_proc: Arc<dyn HostProc> = match init.host_proc {
        // `Arc::from` yields `Arc<dyn HostProc + Send>`; returning it into the
        // `Arc<dyn HostProc>` binding drops the `Send` bound by unsize coercion
        // (the same two-step the `host_fs` rebuild above uses for `Rc`).
        Some(proc) => {
            let proc: Arc<dyn HostProc + Send> = Arc::from(proc);
            proc
        }
        None => Arc::new(StdHostProc),
    };
    let (evloop, mut loop_events) = EventLoop::new(host_proc);
    // `:TSInstall` runs the fetch+compile off-thread (`spawn_blocking`); results
    // come back here and are applied on the one server thread.
    let (install_tx, mut install_events) = unbounded_channel::<InstallOutcome>();
    // Off-tick `:w`s (the daemon save path) push their bytes over the wire from a
    // spawned task; the finished write comes back here and finalizes on the one
    // server thread. Idle for a local/bare session (no daemon fs → no off-tick saves).
    let (save_done_tx, mut save_done_rx) = unbounded_channel::<save::SaveDone>();
    // The `HostWatch` leg: the daemon pushes `fs_changed`, the [`RemoteHostFs`] demux
    // forwards each into this channel, and the `watch_rx` `select!` arm reconciles it
    // off the editor tick. Created unconditionally (idle for a local/bare session) so
    // the arm is always valid; `watch_tx` stays bound here for the whole loop, keeping
    // the channel open (so a daemon push that arrives before any local change can't
    // close it). A daemon session spawns a forwarder from the fs's own receiver.
    let (watch_tx, mut watch_rx) = unbounded_channel::<WatchEvent>();
    if let Some(mut rx) = host_fs_async.as_ref().and_then(|fs| fs.take_watch_events()) {
        let watch_tx = watch_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if watch_tx.send(ev).is_err() {
                    break;
                }
            }
        });
    }

    let mut server = Server {
        editor,
        lua,
        // The native outbound-effect seam: the client wire ([`Rpc`]), the event-loop
        // actor ([`EventLoop`]), the off-tick daemon fs (read/write/watch + the
        // `open_tx` / `save_done_tx` deliveries), and the LSP command sink ([`LspManager`])
        // the editor tick fires through. The wasm build (Phase 5) swaps a JS-interop +
        // daemon-link implementor here.
        fx: Box::new(NativeEffects::new(
            rpc,
            evloop,
            host_fs_async,
            open_tx,
            save_done_tx,
            lsp,
        )),
        ui: None,
        syntax_states: HashMap::new(),
        ts_resolved_langs: HashSet::new(),
        lsp_states: HashMap::new(),
        lsp_servers: HashMap::new(),
        lsp_ensured: HashSet::new(),
        next_lsp_client_id: 1,
        lsp_dirty: false,
        lsp_req_gen: 0,
        lsp_requests: HashMap::new(),
        inlay_resolves: HashMap::new(),
        inlay_resolve_seq: 0,
        completion: None,
        lsp_code_actions: Vec::new(),
        diag_config: DiagnosticConfig::default(),
        semantic_tokens_enabled: true,
        last_buffer_id: None,
        announced: HashSet::new(),
        known_buffers: Vec::new(),
        last_mode: Mode::Normal,
        last_window_id: None,
        known_windows: Vec::new(),
        last_window_rects: None,
        last_tab_id: None,
        known_tabs: Vec::new(),
        keymaps: Keymaps::default(),
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
        install_tx,
        ephemeral_extmarks: HashMap::new(),
        decor_tick: 0,
        saves_inflight: HashSet::new(),
        saves_queued: HashMap::new(),
        quit_all_gate: None,
        buf_watches: HashMap::new(),
        remote_watches: HashSet::new(),
        reload_posts: HashSet::new(),
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
    // Then source the package `plugin/` / `after/plugin/` Lua scripts across the
    // runtimepath — neovim's startup package load, after `init.lua` and before the
    // first buffer's lifecycle events, so a plugin's autocmds/registration are in
    // place (this is what initializes nvim-cmp's engine, cmp-buffer's source, etc.).
    server.source_plugins();

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
    // Seed the buffer set too, so the startup buffer isn't seen as "newly gone"
    // and a never-deleted buffer never triggers a spurious cleanup.
    server.known_buffers = server.editor.buffer_ids();
    // Load the shada (persistence) store before the first frame: it recency-merges
    // + compacts any sibling stores and returns this session's registers (later:
    // marks, history, jumplist) to seed, so a plugin reading them at `VimEnter`
    // sees the restored state. `None` = persistence disabled (the test default,
    // unless a test opts in). A store that won't load is surfaced and then dropped
    // (no flush at exit) — the editor runs on without persistence rather than dying.
    let mut shada = init.shada;
    if let Some(store) = shada.as_mut() {
        match store.load() {
            Ok(state) => server.editor.import_persist(state),
            Err(e) => {
                server
                    .editor
                    .echo(format!("shada: could not open store: {e}"));
                shada = None;
            }
        }
    }
    server.emit_lifecycle_events();
    server.run_pending();
    // The startup VimEnter point has passed: `v:vim_did_enter` is now 1, so a
    // plugin that gates "the editor has finished starting" reads it as true.
    let _ = server.lua.set_vim_did_enter(true);

    // The run loop is a thin translator: each arm receives one event off a transport and
    // hands the whole batch to an inbound-seam handler (`inbound.rs`), which coalesces the
    // channel, runs the per-event tick method, and settles. No arm touches editor / Lua
    // state directly — that's the property the `EditHost` hoist (Phase 4e) needs.
    loop {
        tokio::select! {
            // Editor input / API calls from the UI client.
            message = incoming.recv() => {
                let Some(message) = message else { break };
                if server.on_client_message(message).await {
                    break;
                }
            }
            // Replies from the language servers (initialize handshakes, published
            // diagnostics, server exits, log messages). Selecting here keeps the
            // editor responsive regardless of any server's speed or health.
            Some(event) = lsp_events.recv() => server.on_lsp_events(event, &mut lsp_events),
            // Timers and child-process completions from the event-loop actor — the
            // first thing that wakes the server on wall-clock time rather than RPC.
            Some(event) = loop_events.recv() => server.on_loop_events(event, &mut loop_events),
            // Bytes for an off-tick open arrived from the daemon's fs — the startup
            // file (kept from freezing startup) or a later `:edit`. Idle for a
            // bare/local session.
            Some(open) = open_rx.recv() => server.on_opens(open, &mut open_rx),
            // A `:TSInstall` background job finished (grammar fetched + compiled, or it
            // failed): reload the grammar so open buffers re-highlight/indent, echo.
            Some(outcome) = install_events.recv() => server.on_installs(outcome, &mut install_events),
            // An off-tick `:w` finished on the daemon (the save path): finalize the
            // buffer's saved-state and replay any deferred `:wq`/`:x` quit. The replayed
            // quit can ask the editor to exit — the one non-input arm that can.
            Some(done) = save_done_rx.recv() => {
                if server.on_save_dones(done, &mut save_done_rx) {
                    break;
                }
            }
            // The daemon's watch leg pushed a file change (`HostWatch`): reconcile it off
            // the editor tick. Idle for a local/bare session (nothing ever sends here).
            Some(ev) = watch_rx.recv() => server.on_watch_events(ev, &mut watch_rx),
        }
    }
    // The loop has exited (quit or client disconnect): flush the final snapshot to
    // this instance's store, then drop it (releasing the file lock) so the next
    // instance can merge this one's clean checkpoint. Best-effort — we're leaving.
    if let Some(store) = shada.as_mut() {
        if let Err(e) = store.flush(&server.editor.export_persist()) {
            eprintln!("shada: final flush failed: {e}");
        }
    }
    Ok(())
}

/// Run the **daemon** role (`nxvim --daemon`) over separate read/write halves
/// (this process's `stdin` + `stdout`): serve every leg of the edit-host wire — fs
/// reads/writes, the watch push, child processes, the blocking `vim.system`
/// shell-out, language servers, and the Lua-visible filesystem — against *this*
/// host's real disk and processes. Unlike [`run`] there is **no editor, no Lua,
/// and no config sourcing**: the daemon is pure I/O, and LSP/process discovery
/// (program/args/cwd) plus the project tree all arrive on the wire from the local
/// edit-host.
///
/// **The multiplexer (the one new mechanism).** Every `serve_*` leg was written
/// assuming it owns the whole transport — each calls `connect` itself, which is how
/// the per-leg tests drive it over a private duplex. Here all six classes share one
/// ordered stdio stream (the ssh hop), so `connect` runs *once* and a demux loop fans
/// each inbound message to its leg's connection-agnostic `*_on` core by method
/// namespace (`fs_*` / `proc_*` / `sys_run` / `lsp_*` / `luafs` — disjoint, so the
/// routing is unambiguous). Every leg writes back through a clone of the single shared
/// [`Rpc`], whose one out-channel serializes the concurrent replies; request responses
/// (`fs_read`/`fs_write`/`sys_run`/`luafs`) are msgid-routed *inside* `Rpc` and never
/// surface here. EOF on `reader` (the edit-host hung up) ends the loop, drops the
/// per-leg senders so each leg winds down and reaps its children, and awaits them.
pub async fn run_daemon_io<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    use nxvim_lua::{StdBlockingSystem, StdLuaFs};
    use tokio::sync::mpsc::unbounded_channel;

    let (rpc, mut incoming) = connect(reader, writer);

    // One forwarding channel per leg; each leg runs its existing loop over its own
    // demuxed inbound stream and a clone of the shared `Rpc`. The daemon backs every
    // leg with the same `Std*` impl the local server uses, so a file/process/server
    // behaves identically run here or across the wire.
    let (fs_tx, fs_rx) = unbounded_channel();
    let (proc_tx, proc_rx) = unbounded_channel();
    let (sys_tx, sys_rx) = unbounded_channel();
    let (lsp_tx, lsp_rx) = unbounded_channel();
    let (luafs_tx, luafs_rx) = unbounded_channel();

    let legs = [
        tokio::spawn(daemon::serve_fs_daemon_on(
            rpc.clone(),
            fs_rx,
            Box::new(StdHostFs),
        )),
        tokio::spawn(daemon::serve_proc_daemon_on(rpc.clone(), proc_rx)),
        tokio::spawn(daemon::serve_sys_daemon_on(
            rpc.clone(),
            sys_rx,
            Box::new(StdBlockingSystem),
        )),
        tokio::spawn(daemon::serve_lsp_daemon_on(rpc.clone(), lsp_rx)),
        tokio::spawn(daemon::serve_luafs_daemon_on(
            rpc.clone(),
            luafs_rx,
            Box::new(StdLuaFs::new()),
        )),
    ];

    // The multiplexer: route each inbound message to its leg by method namespace.
    while let Some(msg) = incoming.recv().await {
        let leg = {
            let method = match &msg {
                Incoming::Request { method, .. } | Incoming::Notification { method, .. } => {
                    method.as_str()
                }
            };
            if method.starts_with("fs_") {
                Some(&fs_tx)
            } else if method.starts_with("proc_") {
                Some(&proc_tx)
            } else if method == "sys_run" {
                Some(&sys_tx)
            } else if method.starts_with("lsp_") {
                Some(&lsp_tx)
            } else if method == "luafs" {
                Some(&luafs_tx)
            } else {
                None // unknown method: drop (the peer is the same build)
            }
        };
        // A leg whose task has exited closes its receiver; ignore the send error and
        // keep multiplexing the rest.
        if let Some(tx) = leg {
            let _ = tx.send(msg);
        }
    }

    // The edit-host hung up: drop the senders so each leg sees EOF and winds down,
    // then wait for them so child reaping completes before we return.
    drop((fs_tx, proc_tx, sys_tx, lsp_tx, luafs_tx));
    for leg in legs {
        let _ = leg.await;
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
