//! The bemtvi server: a headless editor process that owns the core model and
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
mod cwd;
mod edithost;
mod effects;
mod excmd;
mod extmarks;
mod input;
mod keymap;
mod lifecycle;
mod redraw;
mod save;
// The `snippets` completion source + snippet-engine effect drains. Feature-agnostic:
// the native snippet engine lives in core (no async / LSP), so it builds on wasm too.
mod snippet;
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
mod folds;
#[cfg(feature = "native")]
mod host;
#[cfg(feature = "native")]
mod http;
// The `btv.http.mount` listener — inbound HTTP, the twin of `http`'s outbound. Native only:
// a browser tab cannot bind a TCP port, so the wasm build reaches the same mount contract
// through a Service Worker instead.
#[cfg(feature = "native")]
mod httpmount;
#[cfg(feature = "native")]
mod inbound;
// The LSP consumer subtree is synchronous and tick-driven — shared by the native
// build (events from the async `LspManager`) and the wasm build (events from the
// `SyncLspClient`); only the outbound seam + inbound delivery differ by `cfg`.
mod lsp;
#[cfg(feature = "native")]
mod quic;
#[cfg(feature = "native")]
mod reconnect;
// Un-gated: the materialize half + the wire decoder are shared by the native edit-host
// and the wasm one (the browser fetches the bundle over WebTransport, then materializes
// it into emscripten's in-memory FS exactly as the native client stages it on disk).
mod remote_config;
#[cfg(feature = "native")]
mod session_spawn;
#[cfg(feature = "native")]
mod shada;
#[cfg(feature = "native")]
mod treesitter;

/// The wasm twin of [`treesitter::EditHost::resolved_preview_highlights`] — the
/// stateless highlight the picker preview, LSP doc floats and
/// `btv.treesitter.highlight` go through.
///
/// The native side resolves the language's (and its injected languages')
/// runtimepath queries before painting; the whole runtimepath query bridge lives in
/// the native-gated `treesitter` module because the serverless browser build has no
/// tree-sitter engine to install queries into — it highlights JS-side
/// (web-tree-sitter), and its `SyntaxEngine` returns no spans here at all. So on
/// this build there is nothing to resolve and the call is the plain one, keeping the
/// two shared call sites (`redraw.rs`, `effects.rs`) cfg-free.
#[cfg(not(feature = "native"))]
impl EditHost {
    pub(crate) fn resolved_preview_highlights(
        &mut self,
        lang: &str,
        text: &str,
        first: usize,
        last: usize,
    ) -> (Vec<bemtvi_core::Span>, Vec<usize>) {
        self.editor.preview_highlights_bg(lang, text, first, last)
    }

    /// The wasm twin of [`treesitter::EditHost::settle_ts_highlight`]. Nothing to park
    /// on: this build loads no grammars (it highlights JS-side), so the answer this
    /// call produces is the final one and the promise settles here.
    pub(crate) fn settle_ts_highlight(
        &mut self,
        lang: String,
        text: String,
        nlines: usize,
        cb_id: u64,
    ) {
        let (spans, _bg) = self.resolved_preview_highlights(&lang, &text, 0, nlines);
        let spans = spans
            .into_iter()
            .map(|s| (s.line, s.start_byte, s.end_byte, s.group))
            .collect();
        if let Err(e) = self.lua.run_callback(
            cb_id,
            false,
            bemtvi_lua::CallbackArgs::TsHighlight { spans },
        ) {
            self.editor
                .echo(format!("E: btv.treesitter.highlight callback: {e}"));
        }
    }
}

/// The cross-session snapshot the [`ShadaStore`] seam round-trips, re-exported so an
/// out-of-crate store implementor (a test probe, the future wasm OPFS backend) can
/// name the `load`/`flush` payload and its entries without depending on
/// `bemtvi-core` directly.
pub use bemtvi_core::{
    FileChangelist, FileMarkEntry, GlobalMarkEntry, JumpPos, NumberedMark, PersistState,
    RegisterEntry,
};
/// The process-spawning seam (`vim.system` / `jobstart` / `:!`) and its types,
/// re-exported for [`ServerInit::host_proc`] — the edit-host split injects a
/// daemon-backed [`HostProc`] here (the process-side companion to
/// [`bemtvi_core::HostFs`]).
#[cfg(feature = "native")]
pub use host::{HostProc, ProcEvents, ProcSpec, StdHostProc};
/// The persistence (shada) seam and its native redb backend. The store sits
/// behind [`ShadaStore`] so the platform layer injects it through
/// [`ServerInit::shada`] — native binaries pass [`default_shada`] (redb over a
/// file at [`shada_dir`]); the wasm Worker build will pass a redb-over-OPFS store;
/// tests pass a [`RedbFileStore`] over a temp dir, or `None` to disable.
#[cfg(feature = "native")]
pub use shada::{
    default_shada, is_store_file, prepare_remote_shada, resolve_session_shada, shada_dir,
    valid_namespace, workspace_namespace, workspace_shada, RedbFileStore, RemoteShada, ShadaStore,
};

/// The daemon wire protocol for the edit-host split: the daemon-side servers
/// ([`serve_daemon`] for child processes, [`serve_fs_daemon`] for file reads) and the
/// edit-host-side clients ([`RemoteHostProc`], [`RemoteHostFs`]) that forward to them
/// over any [`AsyncRead`](tokio::io::AsyncRead)/[`AsyncWrite`](tokio::io::AsyncWrite)
/// wire (a duplex, or ssh stdio to `bemtvi --daemon`). [`HostFsAsync`] is the async fs
/// seam the server fetches buffer contents through off the editor tick; [`FsRead`] is
/// what one fetch resolves to.
#[cfg(feature = "native")]
pub use daemon::{
    connect_daemon, connect_daemon_reconnecting, connect_daemon_reconnecting_on, parse_connect_uri,
    serve_config_daemon, serve_config_daemon_on, serve_daemon, serve_dproc_daemon_on,
    serve_fs_daemon, serve_fs_daemon_on, serve_git_daemon, serve_git_daemon_on, serve_http_daemon,
    serve_http_daemon_on, serve_lsp_daemon, serve_lsp_daemon_on, serve_luafs_daemon,
    serve_luafs_daemon_on, serve_luafs_watch_daemon, serve_luafs_watch_daemon_on,
    serve_proc_daemon_on, serve_sock_daemon_on, serve_term_daemon_on, DaemonClient, DaemonStatus,
    FsRead, HostFsAsync, ReconnectHandle, ReconnectPolicy, RemoteConfig, RemoteFsJobs,
    RemoteFsWatch, RemoteGitJobs, RemoteHostFs, RemoteHostProc, RemoteHostTerm, RemoteHttp,
    RemoteLspTransport, WatchEvent, CONNECT_URI_SCHEME, DAEMON_TOKEN_ENV,
};
/// The parsed `btv_session_reconnect` spec (§B): the client-persistent session-swap request
/// both native front ends decode and act on. See [`reconnect`].
#[cfg(feature = "native")]
pub use reconnect::{ReconnectSpec, ReconnectTransport, SpawnCommand};
/// Materialize a fetched bundle onto the per-process cache resolved from the environment
/// (`$XDG_CACHE_HOME`/`$HOME`) — the native edit-host's entry point.
#[cfg(feature = "native")]
pub use remote_config::materialize_remote_config;
/// The fetched-config bundle, the wire decoder, and the materialize half — un-gated so the
/// wasm edit-host shares them with native: native fetches over the daemon link and stages
/// the files on disk; wasm fetches over WebTransport (in JS) and stages them in emscripten's
/// in-memory FS. (Phase 2/4 of `docs/plans/2026-06-23-remote-config-and-plugins.md`.)
pub use remote_config::{
    decode_config_bundle, decode_config_bundle_bytes, materialize_remote_config_into,
    RemoteConfigBundle,
};
/// The client-side session-spawn scaffolding every UI binary shares: the blocking
/// spawn-server-thread handshake, the re-spawning stdio-daemon link, and the
/// private daemon stderr log.
#[cfg(feature = "native")]
pub use session_spawn::{
    connect_daemon_respawning, daemon_log_stderr, env_daemon_command, spawn_session_thread,
    SessionGuard, DAEMON_CMD_ENV,
};

/// Where the wasm edit-host stages a fetched [`RemoteConfigBundle`]: a fixed root in
/// emscripten's in-memory FS (MEMFS). One editor per Worker and the FS is fresh on every
/// page load, so a fixed path (no pid) is enough, and "fresh every connect" — the native
/// freshness policy — falls out for free. See [`EditHost::apply_remote_config`].
#[cfg(not(feature = "native"))]
const WASM_REMOTE_CACHE_ROOT: &str = "/bemtvi/remote";

/// One macro playback in flight: the register's keys, how far through them we
/// are, and how many repeats are left (`{count}<F3>a`). Held on a stack by
/// [`EditHost::drive_macro_play`], so a macro that plays another suspends its
/// caller rather than splicing into it.
struct MacroFrame {
    /// The register this frame is playing, mirrored to core for
    /// `btv.macro.executing()` while it is the innermost frame.
    reg: char,
    keys: Vec<Key>,
    pos: usize,
    repeats: usize,
}

/// The native daemon transport (Open Decision #2): a WebTransport/QUIC listener that
/// runs the [`run_daemon_io`] multiplexer over one bidi stream ([`serve_quic`], the
/// `--daemon --listen` role), and the edit-host-side [`connect_quic_reconnecting`] that pins
/// the daemon's self-signed cert TOFU + presents the launch-minted bearer token and returns
/// the same [`DaemonClient`] the ssh path does — plus a [`ReconnectHandle`] so a dropped QUIC
/// link auto-re-dials. [`bind_quic_listener`] mints the identity/token and resolves the bound
/// address (for an ephemeral `:0` port).
#[cfg(feature = "native")]
pub use quic::{
    bind_quic_listener, connect_quic_reconnecting, mint_token, serve_quic, ListenerInfo,
};

use bemtvi_core::{
    BufferId, Editor, FileStat, HostFs, Key, Mode, PendingSave, PluginEntry, PluginNamespace,
    PreWrite, ShadaRequest, StdHostFs, TabId, WindowId,
};
#[cfg(feature = "native")]
use bemtvi_lsp::LspManager;
use bemtvi_lsp::{CodeActionData, ServerKey, ServerSpawn};
use bemtvi_lua::LuaRuntime;
#[cfg(feature = "native")]
use bemtvi_rpc::{connect, connect_bounded, Incoming, Rpc};
/// The outbound async-effect seam the synchronous [`EditHost`] tick emits through
/// (redraws / notifications to the client, off-tick fs, the event-loop / LSP command
/// sinks). Re-exported so the out-of-crate wasm cdylib ([`bemtvi-edithost`], slice 5b)
/// can implement it for the browser transport, the way [`NativeEffects`] implements it
/// for the native server.
pub use edithost::HostEffects;
#[cfg(feature = "native")]
use edithost::NativeEffects;
#[cfg(feature = "native")]
use evloop::{EventLoop, LoopCommand, LoopEvent};
use keymap::Keymaps;
use lsp::{
    DiagnosticConfig, InlayResolveTarget, LspComplete, LspDocState, LspFanout, LspReqKind,
    PendingLspReq, PendingMultiReq, ProgressEntry, ServerRuntime,
};
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
    /// The **global** history store handle for a workspace launch, used iff
    /// `'persisthistory'` resolves to `global` (e.g. `"global"` / `"global,workspace"`):
    /// history then persists here instead of the workspace store (history-only, never
    /// compacting), so it is shared across projects. `None` for a plain launch (its
    /// [`shada`] IS the global store) and for a remote/wasm session (single store). The
    /// binary sets it to [`default_shada`] for a local workspace launch; a test injects a
    /// temp store. The server only uses it when the option targets `global`.
    pub global_shada: Option<Box<dyn ShadaStore + Send>>,
    /// Whether this launch is a session-scoped workspace (its [`shada`] store is a
    /// private `ns/<uuid>` subfolder, set via `--shada-namespace`). When `true`, the
    /// shada additionally **captures** the editor session (open files + exact layout) at
    /// each flush. Default `false` — the global store never persists layout.
    pub workspace_session: bool,
    /// Whether to **restore** a previously captured session (window/tab layout) at boot.
    /// Explicit and separate from capture: `--restore-session` sets it (requires a
    /// namespace), so a plain `--shada-namespace` launch isolates marks/registers without
    /// rearranging your windows. Default `false`.
    pub restore_session: bool,
    /// Seed the layout-capture opt-in (`btv.shada.save_layout`) on at startup, before any
    /// config runs. The `--workspace` flag sets it so a directory session captures its
    /// window/tab layout *without* needing a plugin to opt in; a plain `--shada-namespace`
    /// launch leaves it `false` (the plugin decides). A config may still toggle it via
    /// `btv.shada.save_layout`. Only meaningful together with [`workspace_session`].
    pub session_save_layout: bool,
    /// The shada namespace surfaced to Lua via `btv.shada.namespace()` (the `--shada-namespace`
    /// value, or a `--workspace`-derived token). Seeded into the runtime before config runs.
    /// For a daemon workspace this is derived from the *daemon's* cwd post-connect. `None` =
    /// the global store / not scoped. Independent of where the shada store physically lives.
    pub shada_namespace: Option<String>,
    /// The absolute workspace root surfaced via `btv.workspace` (`None` outside `--workspace`).
    /// The daemon's directory for a remote session. Seeded alongside [`shada_namespace`].
    ///
    /// [`shada_namespace`]: Self::shada_namespace
    pub workspace_dir: Option<String>,
    /// Whether a `--workspace` launch cds into [`workspace_dir`](Self::workspace_dir) at boot
    /// (the canonical `bemtvi --workspace DIR`; `--workspace-no-cwd` clears it). Done *before*
    /// the editor opens any file or restores the session, so a relative startup file and the
    /// session's relative buffer paths resolve against the workspace root with no later
    /// reconciliation. `false` (the default) outside a local cd-ing `--workspace` launch — a
    /// daemon session cds on the daemon instead.
    pub workspace_cwd: bool,
    /// For a `Remote`-config daemon session: the on-daemon shada file the staged local
    /// [`shada`](Self::shada) store syncs to (Approach A — see [`prepare_remote_shada`]).
    /// `None` (the default) keeps shada purely local. When set, the edit-host uploads the
    /// store's bytes to the daemon over the fs seam after each flush, so the session's
    /// editor state persists on the remote machine. Native-only — the web build has its
    /// own OPFS shada path.
    #[cfg(feature = "native")]
    pub remote_shada: Option<RemoteShada>,
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
    /// A fake **second** clock for the editor's monotonic time base — the timeline
    /// undo nodes are stamped on and `vim.fn.localtime()` mirrors. `None` (the
    /// default) uses `start.elapsed()`; a test injects a shared counter and advances
    /// it between edits so `:earlier {N}s` / the `:undolist` age column can be driven
    /// without sleeping. See [`EditHost::mono_stamp_secs`].
    pub mono_clock: Option<Arc<AtomicU64>>,
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
    /// The transport language servers are spawned through. `None` (the default) runs
    /// them as real local children ([`LocalLspTransport`](bemtvi_lsp::LocalLspTransport));
    /// the edit-host split injects a daemon-backed [`RemoteLspTransport`] here so a
    /// language server runs on the remote where the project files are, tunneling its
    /// long-lived stdio over the wire while editing stays local
    /// (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). `Send` (boxed)
    /// so it rides [`ServerInit`] onto the server's own thread, where it is rebuilt into
    /// the shared `Arc<dyn LspTransport>` the [`LspManager`] holds.
    pub lsp_transport: Option<Box<dyn bemtvi_lsp::LspTransport + Send>>,
    /// The terminal seam a daemon session's `:terminal` opens its PTY through. `None` (the
    /// default) opens a **local** PTY via the [`TerminalManager`](crate::terminal::native);
    /// the edit-host split injects a daemon-backed [`RemoteHostTerm`] here so the terminal
    /// child runs on the **remote** where the files are, streaming its output back over the
    /// Term leg while editing stays local — the native twin of the wasm `term_*` path. When
    /// set, [`NativeEffects`](crate::edithost::NativeEffects) routes `:terminal` ops to it
    /// (and the run loop selects on its `TermEvent` stream) instead of the local actor.
    /// Native-only (the wasm build owns its own terminal transport).
    #[cfg(feature = "native")]
    pub host_term: Option<RemoteHostTerm>,
    /// The async `btv.fs` daemon seam. `None` (the default) runs `btv.fs` ops on the local
    /// disk (the event-loop actor's [`StdLuaFs`](bemtvi_lua::StdLuaFs) on its blocking pool);
    /// the edit-host split injects a daemon-backed [`RemoteFsJobs`] so a plugin reads the
    /// *remote* project (file previews, LSP `root_dir` detection, git-status marks) over the
    /// `luafs_op` leg — the whole [`FsJob`](bemtvi_lua::FsJob) crosses in one request,
    /// decomposed daemon-side (`docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
    /// Nothing parks the editor thread: the actor `await`s the reply off the tick. `Send`
    /// (boxed) so it rides [`ServerInit`] onto the server thread, where `run_server` turns
    /// it into the actor's [`FsBackend`](crate::evloop::FsBackend).
    pub fs_jobs: Option<RemoteFsJobs>,
    /// The streaming `btv.fs.watch` daemon seam. `None` (the default) arms watches on the
    /// local disk (the actor's `notify` backend); a daemon session injects a
    /// [`RemoteFsWatch`](crate::daemon::RemoteFsWatch) so a watch is armed on the *daemon*,
    /// where the session's files are — otherwise `btv.fs.watch` (and everything on it: the
    /// LSP `workspace/didChangeWatchedFiles` client, file trees, config reloaders) watched
    /// this machine and never saw a remote change. `run_server` hands it to the actor and
    /// takes its push stream for the `fs_watch_rx` `select!` arm.
    #[cfg(feature = "native")]
    pub fs_watch: Option<crate::daemon::RemoteFsWatch>,
    /// The async `btv.http` daemon seam. `None` (the default) runs `btv.http.fetch` on the
    /// local machine (the event-loop actor's `ureq` on its blocking pool); the edit-host
    /// split injects a daemon-backed [`RemoteHttp`] so a request runs on the *daemon* (which
    /// owns the network) over the `http_op` leg. `run_server` turns it into the actor's
    /// [`HttpBackend`](crate::evloop::HttpBackend). The HTTP twin of [`fs_jobs`](Self::fs_jobs).
    #[cfg(feature = "native")]
    pub http_jobs: Option<RemoteHttp>,
    /// The async `btv.git` daemon seam. `None` (the default) runs `btv.git.*` ops on the local
    /// disk (the actor's gix engine on its blocking pool); a daemon session injects a
    /// daemon-backed [`RemoteGitJobs`] so a plugin queries the *remote* repo (branch / diff /
    /// blame marks) over the `git_op` leg — the whole [`GitJob`](bemtvi_lua::GitJob) crosses in
    /// one request, run daemon-side. `run_server` turns it into the actor's
    /// [`GitBackend`](crate::evloop::GitBackend). The git twin of [`fs_jobs`](Self::fs_jobs).
    #[cfg(feature = "native")]
    pub git_jobs: Option<crate::daemon::RemoteGitJobs>,
    /// Whether to offer bemtvi's built-in default recommended plugin set on a fresh
    /// setup. `false` (the default) leaves the recommended set empty, so the headless
    /// test suites never trip the first-run welcome; the interactive binary sets it
    /// `true`, which activates `btv.plugins._default_recommended` before `init.lua`
    /// runs (a config's own `btv.plugins.recommend{...}` still overrides it). Mirrors
    /// how `shada` / `clipboard` keep tests hermetic while the binary opts in.
    pub offer_default_recommended: bool,
    /// The **system-plugin tier** — local plugins the *client* loads into this session
    /// before `init.lua`, un-shadowable-from-config in load order but never able to
    /// hijack a user module name (they sit AHEAD of managed plugins but BEHIND the
    /// config dir on `package.path`). Empty (the default) so headless suites and
    /// `--lua` stay hermetic, exactly like `offer_default_recommended`; the interactive
    /// binaries populate it from the client-owned system dir
    /// (`stdpath("data")/system/*`, see [`discover_system_plugins`]). Each spec is a
    /// resolved **local dir** — the server only ever sees real dirs, so a system plugin
    /// loads through the same path (rtp + `plugin/` sourcing) as any managed plugin,
    /// with real files (tracebacks + LS visibility). The dirs are spliced into the
    /// runtimepath (after the config dir) and sourced in a dedicated phase before
    /// `init.lua`; the later `source_plugins` pass skips them so they load exactly once.
    /// Runs through the local disk so a system plugin loads locally even in a daemon
    /// session (consistent with the remote-aware plugin manager). See
    /// `docs/plans/2026-07-05-remote-connectors-and-system-plugins.md` → §A.
    pub system_plugins: Vec<SystemPluginSpec>,
    /// Whether to enable command-line completion (`btv.cmdline_complete` — `:`+`<Tab>`)
    /// by default. `false` (the default) leaves the engine off, so the headless test
    /// suites stay byte-for-byte unchanged; the interactive binary sets it `true`,
    /// which runs `btv.cmdline_complete.setup{}` BEFORE `init.lua` so a config's own
    /// `btv.cmdline_complete.setup{ ... }` (e.g. toggling the `docs` preview pane)
    /// still wins. Mirrors `offer_default_recommended`.
    pub cmdline_complete_default: bool,
    /// Route captured Lua message output (`print` / `nvim_echo` / `btv.err_write*`) to the
    /// process's real stdout / stderr instead of the (absent) message line. `false` (the
    /// default) keeps the normal in-editor routing; the `bemtvi --lua CODE` one-shot sets it
    /// `true` so a headless script's `print` reaches the shell/CI that launched it. Only ever
    /// set for that one-shot — an interactive/daemon session's stdout carries the RPC wire, so
    /// writing to it there would corrupt the transport. See [`EditHost::lua_stdio`].
    pub lua_stdio: bool,
    /// The daemon's installed tree-sitter parser languages, so a remote session can
    /// compile the *same* parsers locally (parsers are native artifacts, never fetched
    /// over the wire). Compiled **lazily** — the first time a buffer of each filetype
    /// opens, not all at once at startup. The server hands this set to Lua
    /// (`btv._remote_ts_autoinstall`), which registers a `FileType` autocmd that
    /// `:TSInstall`s each on first sight (see [`set_up_remote_ts_autoinstall`]). Empty
    /// (the default) for a local session — nothing to mirror.
    pub ts_autoinstall: Vec<String>,
    /// The daemon's working directory, to seed [`DirState`](crate::cwd) in a remote
    /// session. `None` (the default) seeds it from the **local** process cwd, as a
    /// local session needs. When set (the edit-host split fetches it over the
    /// `config_bundle` handshake), `:cd`/`:pwd`/`vim.fn.getcwd` operate on the
    /// **daemon's** cwd rather than the local process's — the remote-cwd fix
    /// (`docs/plans/2026-06-23-remote-cwd.md`). The path is the daemon's absolute
    /// cwd and may not exist on the edit-host's local disk, so it is never
    /// `set_current_dir`'d locally; it lives only in `DirState` + the `btv._cwd`
    /// mirror, and `:cd` moves it over the wire (`fs_chdir`).
    pub remote_cwd: Option<PathBuf>,
    /// The daemon's home directory, the base a leading `~` in a file argument (`:e ~/x`)
    /// expands against. `None` (the default) — a local session — expands `~` against this
    /// process's own `$HOME`. When set (fetched over the `config_bundle` handshake, like
    /// [`remote_cwd`](Self::remote_cwd)), `~` resolves on the **daemon**, where the file
    /// read lands even though the core runs on the client.
    pub remote_home: Option<PathBuf>,
    /// A Lua chunk the **client** wants run at startup, after the prelude/built-in
    /// opt-ins and *before* `init.lua` — so a config can still override it. `None` (the
    /// default) for the TUI and tests. The GUI uses it to register its client-handled
    /// virtual commands (`:connect` / `:workspace`) as no-op user commands, so they get
    /// command-name completion, help, and cmdline history (the client still intercepts
    /// them to do the actual session swap; the server-side body is a no-op). Failing
    /// loud: a chunk error is surfaced, not swallowed.
    pub client_init_lua: Option<String>,
    /// The reconnecting daemon link's handle, when the session connects to a `--daemon`
    /// over a reconnectable transport. `None` (the default) for a local/bare session — and
    /// for a one-shot daemon link (the non-reconnecting `connect_daemon`/`connect_quic`
    /// paths). When set, the run loop reflects its [`DaemonStatus`] into the editor + Lua
    /// (`btv.daemon.status()`, the `User DaemonStatusChanged` autocmd, a "run `:reconnect`"
    /// message on give-up), and `:reconnect` / `:disconnect` drive it. Native-only — the
    /// reconnectable link ([`ReconnectHandle`]) lives in the native-gated transport tree; the
    /// wasm edit-host's own reconnect is a later phase.
    #[cfg(feature = "native")]
    pub daemon_link: Option<ReconnectHandle>,
}

/// How the server provides the `"+` / `"*` clipboard registers.
#[cfg(feature = "native")]
#[derive(Default)]
pub enum ClipboardProvider {
    /// Best-effort real host clipboard (the binary's choice): a host tool if one
    /// is usable here, else the client terminal's OSC 52 clipboard
    /// ([`Osc52`](Self::Osc52)) when it declares support. If neither is available
    /// the registers stay unavailable and error loudly on use rather than
    /// silently falling back to the unnamed register.
    System,
    /// The client terminal's **OSC 52** clipboard, armed at attach if the client
    /// declares the `osc52` capability (and left unarmed — loudly unavailable —
    /// if it doesn't). What [`System`](Self::System) resolves to on a machine with
    /// no usable clipboard tool: an ssh session's copy has to reach the terminal
    /// emulator, not the remote host. Selected directly by tests, which must not
    /// depend on what the developer's box has installed.
    Osc52,
    /// No provider — `"+` / `"*` error loudly. The default, so bare-server tests
    /// never touch the host clipboard unless they opt in.
    #[default]
    Disabled,
    /// A caller-supplied provider; tests inject an in-memory fake here.
    Custom(Box<dyn bemtvi_core::Clipboard>),
}

/// Resolve bemtvi's config directory and runtimepath from the environment, the
/// way the real binary starts up. Tests bypass this and pass explicit paths in
/// [`ServerInit`] instead, so they never depend on the host's home directory.
///
/// - **Config dir:** `$BEMTVI_CONFIG`, else `$XDG_CONFIG_HOME/bemtvi`, else
///   `$HOME/.config/bemtvi` (`None` if none resolve).
/// - **Runtimepath:** any `$BEMTVI_RUNTIMEPATH` entries first (explicit override),
///   then the config dir, then every plugin discovered under
///   `<config>/pack/*/start/*` (neovim's package layout, so a plugin checkout is
///   drop-in).
#[cfg(feature = "native")]
pub fn default_runtime() -> (Option<PathBuf>, Vec<PathBuf>) {
    let config_dir = resolve_config_dir();
    let mut runtimepath: Vec<PathBuf> = Vec::new();
    if let Some(rtp) = std::env::var_os("BEMTVI_RUNTIMEPATH") {
        runtimepath.extend(std::env::split_paths(&rtp));
    }
    if let Some(cfg) = &config_dir {
        runtimepath.push(cfg.clone());
        runtimepath.extend(discover_plugins(cfg));
    }
    (config_dir, runtimepath)
}

/// First of `$BEMTVI_CONFIG`, `$XDG_CONFIG_HOME/bemtvi`, `$HOME/.config/bemtvi`.
#[cfg(feature = "native")]
fn resolve_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("BEMTVI_CONFIG") {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg).join("bemtvi"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("bemtvi"))
}

/// One entry in the [system-plugin tier](ServerInit::system_plugins): a resolved
/// local plugin dir plus the `name` it registers under (its require key / shada
/// namespace / directory basename). Not embedded — always a real on-disk repo,
/// cloned into the system dir like any managed plugin.
#[cfg(feature = "native")]
#[derive(Clone, Debug)]
pub struct SystemPluginSpec {
    /// The plugin's name — the system-dir subdirectory basename, its require key,
    /// and the key the tier registry / promotion API address it by.
    pub name: String,
    /// The plugin's resolved local directory (`stdpath("data")/system/<name>`, or a
    /// dev checkout). The server adds this to the runtimepath and sources its
    /// `plugin/` scripts.
    pub dir: PathBuf,
}

/// The client-owned **system-plugin dir**: `stdpath("data")/system`. One plugin repo
/// per immediate subdirectory. Leaned to DATA (managed artifacts, not hand-edited
/// config) per the plan's open decision.
#[cfg(feature = "native")]
pub fn system_plugin_dir() -> PathBuf {
    PathBuf::from(bemtvi_lua::stdpath("data")).join("system")
}

/// Scan the [system-plugin dir](system_plugin_dir) into the tier the interactive
/// binaries thread onto every [`ServerInit`]. Each immediate subdirectory is one
/// system plugin, named for its basename. A missing/unreadable dir yields none (the
/// common no-system-plugins case), so a fresh install stays empty — matching the
/// default-empty `system_plugins` that keeps headless suites hermetic.
#[cfg(feature = "native")]
pub fn discover_system_plugins() -> Vec<SystemPluginSpec> {
    let dir = system_plugin_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut specs: Vec<SystemPluginSpec> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // Skip dotfiles / hidden dirs (`.git`, editor scratch) — a system plugin
            // is a named repo, never a leading-dot dir.
            (!name.starts_with('.')).then(|| SystemPluginSpec {
                name,
                dir: e.path(),
            })
        })
        .collect();
    // Deterministic load order across runs (readdir order is unspecified).
    specs.sort_by(|a, b| a.name.cmp(&b.name));
    specs
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

/// Whether a daemon (edit-host) session runs the **daemon's** config + shada or the
/// **local** machine's. Chosen per connection: the native clients default to `Local`
/// (the `--remote-config` flag opts into `Remote`); the web client is always `Remote`
/// (it has no local config / local disk). `Remote` fetches + materializes the daemon's
/// config tree and (Phase 2) keeps shada on the daemon; `Local` runs
/// [`default_runtime`] and the local shada, fetching only the daemon's cwd / parser set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigSource {
    /// The client's own config + local shada (native default).
    Local,
    /// The daemon's config + plugins (materialized locally) + remote shada (web default).
    Remote,
}

/// The session config resolved from a [`ConfigSource`] against a daemon's config leg
/// ([`RemoteConfig::resolve`](crate::RemoteConfig::resolve)): the `config_dir` /
/// `runtimepath` to source (the daemon's materialized copy for `Remote`, the local roots
/// for `Local`), plus the daemon's cwd and tree-sitter parser set (seeded in both modes,
/// since the buffers / fs are always on the daemon).
#[cfg(feature = "native")]
pub struct ResolvedConfig {
    /// The config dir to source `init.lua` from (`None` = no config).
    pub config_dir: Option<PathBuf>,
    /// The runtimepath, in load order.
    pub runtimepath: Vec<PathBuf>,
    /// The daemon's working directory, to seed `DirState` (`None` = keep local cwd).
    pub remote_cwd: Option<PathBuf>,
    /// The daemon's home directory, the base a leading `~` in a file argument expands
    /// against (`None` = expand `~` against the edit-host's own `$HOME`).
    pub remote_home: Option<PathBuf>,
    /// The daemon's installed tree-sitter parser languages, auto-installed locally.
    pub ts_autoinstall: Vec<String>,
    /// The daemon's shada base dir, where a `Remote`-config session stages + syncs its
    /// shada (`None` if an older daemon omitted it — remote shada then unavailable).
    pub state_dir: Option<String>,
}

/// Gather this machine's whole config surface for the `config_bundle` daemon leg:
/// the [`default_runtime`] roots (the config dir + every runtimepath entry, plugins
/// included) plus every **source** file under them, each paired with its absolute
/// path. The edit-host mirrors these onto a local cache and points its
/// `config_dir`/`runtimepath` at the copy, so a remote session loads the *daemon's*
/// config and plugins (run locally) instead of the client's — see
/// `docs/plans/2026-06-23-remote-config-and-plugins.md`.
///
/// `default_runtime` already folds the config dir into the runtimepath, so its
/// entries are the complete set of roots to walk; the walk dedups by canonical path
/// so a nested or symlinked root never fetches a file twice. Native build artifacts
/// (`.so`/`.dylib`/`.dll`) are **skipped** — tree-sitter parsers and the like are
/// compiled locally on the client, and a remote-arch binary would not load anyway.
///
/// `(config_dir, runtimepath, files, ts_languages, state_dir)` — `files` is each source
/// file's absolute path paired with its bytes; `ts_languages` is the daemon's installed
/// tree-sitter parser languages (so the client can auto-install the same set); `state_dir`
/// is the daemon's shada base dir ([`shada_dir`]), so a `Remote`-config session can keep
/// its shada on the daemon (Phase 2 of the remote-config-and-shada plan).
#[cfg(feature = "native")]
pub(crate) type ConfigBundleData = (
    Option<PathBuf>,
    Vec<PathBuf>,
    Vec<(PathBuf, Vec<u8>)>,
    Vec<String>,
    PathBuf,
);

#[cfg(feature = "native")]
pub(crate) fn collect_config_bundle(include_files: bool) -> std::io::Result<ConfigBundleData> {
    let (config_dir, runtimepath) = default_runtime();
    // The daemon's shada base dir, reported in every bundle so a `Remote`-config session
    // can stage + sync its shada under it (the daemon runs no shada logic itself — only
    // whole files cross the wire; the session's `fs_mkdir` creates the actual
    // `ns/<NS>` (namespaced) or `remote` (global) dir before its first upload). Cheap, so the
    // lite fetch carries it.
    let state_dir = shada_dir();
    // A **local-config** session (`ConfigSource::Local`) fetches only the cheap metadata
    // — the daemon's cwd and its tree-sitter parser set — and runs the *client's* own
    // config, so it never transfers the daemon's config tree. The lite fetch skips the
    // (potentially large) file walk entirely; a **remote-config** session asks for the
    // files (`include_files = true`) and materializes them locally.
    let mut files = Vec::new();
    if include_files {
        let mut seen = std::collections::HashSet::new();
        for root in &runtimepath {
            walk_config_dir(root, &mut files, &mut seen)?;
        }
    }
    // The daemon's installed parser set: the client compiles the same languages
    // locally (parsers are native artifacts, never fetched — see `walk_config_dir`).
    let ts_languages = bemtvi_ts::installed_parsers()
        .into_iter()
        .map(|p| p.lang)
        .collect();
    Ok((config_dir, runtimepath, files, ts_languages, state_dir))
}

/// Recursively append every source file under `dir` to `out` as `(abspath, bytes)`,
/// skipping native artifacts (see [`collect_config_bundle`]). A non-existent root is
/// normal (no config, or an absent plugin dir) → nothing, not an error; any other
/// read failure is loud (a real, surfaced error, never a silent partial bundle).
/// `metadata`/`canonicalize` follow symlinks so a plugin symlinked into `pack/*/start`
/// (a common dev layout) is walked as the directory it points at; the canonical-path
/// `seen` set both dedups overlapping roots and breaks any symlink cycle.
#[cfg(feature = "native")]
fn walk_config_dir(
    dir: &Path,
    out: &mut Vec<(PathBuf, Vec<u8>)>,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> std::io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let canon = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if !seen.insert(canon) {
            continue; // already walked via another (nested/symlinked) root, or a cycle
        }
        let meta = std::fs::metadata(&path)?; // follows symlinks
        if meta.is_dir() {
            walk_config_dir(&path, out, seen)?;
        } else if meta.is_file() && !is_native_artifact(&path) {
            let bytes = std::fs::read(&path)?;
            out.push((path, bytes));
        }
    }
    Ok(())
}

/// A locally-compiled native artifact (`.so`/`.dylib`/`.dll`) that must not ride the
/// `config_bundle` across to a possibly-different-arch client.
#[cfg(feature = "native")]
fn is_native_artifact(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("so" | "dylib" | "dll")
    )
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
/// Public so the out-of-crate wasm cdylib ([`bemtvi-edithost`], slice 5b) can
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
    /// The `btv._cb_fns` callback id to run when the timer fires (the same id space the
    /// native [`LoopEvent::Timer`](crate::evloop::LoopEvent) carries).
    id: u64,
    /// Absolute due time on the Worker's JS clock (ms), `clock_ms + delay` at arm time.
    due_ms: u64,
    /// Repeat interval (ms); `0` is a one-shot (removed after it fires), `>0` re-arms to
    /// `now + repeat_ms` and keeps the Lua callback registered.
    repeat_ms: u64,
}

/// A window-local status line override (`btv.statusline.setup{win=…}`), stored in
/// [`EditHost::statusline_window`]. Absence from that map means the window
/// inherits the global layout.
enum WindowStatusline {
    /// The window shows its own segment layout, overriding the global one.
    Segments(bemtvi_core::statusline::SegmentLayout),
    /// The window uses the `'statusline'` `%`-format even while a global segment
    /// layout is active (the per-region mix).
    Format,
}

/// One step of the gated editor-exit sequence (`QuitPre` → `ExitPre` → `VimLeavePre` →
/// `VimLeave` + `should_quit`). Each variant names the event [`EditHost::drive_exit`] fires
/// *next*; the three `*Pre` stages are **awaited** (a handler may return a promise), so the
/// sequence can span ticks. `Leaving` is the terminal step — it fires the non-gated
/// `VimLeave` and asks the run loop to break. See the field [`EditHost::exit_stage`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ExitStage {
    /// Fire `QuitPre` (gated) next — the first hook a quit reaches.
    QuitPre,
    /// Fire `ExitPre` (gated) next — "the editor is really leaving".
    ExitPre,
    /// Fire `VimLeavePre` (gated) next — the last chance to flush before shada is written.
    VimLeavePre,
    /// Fire `VimLeave` (non-gated) and set [`Editor::should_quit`] — the point of exit.
    Leaving,
}

/// Which stage of a buffer's gated read chain fires **next**. See
/// [`EditHost::read_chains`] and `drive_read_chain`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ReadStage {
    /// Fire `BufReadPost` (or `BufNewFile`) — the buffer's content is present.
    ReadPost,
    /// Fire `FileType`, now that the read stage has fully settled — so a handler that
    /// detected the filetype *asynchronously* from the content is already reflected.
    FileType,
    /// Nothing left to fire: complete the chain (and its deferred `BufEnter`).
    Done,
}

/// A buffer's in-flight gated read chain. Created when the buffer is first announced,
/// dropped when it completes.
pub(crate) struct ReadChain {
    /// The stage to fire next.
    pub(crate) stage: ReadStage,
    /// Set while parked on a stage's async handlers; cleared by `drain_au_gate_done`,
    /// which then re-drives the chain.
    pub(crate) gate: Option<u64>,
    /// Fire `BufEnter` for this buffer when the chain completes. The buffer was
    /// *entering* when the chain started, and `BufEnter` must be sequenced after the
    /// chain's gates — otherwise it would beat a still-settling `FileType`. It stays a
    /// synchronous hot-path event; only its position in the order is deferred.
    pub(crate) deferred_enter: bool,
    /// Fire `BufWinEnter` for this buffer when the chain completes, for the same reason
    /// and with the same mechanism as [`Self::deferred_enter`]: the buffer became
    /// *displayed* while its chain was still parked on an async read handler, and vim's
    /// order is `BufReadPost` → `FileType` → `BufEnter` → `BufWinEnter`. Firing it from
    /// the window walk at that moment would put it *second*, ahead of the very events
    /// the chain exists to order. Set only when a handler is actually registered (the
    /// walk's existing gate), so this stays as cheap as it was.
    ///
    /// Carries the **windows** that displayed it: the fire installs each as current, and
    /// by the time the chain completes the focused window may well be another one. A list
    /// rather than one window because a parked chain spans many diffs and collects every
    /// display in them — a `:vsplit` onto the same file while an async read handler is
    /// still settling gives a *second* window the buffer, and both owe the fire. Held as
    /// a `Vec` (never more than a couple of entries) to keep the display order.
    pub(crate) deferred_win_enter: Vec<WindowId>,
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
    /// The reconnecting daemon link's handle for a remote session, or `None` for a
    /// local/bare/one-shot session. Shared by the run loop's status arm (which reflects
    /// [`DaemonStatus`] changes into the editor + Lua), the `:reconnect` / `:disconnect`
    /// ex-commands, and — via the pushed Lua mirror — `btv.daemon.status()`. Set from
    /// [`ServerInit::daemon_link`] in [`run_io`]. Native-only (the reconnect link is too).
    #[cfg(feature = "native")]
    daemon_link: Option<ReconnectHandle>,
    /// The last [`DaemonStatus`] the run loop reflected, so [`on_daemon_status`] can tell a
    /// genuine **reconnect** (a `Reconnecting`/`Disconnected` → `Connected` transition, which
    /// triggers the state re-sync) from the initial connect (the first `Connected`). `None`
    /// until the first status lands. Native-only (the reconnect link is too).
    ///
    /// [`on_daemon_status`]: EditHost::on_daemon_status
    #[cfg(feature = "native")]
    prev_daemon_status: Option<DaemonStatus>,
    /// The persistence (shada) store, or `None` when persistence is off (the test
    /// default). Loaded before the first frame ([`EditHost::shada_load`]), written by
    /// the debounced live checkpoint ([`EditHost::shada_checkpoint`]) and the
    /// clean-exit flush ([`EditHost::shada_flush_final`]). A capability injected via
    /// [`ServerInit::shada`], like [`fx`](EditHost::fx) — but `load`/`flush` only ever
    /// run off the input tick (startup, the debounce arm, exit), never inside it.
    #[cfg(feature = "native")]
    shada: Option<Box<dyn ShadaStore + Send>>,
    /// The global history store for a workspace launch whose `'persisthistory'` resolves
    /// to `global` (see [`ServerInit::global_shada`]) — history then routes here *instead*
    /// of the workspace store, history-only and never compacting (it shares the global
    /// dir with plain sessions' full-state files). Opened from `init.global_shada` and
    /// restored post-config ([`EditHost::init_global_history`], which drops it when the
    /// session targets the workspace store or `none`). `None` otherwise.
    #[cfg(feature = "native")]
    global_history: Option<Box<dyn ShadaStore + Send>>,
    /// Whether this launch is a session-scoped **workspace** (its shada is a private
    /// `ns/<uuid>` subfolder). Only then does the shada carry the editor *session*
    /// (open files + layout): capture attaches it at flush, and the boot load applies
    /// it. Set from [`ServerInit::workspace_session`] (the binary derives it from
    /// `.bemtvi/workspace.json`); `false` for the global store, so plain shada never
    /// carries layout.
    #[cfg(feature = "native")]
    workspace_session: bool,
    /// Whether to restore the captured session (layout) at boot — the explicit
    /// `--restore-session` opt-in. Separate from [`workspace_session`](Self::workspace_session)
    /// (which only governs capture). From [`ServerInit::restore_session`].
    #[cfg(feature = "native")]
    restore_session: bool,
    /// The captured session pulled out of the store and held until the config has been
    /// sourced — see [`apply_pending_session_restore`](Self::apply_pending_session_restore)
    /// for why the layout must NOT come back before `init.lua` runs. Filled by
    /// [`shada_load`](Self::shada_load) natively and by
    /// [`import_persist`](Self::import_persist) on wasm; `None` when no layout was stored
    /// (or, natively, outside a `--restore-session` boot), and taken (once) at the restore
    /// point. Ungated: both legs carry a layout, so neither drops one on the floor.
    pending_session: Option<bemtvi_core::SessionState>,
    /// The on-daemon shada sync for a `Remote`-config session (Approach A): a handle to
    /// the daemon fs plus the remote file path. `None` for a local-shada session. When
    /// set, [`shada_checkpoint`](Self::shada_checkpoint) uploads the staged store's bytes
    /// to the daemon (fire-and-forget) and [`shada_upload_final`](Self::shada_upload_final)
    /// does the awaited final upload at clean exit. Built in [`run`] from
    /// [`ServerInit::remote_shada`] + a clone of the daemon `host_fs_async`.
    #[cfg(feature = "native")]
    remote_shada: Option<RemoteShadaSync>,
    /// The OSC 52 clipboard's shared state, when this session's `"+` / `"*` are to
    /// ride the client terminal (no usable host tool — see
    /// [`ClipboardProvider::Osc52`]). Armed in [`run`], *installed* on the editor
    /// only once a client attaches declaring `osc52`, since the escape is the
    /// client's to emit. `None` when a real host clipboard was found, or when the
    /// registers are meant to stay unavailable.
    #[cfg(feature = "native")]
    osc52: Option<crate::clipboard::Osc52Handle>,
    /// Attached UI dimensions `(width, height)`, once a client has attached.
    ui: Option<(usize, usize)>,
    /// Whether the attached client has the kitty keyboard protocol active (reported
    /// in the `btv_ui_attach` capabilities map). Gates faithful key parsing in
    /// [`input`](Self::input) so a protocol-on terminal's `<C-i>`/`<C-m>`/`<C-[>`/
    /// `<C-h>` reach the matcher distinct from `<Tab>`/`<CR>`/`<Esc>`/`<BS>`; mirrored
    /// into the keymap registry so a mapping's LHS is parsed to match. Off by default
    /// (every legacy terminal, and until a client reports otherwise).
    keyboard_protocol: bool,
    /// Per-buffer highlight memo, keyed by buffer id (created lazily on first
    /// redraw of a buffer, dropped when the buffer is deleted). The parse tree
    /// itself lives in the editor's [`bemtvi_core::SyntaxEngine`]; this is only the
    /// slim span cache the redraw projects.
    #[cfg(feature = "native")]
    syntax_states: HashMap<BufferId, SyntaxState>,
    /// Buffers whose *first* treesitter highlight has been deferred off the
    /// first-paint frame. A buffer's first highlight pays the one-time,
    /// synchronous cost of loading its language's grammar (dlopen + compiling every
    /// `.scm` query — tens of ms for a big grammar like Python) plus the initial
    /// full-buffer parse. Doing that inside the `redraw` that first shows the buffer
    /// blocks first paint, so a freshly-opened file visibly stalls before appearing.
    /// Instead, the first time a highlightable buffer is seen we skip the query for
    /// one frame (it paints instantly as plain text, like a buffer with no grammar),
    /// record it here, and arm the parse-resume timer; the next frame — now that the
    /// buffer is present in this set — runs the real query, so grammar-load + parse
    /// land *after* first paint and the colour fills in a few ms later. Cleared per
    /// buffer on close (`reap_closed_buffers`). A buffer only defers once; every
    /// later miss (edit, scroll) re-queries synchronously as before.
    #[cfg(feature = "native")]
    first_highlight_deferred: HashSet<BufferId>,
    /// Languages whose runtimepath treesitter queries (`queries/<lang>/*.scm` +
    /// `after/queries/<lang>/*.scm`, `;; extends`) have already been resolved and
    /// pushed to the engine — the query-bridge's push-once guard, so resolution
    /// runs at most once per language rather than every frame.
    #[cfg(feature = "native")]
    resolved_ts_langs: HashSet<String>,
    /// Per-buffer LSP document-sync state, keyed by buffer id (the `syntax_states`
    /// analogue).
    lsp_states: HashMap<BufferId, LspDocState>,
    /// Negotiated runtime state (encoding, sync kind) per started server, learned
    /// from each `initialize` reply.
    lsp_servers: HashMap<ServerKey, ServerRuntime>,
    /// Live `$/progress` tasks per server, in first-sighting order (a server may run
    /// several at once — rust-analyzer routinely does). A task is added on `begin`,
    /// patched on `report`, and removed on `end`, so the list is exactly "what this
    /// server is busy with right now"; a server that exits drops its entry with its
    /// runtime, so a crash can't strand a spinner. Mirrored to
    /// `btv.lsp._progress[client_id]` for `btv.lsp.progress()`.
    lsp_progress: HashMap<ServerKey, Vec<ProgressEntry>>,
    /// The `btv.lsp.config{ priority = … }` **routing rank** of each config name
    /// (absent ⇒ `0`, the default). Higher ranks first: this is what decides which of
    /// a buffer's attached servers a single-target verb asks by default, and the order
    /// the merged surfaces (the hover float, the code-action chooser) present them in.
    ///
    /// Keyed by config **name** rather than [`ServerKey`] because the rank is the
    /// config's stated preference — it holds for every root that config serves, and
    /// survives a respawn under a new key. Read through
    /// [`EditHost::lsp_priority_of`], never indexed directly, so the `0` default lives
    /// in one place.
    lsp_priorities: HashMap<String, i64>,
    /// Whether the user opted into the **signature-help auto-trigger**
    /// (`btv.lsp.signature_help_autotrigger(true)`). It's the latch the per-buffer
    /// trigger set hangs off: when set, an attaching server's advertised trigger chars
    /// are pushed into core; cleared, core's set is emptied so only `<C-k>` triggers
    /// signature help. See [`crate::editor`]'s `signature` module.
    signature_auto: bool,
    /// Server keys already handed to `ensure_server`, so a server is requested
    /// once rather than on every redraw (a lazy-start guard).
    lsp_ensured: HashSet<ServerKey>,
    /// The [`ServerSpawn`] each ensured server was started with, kept so the daemon
    /// reconnect resync can re-`ensure` a fresh server against the new connection without
    /// re-running the Lua `btv.lsp.start` dispatch (the remote child died with the dropped
    /// link, and its respawn would have hit the dead wire). Only a daemon session ever reads
    /// it back; cheap to keep otherwise.
    lsp_spawns: HashMap<ServerKey, ServerSpawn>,
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
    /// The in-flight **fan-out** round per kind (references / document symbols /
    /// code actions): one logical request issued to every capable server, merging
    /// their replies into a single presentation. Separate from `lsp_requests`
    /// because those kinds have N replies in flight for one user action, which the
    /// single-slot map cannot express — see [`LspFanout`].
    lsp_fanouts: HashMap<LspReqKind, LspFanout>,
    /// The in-flight **whole-buffer decoration** requests (semantic tokens, inlay
    /// hints, folding ranges), keyed by the unique generation their token carries.
    /// Separate from `lsp_requests` because a buffer asks every capable server for
    /// its decorations at once and each reply must be decoded against the server
    /// that produced it — see [`PendingMultiReq`].
    lsp_multi_requests: HashMap<u64, PendingMultiReq>,
    /// In-flight `inlayHint/resolve`s, keyed by the `cb_id` their token carries.
    /// Unlike the single-slot `lsp_requests`, many lazy hints can resolve at once,
    /// so each gets a distinct `cb_id` (from `inlay_resolve_seq`) and routes back
    /// by it — the [`InlayResolveTarget`] records which placeholder span to fill.
    inlay_resolves: HashMap<u64, InlayResolveTarget>,
    /// Monotonic source of `cb_id`s for `inlay_resolves` (never reused, so a stale
    /// reply for a superseded resolve finds no target and is dropped).
    inlay_resolve_seq: u64,
    /// Whether the engine's built-in `lsp` completion source is configured
    /// (`btv.complete.setup{ sources = { { "lsp" } } }`). When set, an engine trigger
    /// issues `textDocument/completion` and streams the reply into the unified menu;
    /// accepting an LSP row delegates back here to apply its `textEdit`. Phase 4-C —
    /// the bespoke pmenu it replaces is gone.
    complete_lsp_active: bool,
    /// Merge priority of the `lsp` source (rows rank above lower-priority sources).
    complete_lsp_priority: i32,
    /// The `lsp` source's own `min_chars`: it is dispatched only once the prefix
    /// reaches this length (a manual trigger bypasses). Independent per-source gate,
    /// so `lsp` can fire from 1 char while `buffer` waits for 3. Default 1.
    complete_lsp_min_chars: usize,
    /// The current LSP completion's raw items + word anchor, indexed by the
    /// `MenuItem.key` the engine carries, so a delegated accept can apply the chosen
    /// item's `textEdit` / `additionalTextEdits`. `None` when no LSP completion is in
    /// view. Phase 4-C.
    lsp_complete: Option<LspComplete>,
    /// The completion **row** a `completionItem/resolve` is currently in flight for (the
    /// docs float's lazy-docs fetch, Phase 4-D), as `(lsp_complete.rows index, offer
    /// index within that row)` — so a reply updates the right contributor's item and the
    /// trigger doesn't re-issue while it is pending. A row several servers offered
    /// resolves one contributor at a time, walking the offers in rank order. `None` when
    /// no resolve is outstanding. Reset whenever a fresh completion round opens
    /// ([`EditHost::request_lsp_completion`]). Phase 4-D.
    lsp_complete_resolve_key: Option<(usize, usize)>,
    /// Resolved lazy docs for **plugin** completion rows (`btv.complete.source`'s
    /// `resolve` callback), keyed by the row's resolve handle. An entry (even `""` ⇒
    /// resolved-but-docless) means the docs are in hand and the sidebar renders them
    /// without re-asking. Cleared each fresh completion run (the handles die with the
    /// old menu). Native-only, like the LSP docs cache. Phase 4-E.
    complete_resolve_docs: std::collections::HashMap<u64, String>,
    /// The resolve handle a plugin-row `resolve` is currently in flight for, so the
    /// per-key trigger doesn't re-issue while it's pending. `None` when none is
    /// outstanding; cleared when the reply lands or a fresh run resets the cache.
    /// Phase 4-E.
    complete_resolve_inflight: Option<u64>,
    /// The code actions currently listed in the `:LspCodeAction` panel (Phase 6),
    /// indexed by panel select. A `<CR>` on row `i` applies `lsp_code_actions[i]`'s
    /// edit; cleared on apply. Empty when no code-action panel is active.
    lsp_code_actions: Vec<CodeActionData>,
    /// The server that produced each entry of `lsp_code_actions`, same indices.
    /// A lazy action is finished with `codeAction/resolve`, whose `data` blob only
    /// its own server understands — so a merged list must remember where each came
    /// from, or the resolve goes to the wrong server.
    lsp_code_action_servers: Vec<ServerKey>,
    /// The `vim.diagnostic.config` keys with a backing surface — the underline
    /// spans and the inline virtual text — toggled by `vim.diagnostic.config`.
    diag_config: DiagnosticConfig,
    /// Client-set diagnostics (`vim.diagnostic.set`) per buffer, flattened across
    /// the Lua-side namespaces the prelude tracks (the server doesn't), pushed via
    /// [`LspOp::SetClientDiagnostics`](bemtvi_lua::LspOp). These have **no attached
    /// server**, so their columns are bemtvi's native UTF-8 bytes — the renderer
    /// projects them at [`PositionEncoding::Utf8`] regardless of any LSP server's
    /// negotiated encoding, then merges them with the server-pushed set. Kept apart
    /// from `lsp_states[buf].diagnostics` so the two sources never overwrite.
    client_diagnostics: HashMap<BufferId, Vec<bemtvi_lsp::lsp_types::Diagnostic>>,
    /// Client-set diagnostics written while [`EditHost::diagnostics_paused`] held
    /// (insert mode, `update_in_insert` off), parked here instead of landing in
    /// `client_diagnostics` so a plugin setting diagnostics per keystroke can't make
    /// the display churn any more than a language server can. An **empty** held list
    /// is a pending *clear* — the entry's presence, not its length, is what records
    /// that a write was held. Folded in by
    /// [`EditHost::commit_pending_diagnostics`] on `InsertLeave`.
    pending_client_diagnostics: HashMap<BufferId, Vec<bemtvi_lsp::lsp_types::Diagnostic>>,
    /// How many diagnostics each buffer's [`DIAGNOSTIC_NS`](bemtvi_core::extmark::DIAGNOSTIC_NS)
    /// anchors were placed from ([`EditHost::refresh_diagnostic_marks`]). The anchors
    /// are addressed by position in the merged set, so this is the guard that makes
    /// them safe to read: a merged set of a different length means the anchors are
    /// stale, and the projection falls back to the published ranges (untracked, never
    /// mis-attributed). Absent for a buffer with no diagnostics.
    diag_mark_counts: HashMap<BufferId, usize>,
    /// The extmark-store generation of the diagnostic namespace recorded when the anchors
    /// were placed ([`EditHost::refresh_diagnostic_marks`]). Complements
    /// [`diag_mark_counts`](Self::diag_mark_counts): a count match can survive an
    /// undo that restores an older store holding the same number of marks in the
    /// diagnostic namespace, pointing the anchors at the wrong spans; the
    /// generation then differs (the placement itself bumped it) and the guard fails.
    /// Absent for a buffer with no diagnostics.
    diag_mark_gens: HashMap<BufferId, u64>,
    /// The editor-wide semantic-tokens gate (Phase 3), toggled by
    /// `vim.lsp.semantic_tokens.enable`. Default on; `false` hides the semantic
    /// paint everywhere and stops the refresh requests (the per-buffer
    /// `LspDocState::semantic_enabled` is the narrower override).
    semantic_tokens_enabled: bool,
    /// Registered snippets per filetype (`btv.snippet.add`), feeding the `snippets`
    /// completion source. String bodies only in this phase. Feature-agnostic (the
    /// engine is core), so present on the wasm build too.
    snippet_store: HashMap<String, Vec<snippet::SnippetEntry>>,
    /// Whether the `snippets` completion source is configured (`btv.complete.setup`).
    complete_snippets_active: bool,
    /// Merge priority of the `snippets` source (rows rank against other sources).
    complete_snippets_priority: i32,
    /// The `snippets` source's own `min_chars` (see [`complete_lsp_min_chars`]; a
    /// manual trigger bypasses). Default 1.
    ///
    /// [`complete_lsp_min_chars`]: Self::complete_lsp_min_chars
    complete_snippets_min_chars: usize,
    /// The snippet entries last pushed as completion candidates this trigger, indexed
    /// by `MenuItem.key - SNIPPET_COMPLETE_KEY_BASE`, so a delegated accept finds the
    /// body to expand.
    snippet_complete: Vec<snippet::SnippetEntry>,

    /// The active **global** `btv.statusline.setup{}` layout (ordered segment names
    /// per side). `Some` takes precedence over the `'statusline'` `%`-format for
    /// every window without a [window-local override](EditHost::statusline_window);
    /// `None` ⇒ those windows use the format path. Set by draining `statusline_setups`.
    statusline_layout: Option<bemtvi_core::statusline::SegmentLayout>,
    /// Window-local status line overrides (`btv.statusline.setup{win=…}` /
    /// `reset(win)`) — the `setlocal 'statusline'` analogue. A window present here
    /// overrides the global layout: [`Segments`](WindowStatusline::Segments) shows
    /// its own layout, [`Format`](WindowStatusline::Format) opts back to the
    /// `%`-format (the per-region mix). Absence ⇒ inherit the global layout. Pruned
    /// when a window closes.
    statusline_window: HashMap<WindowId, WindowStatusline>,
    /// Per-`(window, name)` cache of custom statusline-segment cells published from
    /// Lua (`btv._statusline_publish`). Each custom segment is rendered once per
    /// window (its `render(ctx)` sees that window's buffer / focus), so the redraw
    /// path looks a segment up by the window it is painting. Updated only when a
    /// segment is invalidated or the window layout changes — never per frame
    /// (ADR 0002 rule 4); see [`EditHost::refresh_statusline_segments`].
    statusline_cache:
        std::collections::HashMap<(u64, String), Vec<bemtvi_core::statusline::StatusSegment>>,
    /// The custom (non-built-in) segment names the active layout references, so the
    /// server knows which segments to (re)render. Derived when a `setup{}` layout
    /// is drained.
    statusline_custom: Vec<String>,
    /// Custom segment names awaiting a re-render (invalidated, or freshly set up).
    /// Drained per window in [`EditHost::refresh_statusline_segments`] once the
    /// input settles, with a fresh window mirror.
    statusline_pending: std::collections::HashSet<String>,
    /// The window layout the cache was last rendered against —
    /// `(window id, buffer id)` per window plus the focused window. A change here
    /// (a split/close, a focus move, or a window's buffer swapping) re-renders
    /// every custom segment so per-window `focused` / `buf` contexts stay correct,
    /// and prunes cache entries for windows that closed.
    statusline_layout_key: Option<(Vec<(u64, u64)>, u64)>,

    /// The buffer that was current the last time lifecycle events were emitted;
    /// `None` until the startup seed. A change here means a `BufEnter` (fired on
    /// every entry).
    last_buffer_id: Option<BufferId>,
    /// Buffers that have already had their fire-once `BufReadPost` emitted, so
    /// re-entering them doesn't re-announce.
    announced: HashSet<BufferId>,
    /// The `FileType` pattern last fired for each buffer (`None` = none fired yet).
    /// `FileType` is tracked separately from [`announced`](Self::announced) because,
    /// unlike `BufReadPost`, it re-fires whenever a buffer's filetype *changes*
    /// (neovim's `:setfiletype` behavior) — in particular when a window reuses one
    /// buffer in place across kinds (a throwaway `[No Name]` → an `btvdir` listing, a
    /// file → a directory), which keeps the same buffer id and so stays "announced".
    /// That re-fire is what installs the explorer / quickfix buffer-local maps.
    fired_filetype: HashMap<BufferId, Option<String>>,
    /// Each buffer's `'fileencoding'` label as of the last lifecycle diff. Diffed to
    /// fire `EncodingChanged` (neovim alias `FileEncoding`) when a buffer's encoding
    /// is changed *in place* (`:set fileencoding=…`). A (re)read — opening the file or
    /// an `:e ++enc=…` reload — clears the entry so the next diff reseeds the baseline
    /// silently: reading a file at its detected encoding is not a *change* (like
    /// neovim, whose global `encoding` stays fixed, which fires nothing on read).
    fired_encoding: HashMap<BufferId, String>,
    /// Every buffer id present at the last lifecycle diff. Ids added since fire
    /// `BufAdd` (neovim alias `BufCreate`); ids gone since (a `:bdelete` /
    /// `nvim_buf_delete`) fire `BufDelete` and have their Lua-side buffer-local state
    /// (commands, keymaps) purged so a reused bufnr can't inherit it.
    known_buffers: Vec<BufferId>,
    /// Set once the startup buffer baseline (`known_buffers`) has been seeded — after
    /// config sourcing and any session/view restore. Until then `BufAdd` is suppressed
    /// so a config-time `emit_lifecycle_events` doesn't fire it for the startup buffer
    /// (which is baseline, not newly added — the buffer twin of `WinNew`/`TabNew`
    /// skipping the initial window/tab).
    startup_bufs_seeded: bool,
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
    /// Each (non-doc-float) window's displayed buffer at the last diff — the
    /// `BufWinEnter` baseline. A window now holding something else *displayed* it and
    /// fires, which is neovim's rule (it fires from the buffer load/switch paths and
    /// from nothing in `window.c`): a switch fires every time, a second window onto an
    /// already-shown buffer fires again, while a tab switch and `<C-w>w` — which change
    /// no window's buffer — fire nothing. This is also the event a session / workspace
    /// restore needs: restore fills *non-current* windows, which the current-buffer
    /// `BufReadPost`/`BufEnter` diff never visits. Two entries are seeded rather than
    /// diffed: a window created by a bare `:split` is seeded with the buffer it
    /// inherited (`Editor::take_inherited_windows`) so the split is silent, and a
    /// re-read of a displayed buffer (`Editor::take_loaded_in_place`) fires despite
    /// moving nothing, as neovim's `open_buffer` does. Rebuilt every diff so the
    /// baseline stays current even with no handler; the *fire* is gated on a registered
    /// handler, like `WinScrolled`. Not seeded at boot, so the first emit fires it for
    /// the startup / restored windows (as `BufReadPost` fires for the startup file).
    known_window_buffers: HashMap<WindowId, BufferId>,
    /// Each window's `(id, x, y, w, h)` rect at the last diff; a change fires
    /// `WinResized` (splits, `<C-w>`-resizes, terminal resizes). `None` until the
    /// seed so the first emit doesn't spuriously fire it.
    last_window_rects: Option<Vec<WindowRect>>,
    /// Each window's `(id, topline, leftcol)` scroll offset at the last diff; a
    /// change fires `WinScrolled` for that window (vertical or horizontal scroll).
    /// `None` until the first diff that runs with a `WinScrolled` handler active —
    /// gated on a registered handler ([`au_active_events`](Self::au_active_events)),
    /// so scrolling costs nothing when nothing listens (like `CursorMoved`).
    last_window_scroll: Option<Vec<(WindowId, usize, usize)>>,
    /// The active tab at the last lifecycle diff; `None` until the startup seed. A
    /// change fires `TabLeave`(old) → … → `TabEnter`(new), bracketing the window
    /// events the switch causes (`TabLeave → WinLeave → … → WinEnter → TabEnter`).
    last_tab_id: Option<TabId>,
    /// Every tab id seen at the last diff, in tabline order. Ids added since fire
    /// `TabNew`; ids gone fire `TabClosed`.
    known_tabs: Vec<TabId>,
    /// The focused window's cursor `(buffer, line, col)` at the last lifecycle diff;
    /// `None` until the startup seed. A change *within the same buffer* fires
    /// `CursorMoved` (Normal/Visual) or `CursorMovedI` (Insert) — a buffer switch only
    /// re-seeds the baseline (so merely entering a buffer doesn't fire a spurious move).
    /// Gated on a registered handler ([`au_active_events`](Self::au_active_events)), so a
    /// motion costs nothing when nothing listens.
    last_cursor: Option<(BufferId, usize, usize)>,
    /// The current buffer's `(buffer, changedtick)` at the last lifecycle diff; a tick
    /// advance *within the same buffer* fires `TextChanged` (Normal) or `TextChangedI`
    /// (Insert). `None` until the seed. Gated like [`last_cursor`](Self::last_cursor).
    last_text: Option<(BufferId, u64)>,
    /// The `btv._au_version` the cached [`au_active_events`](Self::au_active_events) was
    /// last refreshed against. Read once per input batch; the set is only re-pulled
    /// across the bridge when this advanced (mirrors the keymap-version gate).
    au_event_version: u64,
    /// The distinct event names some autocmd is currently registered for, refreshed
    /// from Lua when [`au_event_version`](Self::au_event_version) advances. The per-key
    /// lifecycle diff consults it before computing / firing a high-frequency event
    /// (`CursorMoved` / `TextChanged`), so the common no-handler config never re-enters
    /// Lua on a bare cursor motion.
    au_active_events: HashSet<String>,
    /// The user-mapping engine: per-mode tries + the withhold/replay buffer that
    /// `EditHost::input` runs every key through before `editor.input`. Rebuilt from
    /// `btv._keymaps` when its version advances (checked once per input batch).
    keymaps: Keymaps,
    /// The last pending key-context pushed to `btv.on_key_pending` listeners, for the
    /// change-detection that keeps the **`KeyPending`** event fire-on-change (not
    /// per keystroke). `None` means the context is currently empty (or never set);
    /// `emit_key_pending` re-pushes only when the live context differs from this.
    last_key_pending: Option<crate::keymap::KeyPending>,
    /// Callback ids queued by `vim.schedule`, drained inside `run_pending` so a
    /// scheduled fn runs at the end of the current convergence (not nested in its
    /// caller). A scheduled fn may schedule more, so this feeds the fixpoint loop.
    scheduled: VecDeque<u64>,
    /// Per-buffer `changedtick` last copied into the `btv._bufs` Lua mirror
    /// ([`EditHost::push_buf_mirror`]), so an unchanged buffer's line array isn't
    /// re-serialized on every Lua entry — only the cheap cursor/window fields
    /// refresh each time (Phase 6).
    buf_mirror_ticks: HashMap<BufferId, u64>,
    /// The core's option-state generation ([`Editor::options_generation`]) the Lua
    /// `bo` mirror was last rebuilt from, so [`EditHost::push_buf_mirror`] skips the
    /// wholesale per-buffer rebuild when no option write / filetype / `ts_highlight`
    /// change / save moved it. Text edits deliberately do not bump the counter —
    /// they only change a row's `modified`, which the per-buffer `changedtick` gate
    /// (`fresh`) covers. Seeded to `u64::MAX` so the first push always rebuilds.
    bo_mirror_gen: u64,
    /// The bufnrs a `bo` mirror row is currently held for in the Lua table, so a
    /// buffer deleted between pushes is dropped from the table explicitly — the
    /// mirror merges rows now, it no longer wholesale-replaces the table.
    bo_mirror_known: HashSet<BufferId>,
    /// Last-pushed per-window jumplist generations ([`Editor::window_jumplist_gen`]):
    /// when a window's generation matches the last push, its row skips the jumplist
    /// and the Lua side keeps the old list — a repaint never re-serializes a whole
    /// jumplist (the structural-generation gate the extmark mirror uses). Pruned
    /// when a window closes; an id absent here pushes its full list.
    win_jump_gens: HashMap<WindowId, u64>,
    /// Per-buffer line count last mirrored, so [`EditHost::push_buf_mirror`] can pass
    /// the old line count as `on_lines`' `lastline` when an attached buffer changes
    /// (`nvim_buf_attach`). Tracked only to fire faithful buffer-change callbacks —
    /// a fuzzy-finder plugin drives its prompt filtering off `on_lines`.
    buf_mirror_lines: HashMap<BufferId, usize>,
    /// Per-buffer extmark-store *structural* generation last serialized into the
    /// `btv._extmarks` Lua mirror ([`EditHost::push_buf_mirror`]). A mark's
    /// decorations (`hl_group`, priority, the sign / line-fill / line-hl payloads,
    /// gravity) are fixed for its lifetime — an edit moves byte anchors and nothing
    /// else — so the full re-serialize is gated on this counter and an edit refreshes
    /// positions alone. Without it, a buffer carrying a few thousand marks (any
    /// diagnostics / git-sign / rainbow plugin) re-serialized every mark on every
    /// keystroke. See `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
    extmark_gens: HashMap<BufferId, u64>,
    /// Per-buffer undo fingerprint last serialized into the `btv._undotree` Lua
    /// mirror ([`EditHost::push_undotree_mirror`]), so an unchanged tree isn't
    /// re-projected on every Lua entry — only edits/undo/redo rebuild it.
    undo_mirror_versions: HashMap<BufferId, (u64, usize, u64, bool)>,
    /// Version of the quickfix / location-list state last serialized into the
    /// `btv._qflist` + `btv._loclist` Lua mirrors: the core's list-write counter paired
    /// with the ids of the windows that then held a location list. See
    /// [`EditHost::push_qflist_mirror`] for why it takes both halves.
    qf_mirror_version: Option<(u64, Vec<u64>)>,
    /// The register file's write counter as of the last `btv._registers` push, with the
    /// read-only specials (`%` `/` `:` `.`) that push carried. The counter covers the
    /// stored cells — whose size is unbounded, and is the reason the push is gated at
    /// all — while the specials resolve from live editor state and so must be compared
    /// by value. See [`EditHost::push_buf_mirror`].
    reg_mirror_gen: Option<u64>,
    reg_mirror_specials: Vec<(char, String, bool)>,
    /// Monotonic base for the editor's time: `start.elapsed()` seconds are stamped
    /// onto undo nodes and handed to `vim.fn.localtime()`. Monotonic so elapsed
    /// labels survive wall-clock jumps; see [`Editor::set_now_mono`].
    start: std::time::Instant,
    /// Optional fake clock for the mouse multi-click timestamp ([`ServerInit::mouse_clock`]);
    /// when set, [`EditHost::mouse_stamp_ms`] reads it instead of `start.elapsed()`.
    mouse_clock: Option<Arc<AtomicU64>>,
    /// Optional fake clock for the monotonic second base ([`ServerInit::mono_clock`]);
    /// when set, [`EditHost::mono_stamp_secs`] reads it instead of `start.elapsed()`.
    mono_clock: Option<Arc<AtomicU64>>,
    /// The highlight-registry [`generation`](bemtvi_core::highlight::Highlights::generation)
    /// last folded into the `btv._hl_defs` Lua mirror ([`EditHost::push_buf_mirror`]).
    /// The mirror (potentially hundreds of groups) is re-pushed only when this
    /// changes — a colorscheme load, a `:hi`/`nvim_set_hl` — so the common chunk
    /// pays nothing for `nvim_get_hl` support. `None` until the first push.
    hl_mirror_gen: Option<u64>,
    /// The global-namespace highlight groups the **currently loaded colorscheme**
    /// defined ([`EditHost::set_colorscheme`]). Loading the next scheme drops
    /// exactly these before sourcing it, so the two palettes replace rather than
    /// stack: a group the old scheme styled and the new one says nothing about
    /// goes back to unstyled instead of showing the old theme's colour through the
    /// new one. Scheme-owned only — a group a plugin defined is never collateral,
    /// and a plugin restyling on `ColorScheme` re-registers after the load anyway.
    /// Empty until a scheme loads.
    scheme_groups: std::collections::HashSet<String>,
    /// The `btv._cb_fns` id of the `vim.ui.input` callback awaiting the open
    /// command-line prompt's result, or `None` when no scripted prompt is open
    /// (Phase 8). Set when a prompt opens; taken when the user submits/cancels.
    pending_ui_input: Option<u64>,
    /// The `btv._cb_fns` id of the `btv.ui.select` callback awaiting the open
    /// menu's result, or `None` when no menu is open. Set when a menu opens;
    /// taken when the user confirms / cancels. Separate from `pending_ui_input`
    /// (a menu and a prompt are distinct surfaces).
    pending_ui_select: Option<u64>,
    /// Whether the open float-list widget is a `btv.picker` (vs a `btv.ui.select`).
    /// Set when a picker opens; cleared when it confirms / cancels. The widget's
    /// outcome (`menu_results`) routes to the picker (`run_picker_result`) when
    /// this is set, and to `pending_ui_select` (`run_ui_select`) otherwise — only
    /// one float-list widget is open at a time, so the two are mutually exclusive.
    picker_active: bool,
    /// Whether the open select menu is the LSP **code-action** chooser (`:LspCodeAction`
    /// / `vim.lsp.buf.code_action`). Set when `show_code_actions` opens the menu; taken
    /// when it confirms / cancels, routing the chosen index to `apply_code_action`
    /// (neovim's `vim.ui.select` model — the successor to the retired code-action panel).
    /// Mutually exclusive with `pending_ui_select` / `picker_active`: one widget at a time.
    #[cfg(feature = "native")]
    pending_code_action: bool,
    /// The async `btv.lsp.code_action()` promise's `btv._cb_fns` id, stashed while the
    /// chooser menu is open (`0` = none / a fire-and-forget request). Set alongside
    /// `pending_code_action` when the menu opens; settled (resolve `nil`) when the
    /// picked action's edit applies — through a `codeAction/resolve` round-trip if the
    /// action is lazy — or when the menu is cancelled. Set only on the native confirm
    /// path, but the field is unconditional so `apply_code_action` (not cfg-gated)
    /// compiles under the wasm edit-host too.
    code_action_cb: u64,
    /// Writes whose `BufWritePre` returned an *async* handler promise: the [`PreWrite`]
    /// is parked here, keyed by the `gate_id` handed to Lua, until every handler settles.
    /// Lua then signals `btv._au_gate_done(gate_id)` (a [`LoopOp::AuGateDone`]) which lands
    /// in [`Self::au_gate_done`]; `drain_au_gate_done` pops the parked write and commits
    /// it. Parked writes do **not** keep the fixpoint spinning (that would busy-wait); the
    /// commit is driven by the settle signal, exactly as an LSP/fs promise settles.
    pending_gated_writes: HashMap<u64, PreWrite>,
    /// Monotonic id minting for the `BufWritePre` await gate (`pending_gated_writes` keys
    /// / the `gate_id` passed to `btv._fire_gated`). Distinct per fired gated event so two
    /// concurrent async `BufWritePre`s can't collide.
    next_gate_id: u64,
    /// Gate ids whose `BufWritePre` handlers have all settled (Lua's
    /// `btv._au_gate_done(id)` → [`LoopOp::AuGateDone`]), drained in `run_pending` to commit
    /// the parked [`PreWrite`]. Kept on the loop's break condition so a settle that lands
    /// mid-convergence commits and fires `BufWritePost` in the same pass.
    au_gate_done: Vec<u64>,
    /// A `:wqa` / `:xa` whose write batch has fully completed and now wants its `:qa`
    /// replayed (`Some(bang)`), deferred to `run_pending`'s tail rather than run where the
    /// gate is advanced — which may be mid-fixpoint (a synchronous commit) where a nested
    /// `run_command("qa")` shouldn't run. Set by `advance_quit_all_gate`, taken at the tail.
    quit_all_replay: Option<bool>,
    /// The current stage of the gated editor-exit sequence, or `None` when not exiting.
    /// Set from [`Editor::take_exit_requested`] (`ex_quit_all` committed a quit) and advanced
    /// by [`drive_exit`](Self::drive_exit): each stage fires one gated event
    /// (`QuitPre`/`ExitPre`/`VimLeavePre`), awaiting async handlers; the terminal `Leaving`
    /// stage fires `VimLeave` and sets [`Editor::should_quit`]. A sequence whose handlers all
    /// settle synchronously runs start-to-finish in one `run_pending` convergence.
    exit_stage: Option<ExitStage>,
    /// The `gate_id` the exit sequence is currently parked on: an `ExitPre`/`VimLeavePre`
    /// (or `QuitPre`) handler returned a still-pending promise, so [`drive_exit`](Self::drive_exit)
    /// stashed the stage's gate here and returned. `btv._au_gate_done(id)` lands the id in
    /// [`Self::au_gate_done`]; [`drain_au_gate_done`](Self::drain_au_gate_done) recognizes it,
    /// clears this, and re-drives the sequence. `None` when the current stage settled
    /// synchronously (or between stages).
    exit_gate: Option<u64>,
    /// Each buffer's in-flight **gated read chain** — the `BufReadPost`/`BufNewFile` →
    /// `FileType` → (deferred `BufEnter`) announce sequence, driven one stage at a time
    /// so a stage's async handlers settle before the next stage fires. Neovim gets that
    /// ordering for free by being synchronous; we reproduce it explicitly.
    ///
    /// Keyed per buffer, so a session restore's several concurrently-announcing
    /// background buffers each progress independently. A buffer with a chain in flight
    /// is held out of the announce pass, so a keypress mid-chain can't re-enter it. The
    /// overwhelmingly common case never appears here at all: a stage whose handlers
    /// return no promise advances inline and the chain completes within the one call.
    read_chains: HashMap<BufferId, ReadChain>,
    /// `gate_id` → the buffer whose read chain is parked on it, the read-chain twin of
    /// [`Self::pending_gated_writes`]. [`drain_au_gate_done`](Self::drain_au_gate_done)
    /// uses it to tell a chain gate from a write gate or the exit gate, and re-drives
    /// that buffer's chain.
    chain_gates: HashMap<u64, BufferId>,
    /// The picker preview pane's read cache (Phase 3): the file last read for the
    /// preview, so moving the selection within the results — or simply re-projecting
    /// every frame — re-reads only when the selected row's target *path* changes.
    /// Reset when the path differs; dropped implicitly when the picker closes (the
    /// next picker repopulates it). See [`redraw::project_menu`](crate::redraw).
    preview_cache: redraw::PreviewCache,
    /// `btv.treesitter.highlight` asks waiting on a grammar that is still loading:
    /// `(language, text, line count, callback id)`. Settling one early would fulfil its
    /// promise with an empty span list — "this text has no highlights" — so it is
    /// re-run when the grammar lands ([`EditHost::settle_ts_highlight`]). Native only:
    /// the browser build's engine loads nothing, so nothing is ever pending.
    #[cfg(feature = "native")]
    parked_ts_highlights: Vec<(String, String, usize, u64)>,
    /// The picker preview pane's manual scroll offset: a signed line delta added to
    /// the auto-computed window start (which centers a `location` match ~a third down).
    /// `<C-d>`/`<C-u>`/`<C-f>`/`<C-b>` fold into it in [`redraw::project_preview`]; it
    /// resets to `0` whenever the selected row's preview target changes ([`preview_anchor`]),
    /// so each selection re-centers. Clamped to the file every frame.
    preview_scroll: isize,
    /// The picker preview pane's manual **horizontal** scroll offset: the first visible
    /// column, advanced by a `<S-ScrollWheel>` / horizontal wheel over the pane and
    /// clamped to the widest visible line. Resets to `0` with [`preview_scroll`] when
    /// the selected row's preview target changes ([`preview_anchor`]).
    preview_hscroll: usize,
    /// The preview target the [`preview_scroll`] offset belongs to. When the live target
    /// differs (the selection moved to another row / file), the offset is reset. `None`
    /// before the first preview / on a row with no target.
    preview_anchor: Option<bemtvi_core::PreviewTarget>,
    /// Keys queued by `nvim_feedkeys`, drained after the input batch / off-tick
    /// settle. Each carries whether it should be remapped (the `m` flag) or fed
    /// straight to the editor (the `n` flag). `nvim_feedkeys` with the `i` flag
    /// pushes to the front; otherwise to the back.
    feed_buffer: VecDeque<(Key, bool)>,
    /// Depth of the "these keys are not typed input" guard around macro recording:
    /// a mapping's RHS, the `nvim_feedkeys` typeahead drain, and a macro playing
    /// back all raise it. A macro recording captures only what the user pressed (see
    /// `EditHost::note_macro_keys` and `bemtvi_core`'s `editor::macros`).
    macro_suppress: u32,
    /// The macro playbacks in flight (`{count}<F3>{reg}`), innermost last — a
    /// stack so a macro can play another. Driven by `EditHost::drive_macro_play`.
    macro_play: Vec<MacroFrame>,
    /// The `(recording, executing)` pair last pushed to Lua, so an idle tick skips
    /// the bridge call (`btv._macro_state`).
    macro_state_mirror: (Option<char>, Option<char>),
    /// The keys of a bracketed paste being collected — `Some` between the client's
    /// `<PasteStart>` and `<PasteEnd>` markers, `None` the rest of the time. A paste
    /// is one edit, not a burst of typing, so the payload is gathered here (never
    /// reaching the keymap matcher) and applied in one go by `apply_paste`. Cleared
    /// at the end of every input batch, so a feed truncated before its closing
    /// marker can't strand the payload and swallow the keys that follow.
    paste_payload: Option<Vec<Key>>,
    /// Plugin-test mode, flipped on by the `btv_enable_test_mode` RPC the
    /// `bemtvi --test-plugin` runner sends at startup. While set, [`redraw`] mirrors the
    /// projected UI into `btv._ui` (for `btv.test`'s `t:float()` / `t:message()` reads)
    /// and the `btv.test` framework is installed. Off by default, so a normal editor
    /// session neither pays the mirror cost nor exposes the test API.
    test_mode: bool,
    /// Route captured Lua message output (`print` / `nvim_echo` / the `btv.err_write*`
    /// error writers) to the process's real **stdout / stderr** instead of the editor
    /// message line. Set only by the headless `bemtvi --lua CODE` one-shot (via
    /// [`ServerInit::lua_stdio`]), which has no UI attached — so a script's `print`
    /// reaches the shell/CI that launched it rather than vanishing into a `:messages`
    /// buffer nobody reads. Off by default (an interactive/daemon session's stdout is
    /// the RPC transport, so writing to it would corrupt the wire). Error lines go to
    /// stderr, plain lines to stdout, matching the one-shot's exit-status contract.
    lua_stdio: bool,
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
    /// that buffer (the native file-watch machinery is the sole `FsEvent` producer —
    /// there is no Lua fs-event surface). Local sessions only — a daemon session uses
    /// [`EditHost::remote_watches`] instead.
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
    /// A `:e ++enc=<encoding>` override for an **off-tick** open whose fetch is in flight,
    /// keyed by the target buffer. A deferred [`PendingOpen`](bemtvi_core::PendingOpen)
    /// carries the forced encoding, but the remote read is async: the drain
    /// ([`drain_pending_opens`](Self::drain_pending_opens)) fires the fetch and the bytes
    /// land later in [`apply_open`](Self::apply_open) / the wasm read reply, keyed only by
    /// buffer. This bridges that gap — the drain stashes the encoding here and the landing
    /// (`load_replica_bytes`) removes it and passes it to
    /// [`Editor::load_bytes_into_enc`](bemtvi_core::Editor), so `++enc` decodes a remote
    /// file exactly as it does a local one. Empty for ordinary opens and local sessions.
    forced_fetch_enc: HashMap<BufferId, String>,
    /// Per-terminal-buffer vt100 emulators, keyed by buffer id. Created when a
    /// `:terminal` opens, fed the child's raw PTY bytes (decoding escape sequences
    /// into a screen grid), and projected back into the buffer's mirrored lines +
    /// the redraw's per-cell colors. The byte *transport* differs per build (a local
    /// PTY natively, the daemon over WebTransport on wasm), but this emulation is
    /// pure CPU and shared by both — hence feature-agnostic. See
    /// [`terminal`](crate::terminal) and docs/plans/2026-06-14-terminal-in-buffer.md.
    terminals: HashMap<BufferId, terminal::TermEmu>,
    /// Per-line color runs captured from a terminal's grid the moment its child
    /// exited, keyed by buffer id — the dead terminal's frozen highlighting. A
    /// natural exit drops the live emulator (where the colors live) but the buffer
    /// survives as a plain buffer holding the final output; without this the output
    /// would revert to monochrome. Index-aligned with the buffer's lines (the
    /// captured scrollback ++ screen); the appended `[Process exited]` notice has no
    /// entry and stays uncolored. Populated by [`terminal_freeze`], read by
    /// [`terminal_highlights`] when there is no live emulator, and cleared when the
    /// buffer is killed/wiped or a fresh terminal reopens on the same id.
    ///
    /// [`terminal_freeze`]: EditHost::terminal_freeze
    /// [`terminal_highlights`]: EditHost::terminal_highlights
    terminal_frozen: HashMap<BufferId, Vec<terminal::RowStyles>>,
    /// The three-scope working-directory model behind `:cd` / `:tcd` / `:lcd` (the
    /// global dir, plus per-window and per-tab local dirs). The process cwd
    /// (`std::env::current_dir`, what `vim.fn.getcwd` reads) is kept equal to the
    /// *current window's* effective dir — `:cd`-family commands mutate both this and
    /// the cwd, and a window/tab focus change re-applies the effective dir to the
    /// cwd ([`EditHost::fix_current_dir`]). Seeded from the startup cwd.
    dirs: cwd::DirState,
    /// In-flight daemon `:cd`s, keyed by the token threaded through
    /// [`HostEffects::fs_chdir`]: each holds the scope/window/tab and the optimistic-move
    /// undo, consumed by [`EditHost::apply_chdir`] when the canonical path (or `E344`)
    /// lands. Empty in a local session (`:cd` resolves synchronously there).
    pending_chdirs: HashMap<u64, cwd::PendingChdir>,
    /// Monotonic token source for [`pending_chdirs`](Self::pending_chdirs).
    next_chdir_token: u64,
    /// Workspace-edit edits awaiting an **off-tick** replica buffer's bytes, keyed by
    /// the replica buffer's id. A project-wide rename / code action that touches a file
    /// not open in a buffer must, in a daemon / web session, fetch that file across the
    /// wire before its edits can apply; the edits (still in LSP form, plus the
    /// originating server's encoding) wait here and [`apply_pending_replica_edit`] drains
    /// the entry when the fetch lands (`load_replica_bytes`). Empty
    /// in a local session — there the file is read synchronously and edited inline.
    ///
    /// [`apply_pending_replica_edit`]: EditHost::apply_pending_replica_edit
    pending_replica_edits: HashMap<BufferId, lsp::PendingReplicaEdit>,
    /// Gotos whose target file wasn't open, in a session where opening it is **off-tick**
    /// (a daemon / web one): the LSP position, keyed by the replica buffer whose fetch is
    /// in flight. The `character`→byte-column conversion needs the target line's text, so
    /// off-tick it can only happen when the bytes land — see
    /// [`EditHost::settle_pending_goto`]. Empty in a local session, which reads
    /// synchronously and converts inline.
    pending_goto_cols: HashMap<BufferId, lsp::PendingGoto>,
    /// The **file** operations a workspace edit asked for (`rename` / `delete`
    /// resource operations) that are still in flight, keyed by the off-tick job id
    /// they were queued under ([`WORKSPACE_FS_JOB_BASE`]` + n`). Each one moves or
    /// removes a real file, which can only happen off-tick — the same `FsJob` seam
    /// `btv.fs` rides, so one code path serves the local, daemon and browser sessions
    /// — and the buffer-side half (rebind / wipe) runs here when the result lands.
    /// See [`EditHost::on_workspace_fs_result`].
    workspace_fs_jobs: HashMap<u64, lsp::WorkspaceFsJob>,
    /// Source of the [`workspace_fs_jobs`](Self::workspace_fs_jobs) ids, from
    /// [`WORKSPACE_FS_JOB_BASE`].
    next_workspace_fs_id: u64,
    /// Queued file operations, in `documentChanges` order, with **at most one in
    /// flight** ([`workspace_fs_inflight`](Self::workspace_fs_inflight)): the seam
    /// dispatches each job onto its own task (its own round trip, in a daemon session),
    /// so operations started together would race — and `rename a→b` racing `rename b→c`
    /// renames a file that isn't there yet.
    workspace_fs_queue: std::collections::VecDeque<u64>,
    /// The file operation currently running, if any. Cleared when its result lands, at
    /// which point the next one starts.
    workspace_fs_inflight: Option<u64>,
    /// `create` resource operations whose "is the file already there?" question could
    /// only be answered **off-tick** (`ignoreIfExists` in a daemon / browser session):
    /// the buffer whose replica fetch was enqueued as the probe, mapped to its apply's
    /// `(group, change index)`. The fetch's landing says whether the file existed, which
    /// decides whether this was a create to write out or a file the server asked us to
    /// leave exactly as it was. See [`EditHost::settle_workspace_create`].
    pending_create_writes: HashMap<BufferId, (u64, usize)>,
    /// Edits parked on a user confirmation (`changeAnnotations` with
    /// `needsConfirmation`), by group id: nothing of them has been applied, and the
    /// answer decides which of their changes ever will be. See
    /// [`EditHost::on_workspace_edit_decision`].
    pending_confirm_edits: HashMap<u64, lsp::PendingConfirmEdit>,
    /// Server-initiated `workspace/applyEdit`s whose response is held back until the
    /// file operations they asked for land — the server is told what actually
    /// happened, so the answer can't be minted before the last `rename`/`delete`
    /// settles. Keyed by an internal ticket; empty for an edit with no resource ops
    /// (answered inline) and for every user-driven apply. See
    /// [`EditHost::settle_apply_edit`].
    pending_apply_edits: HashMap<u64, lsp::PendingApplyEdit>,
    /// Source of the per-apply group ids: every `apply_workspace_edit` takes one, the
    /// file operations it queues carry it, and a server-initiated apply keys its
    /// held-back response by it.
    next_workspace_group: u64,
    /// The working directory last pushed into the `btv._cwd` mirror — so
    /// [`EditHost::publish_cwd_mirror`] can report whether a publish actually *moved* the
    /// cwd. A daemon-session focus switch uses that to fire `DirChanged` only on a real
    /// `:lcd`/`:tcd` boundary (the remote analogue of the local `fix_current_dir`), not on
    /// every window change. `None` until the first publish.
    published_cwd: Option<PathBuf>,
    /// Whether this session's `btv.fs` runs against a **remote daemon** whose cwd was seeded
    /// into `DirState` (a `--connect-daemon` native session, or a `?daemon=` web session that
    /// fetched `config_bundle`). Gates the outbound `btv.fs` / spawn cwd rebase in
    /// [`apply_loop_op`](EditHost::apply_loop_op): only such a session needs a relative path
    /// absolutized against `DirState` before the wire (the daemon is stateless). `false` for a
    /// bare/local session (the process cwd *is* the effective dir) and — critically — for a
    /// serverless web session (OPFS is root-relative and cwd-less, so rebasing would break it).
    /// Set where `DirState` is seeded from a daemon cwd (`run_server` native / `apply_remote_config`
    /// wasm); this is the correct daemon-fs gate, unlike `host_fs_offtick()` (true for OPFS too).
    remote_cwd_seeded: bool,
    /// The `'httphost'`/`'httpport'` values as of the last successful bind, or `None` until
    /// a plugin mounts (nothing binds before that).
    ///
    /// Deliberately the *option* values and not the resolved address: with `'httpport' = 0`
    /// the listener lands on some ephemeral port, and comparing that against the option
    /// would read as a change on every tick and rebind forever. What this answers is "have
    /// the options moved since we bound?", so it must be in the options' own terms.
    ///
    /// Compared against the live options each tick to notice a `:set` — there is no
    /// `OptionSet` event to hook. `Some` is also the gate for that check, so a config with
    /// no HTTP plugin pays nothing for it. The resolved origin is mirrored into Lua as
    /// `btv._http_origin` (the one place `Mount:url()` reads), not held here.
    #[cfg(feature = "native")]
    http_serving: Option<(String, u16)>,
    /// A rebind asked for and not yet answered — the `'httphost'`/`'httpport'` values sent
    /// as a [`LoopCommand::HttpRebind`]. Without it the per-tick check would re-send the
    /// same rebind every tick until the reply landed, since `http_serving` still holds the
    /// old address until then.
    #[cfg(feature = "native")]
    http_rebind_inflight: Option<(String, u16)>,
    /// Live `btv.http.mount` routes on the wasm build: mount name (`example`) → its callback
    /// id. The browser analogue of the native actor's route map, and kept on THIS side
    /// rather than in JS on purpose: the Service Worker hands in a raw path, and resolving
    /// it — the `/plugin/<name>/<rest>` split, the miss→404 — must work exactly as the
    /// native listener does, which it can only do by sharing
    /// [`bemtvi_lua::split_mount_path`] with it.
    #[cfg(not(feature = "native"))]
    http_routes: std::collections::HashMap<String, u64>,
    /// The wasm build's timer wheel (slice 5d): pending `vim.defer_fn` / `btv.timer`
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
    /// When the `timeoutlen` idle flush is due (ms on the JS clock), or `None` when
    /// nothing is withheld / `'notimeout'` is set. Armed by [`EditHost::feed`] after
    /// a keystroke leaves an ambiguous mapped prefix; folded into
    /// [`next_timer_deadline`](EditHost::next_timer_deadline) so the Worker's one
    /// `Atomics.wait` also wakes to resolve it, and consumed by
    /// [`fire_due_timers`](EditHost::fire_due_timers) — the wasm analogue of the
    /// native clients' idle-flush timer. Wasm build only.
    #[cfg(not(feature = "native"))]
    flush_due_ms: Option<u64>,
    /// Names of treesitter grammars the browser host has available — the offline set
    /// bundled in `web/vendor/` plus any fetched via `:TSInstall` into OPFS. Seeded at
    /// boot ([`EditHost::seed_ts_installed`]) and extended on each completed install
    /// ([`EditHost::complete_ts_install`]); read by `:TSInstallInfo`. The browser build
    /// highlights JS-side (web-tree-sitter), so it has no on-disk parser dir to scan
    /// like native's [`bemtvi_ts::installed_parsers`] — this set is that listing. Wasm only.
    #[cfg(not(feature = "native"))]
    ts_installed: std::collections::BTreeSet<String>,
    /// The browser client's chord substitutions (`<C-w>` → `<A-w>`), mirrored into
    /// `btv.ui.caps().key_labels` so a which-key popup names the chord the visitor can
    /// actually press. The page computes them — only it knows the platform and which
    /// shortcuts this browser keeps for itself — and hands them in via
    /// [`set_key_labels`](EditHost::set_key_labels) after [`attach_ui`], so they are
    /// held here to survive a re-attach (a resize re-runs `attach_ui`). Natively the
    /// same pairs ride the `btv_ui_attach` capabilities map instead. Wasm only.
    #[cfg(not(feature = "native"))]
    key_labels: Vec<(String, String)>,
}

impl EditHost {
    /// Hand Lua the catalogs core owns: the option catalog (each name's scope, global
    /// tier and doc), the bundled colorscheme names, and the recognized filetypes. The
    /// server is the integrator — bemtvi-lua stays decoupled from editor-core types, so
    /// they cross as plain data.
    ///
    /// This runs at construction, before anything can source a config, because the
    /// prelude *derives* its `vim.o` scope routing (`O_WIN` / `O_BUF`) from the option
    /// rows. Without them every buffer- and window-scoped write falls through to the
    /// lenient `btv._o_store` catch-all and never reaches the core — and because `vim.o`
    /// reads that same store back, the write still *looks* applied. That is not
    /// hypothetical: while this was wired only into [`run_io`], the web edit-host (which
    /// does not go through `run_io`) silently dropped every buffer/window option a
    /// browser config set, `vim.opt.scrolloff = 8` included. Doing it here, in the one
    /// construction site both legs share, is what keeps them from drifting again.
    ///
    /// A failure here is a broken prelude, not a runtime condition — it fails loud
    /// rather than leaving a half-wired VM that mis-routes options.
    fn install_core_catalogs(lua: &LuaRuntime) {
        let option_rows: Vec<bemtvi_lua::OptionCatalogRow> =
            bemtvi_core::options::options_catalog()
                .iter()
                .map(|o| bemtvi_lua::OptionCatalogRow {
                    name: o.name.to_string(),
                    abbrev: o.abbrev.map(str::to_string),
                    kind: o.kind.as_str().to_string(),
                    scope: o.scope.as_str().to_string(),
                    global_tier: bemtvi_core::options::has_global_tier(o.name),
                    doc: o.doc.to_string(),
                })
                .collect();
        lua.set_options_catalog(&option_rows)
            .expect("option catalog init failed");
        // `:colorscheme <Tab>` offers the binary's bundled schemes: the embedded
        // `runtime/colors/` tree is not on the runtimepath, so the completer's
        // `colors/*.lua` glob cannot discover them.
        let builtin_schemes: Vec<String> = crate::excmd::BUILTIN_COLORSCHEMES
            .iter()
            .map(|(name, _)| name.to_string())
            .collect();
        lua.set_builtin_colorschemes(&builtin_schemes)
            .expect("colorscheme catalog init failed");
        // `:setfiletype <Tab>`; core's extension-detection table is the single source.
        let filetypes: Vec<String> = bemtvi_core::known_filetypes()
            .iter()
            .map(|s| s.to_string())
            .collect();
        lua.set_filetypes(&filetypes)
            .expect("filetype catalog init failed");
    }

    /// Construct an edit-host over the given outbound-effect seam, every field at its
    /// startup default. The single construction site for the struct, shared by the
    /// native [`run_io`] (which then seeds `shada` / `mouse_clock` / the LSP keymap
    /// defaults and sources config) and the out-of-crate wasm cdylib (slice 5b, which
    /// calls [`boot`](Self::boot) for the serverless startup). The caller attaches a
    /// UI — [`attach_ui`](Self::attach_ui) on wasm, the `btv_ui_attach` RPC natively —
    /// before the first [`redraw`](Self::redraw).
    pub fn new(editor: Editor, lua: LuaRuntime, fx: Box<dyn HostEffects>) -> EditHost {
        Self::install_core_catalogs(&lua);
        EditHost {
            editor,
            lua,
            fx,
            #[cfg(feature = "native")]
            daemon_link: None,
            #[cfg(feature = "native")]
            prev_daemon_status: None,
            #[cfg(feature = "native")]
            shada: None,
            #[cfg(feature = "native")]
            global_history: None,
            #[cfg(feature = "native")]
            workspace_session: false,
            #[cfg(feature = "native")]
            restore_session: false,
            pending_session: None,
            #[cfg(feature = "native")]
            remote_shada: None,
            #[cfg(feature = "native")]
            osc52: None,
            ui: None,
            keyboard_protocol: false,
            #[cfg(feature = "native")]
            syntax_states: HashMap::new(),
            #[cfg(feature = "native")]
            first_highlight_deferred: HashSet::new(),
            #[cfg(feature = "native")]
            resolved_ts_langs: HashSet::new(),
            lsp_states: HashMap::new(),
            lsp_servers: HashMap::new(),
            lsp_progress: HashMap::new(),
            lsp_priorities: HashMap::new(),
            signature_auto: false,
            lsp_ensured: HashSet::new(),
            lsp_spawns: HashMap::new(),
            next_lsp_client_id: 1,
            lsp_dirty: false,
            lsp_req_gen: 0,
            lsp_requests: HashMap::new(),
            lsp_fanouts: HashMap::new(),
            lsp_multi_requests: HashMap::new(),
            inlay_resolves: HashMap::new(),
            inlay_resolve_seq: 0,
            complete_lsp_active: false,
            complete_lsp_priority: 0,
            complete_lsp_min_chars: 1,
            lsp_complete: None,
            lsp_complete_resolve_key: None,
            complete_resolve_docs: std::collections::HashMap::new(),
            complete_resolve_inflight: None,
            lsp_code_actions: Vec::new(),
            lsp_code_action_servers: Vec::new(),
            diag_config: DiagnosticConfig::default(),
            client_diagnostics: HashMap::new(),
            pending_client_diagnostics: HashMap::new(),
            diag_mark_counts: HashMap::new(),
            diag_mark_gens: HashMap::new(),
            semantic_tokens_enabled: true,
            snippet_store: HashMap::new(),
            complete_snippets_active: false,
            complete_snippets_priority: 0,
            complete_snippets_min_chars: 1,
            snippet_complete: Vec::new(),
            statusline_layout: None,
            statusline_window: HashMap::new(),
            statusline_cache: std::collections::HashMap::new(),
            statusline_custom: Vec::new(),
            statusline_pending: std::collections::HashSet::new(),
            statusline_layout_key: None,
            last_buffer_id: None,
            announced: HashSet::new(),
            fired_filetype: HashMap::new(),
            fired_encoding: HashMap::new(),
            known_buffers: Vec::new(),
            startup_bufs_seeded: false,
            last_mode: Mode::Normal,
            last_window_id: None,
            known_windows: Vec::new(),
            known_window_buffers: HashMap::new(),
            last_window_rects: None,
            last_window_scroll: None,
            last_tab_id: None,
            known_tabs: Vec::new(),
            last_cursor: None,
            last_text: None,
            au_event_version: 0,
            au_active_events: HashSet::new(),
            keymaps: Keymaps::default(),
            last_key_pending: None,
            scheduled: VecDeque::new(),
            buf_mirror_ticks: HashMap::new(),
            buf_mirror_lines: HashMap::new(),
            bo_mirror_gen: u64::MAX,
            bo_mirror_known: HashSet::new(),
            win_jump_gens: HashMap::new(),
            extmark_gens: HashMap::new(),
            undo_mirror_versions: HashMap::new(),
            qf_mirror_version: None,
            reg_mirror_gen: None,
            reg_mirror_specials: Vec::new(),
            start: std::time::Instant::now(),
            mouse_clock: None,
            mono_clock: None,
            hl_mirror_gen: None,
            scheme_groups: std::collections::HashSet::new(),
            pending_ui_input: None,
            pending_ui_select: None,
            #[cfg(feature = "native")]
            pending_code_action: false,
            code_action_cb: 0,
            pending_gated_writes: HashMap::new(),
            next_gate_id: 0,
            au_gate_done: Vec::new(),
            exit_stage: None,
            exit_gate: None,
            read_chains: HashMap::new(),
            chain_gates: HashMap::new(),
            quit_all_replay: None,
            picker_active: false,
            preview_cache: redraw::PreviewCache::default(),
            #[cfg(feature = "native")]
            parked_ts_highlights: Vec::new(),
            preview_scroll: 0,
            preview_hscroll: 0,
            preview_anchor: None,
            feed_buffer: VecDeque::new(),
            macro_suppress: 0,
            macro_play: Vec::new(),
            macro_state_mirror: (None, None),
            paste_payload: None,
            test_mode: false,
            lua_stdio: false,
            saves_inflight: HashSet::new(),
            saves_queued: HashMap::new(),
            quit_all_gate: None,
            buf_watches: HashMap::new(),
            remote_watches: HashSet::new(),
            reload_posts: HashSet::new(),
            forced_fetch_enc: HashMap::new(),
            terminals: HashMap::new(),
            terminal_frozen: HashMap::new(),
            dirs: cwd::DirState::new(std::env::current_dir().unwrap_or_default()),
            pending_chdirs: HashMap::new(),
            pending_replica_edits: HashMap::new(),
            pending_goto_cols: HashMap::new(),
            workspace_fs_jobs: HashMap::new(),
            next_workspace_fs_id: WORKSPACE_FS_JOB_BASE,
            workspace_fs_queue: std::collections::VecDeque::new(),
            workspace_fs_inflight: None,
            pending_create_writes: HashMap::new(),
            pending_confirm_edits: HashMap::new(),
            pending_apply_edits: HashMap::new(),
            next_workspace_group: 1,
            next_chdir_token: 0,
            published_cwd: None,
            remote_cwd_seeded: false,
            #[cfg(feature = "native")]
            http_serving: None,
            #[cfg(feature = "native")]
            http_rebind_inflight: None,
            #[cfg(not(feature = "native"))]
            http_routes: std::collections::HashMap::new(),
            #[cfg(not(feature = "native"))]
            wasm_timers: Vec::new(),
            #[cfg(not(feature = "native"))]
            clock_ms: 0,
            #[cfg(not(feature = "native"))]
            flush_due_ms: None,
            #[cfg(not(feature = "native"))]
            ts_installed: std::collections::BTreeSet::new(),
            #[cfg(not(feature = "native"))]
            key_labels: Vec::new(),
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
    /// `btv_ui_attach` RPC (the dispatch router is gated off the wasm build), so
    /// [`redraw`](Self::redraw) has dimensions to project the view into, then paints
    /// the initial frame (as `btv_ui_attach` triggers a repaint natively). Also the
    /// resize path — a re-attach at a new size repaints.
    pub fn attach_ui(&mut self, width: usize, height: usize) {
        self.ui = Some((width, height));
        // A browser delivers Ctrl+I distinct from Tab (keydown `code` + `ctrlKey`),
        // like a native window — so the serverless edit host always has full keyboard
        // disambiguation. Declare it so `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>` stay apart from
        // `<Tab>`/`<CR>`/`<Esc>`/`<BS>` (mirrors the TUI/GUI reporting it at attach).
        self.keyboard_protocol = true;
        self.keymaps.set_keyboard_protocol(true);
        // Mirror the same capabilities into `btv.ui.caps()` the native `btv_ui_attach`
        // does: a browser canvas paints 24-bit color, and the clipboard goes through the
        // browser API rather than an OSC 52 escape. `UIEnter` is NOT fired here — this
        // attach runs at `eh_new`, before the Worker sources `init.lua`, so a config
        // subscribing to it wouldn't exist yet; `boot_finish` fires it once startup is
        // done, which is the native order (config, `VimEnter`, then the UI is known).
        let _ = self.lua.set_ui_caps(true, true, false, &self.key_labels);
        self.redraw();
    }

    /// Declare the chord substitutions this page performs — `("<C-w>", "<A-w>")` says
    /// a keypress of `Alt+w` is what arrives here as `<C-w>`, because the browser keeps
    /// the real chord for itself. Mirrored into `btv.ui.caps().key_labels` for whatever
    /// *displays* keys (a which-key popup, a cheat sheet); nothing about input changes,
    /// since the page already substituted before sending. Only the page can compute
    /// this — the platform and the browser's own reserved list are its to know — so it
    /// is pushed in rather than inferred. Call after [`attach_ui`](Self::attach_ui) and
    /// before [`boot_finish`](Self::boot_finish), so `UIEnter` handlers see them.
    pub fn set_key_labels(&mut self, labels: Vec<(String, String)>) {
        self.key_labels = labels;
        let _ = self.lua.set_ui_caps(true, true, false, &self.key_labels);
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
        let name = self.editor.display_name(buf);
        let ft = filetype_of(self.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = self.lua.set_buf_snapshot(buf.0, &name, ft);
        self.push_buf_mirror();
        self.publish_workspace_identity();
        self.seed_startup_baselines();
    }

    /// Report this session as a workspace rooted at its current directory, so
    /// `btv.workspace.active()` / `btv.workspace.dir()` tell the truth on the web build.
    ///
    /// On the web the ORIGIN is the workspace: the shada blob lives in that origin's OPFS
    /// and there is exactly one session per origin, which is what `--workspace` names
    /// natively (see docs/plans/2026-08-14-web-session-restore.md). Reporting `false` /
    /// `nil` — what these did — is not a neutral default: a plugin that gates its
    /// persistence on `btv.workspace.active()` (bemtvi-dap keys its store that way) simply
    /// skipped it in a browser, with nothing said.
    ///
    /// `btv.shada.namespace()` is deliberately left `nil`. Natively it is the `ns/<id>/`
    /// token that isolates one launch's store from another's; on the web the browser's
    /// origin does that job and there is no second token to report. Reporting a fake one
    /// would be worse than reporting none.
    ///
    /// Seeded from the *effective* dir, so the serverless build reports the OPFS root and a
    /// daemon session reports the daemon's directory — re-published wherever that root
    /// arrives ([`seed_remote_cwd`](Self::seed_remote_cwd) and `apply_remote_config`), both
    /// of which run before any config line can read it.
    fn publish_workspace_identity(&mut self) {
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (_, dir) = self.dirs.effective(win, tab);
        let root = dir.to_string_lossy().into_owned();
        self.lua.set_workspace_identity(None, Some(root));
    }

    /// Baseline the window / tab / buffer sets the lifecycle diff reads, so the startup
    /// layout fires no `WinNew` / `TabNew` / `BufAdd` (neovim skips them for the initial
    /// window and tab). Enumerated cross-tab, like the diff itself, or a restore's
    /// background-tab windows would all read as new on the first diff.
    ///
    /// Run twice on EVERY boot: once in [`boot_begin`](Self::boot_begin) so the config sees
    /// a seeded editor, and again in [`boot_finish`](Self::boot_finish). The second pass
    /// matters most on a restoring boot — every window, tab and buffer the restore minted is
    /// startup state, not something that "appeared", and without it each one fires a spurious
    /// event at a config's autocmds — but it is unconditional on purpose, because that is
    /// where it lands `run_io`'s semantics: natively the one and only seed runs *after* the
    /// config phase, so anything the config itself opened is baseline there too. Seeding
    /// twice rather than moving the seed keeps `startup_bufs_seeded` true while the config
    /// runs, which is the state `boot_begin` was already establishing.
    fn seed_startup_baselines(&mut self) {
        self.known_windows = self.editor.all_window_ids();
        self.last_window_rects = Some(self.window_rects_snapshot());
        self.known_tabs = self.editor.tab_ids();
        self.known_buffers = self.editor.buffer_ids();
        self.startup_bufs_seeded = true;
    }

    /// Serverless startup, phase 2: fire the startup lifecycle events and mark
    /// `v:vim_did_enter`. Run after [`boot_begin`](Self::boot_begin) and the optional
    /// `init.lua` sourcing ([`source_config`](Self::source_config)).
    pub fn boot_finish(&mut self) {
        // Rebuild the layout `import_persist` held back, in `run_io`'s order: after the
        // config (so restored windows inherit its window-local options — the whole reason
        // the layout is held back) and before the lifecycle seed below. A no-op when the
        // config never opted into capture, or when nothing was stored.
        //
        // Each restored leaf is an ASYNC open here: the web fs is off-tick, so
        // `open_buffer_for_restore` enqueues a replica open the Worker fulfills over OPFS.
        // The window tree is therefore correct immediately and the text arrives a tick or
        // two later, exactly as a `:e` behaves on this build.
        self.apply_pending_session_restore();
        // Hand each persisted `btv.view` slot the restore reserved to its owning plugin,
        // now that the config (and, in a daemon session, the remote plugin surface) has
        // registered its `btv.view.on_restore` handlers. Unclaimed slots collapse rather
        // than lingering as empty placeholder windows. Done BEFORE the baseline below so
        // the placeholder churn fires no spurious `WinNew` / `WinClosed` — the same order
        // `run_io` uses.
        self.restore_persisted_views();
        // Re-baseline over the restored layout, or every window/tab/buffer it minted
        // reads as new and fires a spurious `WinNew` / `TabNew` / `BufAdd`.
        self.seed_startup_baselines();
        self.emit_lifecycle_events();
        self.run_pending();
        let _ = self.lua.set_vim_did_enter(true);
        self.fire_vim_enter();
        // Last word on focus: re-assert the layer the restored session was quit from, so a
        // session left in a dock reopens there rather than on the main layer. A no-op
        // unless a focus layer was captured.
        if self.editor.finalize_session_focus() {
            self.apply_lua_effects();
            self.run_pending();
        }
        // Then `UIEnter`, in the native order (startup first, the client second). The
        // browser's UI is already attached — `eh_new` did it before the config was
        // sourced — so firing it here is what gives a config or plugin the same chance
        // to subscribe that it has natively.
        if self.ui.is_some() {
            self.fire_ui_enter();
        }
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

    /// Apply a fetched [`RemoteConfigBundle`] in a **wasm edit-host (daemon) session** —
    /// the browser analogue of the native edit-host's fetch→materialize→source path
    /// (`run_edit_host_session` in `bemtvi/src/main.rs`). Run between
    /// [`boot_begin`](Self::boot_begin) and [`boot_finish`](Self::boot_finish), *instead*
    /// of the serverless single-file [`source_config`](Self::source_config): in daemon
    /// mode the editor is *born remote*, so its whole config + plugin surface comes from
    /// the daemon, not the local OPFS `init.lua`.
    ///
    /// The bundle's files are staged into the in-memory FS under [`WASM_REMOTE_CACHE_ROOT`]
    /// (emscripten MEMFS — the same synchronous FS Lua's `require` / `package.path` and
    /// `nvim_get_runtime_file` read from), the daemon's `runtimepath` is rebased onto that
    /// copy and seeded into the VM, the daemon's cwd seeds `DirState`, the daemon's
    /// tree-sitter parser set is wired for lazy auto-install, and finally the staged
    /// `init.lua` + package `plugin/` scripts are sourced through the real effects path —
    /// exactly the native order. A staging failure is a loud `Err` for the Worker to
    /// surface; a Lua error in the config surfaces on the message line and still lets the
    /// editor finish booting.
    ///
    /// Gated to the no-`native` build: the native edit-host fetches + materializes on the
    /// async session path (`run_edit_host_session`), not through this synchronous method.
    #[cfg(not(feature = "native"))]
    pub fn apply_remote_config(&mut self, bundle: RemoteConfigBundle) -> Result<(), String> {
        // Read the fields materialize doesn't consume before it takes the bundle by value.
        let ts_languages = bundle.ts_languages.clone();
        let remote_cwd = bundle.cwd.clone().map(PathBuf::from);
        let remote_home = bundle.home.clone().map(PathBuf::from);
        let (config_dir, runtimepath) =
            materialize_remote_config_into(Path::new(WASM_REMOTE_CACHE_ROOT), bundle)
                .map_err(|e| format!("staging remote config failed: {e}"))?;

        // The daemon's cwd is the one true cwd of a remote session — seed `DirState` and
        // publish the mirror before any config line reads `getcwd()` (native ordering).
        if let Some(cwd) = remote_cwd {
            self.dirs = cwd::DirState::new(cwd);
            self.publish_cwd_mirror();
            self.publish_workspace_identity();
            // A `?daemon=` web session: `btv.fs` routes to the daemon (stateless cwd), so
            // mark it for the `apply_loop_op` relative-path rebase. A serverless session
            // has `remote_cwd == None` and stays `false` (OPFS is root-relative, cwd-less).
            self.remote_cwd_seeded = true;
        }
        // A leading `~` in a file argument expands against the daemon's home over the wire.
        if let Some(home) = remote_home {
            self.editor.set_remote_home(home);
        }

        // Point `require` / runtime-file lookup at the staged copy: each rebased entry's
        // `lua/` joins `package.path` and its root joins the runtimepath.
        for rt in &runtimepath {
            if let Err(e) = self.lua.add_runtimepath(rt) {
                self.editor
                    .echo(format!("bemtvi: runtimepath seed failed: {e}"));
            }
        }

        // Lazily install the daemon's tree-sitter parser set: register the `FileType`
        // autocmd (in Lua) that `:TSInstall`s a language the first time a buffer of that
        // type opens. Registered BEFORE sourcing so the startup buffer's own `FileType` is
        // caught. (Unlike native, no local-installed filter — the wasm grammar set is
        // JS-side and `:TSInstall` no-ops an already-present one; the Lua side dedups.)
        if !ts_languages.is_empty() {
            let list = ts_languages
                .iter()
                .map(|lang| format!("{lang:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            if let Err(e) = self
                .lua
                .exec(&format!("btv._remote_ts_autoinstall({{ {list} }})"))
            {
                self.editor
                    .echo(format!("bemtvi: remote tree-sitter setup failed: {e}"));
            }
        }

        // Source the daemon's `init.lua`, then its package `plugin/` / `after/plugin/`
        // scripts across the runtimepath — neovim's startup order, through the same real
        // effects path the native edit-host uses (reads resolve against the staged FS).
        if let Some(config_dir) = &config_dir {
            self.source_init(&config_dir.join("init.lua"));
        }
        self.source_plugins();
        Ok(())
    }

    /// Seed the session cwd from a **runtime** daemon connect (`:connect bemtvi://…`), the
    /// browser twin of the boot-time [`apply_remote_config`](Self::apply_remote_config) cwd
    /// seed. A runtime `:connect` re-points the fs seam at a new daemon but does NOT re-fetch
    /// `config_bundle` (that would re-source the whole config), so the daemon's cwd — the one
    /// true cwd of the now-remote session — never reached `DirState`, and a relative `btv.fs`
    /// path stayed unrebased. The Worker fetches it (a `realpath(".")` over the fresh `luafs`
    /// leg) and hands it here: install it as the effective dir, refresh the `btv._cwd` mirror
    /// (`getcwd`), and mark the session daemon-fs so [`apply_loop_op`](Self::apply_loop_op)
    /// rebases relative `btv.fs` / spawn paths against it — exactly as the boot path does.
    #[cfg(not(feature = "native"))]
    pub fn seed_remote_cwd(&mut self, cwd: std::path::PathBuf) {
        self.dirs = cwd::DirState::new(cwd);
        self.publish_cwd_mirror();
        // The daemon's directory is this session's workspace root, not the local `/`.
        self.publish_workspace_identity();
        self.remote_cwd_seeded = true;
    }

    /// Feed vim key-notation and project the resulting frame — the wasm Worker's
    /// keystroke tick: `input` settles the core (mappings, ex-commands, queued Lua),
    /// then `redraw` pushes a frame through the [`HostEffects`] seam to the UI. Mirrors
    /// one turn of the native [`run`] loop's input arm.
    pub fn feed(&mut self, keys: &str) {
        // The user is driving now — release any restored-session focus hold so their own
        // navigation wins from here, exactly as the native `btv_input` dispatch arm does.
        // Without this the hold has no release point on the web build at all: `settle_events`
        // re-asserts the captured layer on EVERY settle (an fs completion, an LSP reply, a
        // watch, a proc line — `fire_due_timers` is the one wake that does not settle), so
        // a restored session would keep yanking focus back out of wherever you moved it.
        self.editor.clear_session_focus_hold();
        self.input(keys);
        // Arm the `timeoutlen` idle flush, the wasm analogue of the native clients'
        // flush timer: when this keystroke left an ambiguous mapped prefix withheld
        // and `'timeout'` is on, schedule a flush `timeoutlen` ms out so the Worker's
        // next `Atomics.wait` (parked on `next_timer_deadline`) wakes to resolve it.
        // The next key re-runs `feed`, re-arming or clearing it; `'notimeout'` leaves
        // it `None` so the prefix waits forever (a which-key popup stays up).
        self.flush_due_ms = if !self.keymaps.pending_empty() && self.editor.timeout_enabled() {
            Some(self.clock_ms.saturating_add(self.editor.timeoutlen_ms()))
        } else {
            None
        };
        self.redraw();
    }

    /// Apply a mouse gesture and repaint — the wasm Worker's `eh_input_mouse` tick.
    /// `button`/`action`/`modifier` are the `btv_input_mouse` strings and `row`/`col`
    /// the 0-based global screen cell; core owns the hit-test, multi-click word/line
    /// selection, drag-select, and wheel scroll, exactly as the native dispatch path
    /// does. The event is stamped from the Worker's JS clock ([`set_clock`](Self::set_clock),
    /// which the Worker sets before this call) so `'mousetime'` multi-click detection
    /// works. A malformed gesture surfaces a loud message rather than a silent no-op.
    pub fn mouse(&mut self, button: &str, action: &str, modifier: &str, row: usize, col: usize) {
        // A click is the user taking over too — release the restored-focus hold so a click
        // into (or away from) a sidebar sticks, matching `btv_input_mouse` natively.
        self.editor.clear_session_focus_hold();
        match bemtvi_core::MouseEvent::parse(button, action, modifier, row, col) {
            Ok(mut ev) => {
                ev.stamp_ms = self.clock_ms;
                self.editor.mouse(ev);
                // Resolve a mouse-button press against the keymaps (the same
                // `<n-LeftMouse>` mapping path as the native dispatch — the "two mouse
                // entry points need settle parity" rule), so a bound mouse map (e.g.
                // the explorer's `<2-LeftMouse>`) fires in the fully-client web build
                // too, before the gesture's effects are drained below.
                self.resolve_mouse_clicks();
                // Drain the effects the gesture can queue, exactly as the native
                // dispatch path does: a picker / select confirm or cancel
                // (`menu_results`), a completion accept's delegated edit
                // (`complete_accept_request`), a status-line `%@…%X` handler, and any
                // callback those fire (which may feed keys). Without this a click that
                // confirms a widget would queue the choice but never run its handler.
                self.run_pending();
                self.dispatch_statusline_clicks();
                self.drain_feedkeys();
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

    /// The full text of every visible, non-terminal buffer that is **not** the
    /// focused one, as `(file_name, text)` pairs (deduped by buffer; empty when only
    /// the focused buffer is on screen).
    ///
    /// The wasm UI highlights each visible window from its own buffer's text, but
    /// [`lines`](Self::lines) (`eh_lines`) only ships the *focused* buffer's. A
    /// window that has never held focus — the file beneath a grabbing float opened at
    /// startup — would otherwise have no text for the JS highlighter and render dark
    /// until focused. This is the background half of that readout; the native build
    /// has no analogue (it highlights every visible buffer server-side in
    /// `refresh_highlights`).
    pub fn aux_visible_lines(&mut self) -> Vec<(String, String)> {
        let Some((w, h)) = self.ui else {
            return Vec::new();
        };
        let view = self.editor.view(w, h);
        let focused = view.focused().buffer;
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for win in &view.windows {
            if win.buffer == focused || !seen.insert(win.buffer) {
                continue;
            }
            if self.editor.is_terminal_buffer(win.buffer) {
                continue;
            }
            if let Some(lines) = self.editor.lines_of(win.buffer) {
                out.push((win.file_name.clone(), lines.join("\n")));
            }
        }
        out
    }

    /// Snapshot the cross-session (shada) state for persistence. The native server
    /// serializes this into its redb store ([`shada`]); the wasm Worker serializes it to
    /// a JSON blob in OPFS. Folds in the opt-in **plugin** data, which lives in the Lua
    /// runtime (not the editor model), so both fronts persist it through this one seam —
    /// the native flush sites and the wasm `eh_export_shada` alike. `exit_cursor` is
    /// layered on by the caller.
    ///
    /// The window/tab layout rides along under the same
    /// [`session_captures_layout`](Self::session_captures_layout) gate the three native
    /// flush sites use — default off, so a browser session that never opted in stores no
    /// layout, exactly as a plain native launch does.
    pub fn export_persist(&self) -> bemtvi_core::PersistState {
        let mut state = self.editor.export_persist();
        state.plugin_data = tuples_to_plugin_shada(self.lua.plugin_shada_export());
        if self.session_captures_layout() {
            state.session = self.editor.export_session();
        }
        // The `btv.wso` overlay rides along unconditionally: natively it is gated on
        // `workspace_session` because a global store must never carry per-workspace
        // overrides, but on the web every session IS workspace-scoped (one blob per
        // origin), so there is no global store to keep clean. `Editor::apply_persist`
        // already seeds it back on load — only this half was missing, which made a
        // `btv.wso` write look like it persisted and then quietly not.
        state.workspace_options = self.editor.workspace_options().clone();
        state
    }

    /// Seed cross-session (shada) state restored from the store, before the startup
    /// lifecycle fires (so a restored `` `" `` / registers / history are live for the
    /// first frame). Seeds the opt-in **plugin** data back into the Lua runtime too (the
    /// inverse of [`export_persist`](Self::export_persist)), so a plugin's `get` returns
    /// last session's value on web exactly as on native. The wasm Worker calls this
    /// between config-sourcing and [`boot_finish`](Self::boot_finish); native's
    /// `shada_load` calls it after pulling the session out.
    pub fn import_persist(&mut self, mut state: bemtvi_core::PersistState) {
        let plugin_data = std::mem::take(&mut state.plugin_data);
        // The layout is held back, not applied here: `Editor::apply_persist` ignores the
        // field entirely, and a layout must not be rebuilt until the config has been
        // sourced (see `apply_pending_session_restore`). Taking it into `pending_session`
        // is what the native `shada_load` does with it; the restore point itself is
        // Phase 2 of docs/plans/2026-08-14-web-session-restore.md.
        self.pending_session = state.session.take();
        self.editor.import_persist(state);
        self.lua
            .plugin_shada_seed(plugin_shada_to_tuples(plugin_data));
    }

    /// Set the Worker's current JS clock (ms) so a [`WasmTimer`] armed during the next
    /// tick computes its `due_ms` relative to *now*. The Worker calls this before
    /// feeding input; [`fire_due_timers`](Self::fire_due_timers) sets it too.
    ///
    /// It also advances the editor's **monotonic second base** — the wasm twin of the
    /// per-message stamp the native `handle()` does. That base is the undo timeline:
    /// every node commits with the second its change group began, and `:undolist`'s age
    /// column and `:earlier`/`:later {N}s|m|h|d` read the deltas. Unstamped it stays 0
    /// forever, so on the web every state would be the same age and every timed travel
    /// would run off the end of the history. `performance.now()` is milliseconds since
    /// page load, so `/ 1000` is the same seconds-since-start basis native uses (the
    /// root node's `time: 0` included). `vim.fn.localtime()` mirrors it, as it does
    /// natively.
    pub fn set_clock(&mut self, now_ms: u64) {
        self.clock_ms = now_ms;
        let secs = (now_ms / 1000) as i64;
        self.editor.set_now_mono(secs);
        let _ = self.lua.set_mono_secs(secs);
    }

    /// The soonest pending timer deadline (ms on the JS clock), or `None` when no timer
    /// is armed. The Worker parks on `Atomics.wait` with this as its timeout — so the one
    /// wait that wakes on a keystroke also wakes to fire the next timer (slice 5d's "one
    /// mechanism" — no busy loop, no separate timer thread).
    pub fn next_timer_deadline(&self) -> Option<u64> {
        // The `timeoutlen` idle flush rides the same wheel, so the one wait that wakes
        // for a `vim.defer_fn` also wakes to resolve a withheld mapped prefix.
        self.wasm_timers
            .iter()
            .map(|t| t.due_ms)
            .chain(self.flush_due_ms)
            .min()
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
        // Through `set_clock` so a callback that edits commits its undo node on this
        // wake's timestamp, not the last keystroke's.
        self.set_clock(now_ms);
        let mut due: Vec<WasmTimer> = self
            .wasm_timers
            .iter()
            .copied()
            .filter(|t| t.due_ms <= now_ms)
            .collect();
        due.sort_by_key(|t| t.due_ms);
        let mut fired_any = false;
        for timer in due {
            // Skip a timer a prior callback in this pass stopped (via its `btv.timer` /
            // `vim.defer_fn` handle); re-arm a repeat or drop a one-shot *before* running the callback.
            let Some(idx) = self.wasm_timers.iter().position(|t| t.id == timer.id) else {
                continue;
            };
            let keep = timer.repeat_ms > 0;
            if keep {
                self.wasm_timers[idx].due_ms = now_ms.saturating_add(timer.repeat_ms);
            } else {
                self.wasm_timers.remove(idx);
            }
            // The editor's own watchdog over a workspace edit's file operations rides
            // this wheel too (the browser twin of the native run loop's arm): it is not
            // a Lua callback, so it never reaches `run_callback`.
            if timer.id == WORKSPACE_FS_TIMEOUT_TIMER_ID {
                self.on_workspace_fs_timeout();
                self.apply_lua_effects();
                fired_any = true;
                continue;
            }
            // Likewise the diagnostic-update debounce (the browser twin of the native
            // run loop's arm): typing went quiet, so apply what the pause held.
            if timer.id == DIAG_DEBOUNCE_TIMER_ID {
                self.on_diag_debounce();
                self.apply_lua_effects();
                fired_any = true;
                continue;
            }
            if let Err(e) = self
                .lua
                .run_callback(timer.id, keep, bemtvi_lua::CallbackArgs::None)
            {
                self.editor
                    .echo(format!("E5108: Error in timer callback: {e}"));
            }
            self.apply_lua_effects();
            fired_any = true;
        }
        // The `timeoutlen` idle flush, fired off the same wheel: if its deadline has
        // passed, resolve the withheld mapped prefix (`input_flush` is itself a no-op
        // under `'notimeout'`, but it's never armed then). Disarm so it fires once.
        if self.flush_due_ms.is_some_and(|due| due <= now_ms) {
            self.flush_due_ms = None;
            self.input_flush();
            fired_any = true;
        }
        if fired_any {
            self.run_pending();
            self.redraw();
        }
        fired_any
    }

    /// Arm (or re-arm) a Worker-side timer — the wasm branch of
    /// [`apply_loop_op`](Self::apply_loop_op)'s [`LoopOp::TimerStart`](bemtvi_lua::LoopOp::TimerStart).
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

    /// Cancel the Worker-side timer armed under `id` (an `btv.timer` handle's stop, or a
    /// `vim.defer_fn` handle's stop) — the wasm branch of
    /// [`LoopOp::TimerStop`](bemtvi_lua::LoopOp::TimerStop). A no-op if it already fired.
    pub(crate) fn stop_wasm_timer(&mut self, id: u64) {
        self.wasm_timers.retain(|t| t.id != id);
    }

    /// Turn on **off-tick fs** for the serverless browser build (Phase 6, the OPFS
    /// slice). The editor then defers `:e` / `:w` to a [`PendingOpen`](bemtvi_core::editor)
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

    /// Turn on command-line completion (`:`+`<Tab>` — `btv.cmdline_complete`) by default,
    /// the wasm analogue of the native binary's `cmdline_complete_default` opt-in
    /// (`run_io` → `btv.cmdline_complete.setup{}`). The serverless build has no
    /// [`ServerInit`] to carry that flag, so the cdylib calls this in `eh_new` *after*
    /// [`boot_begin`](Self::boot_begin) and *before* the optional `init.lua`
    /// ([`source_config`](Self::source_config)) — exactly the native ordering, so a
    /// config's own `btv.cmdline_complete.setup{ ... }` (e.g. toggling the docs pane)
    /// still wins: both setups queue and drain in order on the next `apply_lua_effects`
    /// (init.lua's, or the first keypress's), last config wins. Without this, the web
    /// build never enables the engine and `:`+`<Tab>` does nothing.
    pub fn enable_cmdline_complete(&mut self) {
        let _ = self.lua.exec("btv.cmdline_complete.setup{}");
    }

    /// Apply a finished off-tick OPFS **file** read the Worker fetched for `buffer` (the
    /// `:edit` / startup analogue of the native [`apply_open`](Self::apply_open), minus the
    /// daemon-only `FsRead` / watch machinery). `kind`: `0` = an existing file (`contents`
    /// is its UTF-8 text), `1` = a not-yet-existing path (a new-file buffer, no bytes), any
    /// other = a read error (`contents` carries the message). A **directory** read does not
    /// arrive here — the cdylib routes it to [`complete_fs_read_dir`](Self::complete_fs_read_dir)
    /// with its entries; a stray `kind == 2` here is a defensive loud echo, never a silent
    /// empty buffer. Repaints once the buffer lands.
    pub fn complete_fs_read(
        &mut self,
        buffer: BufferId,
        path: String,
        kind: u8,
        bytes: &[u8],
        err: &str,
        stat: Option<FileStat>,
    ) {
        // A reserved preview fetch (the off-tick branch of `ensure_preview`): route the
        // bytes to the picker preview cache, NOT into a buffer — a read-only preview must
        // not run buffer lifecycle. Then repaint, like the buffer path below.
        if buffer == crate::redraw::PREVIEW_FETCH_BUF {
            let (lines, ok) = match kind {
                0 => (crate::redraw::bytes_to_preview_lines(bytes), true),
                1 => (Vec::new(), true),
                _ => (vec![format!("{path}: {err}")], false),
            };
            self.apply_preview(path, lines, ok);
            self.redraw();
            return;
        }
        match kind {
            // kind 0 = an existing file (real bytes); kind 1 = a new file that wasn't on
            // disk. The flag stamps the read-from-disk baseline for the former so it fires
            // `BufReadPost`, not `BufNewFile` (see `load_replica_bytes`).
            0 => self.load_replica_bytes(buffer, path, bytes, true, stat),
            1 => self.load_replica_bytes(buffer, path, b"", false, None),
            2 => {
                // A workspace edit never targets a directory; drop any stranded stash.
                self.pending_replica_edits.remove(&buffer);
                self.editor.echo(format!(
                    "bemtvi: directory read of {path} reached the file applier (use complete_fs_read_dir)"
                ));
            }
            _ => {
                // The OPFS read failed — a workspace edit waiting on this file can't
                // apply; drop its stash and report (else a generic open error).
                if self.pending_replica_edits.remove(&buffer).is_some() {
                    self.editor.echo(format!(
                        "apply_workspace_edit: could not open {path}: {err}"
                    ));
                } else {
                    self.editor
                        .echo(format!("bemtvi: could not open {path}: {err}"));
                }
            }
        }
        self.redraw();
    }

    /// Apply a finished off-tick OPFS directory landing into `buffer` — the wasm analogue
    /// of the native `apply_open`'s `FsRead::Dir` arm. The Worker enumerated OPFS directory
    /// `dir`; this fills the file-explorer listing from those `entries`
    /// ([`load_dir_listing`](Self::load_dir_listing)), so `:e <dir>` and descending /
    /// going up navigate the browser's OPFS tree exactly as a daemon session navigates the
    /// remote tree — the listing is the same shape the explorer plugin produces locally,
    /// so its navigation and decor work over the wire. Repaints after.
    pub fn complete_fs_read_dir(
        &mut self,
        buffer: BufferId,
        dir: String,
        entries: Vec<bemtvi_core::DirEntry>,
    ) {
        // A reserved preview fetch that resolved to a directory: show a placeholder in the
        // preview cache rather than building an explorer listing into a nonexistent buffer.
        if buffer == crate::redraw::PREVIEW_FETCH_BUF {
            self.apply_preview(dir, vec!["<directory>".to_string()], false);
            self.redraw();
            return;
        }
        self.load_dir_listing(buffer, dir, entries);
        self.redraw();
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
                .echo(format!("E5108: Error in btv.run_stream handler: {e}"));
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
        let args = bemtvi_lua::CallbackArgs::Process {
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

    /// Land the typed result of an off-tick `btv.fs` op (the `luafs_op` leg's reply) — the
    /// wasm entry the Worker calls when the daemon answers the op enqueued under `id` (a
    /// `btv._cb_fns` promise). `reply` is the `["ok", <fs-value>] | ["err", code, message]`
    /// envelope re-encoded to msgpack bytes by the Worker; this decodes it (via
    /// [`bemtvi_lua::fs_result_from_value`]) into the `Result<FsValue, FsError>` the promise
    /// resolves / rejects with, runs the callback, drains the effects it queues, then settles
    /// and repaints — the wasm twin of the native `on_loop_event`'s `LoopEvent::FsResult` arm.
    /// A malformed reply decodes to a loud `EWIRE` `FsError`, never a silent success. The
    /// callback may itself queue further async (a chained `btv.fs`, an off-tick `:edit`), so
    /// `settle_events` drives the convergence, exactly as [`Self::proc_exited`] does.
    pub fn fs_op_result(&mut self, id: u64, reply: Vec<u8>) {
        let result = match rmpv::decode::read_value(&mut &reply[..]) {
            Ok(value) => bemtvi_lua::fs_result_from_value(&value),
            Err(e) => Err(bemtvi_lua::FsError {
                code: "EWIRE".to_string(),
                message: format!("btv.fs: malformed luafs_op reply: {e}"),
            }),
        };
        // A workspace edit's own file operation (`rename`/`delete`) rides the same leg
        // under an id above `WORKSPACE_FS_JOB_BASE`, and settles in the editor (the
        // buffer rebind/wipe, and the `workspace/applyEdit` response) rather than in a
        // Lua promise — the wasm twin of the native `FsResult` arm's first branch.
        if self.on_workspace_fs_result(id, &result) {
            self.apply_lua_effects();
            self.settle_events(true);
            return;
        }
        if let Err(e) =
            self.lua
                .run_callback(id, false, bemtvi_lua::CallbackArgs::FsResult { result })
        {
            self.editor
                .echo(format!("E5108: Error in btv.fs handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land the typed result of an off-tick `btv.git.*` op (the daemon `git_op` leg's reply) —
    /// the wasm twin of the native `on_loop_event`'s [`LoopEvent::GitResult`](crate::evloop)
    /// arm, and the git sibling of [`Self::fs_op_result`]. `reply` is the `["ok", <git-value>]
    /// | ["err", code, message]` envelope re-encoded to msgpack bytes by the Worker (bytes,
    /// since `show`'s blob is raw); this decodes it (via [`bemtvi_lua::git_result_from_value`])
    /// into the `Result<GitValue, GitError>` the promise resolves / rejects with, runs the
    /// callback, drains, then settles and repaints. A malformed reply decodes to a loud
    /// `EWIRE` `GitError`, never a silent success.
    pub fn git_op_result(&mut self, id: u64, reply: Vec<u8>) {
        let result = match rmpv::decode::read_value(&mut &reply[..]) {
            Ok(value) => bemtvi_lua::git_result_from_value(&value),
            Err(e) => Err(bemtvi_lua::GitError {
                code: "EWIRE".to_string(),
                message: format!("btv.git: malformed git_op reply: {e}"),
            }),
        };
        if let Err(e) =
            self.lua
                .run_callback(id, false, bemtvi_lua::CallbackArgs::GitResult { result })
        {
            self.editor
                .echo(format!("E5108: Error in btv.git handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land the typed result of an off-tick `btv.http.fetch` (the `http_op` leg's reply, or the
    /// browser `fetch()`'s result) — the wasm twin of the native `on_loop_event`'s
    /// [`LoopEvent::HttpResult`](crate::evloop) arm. `reply` is the `["ok", <response>] |
    /// ["err", message]` envelope re-encoded to msgpack bytes by the Worker (bytes, not a C
    /// string, because a response body carries raw bytes); this decodes it (via
    /// [`bemtvi_lua::http_result_from_value`]) into the `Result<HttpResponse, HttpError>` the
    /// promise resolves / rejects with, runs the callback, drains the effects it queues, then
    /// settles and repaints. A malformed reply decodes to a loud transport error, never a
    /// silent success.
    pub fn http_result(&mut self, id: u64, reply: Vec<u8>) {
        let result = match rmpv::decode::read_value(&mut &reply[..]) {
            Ok(value) => bemtvi_lua::http_result_from_value(&value),
            Err(e) => Err(bemtvi_lua::HttpError {
                message: format!("btv.http: malformed http_op reply: {e}"),
            }),
        };
        if let Err(e) =
            self.lua
                .run_callback(id, false, bemtvi_lua::CallbackArgs::HttpResult { result })
        {
            self.editor
                .echo(format!("E5108: Error in btv.http handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }
    /// Settle an `btv.http.mount` promise from the Worker's Service Worker registration: `ok`
    /// resolves it with `text` as the page's origin, else rejects with `text` as the reason.
    /// The wasm twin of the native [`LoopEvent::HttpMountResult`].
    ///
    /// A reject is load-bearing rather than defensive: a Service Worker needs a secure
    /// context, so a plain-`http://` non-localhost origin genuinely cannot serve mounts, and
    /// the plugin has to learn that instead of receiving a URL that would 404.
    #[cfg(not(feature = "native"))]
    pub fn http_mount_result(&mut self, id: u64, ok: bool, text: String) {
        if !ok {
            // The mount never published — drop the route so a later request cannot find a
            // handler the browser can never reach.
            self.http_routes.retain(|_, mount| *mount != id);
        }
        let result = if ok {
            Ok(text)
        } else {
            Err(bemtvi_lua::HttpMountError { message: text })
        };
        if let Err(e) = self.lua.run_callback(
            id,
            false,
            bemtvi_lua::CallbackArgs::HttpMountResult { result },
        ) {
            self.editor
                .echo(format!("E5108: Error in btv.http.mount handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Run one Service-Worker-intercepted request through its mount's handler — the wasm twin
    /// of the native listener's axum handler, and the browser's entry into the *same*
    /// `HttpServerRequest` contract.
    ///
    /// Takes the raw pieces rather than the Worker's JSON: the JSON boundary belongs to
    /// `bemtvi-edithost`, which owns every other `eh_*` conversion.
    ///
    /// The routing is deliberately here rather than in JS: the `/plugin/<name>/<rest>` split,
    /// the mount lookup, and the miss→`404` all go through the same
    /// [`bemtvi_lua::build_server_request`] the native listener uses, so `req.path` cannot come
    /// to mean two different things in the two worlds.
    #[cfg(not(feature = "native"))]
    pub fn http_server_request(
        &mut self,
        req_id: u64,
        method: &str,
        path: &str,
        query: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) {
        let query = (!query.is_empty()).then_some(query);
        let request = bemtvi_lua::build_server_request(method, path, query, headers, body);
        let mount = request
            .as_ref()
            .and_then(|r| self.http_routes.get(r.name.as_str()).copied());
        let (Some(request), Some(id)) = (request, mount) else {
            // No mount by that name (or not under /plugin/ at all): the editor's own 404,
            // without entering Lua — exactly what the native listener answers.
            self.reply_http_server(req_id, 404, "bemtvi: no plugin mounted here\n");
            return;
        };
        // A live mount's bare root redirects to the trailing-slash form, so a page served
        // there resolves its relative URLs against the mount — the same rule the native
        // listener applies, via the same shared helper.
        if let Some(location) = bemtvi_lua::mount_root_redirect(path) {
            let location = match query {
                Some(q) => format!("{location}?{q}"),
                None => location,
            };
            self.redirect_http_server(req_id, &location);
            return;
        }
        if let Err(e) = self.lua.run_http_server_request(id, req_id, request) {
            self.editor
                .echo(format!("E5108: Error in btv.http.mount handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Redirect a Service-Worker request to `location` (a `308`), bypassing Lua — the browser
    /// twin of the native listener's trailing-slash redirect. Queued on the same reply path a
    /// handler's `respond` uses, so the Worker relays it identically and the browser follows.
    #[cfg(not(feature = "native"))]
    fn redirect_http_server(&mut self, req_id: u64, location: &str) {
        self.fx.http_respond(
            req_id,
            bemtvi_lua::HttpServerReply {
                status: 308,
                headers: vec![("location".to_string(), location.to_string())],
                body: Vec::new(),
            },
        );
    }

    /// Answer a Service-Worker request the editor itself is refusing (a `404`), bypassing Lua
    /// — the browser twin of the native listener's `text_response`. Queued on the same reply
    /// path a handler's `respond` uses, so the Worker relays it identically.
    #[cfg(not(feature = "native"))]
    fn reply_http_server(&mut self, req_id: u64, status: u16, body: &str) {
        self.fx.http_respond(
            req_id,
            bemtvi_lua::HttpServerReply {
                status,
                headers: vec![(
                    "content-type".to_string(),
                    "text/plain; charset=utf-8".to_string(),
                )],
                body: body.as_bytes().to_vec(),
            },
        );
    }

    /// Feed one `lsp_stdout` push (the LSP leg's daemon→edit-host direction) into the
    /// [`SyncLspClient`](bemtvi_lsp::SyncLspClient), then drain the events it produced —
    /// the wasm twin of the native run loop's `lsp_events` arm ([`on_lsp_events`]). The
    /// feed may complete a handshake or land a reply, which `on_lsp_event` turns into a
    /// `didOpen` / hover float / diagnostics; the outbound JSON-RPC those issue is flushed
    /// to the daemon by the host (drained by the Worker after this call). The Worker calls
    /// this from `RpcClient.onNotify`.
    ///
    /// [`on_lsp_events`]: EditHost::on_lsp_events
    pub fn lsp_stdout(&mut self, id: u64, bytes: Vec<u8>) {
        self.fx.lsp_stdout(id, bytes);
        self.drain_lsp_events();
    }

    /// Feed one `lsp_stderr` push — dropped (diagnostic only; the browser has no LSP log
    /// file, and the native manager only logs a server's stderr). No event, so no settle.
    pub fn lsp_stderr(&mut self, id: u64, bytes: Vec<u8>) {
        self.fx.lsp_stderr(id, bytes);
    }

    /// Land a raw output chunk from a duplex `btv.process` daemon child (the `dproc_*`
    /// leg's `dproc_out` push) — the wasm twin of the native `on_loop_event`'s
    /// [`LoopEvent::ProcOut`](crate::evloop) arm. Hands the chunk to the persistent Lua
    /// receiver (`btv._proc_recv`), drains the effects it queues (the DAP framing /
    /// dispatch — breakpoint signs, view renders), and settles + repaints.
    pub fn dproc_out(&mut self, id: u64, data: Vec<u8>, stderr: bool) {
        if let Err(e) = self.lua.run_process_recv(id, data, stderr) {
            self.editor
                .echo(format!("E5108: Error in btv.process handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land a duplex `btv.process` daemon child's exit (`dproc_exit`). Fires the Lua
    /// `on_exit` once and settles.
    pub fn dproc_exit(&mut self, id: u64, code: i32) {
        if let Err(e) = self.lua.run_process_exit(id, code) {
            self.editor
                .echo(format!("E5108: Error in btv.process on_exit: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land an `btv.socket` daemon connection's `connected` (`sock_connected`). Fires
    /// the Lua `on_connect` and settles.
    pub fn sock_connected(&mut self, id: u64) {
        if let Err(e) = self.lua.run_socket_connected(id) {
            self.editor
                .echo(format!("E5108: Error in btv.socket on_connect: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land a raw inbound chunk from an `btv.socket` daemon connection (`sock_data`).
    pub fn sock_data(&mut self, id: u64, data: Vec<u8>) {
        if let Err(e) = self.lua.run_socket_data(id, data) {
            self.editor
                .echo(format!("E5108: Error in btv.socket handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land an `btv.socket` daemon connection's close (`sock_closed`) — `error` set on a
    /// connect / I-O failure. Fires the Lua `on_close` once and settles.
    pub fn sock_closed(&mut self, id: u64, error: Option<String>) {
        if let Err(e) = self.lua.run_socket_closed(id, error) {
            self.editor
                .echo(format!("E5108: Error in btv.socket on_close: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land an `lsp_exited` push: the server (wire `id`) exited or its pipe closed. The
    /// `SyncLspClient` surfaces an `LspEvent::ServerExited`, drained here so the editor
    /// tells the user (it re-`ensure`s on the next `FileType`). A negative `code`/`signal`
    /// means "not collected" (a kill, a dropped link), per the proc-leg convention.
    pub fn lsp_exited(&mut self, id: u64, code: i32, signal: i32) {
        let code = (code >= 0).then_some(code);
        let signal = (signal >= 0).then_some(signal);
        self.fx.lsp_exited(id, code, signal);
        self.drain_lsp_events();
    }

    /// Drain the `SyncLspClient`'s distilled events into `on_lsp_event` and settle — the
    /// wasm analogue of the native [`on_lsp_events`](EditHost::on_lsp_events), which drains
    /// the `lsp_events` channel. Coalesces a burst into one repaint via `lsp_dirty`.
    fn drain_lsp_events(&mut self) {
        for event in self.fx.lsp_take_events() {
            self.on_lsp_event(event);
        }
        let dirty = std::mem::take(&mut self.lsp_dirty);
        self.settle_events(dirty);
    }

    /// Land a streaming `btv.fs.watch` change batch (the daemon `luafs_change` push) — the wasm
    /// entry the Worker calls when the daemon reports a coalesced change for the watch armed under
    /// `id`. `kind` is the change class (`"create"`/`"modify"`/`"remove"`/`"rename"`) and `paths`
    /// the affected absolute paths; fires the stream's pump (`btv._run_fs_watch`), drains the
    /// effects, and settles + repaints — the wasm twin of the native `on_loop_event`'s
    /// `LoopEvent::FsEvent` (no-error) arm.
    pub fn fs_watch_event(&mut self, id: u64, kind: String, paths: Vec<String>) {
        let paths = paths.into_iter().map(std::path::PathBuf::from).collect();
        if let Err(e) = self.lua.run_fs_watch_event(id, None, Some(&kind), paths) {
            self.editor
                .echo(format!("E5108: Error in btv.fs.watch handler: {e}"));
        }
        self.apply_lua_effects();
        self.settle_events(true);
    }

    /// Land a streaming `btv.fs.watch` terminal error (the daemon `luafs_watch_err` push) — the
    /// arm failed (bad path / watch limit) or the backend errored. Rejects the stream's pull
    /// (ending the iteration loud, never a dead watch), drains the effects, and settles. The wasm
    /// twin of the native `LoopEvent::FsEvent` (error) arm.
    pub fn fs_watch_error(&mut self, id: u64, message: String) {
        if let Err(e) = self
            .lua
            .run_fs_watch_event(id, Some(message), None, Vec::new())
        {
            self.editor
                .echo(format!("E5108: Error in btv.fs.watch handler: {e}"));
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
    /// answer to native's on-disk parser scan ([`bemtvi_ts::installed_parsers`]).
    pub fn ts_installed_list(&self) -> Vec<String> {
        self.ts_installed.iter().cloned().collect()
    }
}

/// Base for the loop ids of the server's **internal** per-buffer file watches, set
/// far above any Lua-allocated callback id so a [`LoopEvent::FsEvent`]
/// can be classified by `id >= INTERNAL_WATCH_BASE` alone. Buffer `b`'s watch id is
/// `INTERNAL_WATCH_BASE + b.0`, so the change routes straight back to the buffer with
/// no side table. (Lua callback ids are monotonic from 1 and never approach `1 << 48`.)
#[cfg(feature = "native")]
pub(crate) const INTERNAL_WATCH_BASE: u64 = 1 << 48;

/// Base for the ids of the off-tick [`FsJob`](bemtvi_lua::FsJob)s the **editor itself**
/// queues — a workspace edit's `rename` / `delete` resource operations
/// ([`EditHost::workspace_fs_jobs`]). They ride the same `LoopOp::Fs` seam `btv.fs`
/// does (that is what makes one implementation serve the local, daemon and browser
/// sessions), so the two landing sites — the native `LoopEvent::FsResult` arm and the
/// wasm `EditHost::fs_op_result` — tell an editor-owned job from a Lua promise by
/// `id >= WORKSPACE_FS_JOB_BASE` alone, exactly as `INTERNAL_WATCH_BASE` does for
/// watches. (Lua callback ids are monotonic from 1 and never approach `1 << 51`.)
///
/// Distinct from every other internal id — including the *timer* ones below, whose
/// events are a different [`LoopEvent`] variant and so could technically share a
/// number. They don't, deliberately: one id space with no overlaps is a property
/// worth keeping, and the watchdog timer that guards these very jobs
/// ([`WORKSPACE_FS_TIMEOUT_TIMER_ID`]) is exactly the case where a shared number
/// would read as a bug.
pub(crate) const WORKSPACE_FS_JOB_BASE: u64 = 1 << 51;

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

/// The loop id of the **progressive-parse resume** timer. Set above the shada flush
/// id (which is above the Lua callback ids and per-buffer watch ids), so a
/// [`LoopEvent::Timer`] carrying it is unambiguously the parse-resume wake. While a
/// large file's treesitter parse is still in flight (cancelled by the engine's
/// per-frame deadline), each redraw re-arms this one-shot to wake the run loop and
/// repaint — every repaint resumes the parse one budget further — until it converges.
#[cfg(feature = "native")]
pub(crate) const PARSE_RESUME_TIMER_ID: u64 = 1 << 50;

/// How soon to wake for the next parse-resume frame. Each frame already spends the
/// engine's parse budget (~one deadline) doing real work, so this delay is just a
/// yield back to the run loop between budgets — short enough to converge quickly,
/// non-zero so input/other events can interleave.
#[cfg(feature = "native")]
pub(crate) const PARSE_RESUME_DELAY: std::time::Duration = std::time::Duration::from_millis(5);

/// The loop id of the **workspace file-operation watchdog** — a one-shot re-armed
/// each time a workspace edit's `rename` / `delete` / `mkdir` is dispatched, and
/// disarmed when the queue drains. It exists because a server→client
/// `workspace/applyEdit` is a *request*: the server is blocked until bemtvi answers,
/// and the answer waits for these operations. An fs leg that never delivers a result
/// (a daemon link that goes quiet rather than erroring) would otherwise block that
/// server forever — the watchdog fails the stalled operation instead, so the server
/// gets a truthful `applied: false`.
pub(crate) const WORKSPACE_FS_TIMEOUT_TIMER_ID: u64 = 1 << 52;

/// The loop id of the **diagnostic-update debounce** — the one-shot that ends a
/// pause while you are still typing. A language server publishes after every
/// `didChange`, so each publish landing in insert mode is parked and this timer
/// re-armed (replacing the pending one, the shada-flush pattern); it fires once
/// typing has been quiet for `update_in_insert`'s interval, applying the newest
/// parked set without waiting for `InsertLeave`. Not armed at all when the interval
/// is `0` (apply at once) or `update_in_insert` is `false` (hold to `InsertLeave`).
pub(crate) const DIAG_DEBOUNCE_TIMER_ID: u64 = 1 << 53;

/// How long one workspace file operation may take before the watchdog gives up on it.
/// Generous — a single `rename` / `delete` / `mkdir` is milliseconds locally and one
/// round trip over a daemon, so this only fires when a leg has genuinely stopped
/// answering — and overridable through `$BEMTVI_WORKSPACE_FS_TIMEOUT_MS` (the test
/// hook, which is how the give-up path is exercised at all).
pub(crate) fn workspace_fs_timeout_ms() -> u64 {
    std::env::var("BEMTVI_WORKSPACE_FS_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30_000)
}

/// Whether `event` is the workspace file-operation watchdog firing (vs. a real Lua
/// timer / the shada or parse-resume wakes).
#[cfg(feature = "native")]
pub(crate) fn is_workspace_fs_timeout_timer(event: &LoopEvent) -> bool {
    matches!(event, LoopEvent::Timer { id, .. } if *id == WORKSPACE_FS_TIMEOUT_TIMER_ID)
}

/// Whether `event` is the diagnostic-update debounce firing (vs. a real Lua timer /
/// the shada or parse-resume wakes).
#[cfg(feature = "native")]
pub(crate) fn is_diag_debounce_timer(event: &LoopEvent) -> bool {
    matches!(event, LoopEvent::Timer { id, .. } if *id == DIAG_DEBOUNCE_TIMER_ID)
}

/// Whether `event` is the progressive-parse resume timer firing (vs. the shada
/// checkpoint or a real Lua timer / process / watch event).
#[cfg(feature = "native")]
pub(crate) fn is_parse_resume_timer(event: &LoopEvent) -> bool {
    matches!(event, LoopEvent::Timer { id, .. } if *id == PARSE_RESUME_TIMER_ID)
}

/// Whether `event` is the shada debounced-checkpoint timer firing (vs. a real Lua
/// timer / process / watch event the run loop hands to [`EditHost::on_loop_event`]).
#[cfg(feature = "native")]
pub(crate) fn is_shada_flush_timer(event: &LoopEvent) -> bool {
    matches!(event, LoopEvent::Timer { id, .. } if *id == SHADA_FLUSH_TIMER_ID)
}

/// Convert the Lua runtime's plugin-shada tuples (`namespace -> [(key, value)…]`)
/// into the [`PersistState`] carrier types for a flush. The runtime trades plain
/// tuples because `bemtvi-lua` doesn't depend on `bemtvi-core`; this is the single
/// seam that names both.
fn tuples_to_plugin_shada(data: Vec<(String, Vec<(String, String)>)>) -> Vec<PluginNamespace> {
    data.into_iter()
        .map(|(namespace, entries)| PluginNamespace {
            namespace,
            entries: entries
                .into_iter()
                .map(|(key, value)| PluginEntry { key, value })
                .collect(),
        })
        .collect()
}

/// The inverse of [`tuples_to_plugin_shada`]: a loaded [`PersistState`]'s plugin
/// data back into the runtime's tuple shape for [`LuaRuntime::plugin_shada_seed`] /
/// [`LuaRuntime::plugin_shada_merge`](bemtvi_lua::LuaRuntime::plugin_shada_merge).
fn plugin_shada_to_tuples(data: Vec<PluginNamespace>) -> Vec<(String, Vec<(String, String)>)> {
    data.into_iter()
        .map(|ns| {
            (
                ns.namespace,
                ns.entries.into_iter().map(|e| (e.key, e.value)).collect(),
            )
        })
        .collect()
}

/// The on-daemon shada sync handle for a `Remote`-config session (Approach A, per-instance
/// mirror): the daemon fs seam, the remote shada **directory**, and the sibling filenames
/// downloaded at connect (deleted on the daemon at clean-exit compaction). The session
/// uploads its *own* instance file into `remote_dir` after each flush.
///
/// `upload` serializes those uploads, and it has to be a lock rather than a flag because the
/// two writers want *different* things from it. They all target the same remote path, so two
/// in flight at once can land in either order — and the loser is whichever the daemon happens
/// to write last, not whichever is newest. A debounced checkpoint that finds the lock taken
/// **skips** (`try_lock_owned`): it is fire-and-forget so the editor tick never blocks on the
/// network, and the next checkpoint — or the final upload — carries the newer bytes anyway.
/// The clean-exit upload instead **waits** (`lock().await`): it carries the session's last
/// state and must be the last write to land, so it has to outlive any checkpoint still
/// crossing a slow link rather than race it.
#[cfg(feature = "native")]
struct RemoteShadaSync {
    fs: Arc<dyn HostFsAsync>,
    remote_dir: String,
    downloaded: Vec<String>,
    upload: Arc<tokio::sync::Mutex<()>>,
}

#[cfg(feature = "native")]
impl RemoteShadaSync {
    fn new(fs: Arc<dyn HostFsAsync>, remote_dir: String, downloaded: Vec<String>) -> Self {
        Self {
            fs,
            remote_dir,
            downloaded,
            upload: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

/// The shada (persistence) glue on the [`EditHost`]: load before the first frame,
/// the debounced live checkpoint, the clean-exit flush, and the per-message
/// debounce arming. All run **off** the editor input tick — the store's I/O never
/// blocks a keystroke. A no-op throughout when persistence is off (`shada: None`).
/// For a `Remote`-config session the staged store also syncs to the daemon after each
/// flush (Approach A — see [`RemoteShadaSync`]).
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
            Ok(mut state) => {
                // Pull the workspace session out before import_persist (which only seeds
                // marks/registers/history); the layout is rebuilt ONCE at boot, and only
                // when `--restore-session` asked for it — a `:rshada` re-read must not
                // re-spawn windows, and a plain namespaced launch must not rearrange.
                let session = state.session.take();
                // Plugin data lives in the Lua runtime, not the editor model — pull it
                // out (like the session) and seed the opted-in plugins' stores, so a
                // plugin's `get` returns last session's value on first read. (The wasm
                // front does the equivalent inside its own `EditHost::import_persist`.)
                let plugin_data = std::mem::take(&mut state.plugin_data);
                self.editor.import_persist(state);
                self.lua
                    .plugin_shada_seed(plugin_shada_to_tuples(plugin_data));
                if self.restore_session {
                    // Hold the session until the config has been sourced — the layout is
                    // rebuilt at `apply_pending_session_restore`, not here.
                    self.pending_session = session;
                }
            }
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

    /// Arm the one-shot parse-resume timer when the current buffer's treesitter
    /// parse is still in flight (a large file the engine couldn't finish in one
    /// frame's budget). The wake repaints, which resumes the parse a budget further
    /// and re-arms this if it's still going — so a big file colours in progressively
    /// even while the user sits idle. A no-op once the parse converges; routed
    /// through `fx` like the shada timer, so the wasm Worker could drive it too.
    pub(crate) fn arm_parse_resume_if_pending(&mut self) {
        let buffer = self.editor.current_buffer_id();
        if self.editor.ts_parse_pending(buffer) {
            self.fx.loop_command(LoopCommand::TimerStart {
                id: PARSE_RESUME_TIMER_ID,
                delay: PARSE_RESUME_DELAY,
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
        // Capture the layout only for a namespaced launch whose plugin opted in via
        // `btv.shada.save_layout(true)` — default off, so a plain session never does.
        if self.session_captures_layout() {
            snap.session = self.editor.export_session();
        }
        // Fold in the opt-in plugin shada (it lives in the runtime, not the editor).
        snap.plugin_data = tuples_to_plugin_shada(self.lua.plugin_shada_export());
        // A workspace-scoped session persists its `btv.wso` option overlay too (independent
        // of layout capture); the global store never carries per-workspace overrides.
        if self.workspace_session {
            snap.workspace_options = self.editor.workspace_options().clone();
        }
        // `'persisthistory'` routing: drop history from the primary store's snapshot when
        // the primary isn't the chosen history store; a workspace launch targeting the
        // global store writes history there instead (below).
        self.gate_primary_history(&mut snap);
        let mut flushed = false;
        if let Some(store) = self.shada.as_mut() {
            // A live checkpoint never compacts: deleting an absorbed sibling while we
            // hold our lock would hide its data from a concurrent launcher. Only the
            // clean-exit flush compacts.
            match store.flush(&snap, false) {
                Ok(()) => flushed = true,
                Err(e) => eprintln!("shada: checkpoint flush failed: {e}"),
            }
        }
        // A workspace launch targeting `global`: write history to the global store
        // instead (history-only, never compacting). A no-op when it isn't the target.
        self.global_history_flush();
        // For a `Remote`-config session, push the freshly-committed store to the daemon
        // (fire-and-forget — the editor tick never blocks on the network).
        if flushed {
            self.upload_shada_checkpoint();
        }
    }

    /// The clean-exit flush: write the final snapshot — *with* `exit_cursor` (where
    /// the cursor sits now), which the store turns into `'0` next launch — then let
    /// the store drop (releasing its file lock) so the next instance can merge this
    /// one's checkpoint. Best-effort; we're leaving.
    pub(crate) fn shada_flush_final(&mut self) {
        let mut snap = self.editor.export_persist();
        if self.session_captures_layout() {
            snap.session = self.editor.export_session();
        }
        snap.plugin_data = tuples_to_plugin_shada(self.lua.plugin_shada_export());
        // A workspace-scoped session persists its `btv.wso` option overlay too (independent
        // of layout capture); the global store never carries per-workspace overrides.
        if self.workspace_session {
            snap.workspace_options = self.editor.workspace_options().clone();
        }
        self.gate_primary_history(&mut snap);
        if let Some(store) = self.shada.as_mut() {
            // The clean exit: compact (delete the siblings absorbed at load) *after*
            // this final snapshot — which folds in their data — is durable.
            if let Err(e) = store.flush(&snap, true) {
                eprintln!("shada: final flush failed: {e}");
            }
        }
    }

    /// The single store history persists to this session, from the live
    /// `'persisthistory'` priority list + whether this is a workspace launch (see
    /// [`effective_history_scope`](bemtvi_core::effective_history_scope)).
    fn history_scope(&self) -> bemtvi_core::HistoryScope {
        bemtvi_core::effective_history_scope(
            &self.editor.global_options().persisthistory,
            self.workspace_session,
        )
    }

    /// Whether the primary store ([`shada`](Self::shada)) is the chosen history store:
    /// its scope is `Workspace` for a workspace launch, else `Global`. History rides the
    /// primary's normal flush iff the chosen scope matches; otherwise it routes to the
    /// separate global store (a workspace launch targeting `global`) or nowhere (`none`).
    fn primary_keeps_history(&self) -> bool {
        use bemtvi_core::HistoryScope::{Global, Workspace};
        match self.history_scope() {
            Workspace => self.workspace_session,
            Global => !self.workspace_session,
            bemtvi_core::HistoryScope::None => false,
        }
    }

    /// Drop the history fields from a primary-store snapshot when the primary isn't the
    /// chosen history store (so `none`, or a workspace launch targeting `global`, writes
    /// no history to the workspace store).
    fn gate_primary_history(&self, snap: &mut bemtvi_core::PersistState) {
        if !self.primary_keeps_history() {
            snap.ex_history.clear();
            snap.search_history.clear();
            snap.input_history.clear();
        }
    }

    /// Post-config: when a **workspace** launch targets the **global** store
    /// (`'persisthistory'` resolves to `global` — e.g. `"global"` or `"global,workspace"`),
    /// restore the shared global history into the rings and keep the store handle for the
    /// flush sites. Otherwise (the default `workspace`, a plain launch whose primary is
    /// already global, or `none`) drop the handle so nothing flushes to it. Runs after
    /// `init.lua` — the primary load (which restores the workspace history) is pre-config.
    pub(crate) fn init_global_history(&mut self) {
        let targets_global =
            self.workspace_session && self.history_scope() == bemtvi_core::HistoryScope::Global;
        if !targets_global {
            self.global_history = None;
            return;
        }
        let Some(store) = self.global_history.as_mut() else {
            return;
        };
        match store.load() {
            Ok(state) => self.editor.merge_persisted_history(
                state.ex_history,
                state.search_history,
                state.input_history,
            ),
            Err(e) => {
                self.editor
                    .echo(format!("shada: could not open global history store: {e}"));
                self.global_history = None;
            }
        }
    }

    /// Build the history-only snapshot for the global store (everything else default —
    /// the global store carries no marks / registers / session from a workspace launch).
    fn global_history_snapshot(&self) -> bemtvi_core::PersistState {
        let (ex_history, search_history, input_history) = self.editor.export_history();
        bemtvi_core::PersistState {
            ex_history,
            search_history,
            input_history,
            ..Default::default()
        }
    }

    /// Live-checkpoint the global history store (history-only, `compact = false`): it
    /// shares the global dir with plain sessions' full-state files, so compacting here
    /// would delete their marks / registers — only a plain global session compacts.
    pub(crate) fn global_history_flush(&mut self) {
        let snap = self.global_history_snapshot();
        if let Some(store) = self.global_history.as_mut() {
            if let Err(e) = store.flush(&snap, false) {
                eprintln!("shada: global history flush failed: {e}");
            }
        }
    }

    /// The global history store's clean-exit flush — still history-only and still
    /// **never compacting**, for the same reason.
    pub(crate) fn global_history_flush_final(&mut self) {
        let snap = self.global_history_snapshot();
        if let Some(store) = self.global_history.as_mut() {
            if let Err(e) = store.flush(&snap, false) {
                eprintln!("shada: global history final flush failed: {e}");
            }
        }
    }

    /// The staged store's own instance file as `(remote_target, bytes)` for an upload: the
    /// file [`current_path`](ShadaStore::current_path) points at, uploaded into the remote
    /// shada dir under its *own* name (the per-instance mirror). Snapshotted *after* a flush
    /// has committed it, so the bytes are a consistent redb file. `None` when remote shada
    /// is off, the store has no backing file, or the read fails (logged, never fatal).
    fn staged_shada_upload(&self) -> Option<(String, Vec<u8>)> {
        let sync = self.remote_shada.as_ref()?;
        let path = self.shada.as_ref()?.current_path()?;
        let name = path.file_name()?.to_str()?;
        let target = format!("{}/{}", sync.remote_dir, name);
        match std::fs::read(&path) {
            Ok(bytes) => Some((target, bytes)),
            Err(e) => {
                eprintln!("shada: could not read staged store for remote upload: {e}");
                None
            }
        }
    }

    /// Upload our instance file to the daemon after a checkpoint flush (Approach A),
    /// **fire-and-forget**: snapshot the bytes now (consistent — the flush just
    /// committed), then write them over the fs seam on a spawned task so the editor tick
    /// never blocks on the network. Holds [`RemoteShadaSync::upload`] for the write, and
    /// skips outright when it can't take it — a debounced checkpoint has nothing worth
    /// waiting for (the upload already in flight, or the next checkpoint, carries bytes at
    /// least as new). A no-op when remote shada is off.
    fn upload_shada_checkpoint(&self) {
        let Some(sync) = self.remote_shada.as_ref() else {
            return;
        };
        // Skip if any upload is still in flight — the next checkpoint (or the awaited
        // final upload) carries the latest bytes; this avoids pile-up and out-of-order
        // writes. The guard rides the spawned task, so the lock is held for the whole
        // write and released even if that task is dropped at exit.
        let Ok(guard) = sync.upload.clone().try_lock_owned() else {
            return;
        };
        let Some((target, bytes)) = self.staged_shada_upload() else {
            return; // `guard` drops here — nothing was written, so nothing to serialize.
        };
        let fs = sync.fs.clone();
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(e) = fs.write(target, bytes).await {
                eprintln!("shada: remote checkpoint upload failed: {e}");
            }
        });
    }

    /// The **awaited** clean-exit sync (Approach A, per-instance mirror): after
    /// [`shada_flush_final`](Self::shada_flush_final) wrote + locally compacted the staged
    /// store, upload our instance file to the daemon and *wait* for it (a fire-and-forget
    /// spawn could be dropped as the process exits, losing the last session's state), then
    /// delete the absorbed siblings on the daemon so the remote dir stays bounded by live
    /// sessions. Best-effort (failures logged; we're leaving). A no-op when remote shada is
    /// off or the store never loaded (nothing absorbed → nothing to remove).
    ///
    /// Takes [`RemoteShadaSync::upload`] and **waits**: a debounced checkpoint may still be
    /// crossing the link with an older snapshot of the same remote path, and two concurrent
    /// writes land in whichever order the daemon finishes them — so letting this one race
    /// would let the stale bytes win and silently lose the session's last state.
    pub(crate) async fn shada_upload_final(&self) {
        let Some(sync) = self.remote_shada.as_ref() else {
            return;
        };
        if self.shada.is_none() {
            return;
        }
        let _guard = sync.upload.lock().await;
        if let Some((target, bytes)) = self.staged_shada_upload() {
            // Bounded: the daemon link may be half-dead at exit (TCP still up, the
            // daemon hung), and an unbounded await would hang the process's exit
            // forever on a best-effort write. Best-effort — we're leaving.
            if let Err(e) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                sync.fs.write(target, bytes),
            )
            .await
            {
                eprintln!("shada: final remote upload failed: {e}");
            }
        }
        // Mirror the local clean-exit compaction onto the daemon: the siblings we
        // downloaded were absorbed into our just-uploaded file, so delete them remotely.
        // Bounded like the upload above.
        for name in &sync.downloaded {
            let target = format!("{}/{}", sync.remote_dir, name);
            if let Err(e) =
                tokio::time::timeout(std::time::Duration::from_secs(5), sync.fs.remove(target))
                    .await
            {
                eprintln!("shada: remote compaction (remove {name}) failed: {e}");
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
        // Capture the layout under the same gate as the checkpoint and the final
        // flush — a snapshot without one makes the store *clear* the SESSION row,
        // so an ungated `:wshada` would silently erase the persisted workspace
        // session (`--restore-session` would find no layout after the next crash).
        if self.session_captures_layout() {
            snap.session = self.editor.export_session();
        }
        snap.plugin_data = tuples_to_plugin_shada(self.lua.plugin_shada_export());
        // A workspace-scoped session persists its `btv.wso` option overlay too (independent
        // of layout capture); the global store never carries per-workspace overrides.
        if self.workspace_session {
            snap.workspace_options = self.editor.workspace_options().clone();
        }
        // `:wshada` is not an exit — don't compact (we're still live and locked).
        let result = self.shada.as_mut().unwrap().flush(&snap, false);
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
            Ok(mut state) => {
                // Plugin data is the runtime's, not the editor's — merge it into the
                // live stores (`replace` overwrites a conflicting live key, else fills
                // only an unset one), mirroring `apply_persist`'s own rule.
                let plugin_data = std::mem::take(&mut state.plugin_data);
                self.editor.apply_persist(state, replace);
                self.lua
                    .plugin_shada_merge(plugin_shada_to_tuples(plugin_data), replace);
            }
            Err(e) => self.editor.echo(format!("E: shada read failed: {e}")),
        }
    }
}

/// A finished `:TSInstall` job: the requested language and the install result
/// (the report, or a loud error). Delivered from the blocking worker to the
/// server's `select!` loop.
#[cfg(feature = "native")]
type InstallOutcome = (String, anyhow::Result<bemtvi_ts::install::InstallReport>);

/// A finished off-tick grammar load on its way back to the editor thread: the
/// language, and the engine's own opaque result payload (only the engine reads it —
/// see [`HostEffects::ts_load_grammar`](crate::edithost::HostEffects::ts_load_grammar)).
#[cfg(feature = "native")]
type GrammarOutcome = (String, Box<dyn std::any::Any + Send>);

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
async fn run_io<R, W>(reader: R, writer: W, mut init: ServerInit) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, mut incoming) = connect(reader, writer);

    // A `--workspace DIR` launch cds into the workspace root **now**, at boot — before the
    // editor opens the startup file, seeds its `DirState`, or restores the session. Because
    // the cwd decision is a CLI flag (not the old `'workspacecwd'` Lua option, which was only
    // known after `init.lua` and forced a late cd plus path-reconciliation hacks), the cwd is
    // correct from the first instruction: a relative startup file and the session's relative
    // buffer paths just resolve against the workspace root. `--workspace-no-cwd` clears the
    // flag; a daemon session cds on the daemon, so its local half leaves this off.
    if init.workspace_cwd {
        if let Some(dir) = &init.workspace_dir {
            if let Err(e) = std::env::set_current_dir(dir) {
                eprintln!("bemtvi: could not cd into workspace {dir:?}: {e}");
            }
        }
    }

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
    // Grammar loads go through the run loop's worker rather than the tick that first
    // needs the language — the server drives the other half of that handshake
    // (`dispatch_grammar_requests` → `ts_load_grammar` → `on_grammars_loaded`).
    let mut engine = bemtvi_ts::Engine::new(bemtvi_ts::data_dir());
    engine.defer_loads(true);
    editor.set_syntax_engine(Box::new(engine));
    // The `"+` / `"*` registers route through an injected clipboard provider.
    // `System` resolves a real host clipboard tool (best effort), falling back to
    // the client terminal's OSC 52 clipboard when this machine has none that could
    // reach the user; `Custom` is a caller-supplied fake (tests); `Disabled`
    // installs nothing and lets `"+` error loudly. The OSC 52 provider is *armed*
    // rather than installed — it needs a client that can emit the escape, which is
    // only known once one attaches (`btv_ui_attach`).
    let mut osc52: Option<clipboard::Osc52Handle> = None;
    match init.clipboard {
        ClipboardProvider::System => match clipboard::SystemClipboard::detect() {
            Some(cb) => editor.set_clipboard(Box::new(cb)),
            // Nothing on this host can reach a clipboard the user would see — a
            // bare server, or (the common case) an ssh session, where a tool would
            // only ever set the *remote* machine's clipboard. Fall back to asking
            // the terminal itself.
            None => osc52 = Some(clipboard::Osc52Handle::default()),
        },
        ClipboardProvider::Osc52 => osc52 = Some(clipboard::Osc52Handle::default()),
        ClipboardProvider::Custom(cb) => editor.set_clipboard(cb),
        ClipboardProvider::Disabled => {}
    }
    // System-plugin tier (§A): splice the client-seeded system-plugin dirs into the
    // runtimepath right AFTER the config dir, so `seed_package_path` (which orders
    // `package.path` by runtimepath position) places their `lua/` modules ahead of any
    // managed plugin but behind the user's own config — a system plugin can never hijack
    // a config module name. Deduped against dirs already present. Captured as
    // `(name, dir)` pairs for the early system-load phase below; the dirs are sourced
    // there (before `init.lua`) and skipped by the later `source_plugins` pass, so each
    // loads exactly once.
    let system_specs: Vec<(String, PathBuf)> = init
        .system_plugins
        .iter()
        .map(|s| (s.name.clone(), s.dir.clone()))
        .collect();
    {
        let mut insert_at = init
            .config_dir
            .as_ref()
            .and_then(|c| init.runtimepath.iter().position(|p| p == c))
            .map_or(0, |i| i + 1);
        for (_, dir) in &system_specs {
            if !init.runtimepath.contains(dir) {
                init.runtimepath.insert(insert_at, dir.clone());
                insert_at += 1;
            }
        }
    }
    let lua =
        LuaRuntime::new(init.runtimepath).map_err(|e| anyhow::anyhow!("lua init failed: {e}"))?;
    // The option / colorscheme / filetype catalogs core owns are installed by
    // `EditHost::new` below — the one construction site the native server and the wasm
    // edit-host share — so the two legs cannot drift on them again.
    // Seed the layout-capture opt-in before any config runs. `--workspace` turns it on so
    // a directory session captures its window/tab layout without needing a plugin to call
    // `btv.shada.save_layout`; a plain `--shada-namespace` launch leaves it off (the config
    // / a plugin decides). A config may still toggle it afterwards either way.
    if init.session_save_layout {
        lua.set_session_save_layout(true);
    }
    // Seed the shada namespace + workspace root that `btv.shada.namespace()` / `btv.workspace`
    // report. These are runtime values (not env) because a daemon session derives them from
    // the daemon's cwd after it connects — the binary can't stamp them up front.
    lua.set_workspace_identity(init.shada_namespace.clone(), init.workspace_dir.clone());
    // Where the event-loop actor runs off-tick `btv.fs` ops (the ONLY fs surface — there
    // are no synchronous editor-thread fs callers anymore). A bare/local session runs
    // them against a local `StdLuaFs` on the actor's blocking pool; the edit-host split
    // injects a daemon `RemoteFsJobs` so the whole job crosses over `luafs_op` and runs
    // remote. Either way it's off the editor tick — nothing blocks.
    let fs_backend = match init.fs_jobs {
        Some(remote) => evloop::FsBackend::Remote(remote),
        None => evloop::FsBackend::Local(Arc::new(bemtvi_lua::StdLuaFs::new())),
    };
    // Where the actor runs off-tick `btv.http.fetch` requests: a local `ureq` round-trip
    // (bare/local session), or the daemon's `http_op` leg (edit-host split — the request
    // runs on the daemon, which owns the network). Like `fs_backend`, off the editor tick.
    let http_backend = match init.http_jobs {
        Some(remote) => evloop::HttpBackend::Remote(remote),
        None => evloop::HttpBackend::Local,
    };
    // Where the actor runs off-tick `btv.git.*` ops: the local gix engine on the blocking
    // pool (bare/local session), or the daemon's `git_op` leg (a daemon session — git runs
    // where the files are). Like `fs_backend`, off the editor tick.
    let git_backend = match init.git_jobs {
        Some(remote) => evloop::GitBackend::Remote(remote),
        None => evloop::GitBackend::Local,
    };
    // Language servers are spawned through this transport — real local children by
    // default, or an injected daemon-backed tunnel. Rebuilt here, on the server thread,
    // into the shared `Arc<dyn LspTransport>` the manager holds (`ServerInit` carried it
    // `Send` across the thread boundary; the two-step drops `Send` by unsize coercion, as
    // the `host_proc` rebuild does). `None` keeps the default local spawn.
    let (lsp, mut lsp_events) = match init.lsp_transport {
        Some(transport) => {
            let transport: Arc<dyn bemtvi_lsp::LspTransport + Send> = Arc::from(transport);
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
    // The LOCAL twins of the session's proc/fs seams, for `local`-flagged ops — the
    // `btv.plugins` manager's git + discovery. Always the real local disk / local child
    // process, so plugins are cloned and discovered locally (they load into this local
    // Lua VM via the local runtimepath) even in a daemon session, where `host_proc`/
    // `fs_backend` above route to the remote. In a bare/local session these resolve to the
    // same local disk the session already uses, so behavior is unchanged.
    // See `docs/plans/2026-07-03-remote-aware-plugin-manager.md`.
    let local_host_proc: Arc<dyn HostProc> = Arc::new(StdHostProc);
    let local_fs_backend = evloop::FsBackend::Local(Arc::new(bemtvi_lua::StdLuaFs::new()));
    // The plugin manager's `btv.git_local` always runs on the local disk (its repos live
    // there), so the local git twin is always `Local` — even in a daemon session.
    let local_git_backend = evloop::GitBackend::Local;
    // Where a streaming `btv.fs.watch` is armed: the daemon's `luafs_watch` leg in a daemon
    // session (its files live there), else the actor's local `notify` backend. The daemon's
    // change pushes are forwarded into one always-present channel (the `watch_rx` idiom) so
    // the `select!` arm below is valid — and idle — in a local session too.
    let fs_watch = init.fs_watch;
    let (remote_fs_watch_tx, mut remote_fs_watch_rx) = unbounded_channel::<evloop::LoopEvent>();
    if let Some(mut rx) = fs_watch.as_ref().and_then(|w| w.take_events()) {
        let tx = remote_fs_watch_tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        });
    }
    let (evloop, mut loop_events) = EventLoop::new(evloop::LoopBackends {
        host_proc,
        fs: fs_backend,
        http: http_backend,
        git: git_backend,
        fs_watch,
        local_host_proc,
        local_fs: local_fs_backend,
        local_git: local_git_backend,
    });
    // The terminal actor: owns the local PTYs `:terminal` spawns, streaming their
    // output back on `term_events`. Lazily started on the first open (the `EventLoop`
    // pattern), so a session with no terminal spawns nothing.
    let (terminals, local_term_events) = terminal::native::TerminalManager::new();
    // A daemon session routes `:terminal` to the remote instead: take its inbound
    // `TermEvent` stream (decoded from the daemon's `term_data`/`term_exit` pushes) so the
    // run loop's `on_term_events` arm consumes a remote terminal exactly like a local one,
    // and hand the command seam to `NativeEffects` below. A local session keeps the local
    // actor's stream. Either way the arm's type is one `Receiver<TermEvent>`.
    let mut host_term = init.host_term;
    let mut term_events = match host_term.as_mut().and_then(|t| t.take_events()) {
        Some(remote_events) => remote_events,
        None => local_term_events,
    };
    // `:TSInstall` runs the fetch+compile off-thread (`spawn_blocking`); results
    // come back here and are applied on the one server thread.
    let (install_tx, mut install_events) = unbounded_channel::<InstallOutcome>();
    // A grammar the engine asked for is loaded on a `spawn_blocking` worker (compiling
    // its queries is hundreds of ms); the loaded grammar comes back here and is
    // installed on the one server thread.
    let (grammar_tx, mut grammar_events) = unbounded_channel::<GrammarOutcome>();
    // Off-tick `:w`s (the daemon save path) push their bytes over the wire from a
    // spawned task; the finished write comes back here and finalizes on the one
    // server thread. Idle for a local/bare session (no daemon fs → no off-tick saves).
    let (save_done_tx, mut save_done_rx) = unbounded_channel::<save::SaveDone>();
    // The off-tick `:cd` delivery: `ex_chdir` enqueues a daemon `fs_chdir` on a spawned
    // task; the resolved canonical dir (or `E344`) comes back here and `apply_chdir`
    // installs it into `DirState` on the one server thread. Idle for a local session
    // (`:cd` resolves synchronously through the local disk there).
    let (chdir_done_tx, mut chdir_done_rx) = unbounded_channel::<cwd::ChdirDone>();
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
    // The reconnecting daemon link's status feed: the supervisor publishes
    // Connected/Reconnecting/Disconnected on a `watch`, and the `daemon_status_rx` arm
    // reflects each into the editor + Lua off the tick. A local/bare/one-shot session has no
    // link, so a placeholder channel keeps the arm valid but idle (its sender, kept bound for
    // the whole loop, never sends — so `changed()` pends forever).
    let (_daemon_status_keep, mut daemon_status_rx) = match init.daemon_link.as_ref() {
        Some(link) => (None, link.subscribe()),
        None => {
            let (tx, rx) = tokio::sync::watch::channel(daemon::DaemonStatus::Connected);
            (Some(tx), rx)
        }
    };

    // The native outbound-effect seam: the client wire ([`Rpc`]), the event-loop actor
    // ([`EventLoop`]), the off-tick daemon fs (read/write/watch + the `open_tx` /
    // `save_done_tx` deliveries), and the LSP command sink ([`LspManager`]) the editor
    // tick fires through. The wasm build (slice 5b) swaps a JS-interop implementor here.
    // Clone the daemon fs handle (an `Arc`) for the remote-shada sync *before* it is moved
    // into `NativeEffects` below — the staged shada store uploads its bytes back over this
    // same seam after each flush (Approach A). `None` for a local/bare session.
    let shada_fs = host_fs_async.clone();
    let mut host = EditHost::new(
        editor,
        lua,
        Box::new(NativeEffects::new(
            rpc,
            evloop,
            host_fs_async,
            open_tx,
            save_done_tx,
            chdir_done_tx,
            lsp,
            install_tx,
            grammar_tx,
            terminals,
            host_term,
        )),
    );
    // The terminal-backed clipboard, if this session resolved to one: held until a
    // client attaches and says it can emit the escape (`btv_ui_attach`).
    host.osc52 = osc52;
    // The two capabilities `new` defaults but the native session injects: the
    // persistence store (loaded before the first frame) and the optional fake mouse
    // clock (multi-click timing in tests).
    host.shada = init.shada;
    host.global_history = init.global_shada;
    host.workspace_session = init.workspace_session;
    host.restore_session = init.restore_session;
    // For a `Remote`-config session, pair the remote shada target with the daemon fs
    // handle so checkpoints + the exit flush sync the staged store to the daemon. Both
    // are present together (a `Remote` session has a daemon fs); either missing → no sync.
    host.remote_shada = match (init.remote_shada, shada_fs) {
        (Some(rs), Some(fs)) => Some(RemoteShadaSync::new(fs, rs.remote_dir, rs.downloaded)),
        _ => None,
    };
    host.mouse_clock = init.mouse_clock;
    host.mono_clock = init.mono_clock;
    // The `--lua` one-shot routes `print` / echo / error output to the real stdout/stderr
    // (no UI is attached to show the message line). Safe only here: this session's client
    // wire is an in-process duplex, so the process stdout is free.
    host.lua_stdio = init.lua_stdio;
    // The reconnecting daemon link handle (status + `:reconnect`/`:disconnect`), moved onto
    // the host now that `daemon_status_rx` is already subscribed above. `None` for a
    // local/bare/one-shot session.
    host.daemon_link = init.daemon_link;
    // In a daemon session the one true cwd is the *daemon's*, not the local process's
    // — seed `DirState` from it so `:pwd` / `vim.fn.getcwd` report the remote dir and
    // `:cd` moves it over the wire (`docs/plans/2026-06-23-remote-cwd.md`). No local
    // dirs exist yet at startup, so replacing the whole state is safe. A local session
    // leaves the process-cwd seed `EditHost::new` installed.
    if let Some(cwd) = init.remote_cwd {
        host.dirs = cwd::DirState::new(cwd);
        // This session's `btv.fs` runs against the daemon; mark it so `apply_loop_op`
        // rebases relative paths against `DirState` before they cross the wire.
        host.remote_cwd_seeded = true;
    }
    // A leading `~` in a file argument (`:e ~/x`) must expand against the daemon's home,
    // not the local process's — the read lands on the daemon. Seed it from the same
    // `config_bundle` handshake that carried the cwd above.
    if let Some(home) = init.remote_home {
        host.editor.set_remote_home(home);
    }
    // Publish the seeded effective dir into the `btv._cwd` mirror so `vim.fn.getcwd`
    // reads the authoritative cwd from the very first config line (a remote session's
    // `init.lua` calling `getcwd()` must see the daemon's dir, not the local process's).
    host.publish_cwd_mirror();

    // No built-in keymap defaults are installed natively any more: the LSP keys
    // (`gd`/`gD`/`gr`/`K`/`<C-k>`) are installed buffer-local by `prelude/lsp.lua` on
    // `LspAttach`, and the completion triggers (`<C-Space>`/`<C-x><C-o>`) by
    // `btv.complete.setup` — both as ordinary overridable Lua maps. The native-action
    // keymap rung (`BuiltinAction`/`NativeDefault`) was retired with them.

    // Seed the current-buffer snapshot before sourcing config, so a buffer-local
    // map declared with `buffer = 0` (or `nvim_create_autocmd`'s `buffer = 0`)
    // resolves to the real startup buffer rather than the default `0` — the buffer
    // already exists at config time, matching neovim. Carrying the filetype too
    // lets a `vim.lsp.enable(...)` in `init.lua` start a server for it. Lifecycle
    // emission refreshes it again before each autocmd fires; this makes it valid
    // earlier.
    {
        let buf = host.editor.current_buffer_id();
        let name = host.editor.display_name(buf);
        let ft = filetype_of(host.editor.buffer().path.as_deref()).unwrap_or("");
        let _ = host.lua.set_buf_snapshot(buf.0, &name, ft);
    }
    // Seed the buffer mirror too, so `init.lua` can read buffer lines / the cursor.
    host.push_buf_mirror();

    // Load the shada (persistence) store before *anything* fires the startup
    // buffer's lifecycle events — sourcing `init.lua` settles via `run_pending`,
    // which emits `BufReadPost`, and a `BufReadPost` handler (e.g. the built-in
    // `restorecursor` jump to the `"` mark) must see the restored per-file marks.
    // It recency-merges + compacts sibling stores and seeds this session's
    // registers / marks / history / jumplist; so `init.lua` and a `VimEnter`
    // plugin also see the restored state. A no-op when persistence is disabled
    // (`shada: None`, the test default); a store that won't load is surfaced and
    // dropped (the editor runs on without persistence rather than dying). The store
    // lives on `host` from here, so the debounced checkpoint and the exit flush both
    // reach it through the seam.
    host.shada_load();

    // System-plugin tier (§A): load the client-seeded system plugins BEFORE the
    // recommended set / `client_init_lua` / `init.lua`, so a connector (or any system
    // plugin) is guaranteed present before any config line runs. Their dirs are already
    // on the runtimepath (spliced after the config dir above), so `require` and their
    // `colors/`/`queries/`/`lsp/` resolve; here we register them in the `btv.plugins`
    // tier registry and source their `plugin/` scripts synchronously — the manager's
    // sourcing loop, reused. Sourcing reads the LOCAL disk (`std::fs`), so a system
    // plugin loads locally even in a daemon session, consistent with the remote-aware
    // manager. The later `source_plugins` pass skips these dirs (it queries the live
    // tier registry), so each loads exactly once.
    if !system_specs.is_empty() {
        let list = system_specs
            .iter()
            .map(|(name, dir)| format!("{{ name = {name:?}, dir = {:?} }}", dir.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(", ");
        if let Err(e) = host
            .lua
            .exec(&format!("btv.plugins._register_system({{ {list} }})"))
        {
            host.editor
                .echo(format!("bemtvi: system-plugin registration failed: {e}"));
        }
        let dirs: Vec<PathBuf> = system_specs.iter().map(|(_, dir)| dir.clone()).collect();
        host.source_specific_plugins(&dirs);
    }

    // Offer the built-in default recommended set on a fresh setup — BEFORE init.lua,
    // so a config's own `btv.plugins.recommend{...}` (or declaring any plugin) still
    // wins. The interactive binary opts in; tests leave this off and stay hermetic.
    if init.offer_default_recommended {
        let _ = host.lua.exec("btv.plugins._use_default_recommended()");
    }
    // Turn on command-line completion (`:`+<Tab>) by default for the interactive
    // binary — BEFORE init.lua, so a config's own `btv.cmdline_complete.setup{ ... }`
    // (e.g. to toggle the docs pane) still wins. Tests leave this off and stay
    // hermetic. The queued setup drains on the next `apply_lua_effects` (init.lua's,
    // or the first keypress's) — well before any `:`+<Tab> can be typed.
    if init.cmdline_complete_default {
        let _ = host.lua.exec("btv.cmdline_complete.setup{}");
    }
    // Run the client's startup Lua (the GUI registers its `:connect` / `:workspace`
    // virtual commands here) — BEFORE init.lua, so a config can still override it. A
    // chunk error is surfaced loudly rather than swallowed.
    if let Some(code) = &init.client_init_lua {
        if let Err(e) = host.lua.exec(code) {
            host.editor.echo(format!("client init lua: {e}"));
        }
    }
    // Mirror the daemon's installed tree-sitter parsers **lazily**: set up a `FileType`
    // autocmd (in Lua — `btv._remote_ts_autoinstall`) that `:TSInstall`s a remote
    // language the first time a buffer of that filetype opens. Parsers are native
    // artifacts, never fetched over the wire; the compile runs in the background. We
    // filter to languages not already installed here (the daemon can't know the client's
    // set), so the Lua side only handles per-session dedup. Registered BEFORE `init.lua`
    // (like the other built-in setups), so it's in place when the startup buffer's own
    // `FileType` fires during config sourcing. Empty for a local session.
    set_up_remote_ts_autoinstall(&mut host, &init.ts_autoinstall);

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

    // Fold in the global history store, now that config has set `'persisthistory'`
    // (sourced above; `shada_load` ran before it). A workspace launch whose option
    // includes `global` merges the shared global history into its rings here.
    host.init_global_history();

    // The startup file arg was opened (as text) at editor construction, before the
    // config above ran — so a config that turned on `'imagepreview'` couldn't affect
    // that first open. Reconcile it now: if the startup buffer is an image and
    // previews are on, reload it as a preview, matching what `:e` would do. (Runs
    // before the lifecycle seed below, so the events fire over the final buffer.)
    host.editor.reconcile_image_preview();

    // Rebuild the workspace session's layout, now that the config has configured the
    // startup window/buffer every restored one is minted from (see
    // `apply_pending_session_restore` for why this can't run at `shada_load`). A no-op
    // outside a `--restore-session` boot.
    host.apply_pending_session_restore();

    // Dispatch any persisted-view restores the session reopened: the layout came back
    // just above with each persisted `btv.view` slot reserved as a placeholder; now that
    // `init.lua` + the plugins are sourced (so their `btv.view.on_restore` handlers are
    // registered), hand each reserved slot to its owning plugin to adopt, and collapse any
    // slot left unclaimed. Done BEFORE the window-set seed below so the placeholder churn
    // fires no spurious `WinNew`/`WinClosed`.
    host.restore_persisted_views();

    // Startup seed: the initial buffer and the config's autocmds both exist now,
    // so fire the first buffer's lifecycle events (`BufReadPost`→`FileType`→
    // `BufEnter` for a file arg, `BufEnter` alone for the bare `[No Name]`).
    // Pre-seed the window set so the first window doesn't fire `WinNew` (neovim
    // skips it for the initial window); `last_window_id` stays `None` so the
    // first `WinEnter` still fires alongside `BufEnter`, the window analogue.
    // Cross-tab, like the diff's own enumeration: a session restore has already built
    // every tab by now, and its background-tab windows must not read as new.
    host.known_windows = host.editor.all_window_ids();
    host.last_window_rects = Some(host.window_rects_snapshot());
    // Pre-seed the tab set so the initial tab doesn't fire `TabNew` (neovim, like
    // for the first window, doesn't); `last_tab_id` stays `None` so a later switch
    // still fires the first `TabEnter`/`TabLeave` pair.
    host.known_tabs = host.editor.tab_ids();
    // Seed the buffer set too, so the startup buffer isn't seen as "newly gone"
    // and a never-deleted buffer never triggers a spurious cleanup. Arm `BufAdd`
    // from here on: the startup buffer is now baseline, so only later additions fire.
    host.known_buffers = host.editor.buffer_ids();
    host.startup_bufs_seeded = true;
    host.emit_lifecycle_events();
    host.run_pending();
    // The startup VimEnter point has passed: `v:vim_did_enter` is now 1, so a
    // plugin that gates "the editor has finished starting" reads it as true.
    let _ = host.lua.set_vim_did_enter(true);
    // Fire the VimEnter autocmd (the package manager's first-run prompt hooks it).
    host.fire_vim_enter();

    // Last word on focus: re-assert the layer the restored session was quit from (`"main"`
    // or a dock). The layout came back focused on the main layer, but a dock plugin that
    // re-adopts its window during `restore_persisted_views` / `VimEnter` can yank focus into
    // the dock — so a session quit from the main area would reopen with the cursor stranded
    // there. This runs after both, so it restores where you actually left off. A no-op
    // unless a focus layer was captured.
    if host.editor.finalize_session_focus() {
        host.apply_lua_effects();
        host.run_pending();
    }

    // Reflect the initial daemon link status (Connected) into the editor + Lua so a
    // statusline shows it from the first frame; subsequent changes arrive on the
    // `daemon_status_rx` arm below.
    if host.daemon_link.is_some() {
        let status = *daemon_status_rx.borrow();
        host.on_daemon_status(status);
    }

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
                host.on_client_message(message).await;
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
            // A streaming `btv.fs.watch` change from the **daemon** (`luafs_change`): the
            // same `LoopEvent::FsEvent` the local watcher produces, so it lands in the same
            // handler — a watch behaves identically whether its files are here or across
            // the wire. Idle for a local/bare session (nothing ever sends here).
            Some(event) = remote_fs_watch_rx.recv() => {
                host.on_loop_events(event, &mut remote_fs_watch_rx)
            }
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
            // An off-tick grammar load finished: install it into the engine and repaint,
            // so the buffer that opened before its language was ready colours in.
            Some(loaded) = grammar_events.recv() => host.on_grammars_loaded(loaded, &mut grammar_events),
            // An off-tick `:w` finished on the daemon (the save path): finalize the
            // buffer's saved-state and replay any deferred `:wq`/`:x` quit. A replayed quit
            // may ask the editor to exit — caught by the post-`select!` quit funnel below.
            Some(done) = save_done_rx.recv() => host.on_save_dones(done, &mut save_done_rx),
            // An off-tick `:cd` finished on the daemon (`fs_chdir`): install the resolved
            // canonical dir into `DirState` (or echo its `E344`). Idle for a local session,
            // where `:cd` resolves synchronously.
            Some(done) = chdir_done_rx.recv() => host.on_chdir_dones(done, &mut chdir_done_rx),
            // The daemon's watch leg pushed a file change (`HostWatch`): reconcile it off
            // the editor tick. Idle for a local/bare session (nothing ever sends here).
            Some(ev) = watch_rx.recv() => host.on_watch_events(ev, &mut watch_rx),
            // The reconnecting daemon link changed state (Connected/Reconnecting/
            // Disconnected): reflect it into the editor + Lua off the tick — the
            // `btv.daemon.status()` mirror, the `User DaemonStatusChanged` autocmd, and a
            // "run :reconnect" message once the auto-retry budget is spent. Idle for a
            // local/bare/one-shot session (the placeholder sender never sends).
            Ok(()) = daemon_status_rx.changed() => {
                let status = *daemon_status_rx.borrow_and_update();
                host.on_daemon_status(status);
            }
        }
        // Single quit funnel: whichever arm just ran may have completed the (possibly async)
        // gated exit sequence — a `:qa` typed at the client, its `:wqa` write batch acking,
        // or a timer settling an `ExitPre`/`VimLeavePre` handler's promise. Check once, here,
        // so the break doesn't depend on which arm the settle happened to land on. `quitting`
        // notifies the client (`bemtvi_exit`) exactly once before we break.
        if host.quitting() {
            break;
        }
    }
    // The loop has exited (quit or client disconnect): flush the final snapshot to
    // this instance's store, then drop it (releasing the file lock) so the next
    // instance can merge this one's clean checkpoint. Unlike the debounced live
    // checkpoint, this *clean-exit* flush carries `exit_cursor` (where the cursor sits
    // now) — the store turns it into `'0` on the next launch, so `'0` only ever
    // reflects a clean exit. Best-effort — we're leaving.
    host.shada_flush_final();
    // The global history store's own clean-exit flush (history-only, never compacting).
    host.global_history_flush_final();
    // For a `Remote`-config session, push the final store to the daemon and **await** it
    // (a fire-and-forget spawn could be dropped as the process exits, losing the last
    // session's state). Best-effort; a no-op for a local-shada session.
    host.shada_upload_final().await;
    Ok(())
}

/// The per-leg daemon server tasks for one connection — or, on a multi-stream transport,
/// one *stream* of a connection — plus the demux that routes inbound methods to them. A
/// leg is spawned only for the [`LegGroup`](daemon::LegGroup)s passed to [`spawn`]: a
/// single-stream transport (ssh/stdio, the in-process test duplex) spawns **all four**
/// groups over one shared [`Rpc`]; a multi-stream transport (QUIC/WebTransport) spawns one
/// group per stream over that stream's own [`Rpc`]. Either way each leg is the *same*
/// connection-agnostic `serve_*_daemon_on` core, so a file/process/server behaves
/// identically however its bytes were carried.
///
/// [`spawn`]: DaemonLegs::spawn
#[cfg(feature = "native")]
struct DaemonLegs {
    fs: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    proc: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    /// The duplex `btv.process` leg (`dproc_*`) — a DAP / framed-protocol transport. Rides
    /// the Proc group's stream alongside `proc`.
    dproc: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    /// The `btv.socket` TCP leg (`sock_*`) — a DAP `type="server"` adapter transport, also on
    /// the Proc stream.
    sock: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    term: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    lsp: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    luafs: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    luafs_watch: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    /// The `btv.http` leg (`http_op`) — a request/response per fetch, run daemon-side. Rides
    /// the Control group's stream alongside `luafs`/`fs`/`config`.
    http: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    /// The `btv.git` leg (`git_op`) — a request/response per op, run daemon-side (git runs
    /// where the files are). Rides the Control group's stream alongside `http`/`luafs`.
    git: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    config: Option<tokio::sync::mpsc::UnboundedSender<Incoming>>,
    handles: Vec<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

#[cfg(feature = "native")]
impl DaemonLegs {
    /// Spawn the leg tasks for every group in `groups`, each writing back through a clone
    /// of `rpc`, and return the senders the demux routes inbound messages onto. The daemon
    /// backs every leg with the same `Std*` impl the local server uses.
    fn spawn(groups: &[daemon::LegGroup], rpc: &Rpc) -> Self {
        use bemtvi_lua::StdLuaFs;
        use daemon::LegGroup;
        use tokio::sync::mpsc::unbounded_channel;

        let mut legs = DaemonLegs {
            fs: None,
            proc: None,
            dproc: None,
            sock: None,
            term: None,
            lsp: None,
            luafs: None,
            luafs_watch: None,
            http: None,
            git: None,
            config: None,
            handles: Vec::new(),
        };
        for &group in groups {
            match group {
                LegGroup::Control => {
                    let (fs_tx, fs_rx) = unbounded_channel();
                    let (luafs_tx, luafs_rx) = unbounded_channel();
                    let (luafs_watch_tx, luafs_watch_rx) = unbounded_channel();
                    let (http_tx, http_rx) = unbounded_channel();
                    let (git_tx, git_rx) = unbounded_channel();
                    let (config_tx, config_rx) = unbounded_channel();
                    legs.handles.push(tokio::spawn(daemon::serve_fs_daemon_on(
                        rpc.clone(),
                        fs_rx,
                        Box::new(StdHostFs),
                    )));
                    legs.handles
                        .push(tokio::spawn(daemon::serve_luafs_daemon_on(
                            rpc.clone(),
                            luafs_rx,
                            Box::new(StdLuaFs::new()),
                        )));
                    legs.handles
                        .push(tokio::spawn(daemon::serve_luafs_watch_daemon_on(
                            rpc.clone(),
                            luafs_watch_rx,
                        )));
                    legs.handles.push(tokio::spawn(daemon::serve_http_daemon_on(
                        rpc.clone(),
                        http_rx,
                    )));
                    legs.handles.push(tokio::spawn(daemon::serve_git_daemon_on(
                        rpc.clone(),
                        git_rx,
                    )));
                    legs.handles
                        .push(tokio::spawn(daemon::serve_config_daemon_on(
                            rpc.clone(),
                            config_rx,
                        )));
                    legs.fs = Some(fs_tx);
                    legs.luafs = Some(luafs_tx);
                    legs.luafs_watch = Some(luafs_watch_tx);
                    legs.http = Some(http_tx);
                    legs.git = Some(git_tx);
                    legs.config = Some(config_tx);
                }
                LegGroup::Proc => {
                    let (proc_tx, proc_rx) = unbounded_channel();
                    let (dproc_tx, dproc_rx) = unbounded_channel();
                    let (sock_tx, sock_rx) = unbounded_channel();
                    legs.handles.push(tokio::spawn(daemon::serve_proc_daemon_on(
                        rpc.clone(),
                        proc_rx,
                    )));
                    legs.handles
                        .push(tokio::spawn(daemon::serve_dproc_daemon_on(
                            rpc.clone(),
                            dproc_rx,
                        )));
                    legs.handles.push(tokio::spawn(daemon::serve_sock_daemon_on(
                        rpc.clone(),
                        sock_rx,
                    )));
                    legs.proc = Some(proc_tx);
                    legs.dproc = Some(dproc_tx);
                    legs.sock = Some(sock_tx);
                }
                LegGroup::Lsp => {
                    let (lsp_tx, lsp_rx) = unbounded_channel();
                    legs.handles.push(tokio::spawn(daemon::serve_lsp_daemon_on(
                        rpc.clone(),
                        lsp_rx,
                    )));
                    legs.lsp = Some(lsp_tx);
                }
                LegGroup::Term => {
                    let (term_tx, term_rx) = unbounded_channel();
                    legs.handles.push(tokio::spawn(daemon::serve_term_daemon_on(
                        rpc.clone(),
                        term_rx,
                    )));
                    legs.term = Some(term_tx);
                }
            }
        }
        legs
    }

    /// Route one inbound method to its leg's sender, or `None` when this set of legs
    /// doesn't carry it — an unknown method (the peer is the same build, so it's dropped),
    /// a daemon→client-only push (`luafs_change`/`luafs_watch_err` never arrive here), or —
    /// on a multi-stream connection — a method whose group rides a *different* stream.
    fn route(&self, method: &str) -> Option<&tokio::sync::mpsc::UnboundedSender<Incoming>> {
        use daemon::LegGroup;
        match LegGroup::classify(method)? {
            LegGroup::Control => {
                if method == "luafs_op" {
                    self.luafs.as_ref()
                } else if method == "luafs_watch" || method == "luafs_unwatch" {
                    self.luafs_watch.as_ref()
                } else if method == "http_op" {
                    self.http.as_ref()
                } else if method == "git_op" {
                    self.git.as_ref()
                } else if method.starts_with("fs_") {
                    self.fs.as_ref()
                } else if method.starts_with("config_") {
                    self.config.as_ref()
                } else {
                    None
                }
            }
            LegGroup::Proc => {
                if method.starts_with("dproc_") {
                    self.dproc.as_ref()
                } else if method.starts_with("sock_") {
                    self.sock.as_ref()
                } else {
                    self.proc.as_ref()
                }
            }
            LegGroup::Lsp => self.lsp.as_ref(),
            LegGroup::Term => self.term.as_ref(),
        }
    }

    /// Drop every leg sender so each leg sees EOF and winds down, then await the tasks so
    /// child reaping completes before the connection (or stream) closes.
    async fn shutdown(self) {
        drop((
            self.fs,
            self.proc,
            self.dproc,
            self.sock,
            self.term,
            self.lsp,
            self.luafs,
            self.luafs_watch,
            self.http,
            self.git,
            self.config,
        ));
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

/// Drive one inbound stream into `legs`: route each message to its leg by method until
/// the peer hangs up (EOF), then wind the legs down. Shared by the single-stream
/// [`run_daemon_io`] (all four groups over one `Rpc`) and the per-stream
/// [`run_daemon_group`] (one group over its own stream's `Rpc`).
#[cfg(feature = "native")]
async fn pump_daemon_legs(mut incoming: tokio::sync::mpsc::Receiver<Incoming>, legs: DaemonLegs) {
    while let Some(msg) = incoming.recv().await {
        let method = match &msg {
            Incoming::Request { method, .. } | Incoming::Notification { method, .. } => {
                method.as_str()
            }
        };
        // A leg whose task has exited closes its receiver; ignore the send error and keep
        // multiplexing the rest.
        if let Some(tx) = legs.route(method) {
            let _ = tx.send(msg);
        }
    }
    legs.shutdown().await;
}

/// Serve one [`LegGroup`](daemon::LegGroup)'s legs over a single multiplexed stream's
/// halves — the multi-stream path, one QUIC/WebTransport stream per group. The stream's
/// leading group-tag byte has already been consumed by the caller (which is how it chose
/// `group`), so this is transport-agnostic: it owns the stream's own [`Rpc`] so the group
/// writes its replies and pushes back on *its* stream, never blocking — or blocked by —
/// another group's stream. The QUIC listener ([`serve_quic`]) drives one of these per
/// accepted stream.
#[cfg(feature = "native")]
pub(crate) async fn run_daemon_group<R, W>(
    reader: R,
    writer: W,
    group: daemon::LegGroup,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // Bounded like the four-group mux below (same reasoning): the daemon issues no
    // requests, so a reader parked on a full channel strands nothing — it only delays
    // draining while the pump is busy serving an earlier op.
    let (rpc, incoming) = connect_bounded(reader, writer, daemon::SPLIT_LINK_IN_CAP);
    let legs = DaemonLegs::spawn(&[group], &rpc);
    pump_daemon_legs(incoming, legs).await;
    Ok(())
}

/// Run the **daemon** role (`bemtvi --daemon`) over separate read/write halves
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
    // Single-stream transport (ssh/stdio, the in-process test duplex): all four leg
    // groups share one ordered stream and one `Rpc`, demuxed by method. (The QUIC /
    // WebTransport transports give each group its own stream via [`run_daemon_group`].)
    // The shared inbound is bounded (see `daemon::SPLIT_LINK_IN_CAP`): a Term flood must
    // stall the wire itself, not pile up in a queue the reader keeps feeding. The daemon
    // issues no requests, so a parked reader strands nothing; and the daemon's own
    // `term_data` forwarder awaits its bounded stream channel, so a stalled wire is what
    // throttles the child.
    let (rpc, incoming) = connect_bounded(reader, writer, daemon::SPLIT_LINK_IN_CAP);
    let legs = DaemonLegs::spawn(
        &[
            daemon::LegGroup::Control,
            daemon::LegGroup::Proc,
            daemon::LegGroup::Lsp,
            daemon::LegGroup::Term,
        ],
        &rpc,
    );
    pump_daemon_legs(incoming, legs).await;
    Ok(())
}

/// Set up the **lazy** tree-sitter auto-install for a daemon session: register the Lua
/// `FileType` autocmd (`btv._remote_ts_autoinstall`) that `:TSInstall`s a remote language
/// the first time a buffer of that filetype opens. `remote` is the daemon's installed
/// parser set; we filter to languages not already installed locally (the daemon can't
/// know the client's set) and hand only those to Lua, which dedups per session. A no-op
/// for a local session (empty `remote`) or when every remote language is already here.
/// Must run before the startup lifecycle seed so the first buffer's `FileType` is caught.
#[cfg(feature = "native")]
fn set_up_remote_ts_autoinstall(host: &mut EditHost, remote: &[String]) {
    if remote.is_empty() {
        return;
    }
    let installed: HashSet<String> = bemtvi_ts::installed_parsers()
        .into_iter()
        .map(|p| p.lang)
        .collect();
    let missing: Vec<String> = remote
        .iter()
        .filter(|lang| !installed.contains(lang.as_str()))
        .cloned()
        .collect();
    if missing.is_empty() {
        return;
    }
    // Build a Lua array literal of the missing languages; `{:?}` quotes + escapes each
    // (parser stems are simple identifiers, so this is just `"rust", "lua"`). Registering
    // the autocmd is a synchronous `btv._autocmds` mutation and `FileType` dispatch reads
    // that table directly, so no effect-drain is needed — and we must NOT drain here, or
    // an early settle would fire the startup `FileType` seed before the user's own config
    // autocmds are registered.
    let list = missing
        .iter()
        .map(|lang| format!("{lang:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    if let Err(e) = host
        .lua
        .exec(&format!("btv._remote_ts_autoinstall({{ {list} }})"))
    {
        host.editor
            .echo(format!("bemtvi: remote tree-sitter setup failed: {e}"));
    }
}

/// Map a buffer's file extension to a treesitter language / filetype name (the
/// FileType autocmd and LSP server selection use this too). Delegates to
/// [`bemtvi_core::language_of_path`] so the table lives in exactly one place — the
/// editor needs the same mapping to drive its in-process treesitter engine.
pub(crate) fn filetype_of(path: Option<&std::path::Path>) -> Option<&'static str> {
    bemtvi_core::language_of_path(path)
}
