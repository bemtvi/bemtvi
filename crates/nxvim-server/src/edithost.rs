//! The `HostEffects` seam — the boundary the **synchronous editor tick** emits its
//! async/external side effects through (Phase 4, Open Decision #6 option (a):
//! *extract a reusable sync `EditHost`*; see
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md`).
//!
//! The keystroke → core → redraw path is sync and local in every world (native and
//! wasm); the *only* things that ever reach async or off-thread machinery are a small,
//! bounded set of **outbound effects** the tick fires and forgets: pushing a redraw /
//! notification / response to the client wire, and handing the event-loop actor a
//! timer / process / watch command. This trait names that set. The sync tick calls it
//! through a trait object so the same editing logic can run behind two transports:
//!
//! - **native** ([`NativeEffects`]): the client wire is msgpack-RPC ([`Rpc`]) and the
//!   command sink is the tokio [`EventLoop`] actor — today's behavior, verbatim.
//! - **wasm** (Phase 5, later): the wire is JS interop posting redraws back to the UI
//!   thread, and the command sink is the Worker-side timer wheel / the daemon link.
//!
//! Only **outbound** effects live here. The matching *inbound* events (a child exited,
//! a timer fired, an LSP reply, a file fetched) are owned by the run loop's `select!`
//! and fed into editor-tick methods through the [`inbound`](crate::inbound) translator.
//! Between the two seams, the synchronous tick — now hoisted onto the standalone
//! [`EditHost`](crate::EditHost) — holds no transport directly: every async edge is
//! either an `fx` call (outbound, here) or a loop arm driving a tick method (inbound).
//! The full outbound surface is the five effect classes the tick fires: the client wire
//! (`notify` / `respond`), the event-loop command sink (`loop_command`), the off-tick fs
//! (`fs_*`), the LSP command sink (`lsp_*`), and the `:TSInstall` grammar fetch
//! (`ts_install`). [`NativeEffects`] implements all of them over today's tokio/RPC/LSP
//! machinery; the wasm Worker (Phase 5) supplies its own implementor.

use nxvim_core::{BufferId, PendingSave};
use rmpv::Value;

// Native-only: the event-loop command type the (gated) `loop_command` method names, the
// LSP request types the (gated) `lsp_*` methods name, and everything `NativeEffects`
// holds (the wire, the daemon fs, the LSP manager, tokio senders).
#[cfg(feature = "native")]
use crate::daemon::{FsRead, HostFsAsync};
#[cfg(feature = "native")]
use crate::evloop::{EventLoop, LoopCommand};
#[cfg(feature = "native")]
use crate::save::SaveDone;
// The LSP seam data types are always on (the seam is shared); `LspManager` (the
// async client) and `LspEvent` are native-only.
#[cfg(not(feature = "native"))]
use nxvim_lsp::LspEvent;
#[cfg(feature = "native")]
use nxvim_lsp::LspManager;
use nxvim_lsp::{LspNotify, LspRequest, ReqToken, ServerKey, ServerSpawn};
#[cfg(feature = "native")]
use nxvim_rpc::Rpc;
#[cfg(feature = "native")]
use std::io;
#[cfg(feature = "native")]
use std::sync::Arc;
#[cfg(feature = "native")]
use tokio::sync::mpsc::UnboundedSender;

/// The async-effect boundary the synchronous editor tick emits through. See the
/// module docs for why this is the seam that lets one sync core serve both the native
/// server and the wasm Worker.
pub trait HostEffects {
    /// Push a notification to the attached client (the `redraw` frame, `nxvim_exit`,
    /// scripted panel selects, …).
    fn notify(&mut self, method: &str, params: Vec<Value>);
    /// Answer a client RPC request by msgid (the reply to an `nvim_*` call).
    fn respond(&mut self, id: u64, result: Result<Value, Value>);
    /// Hand a command to the event-loop actor (start/stop a timer, spawn/kill a
    /// child, arm/disarm a native file watch). Fire-and-forget; completions return
    /// as inbound `LoopEvent`s on the run loop's `select!`, not here. Native-only for
    /// now: timers/processes/watches ride the tokio event loop, which the wasm build
    /// has no analogue for yet (the Worker-side timer wheel is slice 5d).
    #[cfg(feature = "native")]
    fn loop_command(&mut self, cmd: LoopCommand);

    /// Hand a command to the terminal actor (open / write / resize / kill a PTY).
    /// Fire-and-forget like [`loop_command`](Self::loop_command); the child's output
    /// and exit return as inbound `TermEvent`s on the run loop's `select!`. Native
    /// only — the wasm build's terminal leg (the daemon PTY over WebTransport) is
    /// Phase 7, with its own seam.
    #[cfg(feature = "native")]
    fn terminal_command(&mut self, cmd: crate::terminal::native::TermCommand);

    /// Whether `:terminal` ops route to a **remote** daemon PTY (a daemon session) rather
    /// than a local one. The native `dispatch_terminal_ops` uses it to resolve the open's
    /// default cwd against the daemon's working dir ([`DirState`](crate::cwd)) instead of the
    /// local process cwd. `false` for a local/bare session. Native only.
    #[cfg(feature = "native")]
    fn has_remote_term(&self) -> bool;

    /// Off-tick fs — fetch a buffer's bytes over the daemon read leg (a startup /
    /// `:edit` open). Fire-and-forget: the fetched bytes (or a read error) return
    /// *inbound* on the run loop's open arm, not here. A silent no-op when no daemon fs
    /// is wired (the editor tick only enqueues opens in off-tick mode, so the gate at
    /// the call site means this is never reached in a local session).
    fn fs_fetch(&mut self, buffer: BufferId, path: String);

    /// Off-tick fs — push a buffer snapshot over the daemon write leg (the `:w`). Takes
    /// the snapshot's bytes out of `save` and writes them; the daemon's ack (the new
    /// [`FileStat`](nxvim_core::FileStat)) or a loud error returns *inbound* on the save
    /// arm — the buffer's saved-state clears only on that ack. A no-op without a daemon
    /// fs (unreachable; the caller gates on [`Self::has_remote_fs`]).
    fn fs_save(&mut self, save: PendingSave);

    /// Off-tick fs — resolve + validate a deferred `:cd` `target` on the daemon (the
    /// remote analogue of the synchronous local `:cd`, which can't stat the remote disk
    /// on the editor thread). The daemon's canonical directory or its `E344` error returns
    /// *inbound* on the run loop's chdir arm, where [`EditHost::apply_chdir`] reconciles it
    /// against the optimistic move; `token` keys the [`PendingChdir`](crate::cwd) the
    /// editor stashed (the scope/window/tab + undo). The default is a no-op for a backend
    /// with no daemon `:cd` (the wasm OPFS host — a documented gap in
    /// `docs/plans/2026-06-23-remote-cwd.md`); the native daemon overrides it. On native,
    /// `:cd` only defers in off-tick (daemon) mode, where this is always the overriding
    /// implementation, so the default never runs.
    fn fs_chdir(&mut self, target: String, token: u64) {
        let _ = (target, token);
    }

    /// Off-tick fs — arm a daemon-side watch on `path` (the `HostWatch` leg); a change
    /// returns *inbound* on the watch arm. A no-op without a daemon fs.
    fn fs_watch(&mut self, path: String);

    /// Off-tick fs — disarm the daemon watch on `path` (the buffer closed / lost its
    /// file). A no-op without a daemon fs.
    fn fs_unwatch(&mut self, path: String);

    /// Read an image preview's bytes for a *native* client that can't reach them on
    /// its own disk — a daemon (`:connect`) session, where the file lives on the
    /// remote host while the editor (and so the marker's `path`) runs local. Reads
    /// `path` through the off-tick fs (the daemon) when one is wired, else local disk,
    /// then *responds* to request `id` with the raw bytes (`Value::Binary`) or a loud
    /// error (a missing path / directory / read failure → the client paints its
    /// `[image: …]` placeholder). Fire-and-forget: the read runs off-tick and answers
    /// the msgid directly, so a slow remote fetch never freezes the editor tick.
    /// Native-only — the wasm Worker fetches preview bytes through its own JS seam
    /// (`image_read` → daemon/OPFS), never this RPC.
    #[cfg(feature = "native")]
    fn image_read(&mut self, id: u64, path: String);

    /// Whether a daemon (off-tick) filesystem is wired. Gates the editor tick's remote
    /// vs. local branches — the remote watch arming in `sync_buffer_watches` and the
    /// off-tick open/save drains.
    fn has_remote_fs(&self) -> bool;

    /// Async process spawn (`vim.system` / `jobstart` / `:!` with an `on_exit`) over the
    /// daemon proc leg — the wasm twin of the native `loop_command(LoopCommand::Spawn)`.
    /// Fire-and-forget: the child's pid and exit return *inbound* via
    /// [`EditHost::proc_spawned`](crate::EditHost::proc_spawned) /
    /// [`proc_exited`](crate::EditHost::proc_exited), not here. The editor tick gates this
    /// on [`Self::has_remote_proc`], so it is only reached when a daemon is connected.
    /// Native-only build routes processes through the event-loop actor's `loop_command`
    /// instead, so this method is wasm-only.
    /// `stream` requests incremental stdout (`nx.run_stream`'s streamed stdout): the daemon emits
    /// each batch inbound via [`EditHost::proc_stdout`](crate::EditHost::proc_stdout) before
    /// the single `proc_exited`. `false` is the one-shot `vim.system` (stdout with the exit).
    #[cfg(not(feature = "native"))]
    fn proc_spawn(
        &mut self,
        id: u64,
        cmd: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdin: Vec<u8>,
        stream: bool,
    );

    /// Terminate the daemon child running under `id` (`handle:kill()`) — the wasm twin of
    /// `loop_command(LoopCommand::Kill)`. A no-op if it already exited; the resulting exit
    /// returns inbound on `proc_exited`. Wasm-only (see [`Self::proc_spawn`]).
    #[cfg(not(feature = "native"))]
    fn proc_kill(&mut self, id: u64);

    /// Whether a daemon (and thus a process host) is connected. Gates the wasm editor
    /// tick's async-spawn branch: `true` enqueues a [`proc_spawn`](Self::proc_spawn);
    /// `false` (serverless OPFS — no processes) fails the spawn *loud* in the tick rather
    /// than silently dropping it. Distinct from [`has_remote_fs`](Self::has_remote_fs),
    /// which is always `true` on wasm (OPFS is an off-tick fs even with no daemon), because
    /// a process has no serverless fallback. Wasm-only (native always has local processes).
    #[cfg(not(feature = "native"))]
    fn has_remote_proc(&self) -> bool;

    /// Off-tick `nx.fs` op (`nx._fs_op`) over the daemon `luafs_op` leg — the wasm twin of the
    /// native `loop_command(LoopCommand::Fs)`. Fire-and-forget: the op runs `run_fs_job` on the
    /// daemon and its typed result returns *inbound* via
    /// [`EditHost::fs_op_result`](crate::EditHost::fs_op_result), not here. The editor tick gates
    /// this on a connected daemon ([`Self::has_remote_proc`]); a serverless OPFS session has no
    /// `luafs_op` host and fails the op loud in the tick (the OPFS-fs route is a later phase).
    /// Native-only builds run `nx.fs` ops on the event-loop actor via `loop_command`, so this
    /// method is wasm-only. `job` is the typed op descriptor; the wasm impl forwards it to the
    /// Worker, which sends one `luafs_op` request and lands the reply back through `fs_op_result`.
    #[cfg(not(feature = "native"))]
    fn fs_op(&mut self, id: u64, job: nxvim_lua::FsJob);

    /// Arm a streaming `nx.fs.watch` over the daemon `luafs_watch` leg (Phase 3b) — the wasm twin
    /// of the native `loop_command(LoopCommand::FsEventStart)`. Fire-and-forget: change batches
    /// return *inbound* via [`EditHost::fs_watch_event`](crate::EditHost::fs_watch_event) and a
    /// terminal error via [`fs_watch_error`](crate::EditHost::fs_watch_error), keyed by the stream
    /// `id`. Daemon-only — serverless OPFS has no change source, so the editor tick fails the
    /// watch *loud* (gated on [`Self::has_remote_proc`]) rather than arm a dead watch. Wasm-only
    /// (native rides the event-loop actor's `notify` watcher through `loop_command`).
    #[cfg(not(feature = "native"))]
    fn fs_watch_stream(&mut self, id: u64, path: String, recursive: bool);

    /// Disarm the streaming `nx.fs.watch` armed under `id` (`:stop()`) — the wasm twin of
    /// `loop_command(LoopCommand::FsEventStop)`. A no-op if it was never armed. Wasm-only.
    #[cfg(not(feature = "native"))]
    fn fs_unwatch_stream(&mut self, id: u64);

    /// Open a daemon-side PTY for terminal buffer `buf` (the web `:terminal` — Phase 7),
    /// running `argv` (empty ⇒ the daemon's default shell) in `cwd`, sized `rows`×`cols`.
    /// The wasm twin of the native `terminal_command(TermCommand::Open)`: the browser owns
    /// the vt100 emulation but has no PTY, so the real child runs on the daemon and its
    /// output streams back inbound via [`EditHost::terminal_feed`](crate::EditHost::terminal_feed)
    /// (fed from `term_data` pushes). Fire-and-forget; only reached when a daemon is connected
    /// (the dispatch gates on [`Self::has_remote_proc`] — serverless OPFS has no PTY host and
    /// fails the open loud). Wasm-only (native opens a local PTY through `terminal_command`).
    #[cfg(not(feature = "native"))]
    fn term_open(&mut self, buf: u64, argv: Vec<String>, cwd: Option<String>, rows: u16, cols: u16);

    /// Write input bytes to `buf`'s daemon PTY (a forwarded keystroke / paste / query reply).
    /// The wasm twin of `terminal_command(TermCommand::Write)`. Wasm-only.
    #[cfg(not(feature = "native"))]
    fn term_write(&mut self, buf: u64, bytes: Vec<u8>);

    /// Signal that a `^C` just trimmed `buf`'s flooding scrollback ([`EditHost::terminal_on_input`]),
    /// so the Worker can also *discard* the child's in-flight backlog (the part already sent over
    /// the wire) — otherwise the browser's QUIC receive window keeps feeding seconds of output
    /// after the cancel. Wasm-only (the native leg drains its local PTY fast enough not to need it).
    #[cfg(not(feature = "native"))]
    fn term_interrupted(&mut self, buf: u64);

    /// Resize `buf`'s daemon PTY so the child re-lays-out (the terminal window changed size).
    /// The wasm twin of `terminal_command(TermCommand::Resize)`. Wasm-only.
    #[cfg(not(feature = "native"))]
    fn term_resize(&mut self, buf: u64, rows: u16, cols: u16);

    /// Kill `buf`'s daemon PTY child and forget the session (the terminal closed). The wasm
    /// twin of `terminal_command(TermCommand::Kill)`. Wasm-only.
    #[cfg(not(feature = "native"))]
    fn term_kill(&mut self, buf: u64);

    /// Open a daemon-side **duplex** child (`nx.process.open` — the DAP / framed-protocol
    /// transport) over the `dproc_*` leg. The wasm twin of `loop_command(LoopCommand::ProcOpen)`:
    /// the browser has no local process, so the adapter runs on the daemon and its raw
    /// stdout/stderr stream back inbound via [`EditHost::dproc_out`](crate::EditHost::dproc_out)
    /// before [`EditHost::dproc_exit`](crate::EditHost::dproc_exit). Gated on a connected daemon
    /// ([`Self::has_remote_proc`]); serverless fails the open loud. Wasm-only.
    #[cfg(not(feature = "native"))]
    fn dproc_open(
        &mut self,
        id: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    );

    /// Feed bytes to a duplex daemon child's stdin (`handle:write`). Wasm-only.
    #[cfg(not(feature = "native"))]
    fn dproc_write(&mut self, id: u64, bytes: Vec<u8>);

    /// Terminate a duplex daemon child (`handle:kill`); its exit returns on `dproc_exit`.
    /// Wasm-only.
    #[cfg(not(feature = "native"))]
    fn dproc_kill(&mut self, id: u64);

    /// Open a daemon-side TCP connection (`nx.socket.connect` — a DAP `type="server"` adapter
    /// transport) over the `sock_*` leg. The daemon dials `host:port`; success returns on
    /// [`EditHost::sock_connected`](crate::EditHost::sock_connected), inbound bytes on
    /// [`EditHost::sock_data`](crate::EditHost::sock_data), close on
    /// [`EditHost::sock_closed`](crate::EditHost::sock_closed). Wasm-only.
    #[cfg(not(feature = "native"))]
    fn sock_connect(&mut self, id: u64, host: String, port: u16);

    /// Send bytes over a daemon TCP connection (`handle:write`). Wasm-only.
    #[cfg(not(feature = "native"))]
    fn sock_write(&mut self, id: u64, bytes: Vec<u8>);

    /// Close a daemon TCP connection (`handle:close`); the close returns on `sock_closed`.
    /// Wasm-only.
    #[cfg(not(feature = "native"))]
    fn sock_close(&mut self, id: u64);

    /// LSP — ensure `key`'s language server is running (idempotent), spawning it via
    /// `spawn` on first use. Fire-and-forget; the server's notifications and reply
    /// stream return *inbound* — on the native run loop's `lsp_events` arm, or (wasm)
    /// via [`lsp_take_events`](Self::lsp_take_events). Native runs a local/daemon child
    /// through the `LspManager`; wasm drives the `SyncLspClient` over the daemon wire.
    fn lsp_ensure(&mut self, key: ServerKey, spawn: ServerSpawn);

    /// LSP — fire-and-forget a document-sync notification (`didOpen` / `didChange` /
    /// `didSave` / `didClose`) at `key`'s server. Dropped if no such server is running.
    fn lsp_notify(&mut self, key: ServerKey, note: LspNotify);

    /// LSP — fire a language-feature request at `key`'s server; its reply returns later
    /// *inbound* as an `LspEvent::Reply` carrying `token` (the editor never awaits the
    /// round-trip). Dropped if no such server is running.
    fn lsp_request(&mut self, key: ServerKey, token: ReqToken, req: LspRequest);

    /// LSP (wasm) — feed one `lsp_stdout` push from the daemon into the `SyncLspClient`,
    /// which parses its framed JSON-RPC. Native delivers stdout through the manager's
    /// `async-lsp` loop instead, so this is wasm-only.
    #[cfg(not(feature = "native"))]
    fn lsp_stdout(&mut self, id: u64, bytes: Vec<u8>);

    /// LSP (wasm) — feed one `lsp_stderr` push (diagnostic only; dropped, as the browser
    /// has no LSP log file). Wasm-only.
    #[cfg(not(feature = "native"))]
    fn lsp_stderr(&mut self, id: u64, bytes: Vec<u8>);

    /// LSP (wasm) — the server (wire `id`) exited or its pipe closed; the `SyncLspClient`
    /// surfaces an `LspEvent::ServerExited`. `code`/`signal` are `None` when not collected
    /// (negative on the wire). Wasm-only.
    #[cfg(not(feature = "native"))]
    fn lsp_exited(&mut self, id: u64, code: Option<i32>, signal: Option<i32>);

    /// LSP (wasm) — drain the distilled [`LspEvent`]s the `SyncLspClient` produced (the
    /// browser analogue of the native run loop's `lsp_events` channel), fed to
    /// `on_lsp_event`. Wasm-only.
    #[cfg(not(feature = "native"))]
    fn lsp_take_events(&mut self) -> Vec<LspEvent>;

    /// LSP (wasm) — whether a daemon is connected to run language servers on. A
    /// serverless browser session has none, so `vim.lsp.start` fails *loud* rather than
    /// silently doing nothing. Wasm-only (native always has a process host).
    #[cfg(not(feature = "native"))]
    fn has_remote_lsp(&self) -> bool;

    /// `:TSInstall` — fetch + compile `lang`'s treesitter grammar into the data dir off
    /// the editor thread (network + a C compile, seconds long). Fire-and-forget; the
    /// finished [`InstallReport`](nxvim_ts::install::InstallReport) (or a loud error)
    /// returns *inbound* on the run loop's install arm, where the editor reloads the
    /// grammar and echoes. The native impl runs the work on a `spawn_blocking` worker;
    /// the wasm build (Phase 5) supplies its own grammar-fetch path.
    fn ts_install(&mut self, lang: String);
}

/// The native implementation of [`HostEffects`]: the client wire is msgpack-RPC and
/// the command sink is the tokio [`EventLoop`] actor. Holds both transports so the
/// editor tick reaches neither directly — exactly the indirection the wasm build later
/// swaps for JS interop + the daemon link.
#[cfg(feature = "native")]
pub struct NativeEffects {
    rpc: Rpc,
    evloop: EventLoop,
    /// The daemon filesystem the off-tick read/write/watch legs route through (Phase 3's
    /// [`RemoteHostFs`](crate::RemoteHostFs)); `None` for a local/bare session (no
    /// off-tick fs), where [`Self::has_remote_fs`] is `false` and the editor tick takes
    /// its local branches.
    host_fs_async: Option<Arc<dyn HostFsAsync>>,
    /// Delivery for a finished off-tick read — the run loop's open arm drains it. The
    /// effect spawns the read and forwards `(buffer, path, result)` here.
    open_tx: UnboundedSender<(BufferId, String, io::Result<FsRead>)>,
    /// Delivery for a finished off-tick write — the run loop's save arm drains it. The
    /// effect spawns the write and forwards the [`SaveDone`] (ack-gated saved-state) here.
    save_done_tx: UnboundedSender<SaveDone>,
    /// Delivery for a finished off-tick `:cd` — the run loop's chdir arm drains it. The
    /// effect spawns the daemon `fs_chdir` and forwards the [`ChdirDone`] (canonical dir or
    /// `E344`) here, where `apply_chdir` installs it.
    chdir_done_tx: UnboundedSender<crate::cwd::ChdirDone>,
    /// The LSP command sink — the manager the editor tick fires `ensure` / `notify` /
    /// `request` at. Its inbound event/reply stream (`lsp_events`) is owned by the run
    /// loop's `select!`, not here (the inbound seam is the 4d slice).
    lsp: LspManager,
    /// Delivery for a finished `:TSInstall` job — the run loop's install arm drains it.
    /// The effect spawns the fetch+compile on a `spawn_blocking` worker and forwards the
    /// outcome here.
    install_tx: UnboundedSender<crate::InstallOutcome>,
    /// The terminal command sink — the actor the editor tick fires `Open` / `Write` /
    /// `Resize` / `Kill` at. Its inbound output/exit stream (`term_events`) is owned by
    /// the run loop's `select!`, not here (the [`EventLoop`] pattern).
    terminals: crate::terminal::native::TerminalManager,
    /// The **remote** terminal seam for a daemon session: when `Some`, `:terminal` ops are
    /// forwarded to the daemon's PTY host (over the Term leg) instead of the local
    /// [`terminals`](Self::terminals) actor, so the child runs where the files are. `None` for
    /// a local/bare session, which spawns a local PTY. The matching inbound `TermEvent` stream
    /// is selected on by the run loop (the local actor's stream is left idle).
    host_term: Option<crate::daemon::RemoteHostTerm>,
}

#[cfg(feature = "native")]
impl NativeEffects {
    // The native session's outbound capabilities, injected together at the one
    // construction site ([`run_io`]); grouping them into a struct would just move the
    // list without making it clearer.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rpc: Rpc,
        evloop: EventLoop,
        host_fs_async: Option<Arc<dyn HostFsAsync>>,
        open_tx: UnboundedSender<(BufferId, String, io::Result<FsRead>)>,
        save_done_tx: UnboundedSender<SaveDone>,
        chdir_done_tx: UnboundedSender<crate::cwd::ChdirDone>,
        lsp: LspManager,
        install_tx: UnboundedSender<crate::InstallOutcome>,
        terminals: crate::terminal::native::TerminalManager,
        host_term: Option<crate::daemon::RemoteHostTerm>,
    ) -> Self {
        Self {
            rpc,
            evloop,
            host_fs_async,
            open_tx,
            save_done_tx,
            chdir_done_tx,
            lsp,
            install_tx,
            terminals,
            host_term,
        }
    }
}

#[cfg(feature = "native")]
impl HostEffects for NativeEffects {
    fn notify(&mut self, method: &str, params: Vec<Value>) {
        self.rpc.notify(method, params);
    }

    fn respond(&mut self, id: u64, result: Result<Value, Value>) {
        self.rpc.respond(id, result);
    }

    fn loop_command(&mut self, cmd: LoopCommand) {
        // `EventLoop::send` lazily spawns the actor on first use, so routing through
        // it (rather than a bare cloned sender) preserves the "no task until first
        // command" property.
        self.evloop.send(cmd);
    }

    fn terminal_command(&mut self, cmd: crate::terminal::native::TermCommand) {
        use crate::terminal::native::TermCommand;
        // A daemon session forwards the op to the remote PTY host (the child runs where the
        // files are); a local/bare session spawns a local PTY, lazily starting the terminal
        // actor on first use (same as `loop_command`). Either path's output/exit returns on
        // the one `TermEvent` stream the run loop selects on.
        let Some(term) = &self.host_term else {
            self.terminals.send(cmd);
            return;
        };
        match cmd {
            TermCommand::Open {
                buf,
                argv,
                cwd,
                rows,
                cols,
            } => term.open(buf.0, argv, cwd, rows, cols),
            TermCommand::Write { buf, bytes } => term.write(buf.0, bytes),
            TermCommand::Resize { buf, rows, cols } => term.resize(buf.0, rows, cols),
            TermCommand::Kill { buf } => term.kill(buf.0),
        }
    }

    fn has_remote_term(&self) -> bool {
        self.host_term.is_some()
    }

    fn fs_fetch(&mut self, buffer: BufferId, path: String) {
        let Some(fs) = self.host_fs_async.clone() else {
            return;
        };
        let tx = self.open_tx.clone();
        tokio::spawn(async move {
            let result = fs.read(path.clone()).await;
            let _ = tx.send((buffer, path, result));
        });
    }

    fn fs_save(&mut self, mut save: PendingSave) {
        let Some(fs) = self.host_fs_async.clone() else {
            return;
        };
        // The bytes were snapshotted at command time; move them into the write and keep
        // their length for the `written` echo (the buffer can't be read after the move).
        let bytes = std::mem::take(&mut save.bytes);
        let bytes_len = bytes.len();
        let path = save.path.display().to_string();
        let tx = self.save_done_tx.clone();
        tokio::spawn(async move {
            let result = fs.write(path, bytes).await;
            let _ = tx.send(SaveDone::new(save, bytes_len, result));
        });
    }

    fn fs_chdir(&mut self, target: String, token: u64) {
        let Some(fs) = self.host_fs_async.clone() else {
            return;
        };
        let tx = self.chdir_done_tx.clone();
        tokio::spawn(async move {
            let result = fs.chdir(target).await;
            let _ = tx.send(crate::cwd::ChdirDone { token, result });
        });
    }

    fn fs_watch(&mut self, path: String) {
        if let Some(fs) = &self.host_fs_async {
            fs.watch(path);
        }
    }

    fn fs_unwatch(&mut self, path: String) {
        if let Some(fs) = &self.host_fs_async {
            fs.unwatch(path);
        }
    }

    fn has_remote_fs(&self) -> bool {
        self.host_fs_async.is_some()
    }

    fn image_read(&mut self, id: u64, path: String) {
        // Answer the request off-tick: clone the cloneable RPC handle so the spawned
        // task can `respond` by msgid once the read resolves, without holding the
        // editor tick. A daemon session reads over the wire (`host_fs_async`); a local
        // session reads its own disk (the native client could open the path itself,
        // but routing through the server keeps a single code path and one decode seam).
        let rpc = self.rpc.clone();
        match self.host_fs_async.clone() {
            Some(fs) => {
                tokio::spawn(async move {
                    let reply = image_bytes(&path, fs.read(path.clone()).await);
                    rpc.respond(id, reply);
                });
            }
            None => {
                tokio::spawn(async move {
                    let reply = tokio::fs::read(&path)
                        .await
                        .map(Value::Binary)
                        .map_err(|e| Value::from(format!("nxvim_image_read: {path}: {e}")));
                    rpc.respond(id, reply);
                });
            }
        }
    }

    fn lsp_ensure(&mut self, key: ServerKey, spawn: ServerSpawn) {
        self.lsp.ensure_server(key, spawn);
    }

    fn lsp_notify(&mut self, key: ServerKey, note: LspNotify) {
        self.lsp.notify(key, note);
    }

    fn lsp_request(&mut self, key: ServerKey, token: ReqToken, req: LspRequest) {
        self.lsp.request(key, token, req);
    }

    fn ts_install(&mut self, lang: String) {
        let dir = nxvim_ts::data_dir();
        let tx = self.install_tx.clone();
        tokio::task::spawn_blocking(move || {
            let result = nxvim_ts::install::install(&dir, &lang);
            // The receiver only drops at shutdown; a send error means we're exiting, so
            // there's nothing to report to.
            let _ = tx.send((lang, result));
        });
    }
}

/// Project a daemon [`FsRead`] (the off-tick read result for an `nxvim_image_read`)
/// onto the request reply: a file's bytes become the `Value::Binary` answer; a
/// missing path, a directory, or a read error become a loud error string the client
/// shows as its `[image: …]` placeholder. (A preview only ever points at a file, so
/// `New`/`Dir` here mean the file vanished or the path is wrong — never a silent
/// empty image.)
#[cfg(feature = "native")]
fn image_bytes(path: &str, result: io::Result<FsRead>) -> Result<Value, Value> {
    match result {
        Ok(FsRead::File(bytes, _)) => Ok(Value::Binary(bytes)),
        Ok(FsRead::New) => Err(Value::from(format!(
            "nxvim_image_read: {path}: no such file"
        ))),
        Ok(FsRead::Dir { .. }) => Err(Value::from(format!(
            "nxvim_image_read: {path}: is a directory"
        ))),
        Err(e) => Err(Value::from(format!("nxvim_image_read: {path}: {e}"))),
    }
}
