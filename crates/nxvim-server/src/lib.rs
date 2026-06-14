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
//! [`run`] hosts the `select!` loop; the [`EditHost`] state and its behavior are
//! split across focused sibling modules: [`dispatch`] (the RPC surface),
//! [`input`] (keystrokes/mappings), [`excmd`] (ex-commands), [`lifecycle`]
//! (autocmd emission), [`effects`] (draining queued Lua side effects),
//! [`redraw`] (View→wire projection), [`treesitter`] (highlight projection), and
//! [`lsp`] (language-server integration).

// Without the `native` feature this crate builds only the synchronous `EditHost`
// tick (the Phase 5 wasm subset, slice 5a). Nothing *constructs* an `EditHost`
// here — that's the wasm cdylib's job (slice 5b) — so the tick's methods and the
// struct itself read as dead code, and the imports / locals that serve only the
// (gated) native transport read as unused. Allow all three on that config only;
// the **native** build — the one that ships and is fully tested — keeps strict
// `-D warnings` linting, so nothing real is masked.
#![cfg_attr(
    not(feature = "native"),
    allow(dead_code, unused_imports, unused_variables)
)]

// The synchronous-tick modules — wasm-eligible (the Phase 5 edit-host subset).
mod edithost;
mod effects;
mod excmd;
mod extmarks;
mod input;
mod keymap;
mod lifecycle;
mod redraw;
mod save;
// Feature-agnostic: the vt100 emulator + screen→buffer projection compile on the
// wasm build too (pure CPU, no PTY). The native PTY transport added to this module
// in Phase 3 is gated `#[cfg(feature = "native")]` inside it.
mod terminal;

// The native-transport surface — gated off the wasm build (slice 5a): the
// msgpack wire + RPC router, the daemon/QUIC legs, the event-loop actor and its
// inbound translator, the process seam, the redb shada store, the system
// clipboard, and the not-yet-portable LSP + native treesitter.
#[cfg(feature = "native")]
mod clipboard;
#[cfg(feature = "native")]
mod daemon;
#[cfg(feature = "native")]
mod dispatch;
#[cfg(feature = "native")]
mod evloop;
#[cfg(feature = "native")]
mod host;
#[cfg(feature = "native")]
mod inbound;
#[cfg(feature = "native")]
mod lsp;
#[cfg(feature = "native")]
mod quic;
#[cfg(feature = "native")]
mod shada;
#[cfg(feature = "native")]
mod treesitter;

/// The process-spawning seam (`vim.system` / `jobstart` / `:!`) and its types,
/// re-exported for [`ServerInit::host_proc`] — the edit-host split injects a
/// daemon-backed [`HostProc`] here (the process-side companion to
/// [`nxvim_core::HostFs`]).
#[cfg(feature = "native")]
pub use host::{HostProc, ProcEvents, ProcSpec, StdHostProc};
/// The cross-session snapshot the [`ShadaStore`] seam round-trips, re-exported so an
/// out-of-crate store implementor (a test probe, the future wasm OPFS backend) can
/// name the `load`/`flush` payload and its entries without depending on
/// `nxvim-core` directly.
pub use nxvim_core::{
    FileChangelist, FileMarkEntry, GlobalMarkEntry, JumpPos, NumberedMark, PersistState,
    RegisterEntry,
};
/// The persistence (shada) seam and its native redb backend. The store sits
/// behind [`ShadaStore`] so the platform layer injects it through
/// [`ServerInit::shada`] — native binaries pass [`default_shada`] (redb over a
/// file at [`shada_dir`]); the wasm Worker build will pass a redb-over-OPFS store;
/// tests pass a [`RedbFileStore`] over a temp dir, or `None` to disable.
#[cfg(feature = "native")]
pub use shada::{default_shada, is_store_file, shada_dir, RedbFileStore, ShadaStore};

/// The daemon wire protocol for the edit-host split: the daemon-side servers
/// ([`serve_daemon`] for child processes, [`serve_fs_daemon`] for file reads,
/// [`serve_sys_daemon`] for the blocking `vim.system` shell-out) and the edit-host-side
/// clients ([`RemoteHostProc`], [`RemoteHostFs`], [`RemoteBlockingSystem`]) that forward
/// to them over any [`AsyncRead`](tokio::io::AsyncRead)/[`AsyncWrite`](tokio::io::AsyncWrite)
/// wire (a duplex, or ssh stdio to `nxvim --daemon`). [`HostFsAsync`] is the async fs
/// seam the server fetches buffer contents through off the editor tick; [`FsRead`] is
/// what one fetch resolves to.
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
pub use quic::{bind_quic_listener, connect_quic, mint_token, serve_quic, ListenerInfo};

/// The outbound async-effect seam the synchronous [`EditHost`] tick emits through
/// (redraws / notifications to the client, off-tick fs, the event-loop / LSP command
/// sinks). Re-exported so the out-of-crate wasm cdylib ([`nxvim-edithost`], slice 5b)
/// can implement it for the browser transport, the way [`NativeEffects`] implements it
/// for the native server.
pub use edithost::HostEffects;
#[cfg(feature = "native")]
use edithost::NativeEffects;
#[cfg(feature = "native")]
use evloop::{EventLoop, LoopCommand, LoopEvent};
use keymap::Keymaps;
#[cfg(feature = "native")]
use keymap::{BuiltinAction, NativeDefault};
#[cfg(feature = "native")]
use lsp::{
    CompletionMenu, DiagnosticConfig, InlayResolveTarget, LspDocState, LspReqKind, PendingLspReq,
    ServerRuntime,
};
use nxvim_core::{
    BufferId, Editor, FileStat, HostFs, Key, Mode, PendingSave, ShadaRequest, StdHostFs, TabId,
    WindowId,
};
#[cfg(feature = "native")]
use nxvim_lsp::{CodeActionData, LspManager, ServerKey};
use nxvim_lua::LuaRuntime;
#[cfg(feature = "native")]
use nxvim_rpc::{connect, Incoming};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
#[cfg(feature = "native")]
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(feature = "native")]
use tokio::sync::mpsc::unbounded_channel;
#[cfg(feature = "native")]
use treesitter::SyntaxState;

/// Startup options for the server.
///
/// Not `Clone`/`Debug`: [`ClipboardProvider::Custom`] holds a trait object that
/// is neither. No caller needs those — every construction site builds a fresh
/// value (`..Default::default()`) and moves it straight into [`run`].
#[cfg(feature = "native")]
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
    /// without depending on wall-clock timing. See [`EditHost::mouse_stamp_ms`].
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
    /// [`RemoteLuaFs`] here so a plugin reads the *remote* project (file previews,
    /// LSP `root_dir` detection, git-status marks) instead of the local machine
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → *The full split*,
    /// *Lua-visible filesystem semantics*). Like [`blocking_system`](Self::blocking_system)
    /// it is a synchronous blocking bridge: each call parks the editor thread on the
    /// daemon reply, its wire's RPC tasks on their own thread. `Send` (boxed) so it rides
    /// [`ServerInit`] onto the server thread, where it is rebuilt into the Lua runtime's
    /// `Rc<dyn LuaFs>`.
    pub lua_fs: Option<Box<dyn nxvim_lua::LuaFs + Send>>,
}

/// How the server provides the `"+` / `"*` clipboard registers.
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
/// [`EditHost::last_window_rects`] diff that fires `WinResized`.
type WindowRect = (WindowId, (usize, usize, usize, usize));

/// The synchronous editor tick — the keystroke → core → redraw machine plus the
/// per-frame bookkeeping (highlight/LSP/lifecycle/mirror caches) the projection needs.
/// It owns the [`Editor`] and [`LuaRuntime`] and runs entirely on one thread; the
/// **only** thing that reaches async / off-thread / external machinery is [`fx`](Self::fx),
/// the [`HostEffects`] seam. That single boundary is what lets the same edit-host run
/// behind two transports: the native [`run`] loop (this file, [`NativeEffects`]) and,
/// later, the wasm Worker (Phase 5) with a JS-interop + daemon-link `HostEffects`.
///
/// The inbound side is the loop's, not the host's: the `select!` arms in [`run`] own the
/// transports an event arrives on (client RPC, LSP replies, timers, off-tick fs, watch
/// pushes, `:TSInstall` completions) and hand each to a thin translator in [`inbound`]
/// that drives one of the host's per-event tick methods. So `EditHost` holds no tokio
/// channel or socket directly — every async edge is either `fx` (outbound) or a loop arm
/// feeding a tick method (inbound). See the Phase 4 hoist in
/// `docs/plans/2026-06-09-edit-host-and-browser-lua.md`.
///
/// Public so the out-of-crate wasm cdylib ([`nxvim-edithost`], slice 5b) can
/// construct one ([`new`](Self::new)) and drive it ([`boot`](Self::boot) /
/// [`feed`](Self::feed)) behind a wasm [`HostEffects`]; the native [`run`] loop
/// builds and drives it in-crate. The fields stay private either way.
/// A pending Worker-side timer (the wasm build's analogue of a tokio timer armed via
/// [`LoopCommand::TimerStart`](crate::evloop::LoopCommand), which the wasm build has no
/// event loop for). Holds the Lua callback id, its absolute due time on the JS clock,
/// and the repeat interval (`0` ⇒ a one-shot `vim.defer_fn`). The Worker parks on
/// `Atomics.wait` until the soonest [`due_ms`](WasmTimer::due_ms)
/// ([`EditHost::next_timer_deadline`]), then fires the due ones
/// ([`EditHost::fire_due_timers`]) — one mechanism for both the input wait and the timer
/// wheel (slice 5d).
#[cfg(not(feature = "native"))]
#[derive(Clone, Copy)]
struct WasmTimer {
    /// The `nx._cb_fns` callback id to run when the timer fires (the same id space the
    /// native [`LoopEvent::Timer`](crate::evloop::LoopEvent) carries).
    id: u64,
    /// Absolute due time on the Worker's JS clock (ms), `clock_ms + delay` at arm time.
    due_ms: u64,
    /// Repeat interval (ms); `0` is a one-shot (removed after it fires), `>0` re-arms to
    /// `now + repeat_ms` and keeps the Lua callback registered.
    repeat_ms: u64,
}

pub struct EditHost {
    editor: Editor,
    lua: LuaRuntime,
    /// The outbound async-effect seam (Phase 4, Open Decision #6 (a)): the editor
    /// tick pushes redraws / notifications / responses to the client and hands
    /// timer / process / watch commands to the event-loop actor *through* this,
    /// never touching the [`Rpc`] or [`EventLoop`] directly. [`NativeEffects`] is
    /// today's behavior verbatim; the wasm build swaps in a JS-interop + daemon-link
    /// implementor. See [`edithost`].
    fx: Box<dyn HostEffects>,
    /// The persistence (shada) store, or `None` when persistence is off (the test
    /// default). Loaded before the first frame ([`EditHost::shada_load`]), written by
    /// the debounced live checkpoint ([`EditHost::shada_checkpoint`]) and the
    /// clean-exit flush ([`EditHost::shada_flush_final`]). A capability injected via
    /// [`ServerInit::shada`], like [`fx`](EditHost::fx) — but `load`/`flush` only ever
    /// run off the input tick (startup, the debounce arm, exit), never inside it.
    #[cfg(feature = "native")]
    shada: Option<Box<dyn ShadaStore + Send>>,
    /// Attached UI dimensions `(width, height)`, once a client has attached.
    ui: Option<(usize, usize)>,
    /// Per-buffer highlight memo, keyed by buffer id (created lazily on first
    /// redraw of a buffer, dropped when the buffer is deleted). The parse tree
    /// itself lives in the editor's [`nxvim_core::SyntaxEngine`]; this is only the
    /// slim span cache the redraw projects.
    #[cfg(feature = "native")]
    syntax_states: HashMap<BufferId, SyntaxState>,
    /// Languages whose *on-disk* treesitter queries have already been resolved
    /// Per-buffer LSP document-sync state, keyed by buffer id (the `syntax_states`
    /// analogue).
    #[cfg(feature = "native")]
    lsp_states: HashMap<BufferId, LspDocState>,
    /// Negotiated runtime state (encoding, sync kind) per started server, learned
    /// from each `initialize` reply.
    #[cfg(feature = "native")]
    lsp_servers: HashMap<ServerKey, ServerRuntime>,
    /// Server keys already handed to `ensure_server`, so a server is requested
    /// once rather than on every redraw (a lazy-start guard).
    #[cfg(feature = "native")]
    lsp_ensured: HashSet<ServerKey>,
    /// The next LSP client id to assign. Each `(name, root)` server gets one,
    /// stable across respawns (reused when its runtime is replaced), and it is
    /// the handle `LspAttach`'s `data.client_id` carries to Lua (Slice 3).
    #[cfg(feature = "native")]
    next_lsp_client_id: u64,
    /// Set when an LSP event changed something the client should see (e.g. a fresh
    /// `Initialized` that should trigger a `didOpen`). Coalesced per loop turn so a
    /// burst of replies costs one repaint.
    #[cfg(feature = "native")]
    lsp_dirty: bool,
    /// Monotonic generation counter stamped onto each language-feature request,
    /// so a reply whose generation is behind the latest of its kind is dropped
    /// (Decision 3 — the go-to analogue of the syntax `tick`).
    #[cfg(feature = "native")]
    lsp_req_gen: u64,
    /// The in-flight language-feature request per kind (definition, references,
    /// …), used to match a reply to its intent and drop stale ones.
    #[cfg(feature = "native")]
    lsp_requests: HashMap<LspReqKind, PendingLspReq>,
    /// In-flight `inlayHint/resolve`s, keyed by the `cb_id` their token carries.
    /// Unlike the single-slot `lsp_requests`, many lazy hints can resolve at once,
    /// so each gets a distinct `cb_id` (from `inlay_resolve_seq`) and routes back
    /// by it — the [`InlayResolveTarget`] records which placeholder span to fill.
    #[cfg(feature = "native")]
    inlay_resolves: HashMap<u64, InlayResolveTarget>,
    /// Monotonic source of `cb_id`s for `inlay_resolves` (never reused, so a stale
    /// reply for a superseded resolve finds no target and is dropped).
    #[cfg(feature = "native")]
    inlay_resolve_seq: u64,
    /// The open insert-mode completion popup (Phase 5), or `None`. Server-owned
    /// like the diagnostics cache; projected into the `pmenu` redraw key and
    /// driven by the popup-open key routing in [`EditHost::completion_menu_key`].
    #[cfg(feature = "native")]
    completion: Option<CompletionMenu>,
    /// The code actions currently listed in the `:LspCodeAction` panel (Phase 6),
    /// indexed by panel select. A `<CR>` on row `i` applies `lsp_code_actions[i]`'s
    /// edit; cleared on apply. Empty when no code-action panel is active.
    #[cfg(feature = "native")]
    lsp_code_actions: Vec<CodeActionData>,
    /// The `vim.diagnostic.config` keys with a backing surface — the underline
    /// spans and the inline virtual text — toggled by `vim.diagnostic.config`.
    #[cfg(feature = "native")]
    diag_config: DiagnosticConfig,
    /// The editor-wide semantic-tokens gate (Phase 3), toggled by
    /// `vim.lsp.semantic_tokens.enable`. Default on; `false` hides the semantic
    /// paint everywhere and stops the refresh requests (the per-buffer
    /// `LspDocState::semantic_enabled` is the narrower override).
    #[cfg(feature = "native")]
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
    /// `EditHost::input` runs every key through before `editor.input`. Rebuilt from
    /// `nx._keymaps` when its version advances (checked once per input batch).
    keymaps: Keymaps,
    /// Callback ids queued by `vim.schedule`, drained inside `run_pending` so a
    /// scheduled fn runs at the end of the current convergence (not nested in its
    /// caller). A scheduled fn may schedule more, so this feeds the fixpoint loop.
    scheduled: VecDeque<u64>,
    /// Per-buffer `changedtick` last copied into the `nx._bufs` Lua mirror
    /// ([`EditHost::push_buf_mirror`]), so an unchanged buffer's line array isn't
    /// re-serialized on every Lua entry — only the cheap cursor/window fields
    /// refresh each time (Phase 6).
    buf_mirror_ticks: HashMap<BufferId, u64>,
    /// Per-buffer line count last mirrored, so [`EditHost::push_buf_mirror`] can pass
    /// the old line count as `on_lines`' `lastline` when an attached buffer changes
    /// (`nvim_buf_attach`). Tracked only to fire faithful buffer-change callbacks —
    /// a fuzzy-finder plugin drives its prompt filtering off `on_lines`.
    buf_mirror_lines: HashMap<BufferId, usize>,
    /// Per-buffer undo fingerprint last serialized into the `nx._undotree` Lua
    /// mirror ([`EditHost::push_undotree_mirror`]), so an unchanged tree isn't
    /// re-projected on every Lua entry — only edits/undo/redo rebuild it.
    undo_mirror_versions: HashMap<BufferId, (u64, usize, u64, bool)>,
    /// Monotonic base for the editor's time: `start.elapsed()` seconds are stamped
    /// onto undo nodes and handed to `vim.fn.localtime()`. Monotonic so elapsed
    /// labels survive wall-clock jumps; see [`Editor::set_now_mono`].
    start: std::time::Instant,
    /// Optional fake clock for the mouse multi-click timestamp ([`ServerInit::mouse_clock`]);
    /// when set, [`EditHost::mouse_stamp_ms`] reads it instead of `start.elapsed()`.
    mouse_clock: Option<Arc<AtomicU64>>,
    /// The highlight-registry [`generation`](nxvim_core::highlight::Highlights::generation)
    /// last folded into the `nx._hl_defs` Lua mirror ([`EditHost::push_buf_mirror`]).
    /// The mirror (potentially hundreds of groups) is re-pushed only when this
    /// changes — a colorscheme load, a `:hi`/`nvim_set_hl` — so the common chunk
    /// pays nothing for `nvim_get_hl` support. `None` until the first push.
    hl_mirror_gen: Option<u64>,
    /// The `nx._cb_fns` id of the `vim.ui.input` callback awaiting the open
    /// command-line prompt's result, or `None` when no scripted prompt is open
    /// (Phase 8). Set when a prompt opens; taken when the user submits/cancels.
    pending_ui_input: Option<u64>,
    /// The `nx._cb_fns` id of the `nx.ui.select` callback awaiting the open
    /// menu's result, or `None` when no menu is open. Set when a menu opens;
    /// taken when the user confirms / cancels. Separate from `pending_ui_input`
    /// (a menu and a prompt are distinct surfaces).
    pending_ui_select: Option<u64>,
    /// Whether the open float-list widget is a `nx.picker` (vs a `nx.ui.select`).
    /// Set when a picker opens; cleared when it confirms / cancels. The widget's
    /// outcome (`menu_results`) routes to the picker (`run_picker_result`) when
    /// this is set, and to `pending_ui_select` (`run_ui_select`) otherwise — only
    /// one float-list widget is open at a time, so the two are mutually exclusive.
    picker_active: bool,
    /// Keys queued by `nvim_feedkeys`, drained after the input batch / off-tick
    /// settle. Each carries whether it should be remapped (the `m` flag) or fed
    /// straight to the editor (the `n` flag). `nvim_feedkeys` with the `i` flag
    /// pushes to the front; otherwise to the back.
    feed_buffer: VecDeque<(Key, bool)>,
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
    /// holding the `(path, disk-stat)` it was armed against. [`EditHost::sync_buffer_watches`]
    /// reconciles this against the live buffers every tick: a new file-backed buffer
    /// arms a watch, a closed one disarms, and a changed key (a reload/save gave the
    /// file a new identity) re-arms — so the watch follows the file across atomic
    /// replaces. Each watch's loop id is [`INTERNAL_WATCH_BASE`]` + buffer.0`, which
    /// the [`LoopEvent::FsEvent`] arm uses to route a change back to `checktime` for
    /// that buffer (vs. running a Lua `vim.uv.fs_event` callback). Local sessions
    /// only — a daemon session uses [`EditHost::remote_watches`] instead.
    buf_watches: HashMap<BufferId, (PathBuf, Option<FileStat>)>,
    /// The paths watched on the **daemon** (`HostWatch` leg) in a daemon session — the
    /// remote analogue of [`EditHost::buf_watches`]. [`EditHost::sync_buffer_watches`] arms a
    /// watch (`HostFsAsync::watch`) for each file-backed buffer's path and disarms a
    /// closed one; a `fs_changed` push for one reconciles off-tick. Empty in a local
    /// session (it arms `buf_watches` instead). The daemon owns change detection, so this
    /// holds only paths — no stat snapshot (unlike `buf_watches`).
    remote_watches: HashSet<String>,
    /// Buffers whose off-tick reload (the remote watch leg) is in flight, awaiting a
    /// `FileChangedShellPost` once the re-fetch lands in [`EditHost::apply_open`]. The
    /// remote reload can't be synchronous (it crosses the wire), so the post event is
    /// deferred to the fetch's completion rather than fired inline like the local path.
    reload_posts: HashSet<BufferId>,
    /// Per-terminal-buffer vt100 emulators, keyed by buffer id. Created when a
    /// `:terminal` opens, fed the child's raw PTY bytes (decoding escape sequences
    /// into a screen grid), and projected back into the buffer's mirrored lines +
    /// the redraw's per-cell colors. The byte *transport* differs per build (a local
    /// PTY natively, the daemon over WebTransport on wasm), but this emulation is
    /// pure CPU and shared by both — hence feature-agnostic. See
    /// [`terminal`](crate::terminal) and docs/plans/2026-06-14-terminal-in-buffer.md.
    terminals: HashMap<BufferId, terminal::TermEmu>,
    /// The wasm build's timer wheel (slice 5d): pending `vim.defer_fn` / `nx.timer`
    /// timers, fired by the Worker when their [`due_ms`](WasmTimer::due_ms) passes on the
    /// JS clock — the serverless analogue of the tokio timers the native build arms via
    /// the event loop (which the wasm build gates out). Unused on the native build.
    #[cfg(not(feature = "native"))]
    wasm_timers: Vec<WasmTimer>,
    /// The current JS clock (ms), set by the Worker before each input / timer tick
    /// ([`EditHost::set_clock`] / [`EditHost::fire_due_timers`]); a [`WasmTimer`]'s
    /// `due_ms` is computed relative to it at arm time. Wasm build only.
    #[cfg(not(feature = "native"))]
    clock_ms: u64,
    /// Names of treesitter grammars the browser host has available — the offline set
    /// bundled in `web/vendor/` plus any fetched via `:TSInstall` into OPFS. Seeded at
    /// boot ([`EditHost::seed_ts_installed`]) and extended on each completed install
    /// ([`EditHost::complete_ts_install`]); read by `:TSInstallInfo`. The browser build
    /// highlights JS-side (web-tree-sitter), so it has no on-disk parser dir to scan
    /// like native's [`nxvim_ts::installed_parsers`] — this set is that listing. Wasm only.
    #[cfg(not(feature = "native"))]
    ts_installed: std::collections::BTreeSet<String>,
}

impl EditHost {
    /// Construct an edit-host over the given outbound-effect seam, every field at its
    /// startup default. The single construction site for the struct, shared by the
    /// native [`run_io`] (which then seeds `shada` / `mouse_clock` / the LSP keymap
    /// defaults and sources config) and the out-of-crate wasm cdylib (slice 5b, which
    /// calls [`boot`](Self::boot) for the serverless startup). The caller attaches a
    /// UI — [`attach_ui`](Self::attach_ui) on wasm, the `nvim_ui_attach` RPC natively —
    /// before the first [`redraw`](Self::redraw).
    pub fn new(editor: Editor, lua: LuaRuntime, fx: Box<dyn HostEffects>) -> EditHost {
        EditHost {
            editor,
            lua,
            fx,
            #[cfg(feature = "native")]
            shada: None,
            ui: None,
            #[cfg(feature = "native")]
            syntax_states: HashMap::new(),
            #[cfg(feature = "native")]
            lsp_states: HashMap::new(),
            #[cfg(feature = "native")]
            lsp_servers: HashMap::new(),
            #[cfg(feature = "native")]
            lsp_ensured: HashSet::new(),
            #[cfg(feature = "native")]
            next_lsp_client_id: 1,
            #[cfg(feature = "native")]
            lsp_dirty: false,
            #[cfg(feature = "native")]
            lsp_req_gen: 0,
            #[cfg(feature = "native")]
            lsp_requests: HashMap::new(),
            #[cfg(feature = "native")]
            inlay_resolves: HashMap::new(),
            #[cfg(feature = "native")]
            inlay_resolve_seq: 0,
            #[cfg(feature = "native")]
            completion: None,
            #[cfg(feature = "native")]
            lsp_code_actions: Vec::new(),
            #[cfg(feature = "native")]
            diag_config: DiagnosticConfig::default(),
            #[cfg(feature = "native")]
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
            mouse_clock: None,
            hl_mirror_gen: None,
            pending_ui_input: None,
            pending_ui_select: None,
            picker_active: false,
            feed_buffer: VecDeque::new(),
            saves_inflight: HashSet::new(),
            saves_queued: HashMap::new(),
            quit_all_gate: None,
            buf_watches: HashMap::new(),
            remote_watches: HashSet::new(),
            reload_posts: HashSet::new(),
            terminals: HashMap::new(),
            #[cfg(not(feature = "native"))]
            wasm_timers: Vec::new(),
            #[cfg(not(feature = "native"))]
            clock_ms: 0,
            #[cfg(not(feature = "native"))]
            ts_installed: std::collections::BTreeSet::new(),
        }
    }
}

/// The wasm Worker's drive API for the standalone [`EditHost`] (slice 5b). The native
/// [`run`] loop drives the tick in-crate through `pub(crate)` `input` / `redraw`; the
/// out-of-crate wasm cdylib reaches the same tick through these thin public wrappers
/// plus a serverless [`boot`](Self::boot). Gated to the wasm build so the native API
/// surface is unchanged.
#[cfg(not(feature = "native"))]
impl EditHost {
    /// Attach a UI of `width` × `height` cells — the wasm analogue of the
    /// `nvim_ui_attach` RPC (the dispatch router is gated off the wasm build), so
    /// [`redraw`](Self::redraw) has dimensions to project the view into, then paints
    /// the initial frame (as `nvim_ui_attach` triggers a repaint natively). Also the
    /// resize path — a re-attach at a new size repaints.
    pub fn attach_ui(&mut self, width: usize, height: usize) {
        self.ui = Some((width, height));
        self.redraw();
    }

    /// Run the serverless startup seed: snapshot the initial buffer for Lua, seed the
    /// window / tab / buffer lifecycle sets, fire the first buffer's lifecycle events,
    /// drain queued work, and mark `v:vim_did_enter`. The serverless analogue of
    /// [`run_io`]'s startup, minus the native-only steps (config sourcing, plugin
    /// discovery, shada load, LSP keymap defaults) the v1 browser build doesn't have.
    pub fn boot(&mut self) {
        self.boot_begin();
        self.boot_finish();
    }

    /// Serverless startup, phase 1: seed the Lua buffer snapshot + mirrors and the
    /// window/tab/buffer baselines, so an `init.lua` sourced next reads correct editor
    /// state. Does **not** fire the startup lifecycle events or set `v:vim_did_enter` —
    /// that is [`boot_finish`](Self::boot_finish), run *after* the optional config
    /// sources, so a config's autocmds for the startup buffer (`BufEnter` …) fire
    /// (native ordering: config first, then the startup lifecycle). [`boot`](Self::boot)
    /// runs both back-to-back when there is no config phase.
    pub fn boot_begin(&mut self) {
        let buf = self.editor.current_buffer_id();
        let name = self.editor.buffer_name(buf).unwrap_or_default();
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, &name, ft);
        self.push_buf_mirror();
        self.known_windows = self.editor.window_ids();
        self.last_window_rects = Some(self.window_rects_snapshot());
        self.known_tabs = self.editor.tab_ids();
        self.known_buffers = self.editor.buffer_ids();
    }

    /// Serverless startup, phase 2: fire the startup lifecycle events and mark
    /// `v:vim_did_enter`. Run after [`boot_begin`](Self::boot_begin) and the optional
    /// `init.lua` sourcing ([`source_config`](Self::source_config)).
    pub fn boot_finish(&mut self) {
        self.emit_lifecycle_events();
        self.run_pending();
        let _ = self.lua.set_vim_did_enter(true);
    }

    /// Source the user's single-file `init.lua` (read from OPFS by the Worker) through
    /// the **real** effects path — the serverless analogue of native config sourcing,
    /// run between [`boot_begin`](Self::boot_begin) and [`boot_finish`](Self::boot_finish).
    /// `require` of further modules won't resolve (the browser build's runtimepath is
    /// empty), so this is a single self-contained file: options, keymaps, autocmds, user
    /// commands, highlights. The chunk is named `@init.lua` so a traceback points at it.
    /// A Lua error is returned for the Worker to surface; the editor still finishes
    /// booting, so a broken config can't brick the session.
    pub fn source_config(&mut self, code: &str) -> Result<(), String> {
        let result = self
            .lua
            .exec_named(code, "@init.lua")
            .map_err(|e| e.to_string());
        self.apply_lua_effects();
        self.run_pending();
        result
    }

    /// Feed vim key-notation and project the resulting frame — the wasm Worker's
    /// keystroke tick: `input` settles the core (mappings, ex-commands, queued Lua),
    /// then `redraw` pushes a frame through the [`HostEffects`] seam to the UI. Mirrors
    /// one turn of the native [`run`] loop's input arm.
    pub fn feed(&mut self, keys: &str) {
        self.input(keys);
        self.redraw();
    }

    /// Apply a mouse gesture and repaint — the wasm Worker's `eh_input_mouse` tick.
    /// `button`/`action`/`modifier` are the `nvim_input_mouse` strings and `row`/`col`
    /// the 0-based global screen cell; core owns the hit-test, multi-click word/line
    /// selection, drag-select, and wheel scroll, exactly as the native dispatch path
    /// does. The event is stamped from the Worker's JS clock ([`set_clock`](Self::set_clock),
    /// which the Worker sets before this call) so `'mousetime'` multi-click detection
    /// works. A malformed gesture surfaces a loud message rather than a silent no-op.
    pub fn mouse(&mut self, button: &str, action: &str, modifier: &str, row: usize, col: usize) {
        match nxvim_core::MouseEvent::parse(button, action, modifier, row, col) {
            Ok(mut ev) => {
                ev.stamp_ms = self.clock_ms;
                self.editor.mouse(ev);
            }
            Err(err) => self.editor.message = err,
        }
        self.redraw();
    }

    /// Execute a Lua chunk through the **real** effects path (the queued `vim.cmd`s,
    /// highlights, and deferred work a chunk produces are applied exactly as a `:lua`
    /// from the keystroke tick would be), then project a frame. Returns the eval result
    /// rendered as a string (`int` verbatim, else `Debug`), or the Lua error message.
    pub fn exec_lua(&mut self, code: &str) -> Result<String, String> {
        let rendered = self
            .lua
            .eval_to_value(code)
            .map(|value| {
                value
                    .as_i64()
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| format!("{value:?}"))
            })
            .map_err(|e| e.to_string());
        // The chunk may have queued ex-commands / highlights and deferred work; drain
        // them through the same machinery `input`'s settle uses, then repaint.
        self.apply_lua_effects();
        self.run_pending();
        self.redraw();
        rendered
    }

    /// The current buffer's lines (the wasm `eh_lines` readout).
    pub fn lines(&self) -> Vec<String> {
        self.editor.lines()
    }

    /// Snapshot the cross-session (shada) state for persistence. The native server
    /// serializes this into its redb store ([`shada`]); the wasm Worker serializes it to
    /// a JSON blob in OPFS. Pure — just the editor's [`export_persist`](Editor::export_persist).
    pub fn export_persist(&self) -> nxvim_core::PersistState {
        self.editor.export_persist()
    }

    /// Seed cross-session (shada) state restored from the store, before the startup
    /// lifecycle fires (so a restored `` `" `` / registers / history are live for the
    /// first frame). The wasm Worker calls this between config-sourcing and
    /// [`boot_finish`](Self::boot_finish), mirroring native's load ordering.
    pub fn import_persist(&mut self, state: nxvim_core::PersistState) {
        self.editor.import_persist(state);
    }

    /// Set the Worker's current JS clock (ms) so a [`WasmTimer`] armed during the next
    /// tick computes its `due_ms` relative to *now*. The Worker calls this before
    /// feeding input; [`fire_due_timers`](Self::fire_due_timers) sets it too.
    pub fn set_clock(&mut self, now_ms: u64) {
        self.clock_ms = now_ms;
    }

    /// The soonest pending timer deadline (ms on the JS clock), or `None` when no timer
    /// is armed. The Worker parks on `Atomics.wait` with this as its timeout — so the one
    /// wait that wakes on a keystroke also wakes to fire the next timer (slice 5d's "one
    /// mechanism" — no busy loop, no separate timer thread).
    pub fn next_timer_deadline(&self) -> Option<u64> {
        self.wasm_timers.iter().map(|t| t.due_ms).min()
    }

    /// Fire every timer due at `now_ms` (the Worker calls this on a wake), running each
    /// Lua callback through the **real** effects path and repainting once if any fired.
    /// Returns whether any timer fired (so the Worker knows to post a fresh redraw).
    ///
    /// Only timers already due at entry fire this call — a callback that arms a new,
    /// already-due timer doesn't re-fire in the same pass (it waits for the Worker's next
    /// tick, which is immediate when [`next_timer_deadline`](Self::next_timer_deadline)
    /// is `<= now`), so a 0-delay self-re-arming timer can't spin the wheel here. A
    /// repeating timer (`repeat_ms > 0`) re-arms to `now + repeat_ms` *before* its
    /// callback runs, so the callback sees the next deadline already set.
    pub fn fire_due_timers(&mut self, now_ms: u64) -> bool {
        self.clock_ms = now_ms;
        let mut due: Vec<WasmTimer> = self
            .wasm_timers
            .iter()
            .copied()
            .filter(|t| t.due_ms <= now_ms)
            .collect();
        due.sort_by_key(|t| t.due_ms);
        let mut fired_any = false;
        for timer in due {
            // Skip a timer a prior callback in this pass stopped (`:stop` / `vim.uv`
            // close); re-arm a repeat or drop a one-shot *before* running the callback.
            let Some(idx) = self.wasm_timers.iter().position(|t| t.id == timer.id) else {
                continue;
            };
            let keep = timer.repeat_ms > 0;
            if keep {
                self.wasm_timers[idx].due_ms = now_ms.saturating_add(timer.repeat_ms);
            } else {
                self.wasm_timers.remove(idx);
            }
            if let Err(e) = self
                .lua
                .run_callback(timer.id, keep, nxvim_lua::CallbackArgs::None)
            {
                self.editor
                    .echo(format!("E5108: Error in timer callback: {e}"));
            }
            self.apply_lua_effects();
            fired_any = true;
        }
        if fired_any {
            self.run_pending();
            self.redraw();
        }
        fired_any
    }

    /// Arm (or re-arm) a Worker-side timer — the wasm branch of
    /// [`apply_loop_op`](Self::apply_loop_op)'s [`LoopOp::TimerStart`](nxvim_lua::LoopOp::TimerStart).
    /// Replaces any existing timer with the same `id` (a re-`start` on one handle), so the
    /// wheel never accrues stale duplicates.
    pub(crate) fn arm_wasm_timer(&mut self, id: u64, delay_ms: u64, repeat_ms: u64) {
        let due_ms = self.clock_ms.saturating_add(delay_ms);
        self.wasm_timers.retain(|t| t.id != id);
        self.wasm_timers.push(WasmTimer {
            id,
            due_ms,
            repeat_ms,
        });
    }

    /// Cancel the Worker-side timer armed under `id` (a `:stop` / `vim.uv` close, or a
    /// `defer_fn` handle's stop) — the wasm branch of
    /// [`LoopOp::TimerStop`](nxvim_lua::LoopOp::TimerStop). A no-op if it already fired.
    pub(crate) fn stop_wasm_timer(&mut self, id: u64) {
        self.wasm_timers.retain(|t| t.id != id);
    }

    /// Turn on **off-tick fs** for the serverless browser build (Phase 6, the OPFS
    /// slice). The editor then defers `:e` / `:w` to a [`PendingOpen`](nxvim_core::editor)
    /// / [`PendingSave`] the Worker fulfills against the Origin Private File System off
    /// the keystroke tick — the *exact* seam a daemon session uses, only the transport is
    /// OPFS instead of the wire. OPFS is "off-tick" even though it is local because
    /// acquiring an OPFS file handle is **asynchronous** (only a `FileSystemSyncAccessHandle`'s
    /// *operations* are synchronous), so a synchronous [`HostFs`] read/write is impossible
    /// without Asyncify — which this plan deliberately avoids (Phase 0). Pairs with the
    /// cdylib's `WasmEffects::has_remote_fs() == true`, which routes the editor onto the
    /// off-tick branches ([`drain_pending_opens`](Self::drain_pending_opens) /
    /// [`dispatch_save`](Self::drain_pending_saves)).
    pub fn enable_offtick_fs(&mut self) {
        self.editor.set_host_fs_offtick(true);
    }

    /// Apply a finished off-tick OPFS **file** read the Worker fetched for `buffer` (the
    /// `:edit` / startup analogue of the native [`apply_open`](Self::apply_open), minus the
    /// daemon-only `FsRead` / watch machinery). `kind`: `0` = an existing file (`contents`
    /// is its UTF-8 text), `1` = a not-yet-existing path (a new-file buffer, no bytes), any
    /// other = a read error (`contents` carries the message). A **directory** read does not
    /// arrive here — the cdylib routes it to [`complete_fs_read_dir`](Self::complete_fs_read_dir)
    /// with its entries; a stray `kind == 2` here is a defensive loud echo, never a silent
    /// empty buffer. Repaints once the buffer lands.
    pub fn complete_fs_read(&mut self, buffer: BufferId, path: String, kind: u8, contents: &str) {
        match kind {
            0 => self.load_replica_wasm(buffer, path, contents),
            1 => self.load_replica_wasm(buffer, path, ""),
            2 => self.editor.echo(format!(
                "nxvim: directory read of {path} reached the file applier (use complete_fs_read_dir)"
            )),
            _ => self
                .editor
                .echo(format!("nxvim: could not open {path}: {contents}")),
        }
        self.redraw();
    }

    /// Load `contents` into `buffer` as a freshly-read replica of OPFS file `path`, then
    /// fire the lifecycle a read implies — the wasm-eligible subset of the native
    /// `load_replica`: [`Editor::load_str_into`](nxvim_core::Editor) replaces the buffer
    /// in place; clearing it from `announced` lets the now-named buffer's
    /// `BufReadPost` / `FileType` fire (the latter drives syntax); then refresh the Lua
    /// snapshot / mirror and drain any autocmd-queued work (which may itself enqueue
    /// further opens/saves the Worker picks up next).
    fn load_replica_wasm(&mut self, buffer: BufferId, path: String, contents: &str) {
        self.editor
            .load_str_into(buffer, Some(path.clone()), contents);
        self.announced.remove(&buffer);
        let ft = filetype_of(Some(Path::new(&path))).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buffer.0, &path, ft);
        self.push_buf_mirror();
        self.emit_lifecycle_events();
        // A remote watch reload (the daemon `fs_changed` leg) deferred its
        // `FileChangedShellPost` to this landing point — fire it now, before `run_pending`,
        // so a handler's queued work drains in the same convergence (mirrors `load_replica`).
        if self.reload_posts.remove(&buffer) {
            self.fire_file_changed_post(buffer);
        }
        self.run_pending();
    }

    /// Apply a finished off-tick OPFS directory listing into `buffer` — the wasm analogue
    /// of the native `apply_open`'s `FsRead::Dir` arm (Phase 3g over the wire). The Worker
    /// enumerated OPFS directory `dir` and hands back its `entries`; this turns `buffer`
    /// into the read-only file-explorer listing (netrw), so `:e <dir>` and descending /
    /// going up navigate the browser's OPFS tree exactly as a daemon session navigates the
    /// remote tree. The whole explorer (`enter_dir` / `explorer_open_entry`) is already
    /// off-tick-aware in core; this is the only piece that was missing. Repaints after.
    pub fn complete_fs_read_dir(
        &mut self,
        buffer: BufferId,
        dir: String,
        entries: Vec<nxvim_core::DirEntry>,
    ) {
        self.load_dir_replica_wasm(buffer, dir, entries);
        self.redraw();
    }

    /// Build the file-explorer listing of OPFS directory `dir` into `buffer` from the
    /// off-tick enumeration — the directory analogue of [`load_replica_wasm`](Self::load_replica_wasm),
    /// mirroring the native `load_dir_replica`: [`Editor::load_dir_into`](nxvim_core::Editor)
    /// replaces the buffer with the listing (its `dir` marker routes keys to the explorer);
    /// clearing `announced` lets the now-named buffer's `BufReadPost` fire. A directory has
    /// no filetype, so no `FileType` work — just refresh the snapshot / mirror and drain.
    fn load_dir_replica_wasm(
        &mut self,
        buffer: BufferId,
        dir: String,
        entries: Vec<nxvim_core::DirEntry>,
    ) {
        self.editor
            .load_dir_into(buffer, PathBuf::from(&dir), entries);
        self.announced.remove(&buffer);
        let _ = self.lua.set_buf_snapshot(buffer.0, &dir, "");
        self.push_buf_mirror();
        self.emit_lifecycle_events();
        self.run_pending();
    }

    /// Apply a finished off-tick OPFS **write** of `save`'s snapshot: `ok` gates the ack
    /// exactly as the daemon `fs_write` reply does — on success the buffer's saved-state
    /// finalizes (`modified` clears, the `FileStat` of `size` + optional `mtime_ms` is
    /// stamped, the `written` echo fires, a deferred `:wq` / `:wqa` replays), and on
    /// failure the write surfaces loud and cancels any deferred quit. Reuses the shared
    /// [`apply_save_done`](Self::apply_save_done), so per-buffer write serialization and
    /// the `:wqa` gate behave identically to the daemon save path. Repaints after.
    pub fn complete_fs_write(
        &mut self,
        save: PendingSave,
        ok: bool,
        size: u64,
        mtime_ms: Option<u64>,
        err: &str,
    ) {
        let bytes_len = save.bytes.len();
        let result = if ok {
            Ok(Some(FileStat {
                size,
                mtime: mtime_ms
                    .map(|ms| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms)),
            }))
        } else {
            Err(std::io::Error::other(format!("OPFS write failed: {err}")))
        };
        self.apply_save_done(crate::save::SaveDone::new(save, bytes_len, result));
        // Converge as the native save-ack path does (`on_save_dones` → `settle_events`):
        // `run_pending` refreshes the buffer mirror at entry, so the Lua-visible
        // `vim.bo.modified` reflects the now-cleared `[+]`, and drains any work the ack
        // queued (a replayed `:wq`, the next serialized write). Then repaint.
        self.run_pending();
        self.redraw();
    }

    /// Reconcile a daemon-pushed file change (the `HostWatch` leg's `fs_changed`) in the
    /// browser — the wasm entry the Worker calls from `RpcClient.onNotify` (the daemon→
    /// edit-host push direction the fs leg never used). Decomposed `(path, stat)` instead
    /// of the native `WatchEvent` (the wire types are native-only); `has_stat == 0` means
    /// the file vanished, else `size` + `mtime_ms` (negative = unknown) carry its new
    /// [`FileStat`]. Delegates to the shared [`reconcile_remote_change`](Self::reconcile_remote_change)
    /// — which fires `FileChangedShell` and, on an autoread / `"reload"` choice, enqueues an
    /// off-tick re-fetch the Worker fulfils over the wire (landing via
    /// [`complete_fs_read`](Self::complete_fs_read), which fires `FileChangedShellPost`).
    /// `settle_events` drains that enqueued open into the fs-request queue and repaints,
    /// exactly as the native `on_watch_events` tail does.
    pub fn remote_file_changed(&mut self, path: String, has_stat: bool, size: u64, mtime_ms: i64) {
        let stat = if has_stat {
            let mtime = if mtime_ms < 0 {
                None
            } else {
                Some(std::time::UNIX_EPOCH + std::time::Duration::from_millis(mtime_ms as u64))
            };
            Some(FileStat { size, mtime })
        } else {
            None
        };
        self.reconcile_remote_change(path, stat);
        self.settle_events(true);
    }

    /// Record the OS pid of a daemon-spawned async `vim.system` child (the proc leg's
    /// `proc_spawned` push) — the wasm entry the Worker calls from `RpcClient.onNotify`.
    /// `pid < 0` means the child failed to spawn (no pid). Mirrors the native
    /// `on_loop_event`'s [`LoopEvent::ProcessSpawned`](crate::evloop) arm: the pid can't be
    /// known synchronously, so the handle's `.pid` reads `nil` until this lands. No repaint
    /// — recording a pid changes no view (native doesn't repaint here either).
    pub fn proc_spawned(&mut self, id: u64, pid: i64) {
        let pid = if pid < 0 { None } else { Some(pid as u32) };
        if let Err(e) = self.lua.set_process_pid(id, pid) {
            self.editor
                .echo(format!("E5108: Error recording process pid: {e}"));
        }
    }

    /// Land a streaming child's stdout batch (the proc leg's `proc_stdout` push) — the wasm
    /// twin of the native `on_loop_event`'s [`LoopEvent::ProcessStdout`](crate::evloop) arm.
    /// Fires the persistent `on_stdout` callback under `id` (a picker source's `push` of new
    /// candidates), drains the effects it queues, and settles + repaints so the streamed rows
    /// appear as they arrive.
    pub fn proc_stdout(&mut self, id: u64, lines: Vec<String>) {
        if let Err(e) = self.lua.run_process_stdout(id, lines) {
            self.editor
                .echo(format!("E5108: Error in nx.spawn on_stdout: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land a daemon child's exit (the proc leg's `proc_exited` push) — the wasm entry the
    /// Worker calls from `RpcClient.onNotify` when the child spawned under `id` finishes
    /// (or was killed → `code == -1`). Runs the `vim.system` `on_exit` callback with the
    /// result table (`code` + raw `stdout`/`stderr` bytes), drains the effects it queues,
    /// and settles + repaints — the wasm twin of the native `on_loop_event`'s
    /// [`LoopEvent::ProcessExit`](crate::evloop) arm plus the run loop's trailing
    /// `settle_events`. The callback may itself queue further async (a chained spawn, an
    /// off-tick `:edit`), so `settle_events` drives the convergence + the off-tick drains
    /// (the Worker then fulfils any enqueued read/write), exactly as `remote_file_changed`
    /// does for a watch reconcile.
    pub fn proc_exited(&mut self, id: u64, code: i32, stdout: Vec<u8>, stderr: Vec<u8>) {
        let args = nxvim_lua::CallbackArgs::Process {
            code,
            stdout,
            stderr,
        };
        if let Err(e) = self.lua.run_callback(id, false, args) {
            self.editor
                .echo(format!("E5108: Error in vim.system on_exit: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Seed the set of available treesitter grammars at boot — the offline bundle
    /// plus whatever the previous session installed into OPFS, both discovered by the
    /// Worker (it owns the manifests). Backs `:TSInstallInfo`; idempotent. No echo /
    /// repaint — this runs before the first frame.
    pub fn seed_ts_installed(&mut self, langs: Vec<String>) {
        self.ts_installed.extend(langs);
    }

    /// Apply a finished browser `:TSInstall`: the JS host fetched + cached the grammar
    /// (or failed). On success, record the language so `:TSInstallInfo` lists it and
    /// echo a faithful — but honest — status (only highlighting is wired in the browser;
    /// the cached indents/folds/locals queries have no consumer yet). On failure, echo
    /// the loud reason. Highlighting itself repaints JS-side when the grammar registers,
    /// independent of this echo (the analogue of native's [`on_install_done`], minus the
    /// engine reload — the wasm core has no in-process treesitter engine).
    pub fn complete_ts_install(&mut self, lang: String, ok: bool, msg: String) {
        if ok {
            self.ts_installed.insert(lang.clone());
            let detail = if msg.is_empty() {
                String::new()
            } else {
                format!(" ({msg})")
            };
            self.editor.echo(format!(
                "TSInstall: installed {lang}{detail} — highlighting active"
            ));
        } else {
            let why = if msg.is_empty() { "failed" } else { &msg };
            self.editor.echo(format!("TSInstall: {lang} failed: {why}"));
        }
        // Project the echo into a fresh frame (this runs off the input tick, like the fs
        // completions — `eh_redraw_json` only returns the last *emitted* redraw, so the
        // echo would otherwise sit unseen until the next keystroke).
        self.redraw();
    }

    /// The languages available to highlight, for `:TSInstallInfo` — the browser's
    /// answer to native's on-disk parser scan ([`nxvim_ts::installed_parsers`]).
    pub fn ts_installed_list(&self) -> Vec<String> {
        self.ts_installed.iter().cloned().collect()
    }
}

/// Base for the loop ids of the server's **internal** per-buffer file watches, set
/// far above any Lua-allocated `vim.uv.fs_event` callback id so a [`LoopEvent::FsEvent`]
/// can be classified by `id >= INTERNAL_WATCH_BASE` alone. Buffer `b`'s watch id is
/// `INTERNAL_WATCH_BASE + b.0`, so the change routes straight back to the buffer with
/// no side table. (Lua callback ids are monotonic from 1 and never approach `1 << 48`.)
#[cfg(feature = "native")]
pub(crate) const INTERNAL_WATCH_BASE: u64 = 1 << 48;

/// The loop id of the shada **debounced-checkpoint** timer (Phase 5). Set above
/// both the Lua-allocated callback ids (monotonic from 1) and the per-buffer watch
/// ids ([`INTERNAL_WATCH_BASE`]` + buffer.0`), so a [`LoopEvent::Timer`] carrying it
/// is unambiguously the shada flush and never collides with a real callback.
#[cfg(feature = "native")]
pub(crate) const SHADA_FLUSH_TIMER_ID: u64 = 1 << 49;

/// How long after the last handled message a debounced shada checkpoint fires. Each
/// message re-arms the one-shot timer (replacing the pending one), so continuous
/// activity pushes the flush forward to the next idle gap — a crash then loses at
/// most this window, never the whole session.
#[cfg(feature = "native")]
const SHADA_FLUSH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

/// Whether `event` is the shada debounced-checkpoint timer firing (vs. a real Lua
/// timer / process / watch event the run loop hands to [`EditHost::on_loop_event`]).
#[cfg(feature = "native")]
pub(crate) fn is_shada_flush_timer(event: &LoopEvent) -> bool {
    matches!(event, LoopEvent::Timer { id, .. } if *id == SHADA_FLUSH_TIMER_ID)
}

/// The shada (persistence) glue on the [`EditHost`]: load before the first frame,
/// the debounced live checkpoint, the clean-exit flush, and the per-message
/// debounce arming. All run **off** the editor input tick — the store's I/O never
/// blocks a keystroke. A no-op throughout when persistence is off (`shada: None`).
#[cfg(feature = "native")]
impl EditHost {
    /// Open + merge + compact the store before the first frame and seed the result
    /// into the editor. A load error is surfaced to the user and persistence is then
    /// disabled (the store dropped) — the editor runs on rather than dying.
    pub(crate) fn shada_load(&mut self) {
        // Take the `load` borrow and release it before touching `self.editor` /
        // re-assigning `self.shada`, so the two field accesses don't overlap.
        let result = match self.shada.as_mut() {
            Some(store) => store.load(),
            None => return,
        };
        match result {
            Ok(state) => self.editor.import_persist(state),
            Err(e) => {
                self.editor
                    .echo(format!("shada: could not open store: {e}"));
                self.shada = None;
            }
        }
    }

    /// Arm (re-arm) the one-shot debounce timer through the [`HostEffects`] timer
    /// seam, so the checkpoint fires `SHADA_FLUSH_DEBOUNCE` after the last message.
    /// Re-arming replaces the pending timer, so a burst of activity defers the flush
    /// to the next idle gap. Routed through `fx` (not the evloop directly) so the wasm
    /// Worker can drive the same debounce off its own timer wheel.
    pub(crate) fn arm_shada_checkpoint(&mut self) {
        if self.shada.is_some() {
            self.fx.loop_command(LoopCommand::TimerStart {
                id: SHADA_FLUSH_TIMER_ID,
                delay: SHADA_FLUSH_DEBOUNCE,
                repeat: std::time::Duration::ZERO,
            });
        }
    }

    /// The debounce elapsed: checkpoint the cross-session state into this instance's
    /// store. `exit_cursor` is deliberately left unset — `'0` tracks *clean* exits
    /// only, so a crash must leave the previous session's `'0` intact ([`shada_flush_final`](EditHost::shada_flush_final)
    /// is its sole writer). Best-effort: a write error is logged, never fatal.
    pub(crate) fn shada_checkpoint(&mut self) {
        if self.shada.is_none() {
            return;
        }
        let mut snap = self.editor.export_persist();
        snap.exit_cursor = None;
        if let Some(store) = self.shada.as_mut() {
            if let Err(e) = store.flush(&snap) {
                eprintln!("shada: checkpoint flush failed: {e}");
            }
        }
    }

    /// The clean-exit flush: write the final snapshot — *with* `exit_cursor` (where
    /// the cursor sits now), which the store turns into `'0` next launch — then let
    /// the store drop (releasing its file lock) so the next instance can merge this
    /// one's checkpoint. Best-effort; we're leaving.
    pub(crate) fn shada_flush_final(&mut self) {
        let snap = self.editor.export_persist();
        if let Some(store) = self.shada.as_mut() {
            if let Err(e) = store.flush(&snap) {
                eprintln!("shada: final flush failed: {e}");
            }
        }
    }

    /// Drain the deferred shada requests (`:wshada` / `:rshada`) core raised this
    /// convergence and act on each against the store. Called from the tail of
    /// [`run_pending`](EditHost::run_pending), so a request typed at the command line,
    /// queued from `:lua`, or fired from a timer callback all reach the store. A cheap
    /// no-op when neither command ran.
    pub(crate) fn drain_pending_shada(&mut self) {
        for req in self.editor.take_pending_shada() {
            match req {
                ShadaRequest::Write => self.shada_write_now(),
                ShadaRequest::Read { replace } => self.shada_read_now(replace),
            }
        }
    }

    /// `:wshada` — flush this instance's store now. Unlike the clean-exit flush this
    /// leaves `exit_cursor` unset (`'0` tracks *exits* only, and `:wshada` is not
    /// one). Fails loud: an error, or persistence being off, is echoed rather than
    /// silently dropped — there is no "looked like it saved" outcome.
    fn shada_write_now(&mut self) {
        if self.shada.is_none() {
            self.editor
                .echo("E: shada is disabled — :wshada wrote nothing".to_string());
            return;
        }
        let mut snap = self.editor.export_persist();
        snap.exit_cursor = None;
        let result = self.shada.as_mut().unwrap().flush(&snap);
        if let Err(e) = result {
            self.editor.echo(format!("E: shada write failed: {e}"));
        }
    }

    /// `:rshada[!]` — re-merge the readable store(s) and apply the result to the
    /// running session (`replace` overwrites a conflicting live register, otherwise
    /// only empty slots fill). Fails loud when persistence is off or the read errors.
    fn shada_read_now(&mut self, replace: bool) {
        if self.shada.is_none() {
            self.editor
                .echo("E: shada is disabled — :rshada read nothing".to_string());
            return;
        }
        let result = self.shada.as_mut().unwrap().reload();
        match result {
            Ok(state) => self.editor.apply_persist(state, replace),
            Err(e) => self.editor.echo(format!("E: shada read failed: {e}")),
        }
    }
}

/// A finished `:TSInstall` job: the requested language and the install result
/// (the report, or a loud error). Delivered from the blocking worker to the
/// server's `select!` loop.
#[cfg(feature = "native")]
type InstallOutcome = (String, anyhow::Result<nxvim_ts::install::InstallReport>);

/// Run the server over a connected stream until the client disconnects or the
/// editor quits.
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
    // The terminal actor: owns the local PTYs `:terminal` spawns, streaming their
    // output back on `term_events`. Lazily started on the first open (the `EventLoop`
    // pattern), so a session with no terminal spawns nothing.
    let (terminals, mut term_events) = terminal::native::TerminalManager::new();
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

    // The native outbound-effect seam: the client wire ([`Rpc`]), the event-loop actor
    // ([`EventLoop`]), the off-tick daemon fs (read/write/watch + the `open_tx` /
    // `save_done_tx` deliveries), and the LSP command sink ([`LspManager`]) the editor
    // tick fires through. The wasm build (slice 5b) swaps a JS-interop implementor here.
    let mut host = EditHost::new(
        editor,
        lua,
        Box::new(NativeEffects::new(
            rpc,
            evloop,
            host_fs_async,
            open_tx,
            save_done_tx,
            lsp,
            install_tx,
            terminals,
        )),
    );
    // The two capabilities `new` defaults but the native session injects: the
    // persistence store (loaded before the first frame) and the optional fake mouse
    // clock (multi-click timing in tests).
    host.shada = init.shada;
    host.mouse_clock = init.mouse_clock;

    // Install the built-in LSP keymaps as overridable defaults (design B2/B3),
    // so a user `vim.keymap.set` for the same `(mode, lhs)` shadows them via the
    // user > default precedence rung. *All* the LSP keys ride the matcher now,
    // including the `g`-prefixed go-to trio (`gd`/`gD`/`gr`): the matcher can own
    // the `g` prefix without breaking core's `gg`/`ge`/`dgg`/… motions because the
    // `command_status` oracle (merged from main) releases a withheld `g`-run to the
    // editor the moment it completes a built-in, so `gg` fires whole instead of
    // being folded into `gd`. This retires the bespoke `lsp_pending_g` recognizer
    // (and, earlier, `lsp_pending_ctrl_x` — `<C-x><C-o>` is just a two-key map).
    host.keymaps.set_native_defaults(vec![
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
        let buf = host.editor.current_buffer_id();
        let name = host.editor.buffer_name(buf).unwrap_or_default();
        let ft = filetype_of(host.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = host.lua.set_buf_snapshot(buf.0, &name, ft);
    }
    // Seed the buffer mirror too, so `init.lua` can read buffer lines / the cursor.
    host.push_buf_mirror();

    // Source the user's `init.lua` (if any) before serving the client, exactly
    // as neovim runs config at startup: its options, mappings, and colorscheme
    // are in place by the time the first `redraw` goes out on UI attach.
    if let Some(config_dir) = &init.config_dir {
        host.source_init(&config_dir.join("init.lua"));
    }
    // Then source the package `plugin/` / `after/plugin/` Lua scripts across the
    // runtimepath — neovim's startup package load, after `init.lua` and before the
    // first buffer's lifecycle events, so a plugin's autocmds/registration are in
    // place (this is what initializes a completion plugin's engine, its sources, etc.).
    host.source_plugins();

    // Startup seed: the initial buffer and the config's autocmds both exist now,
    // so fire the first buffer's lifecycle events (`BufReadPost`→`FileType`→
    // `BufEnter` for a file arg, `BufEnter` alone for the bare `[No Name]`).
    // Pre-seed the window set so the first window doesn't fire `WinNew` (neovim
    // skips it for the initial window); `last_window_id` stays `None` so the
    // first `WinEnter` still fires alongside `BufEnter`, the window analogue.
    host.known_windows = host.editor.window_ids();
    host.last_window_rects = Some(host.window_rects_snapshot());
    // Pre-seed the tab set so the initial tab doesn't fire `TabNew` (neovim, like
    // for the first window, doesn't); `last_tab_id` stays `None` so a later switch
    // still fires the first `TabEnter`/`TabLeave` pair.
    host.known_tabs = host.editor.tab_ids();
    // Seed the buffer set too, so the startup buffer isn't seen as "newly gone"
    // and a never-deleted buffer never triggers a spurious cleanup.
    host.known_buffers = host.editor.buffer_ids();
    // Load the shada (persistence) store before the first frame: it recency-merges
    // + compacts any sibling stores and seeds this session's registers / marks /
    // history / jumplist, so a plugin reading them at `VimEnter` sees the restored
    // state. A no-op when persistence is disabled (`shada: None`, the test default);
    // a store that won't load is surfaced and then dropped (the editor runs on
    // without persistence rather than dying). The store lives on `host` from here,
    // so the debounced checkpoint and the exit flush both reach it through the seam.
    host.shada_load();
    host.emit_lifecycle_events();
    host.run_pending();
    // The startup VimEnter point has passed: `v:vim_did_enter` is now 1, so a
    // plugin that gates "the editor has finished starting" reads it as true.
    let _ = host.lua.set_vim_did_enter(true);

    // The run loop is a thin translator over the `host` (the standalone `EditHost`): each
    // arm receives one event off a transport and hands the whole batch to an inbound-seam
    // handler (`inbound.rs`), which coalesces the channel, runs the per-event tick method,
    // and settles. No arm touches `host.editor` / `host.lua` directly, and `host` reaches
    // back out only through its `fx` seam — so this loop + `NativeEffects` are the only
    // native-specific pieces; a wasm Worker (Phase 5) supplies its own of each.
    loop {
        tokio::select! {
            // Editor input / API calls from the UI client.
            message = incoming.recv() => {
                let Some(message) = message else { break };
                if host.on_client_message(message).await {
                    break;
                }
            }
            // Replies from the language servers (initialize handshakes, published
            // diagnostics, server exits, log messages). Selecting here keeps the
            // editor responsive regardless of any server's speed or health.
            Some(event) = lsp_events.recv() => host.on_lsp_events(event, &mut lsp_events),
            // Timers and child-process completions from the event-loop actor — the
            // first thing that wakes the server on wall-clock time rather than RPC. The
            // matching Lua callback runs here; the shada debounce-timer also wakes this
            // arm, and `on_loop_events` sorts its flush out from the real callbacks.
            Some(event) = loop_events.recv() => host.on_loop_events(event, &mut loop_events),
            // A `:terminal` child wrote output or exited: feed the bytes to its vt100
            // emulator (refreshing the buffer's mirrored screen) or record the exit.
            Some(event) = term_events.recv() => host.on_term_events(event, &mut term_events),
            // Bytes for an off-tick open arrived from the daemon's fs — the startup
            // file (kept from freezing startup) or a later `:edit`. Idle for a
            // bare/local session.
            Some(open) = open_rx.recv() => host.on_opens(open, &mut open_rx),
            // A `:TSInstall` background job finished (grammar fetched + compiled, or it
            // failed): reload the grammar so open buffers re-highlight/indent, echo.
            Some(outcome) = install_events.recv() => host.on_installs(outcome, &mut install_events),
            // An off-tick `:w` finished on the daemon (the save path): finalize the
            // buffer's saved-state and replay any deferred `:wq`/`:x` quit. The replayed
            // quit can ask the editor to exit — the one non-input arm that can.
            Some(done) = save_done_rx.recv() => {
                if host.on_save_dones(done, &mut save_done_rx) {
                    break;
                }
            }
            // The daemon's watch leg pushed a file change (`HostWatch`): reconcile it off
            // the editor tick. Idle for a local/bare session (nothing ever sends here).
            Some(ev) = watch_rx.recv() => host.on_watch_events(ev, &mut watch_rx),
        }
    }
    // The loop has exited (quit or client disconnect): flush the final snapshot to
    // this instance's store, then drop it (releasing the file lock) so the next
    // instance can merge this one's clean checkpoint. Unlike the debounced live
    // checkpoint, this *clean-exit* flush carries `exit_cursor` (where the cursor sits
    // now) — the store turns it into `'0` on the next launch, so `'0` only ever
    // reflects a clean exit. Best-effort — we're leaving.
    host.shada_flush_final();
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
#[cfg(feature = "native")]
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
