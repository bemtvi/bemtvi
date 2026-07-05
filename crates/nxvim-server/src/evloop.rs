//! [`EventLoop`]: the async Lua runtime's background actor — nxvim's event loop.
//!
//! The editor core and the Lua VM are `!Send` and live on the server's single
//! thread, which processes one message at a time (`architecture.md` → *Async
//! design*). The event loop therefore is **not** "run callbacks on other threads".
//! It is a `Send` `tokio::spawn`ed actor that owns the things which take
//! wall-clock time — timers and child processes — and, when one completes, sends a
//! small typed [`LoopEvent`] back to the server over a channel. The server, on its
//! one thread, runs the matching Lua callback. The Lua VM never crosses the thread
//! boundary; the actor handles only ids, durations, argv, and bytes.
//!
//! Shape mirrors `nxvim-lsp`'s `LspManager`/`run_supervisor`: a handle that is
//! cheap to construct, two unbounded channels, and a task spawned lazily on the
//! first command (so a session that never sets a timer or spawns a process starts
//! nothing). The server wires the [`LoopEvent`] receiver as a `tokio::select!` arm
//! next to the syntax and LSP arms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use nxvim_lua::{run_fs_job, FsError, FsJob, FsValue, HttpError, HttpRequest, HttpResponse, LuaFs};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::host::{HostProc, ProcEvents, ProcSpec};

/// A command from the server thread to the event-loop actor. Fire-and-forget: the
/// server never awaits a reply (any result returns later as a [`LoopEvent`]),
/// exactly like [`LspManager`](nxvim_lsp::LspManager)'s command path, so the
/// single-threaded editor is never blocked on a timer or a child process.
pub enum LoopCommand {
    /// Arm a timer firing callback `id` after `delay`, then every `repeat` while
    /// `repeat` is non-zero (a one-shot when `repeat` is zero). Re-arming an
    /// existing `id` replaces it.
    TimerStart {
        id: u64,
        delay: Duration,
        repeat: Duration,
    },
    /// Cancel the timer armed under `id` (a no-op if it already fired or was never
    /// armed).
    TimerStop { id: u64 },
    /// Spawn `argv` (program + args, no shell) and run callback `id` with its
    /// result when it exits. The child's pid comes back first as
    /// [`LoopEvent::ProcessSpawned`]; its result later as [`LoopEvent::ProcessExit`].
    /// `stdin` is written to the child's standard input then closed (empty when the
    /// caller feeds no input).
    Spawn {
        id: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdin: Vec<u8>,
        /// Stream stdout incrementally (`nx.run_stream`'s streamed stdout) rather than
        /// delivering it whole with the exit (`vim.system`). See [`ProcSpec`].
        stream: bool,
        /// Run on the actor's **local** [`HostProc`] instead of the session's — the
        /// `nx.plugins` manager's git, so plugins clone onto the local disk even in a
        /// daemon session. `false` for `nx.run`/`nx.run_stream` (session routing).
        local: bool,
    },
    /// Terminate the async child running under `id` (a no-op if it already
    /// exited). The child is terminated via `kill_on_drop`, and its `on_exit`
    /// still fires with `code = -1` (the signal is not honored — see
    /// [`LoopOp::Kill`](nxvim_lua::LoopOp::Kill)).
    Kill { id: u64 },
    /// Spawn `argv` as a **duplex** child (`nx.process.open`): stdin stays open for
    /// [`ProcWrite`](LoopCommand::ProcWrite), and stdout/stderr stream back as raw
    /// byte chunks ([`LoopEvent::ProcOut`]) until the [`LoopEvent::ProcExit`].
    /// Terminate it with the shared [`Kill`](LoopCommand::Kill).
    ProcOpen {
        id: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    },
    /// Feed `data` to the still-open stdin of the duplex child under `id` (a no-op
    /// if it already exited).
    ProcWrite { id: u64, data: Vec<u8> },
    /// Open a TCP client connection (`nx.socket.connect`): on success
    /// [`LoopEvent::SockConnected`] fires, then incoming bytes as
    /// [`LoopEvent::SockData`] until [`LoopEvent::SockClosed`]. Writes go via
    /// [`SockWrite`](LoopCommand::SockWrite); close with [`SockClose`](LoopCommand::SockClose).
    SockConnect { id: u64, host: String, port: u16 },
    /// Send `data` over the connection under `id` (a no-op once closed).
    SockWrite { id: u64, data: Vec<u8> },
    /// Close the connection under `id`.
    SockClose { id: u64 },
    /// Begin watching `path` for changes (native — inotify/FSEvents/kqueue via
    /// `notify`), firing the watcher callback `id` each time it changes until
    /// [`LoopCommand::FsEventStop`]. `recursive` watches a whole subtree (libuv's
    /// `recursive` fs_event flag). Re-arming an `id` replaces its watch.
    FsEventStart {
        id: u64,
        path: String,
        recursive: bool,
    },
    /// Cancel the filesystem watch armed under `id` (a no-op if it was never
    /// armed).
    FsEventStop { id: u64 },
    /// Run an off-tick `nx.fs` op (`nx._fs_op`) through the actor's [`FsBackend`] — local
    /// disk on the blocking pool, or the daemon's `luafs_op` leg — and send the typed
    /// result back as [`LoopEvent::FsResult`] for callback `id`. Off the editor tick, so a
    /// large/slow op (a daemon round-trip, a recursive copy) never janks the editor.
    ///
    /// `local` runs it against the actor's **local** [`FsBackend`] rather than the session's
    /// — the `nx.plugins` manager's clone / discover / source ops, which must see the local
    /// disk (plugins load into the local Lua VM). `false` for `nx.fs.*` (session routing).
    Fs { id: u64, job: FsJob, local: bool },
    /// Run an off-tick `nx.http.fetch` request (`nx._http_fetch`) through the actor's
    /// [`HttpBackend`] — a local `ureq` round-trip on the blocking pool, or the daemon's
    /// `http_op` leg — and send the typed result back as [`LoopEvent::HttpResult`] for
    /// callback `id`. Off the editor tick, so a slow request (a far server, a large body)
    /// never janks the editor. Unlike [`Fs`](LoopCommand::Fs) there is no `local` flag —
    /// HTTP has no local-VM concern (it always follows the session's network routing).
    /// `local` forces it onto the actor's LOCAL `ureq` (`nx.http.fetch_local`) even when the
    /// session's [`HttpBackend`] routes to a daemon; `false` follows the session routing.
    Http {
        id: u64,
        request: HttpRequest,
        local: bool,
    },
}

/// An event from the actor back to the server thread, delivered to the main
/// loop's `select!`. Each carries the callback `id` the server runs (on its one
/// thread) when the event arrives — the wall-clock wake the synchronous model
/// otherwise lacks.
#[derive(Debug)]
pub enum LoopEvent {
    /// A timer elapsed. `keep` is true for a repeating timer (retain its callback,
    /// it will fire again), false for a one-shot (drop the callback after running).
    Timer { id: u64, keep: bool },
    /// A child spawned via [`LoopCommand::Spawn`] is running; carries its OS pid
    /// (`None` if the spawn failed). Lets the `vim.system` handle expose a real pid
    /// shortly after the call returns (it cannot be known synchronously on a
    /// single-threaded runtime — a blocking wait would deadlock the actor).
    ProcessSpawned { id: u64, pid: Option<u32> },
    /// A streaming child (spawned with `stream = true`) emitted a batch of stdout
    /// lines (newline-delimited, the trailing newline stripped). Fires the
    /// persistent `on_stdout` callback under `id`; arrives zero or more times
    /// before the single [`LoopEvent::ProcessExit`]. Only streaming spawns produce
    /// these — a `vim.system` (`stream = false`) delivers its stdout with the exit.
    ProcessStdout { id: u64, lines: Vec<String> },
    /// A child spawned via [`LoopCommand::Spawn`] exited; carries the result its
    /// `on_exit` callback receives (`code = -1` on spawn failure or a kill). A
    /// streaming child's `stdout` here is empty (already delivered as
    /// [`LoopEvent::ProcessStdout`] batches).
    ProcessExit {
        id: u64,
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// A **duplex** child ([`LoopCommand::ProcOpen`]) emitted a raw chunk of output
    /// — `stderr` distinguishes the two streams. Unframed and un-split (no newline
    /// batching): the Lua side (`nx.process` consumer, e.g. a DAP client) owns the
    /// framing. Fires zero or more times before [`LoopEvent::ProcExit`].
    ProcOut {
        id: u64,
        data: Vec<u8>,
        stderr: bool,
    },
    /// A duplex child ([`LoopCommand::ProcOpen`]) exited (`code = -1` on a spawn
    /// failure or a kill). Fires exactly once, after which the `nx.process` handle
    /// is dead.
    ProcExit { id: u64, code: i32 },
    /// A TCP connection ([`LoopCommand::SockConnect`]) was established.
    SockConnected { id: u64 },
    /// A TCP connection emitted a raw chunk of inbound bytes (un-framed — the Lua
    /// side owns the protocol). Fires zero or more times before [`SockClosed`].
    ///
    /// [`SockClosed`]: LoopEvent::SockClosed
    SockData { id: u64, data: Vec<u8> },
    /// A TCP connection closed — `error` carries the cause on a connect / I/O
    /// failure, `None` on a clean EOF / requested close. Fires exactly once.
    SockClosed { id: u64, error: Option<String> },
    /// A path watched via [`LoopCommand::FsEventStart`] changed (a content/attribute
    /// edit, or a create/delete/rename), or `error` when the watch couldn't be
    /// established (the path doesn't exist, a watch limit was hit). The watch fires
    /// until stopped.
    ///
    /// Two consumers, split by `id` (see [`INTERNAL_WATCH_BASE`](crate::INTERNAL_WATCH_BASE)):
    /// the internal per-buffer file watch (`:checktime`) ignores `kind`/`paths` (it
    /// re-stats a known buffer); the Lua `nx.fs.watch` surface reads them. `kind` is
    /// the coalesced change class (`"create"`/`"modify"`/`"remove"`/`"rename"`) and
    /// `paths` the deduped affected paths — both empty for the internal watch, which
    /// doesn't pay to carry them.
    FsEvent {
        id: u64,
        error: Option<String>,
        kind: Option<&'static str>,
        paths: Vec<PathBuf>,
    },
    /// An off-tick `nx.fs` op ([`LoopCommand::Fs`]) settled: the typed result the
    /// promise registered under `id` resolves / rejects with. The server runs the
    /// matching `nx._run_cb` on its one thread (the marshalling to Lua happens there).
    FsResult {
        id: u64,
        result: Result<FsValue, FsError>,
    },
    /// An off-tick `nx.http.fetch` request ([`LoopCommand::Http`]) settled: the typed
    /// result the promise registered under `id` resolves / rejects with. The server runs
    /// the matching `nx._run_cb` on its one thread (the marshalling to Lua happens there).
    HttpResult {
        id: u64,
        result: Result<HttpResponse, HttpError>,
    },
}

/// Where the actor runs off-tick `nx.fs` ops. One path per session topology — never
/// a per-op `LuaFs` round-trip over the wire (the retired `RemoteLuaFs`):
/// - [`Local`](FsBackend::Local) — native-bare: [`run_fs_job`] against a local
///   [`StdLuaFs`](nxvim_lua::StdLuaFs) on the blocking pool (a quick syscall).
/// - [`Remote`](FsBackend::Remote) — native-daemon: the whole [`FsJob`] crosses in one
///   `luafs_op` request via [`RemoteFsJobs`](crate::daemon::RemoteFsJobs), decomposed
///   daemon-side. The same leg the wasm edit-host uses; an `await`, not a thread park.
#[derive(Clone)]
pub enum FsBackend {
    Local(Arc<dyn LuaFs + Send + Sync>),
    Remote(crate::daemon::RemoteFsJobs),
}

/// Where the actor runs off-tick `nx.http.fetch` requests. The HTTP sibling of
/// [`FsBackend`], one path per session topology:
/// - [`Local`](HttpBackend::Local) — native-bare: a local `ureq` round-trip
///   ([`run_http_request`](crate::http::run_http_request)) on the blocking pool.
/// - [`Remote`](HttpBackend::Remote) — native-daemon: the request crosses in one `http_op`
///   request via [`RemoteHttp`](crate::daemon::RemoteHttp), run on the daemon (which owns
///   the network — the same reason `nx.fs` / processes route there). An `await`, not a park.
#[derive(Clone)]
pub enum HttpBackend {
    Local,
    Remote(crate::daemon::RemoteHttp),
}

impl HttpBackend {
    /// Run `request` through this backend and return the typed result. Async — the caller
    /// is the actor's task, so `Local` offloads the blocking `ureq` call to the blocking
    /// pool (never parking the actor) and `Remote` awaits the `http_op` round-trip.
    async fn run(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        match self {
            HttpBackend::Local => {
                tokio::task::spawn_blocking(move || crate::http::run_http_request(&request))
                    .await
                    .unwrap_or_else(|e| {
                        Err(HttpError {
                            message: format!("nx.http: request task failed: {e}"),
                        })
                    })
            }
            HttpBackend::Remote(remote) => remote.run(request).await,
        }
    }
}

/// Handle the server holds to drive the event loop. Cheap to construct; the actor
/// task is spawned lazily on the first [`EventLoop::send`], so a session that
/// never uses a timer or `vim.system` spawns nothing (the
/// `LspManager::ensure_started` pattern).
pub struct EventLoop {
    cmd_tx: UnboundedSender<LoopCommand>,
    /// Taken when the actor is first started.
    start: Option<(UnboundedReceiver<LoopCommand>, UnboundedSender<LoopEvent>)>,
    /// The seam child processes are spawned through — the local disk's
    /// [`StdHostProc`](crate::host::StdHostProc) by default, or an injected
    /// daemon-backed [`HostProc`]. Cloned into the actor when it starts.
    host_proc: Arc<dyn HostProc>,
    /// Where off-tick `nx.fs` ops run — local disk on the blocking pool, or the daemon's
    /// `luafs_op` leg (see [`FsBackend`]). Cloned into the actor when it starts. Preserves
    /// the lazy start — a session that never touches `nx.fs` (or a timer / process) spawns
    /// nothing.
    fs: FsBackend,
    /// Where off-tick `nx.http.fetch` requests run — a local `ureq` round-trip, or the
    /// daemon's `http_op` leg (see [`HttpBackend`]). Cloned into the actor when it starts.
    http: HttpBackend,
    /// The **local** twins of `host_proc` / `fs`, used for `local`-flagged
    /// [`Spawn`](LoopCommand::Spawn) / [`Fs`](LoopCommand::Fs) ops — the `nx.plugins`
    /// manager's git + discovery, which stay on the local disk even in a daemon session
    /// (plugins load into the local Lua VM). In a bare/local session these are the same
    /// disk the session already uses; in a daemon session they bypass the remote routing.
    /// See `docs/plans/2026-07-03-remote-aware-plugin-manager.md`.
    local_host_proc: Arc<dyn HostProc>,
    local_fs: FsBackend,
    started: bool,
}

impl EventLoop {
    /// Create the event loop and the receiver the server loop selects on. Spawns
    /// child processes through `host_proc` and runs off-tick `nx.fs` ops against
    /// `lua_fs`. No task is spawned until the first [`EventLoop::send`].
    pub fn new(
        host_proc: Arc<dyn HostProc>,
        fs: FsBackend,
        http: HttpBackend,
        local_host_proc: Arc<dyn HostProc>,
        local_fs: FsBackend,
    ) -> (EventLoop, UnboundedReceiver<LoopEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let evloop = EventLoop {
            cmd_tx,
            start: Some((cmd_rx, event_tx)),
            host_proc,
            fs,
            http,
            local_host_proc,
            local_fs,
            started: false,
        };
        (evloop, event_rx)
    }

    /// Spawn the actor task if it isn't running yet.
    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        if let Some((cmd_rx, event_tx)) = self.start.take() {
            tokio::spawn(run_evloop(
                cmd_rx,
                event_tx,
                self.host_proc.clone(),
                self.fs.clone(),
                self.http.clone(),
                self.local_host_proc.clone(),
                self.local_fs.clone(),
            ));
            self.started = true;
        }
    }

    /// Fire-and-forget a command at the actor, starting it on first use.
    pub fn send(&mut self, cmd: LoopCommand) {
        self.ensure_started();
        let _ = self.cmd_tx.send(cmd);
    }
}

/// The actor's run loop: own the live timers and child processes, service
/// commands, and send events back as work completes. Lives for the event loop's
/// life; ends when the server drops the command sender (server shutdown). Each
/// timer / process is a child `tokio::spawn`ed task keyed by callback id so it can
/// be cancelled; finished one-shot handles are pruned opportunistically so the
/// maps don't grow with every fired `defer_fn` (the no-leak guarantee).
async fn run_evloop(
    mut cmd_rx: UnboundedReceiver<LoopCommand>,
    event_tx: UnboundedSender<LoopEvent>,
    host_proc: Arc<dyn HostProc>,
    fs: FsBackend,
    http: HttpBackend,
    local_host_proc: Arc<dyn HostProc>,
    local_fs: FsBackend,
) {
    // Live timer tasks and the per-process kill channels, keyed by callback id.
    let mut timers: HashMap<u64, JoinHandle<()>> = HashMap::new();
    let mut procs: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    // The still-open stdin sinks of live duplex children (`nx.process.open`), keyed
    // by callback id. A `ProcWrite` forwards bytes here; the child task drains them
    // and the entry is dropped when the child exits (closing its stdin → EOF).
    let mut proc_stdin: HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
    // Live TCP connections (`nx.socket`): a close signal + the write sink, keyed by
    // callback id. A `SockWrite` forwards bytes to the sink; a `SockClose` (or a
    // dropped close sender) ends the connection task; both are pruned when its task
    // exits (the receivers drop → these senders report closed).
    let mut sock_close: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    let mut sock_write: HashMap<u64, tokio::sync::mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
    // Live filesystem watchers (`notify`), keyed by callback id. Each owns its
    // native backend thread; dropping it (on `FsEventStop`, a re-arm, or actor
    // shutdown) stops the watch.
    let mut fs_watchers: HashMap<u64, RecommendedWatcher> = HashMap::new();
    while let Some(cmd) = cmd_rx.recv().await {
        // Drop handles whose tasks have finished, so a long run of one-shot timers
        // / processes can't accumulate dead entries. (fs watchers are dropped
        // explicitly on `FsEventStop`, not pruned here.)
        timers.retain(|_, h| !h.is_finished());
        // Prune duplex children whose task has ended (the kill/stdin receivers
        // dropped → these senders report closed), so a long-lived session that
        // opens and closes many `nx.process` children doesn't accumulate dead
        // entries. One-shot `vim.system` kill senders are pruned the same way.
        procs.retain(|_, s| !s.is_closed());
        proc_stdin.retain(|_, s| !s.is_closed());
        sock_close.retain(|_, s| !s.is_closed());
        sock_write.retain(|_, s| !s.is_closed());
        match cmd {
            LoopCommand::TimerStart { id, delay, repeat } => {
                // Re-arming an id replaces its timer (a fresh :start on a handle).
                if let Some(h) = timers.remove(&id) {
                    h.abort();
                }
                let event_tx = event_tx.clone();
                let handle = tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    if repeat.is_zero() {
                        let _ = event_tx.send(LoopEvent::Timer { id, keep: false });
                    } else {
                        // Fire now, then every `repeat`, until the channel closes
                        // (server gone) or the task is aborted (`:stop`/`:close`).
                        if event_tx.send(LoopEvent::Timer { id, keep: true }).is_err() {
                            return;
                        }
                        loop {
                            tokio::time::sleep(repeat).await;
                            if event_tx.send(LoopEvent::Timer { id, keep: true }).is_err() {
                                return;
                            }
                        }
                    }
                });
                timers.insert(id, handle);
            }
            LoopCommand::TimerStop { id } => {
                if let Some(h) = timers.remove(&id) {
                    h.abort();
                }
            }
            LoopCommand::Spawn {
                id,
                argv,
                cwd,
                env,
                stdin,
                stream,
                local,
            } => {
                let (kill_tx, kill_rx) = oneshot::channel();
                procs.insert(id, kill_tx);
                let spec = ProcSpec {
                    argv,
                    cwd,
                    env,
                    stdin,
                    stream,
                };
                let events = ProcEvents::new(id, event_tx.clone());
                // `local` git (the plugin manager) runs on the local host; everything else
                // on the session host (the daemon, in an edit-host session).
                let proc = if local { &local_host_proc } else { &host_proc };
                tokio::spawn(proc.run(spec, kill_rx, events));
            }
            LoopCommand::ProcOpen { id, argv, cwd, env } => {
                let (kill_tx, kill_rx) = oneshot::channel();
                let (stdin_tx, stdin_rx) = tokio::sync::mpsc::unbounded_channel();
                procs.insert(id, kill_tx);
                proc_stdin.insert(id, stdin_tx);
                tokio::spawn(crate::host::run_duplex_process(
                    id,
                    argv,
                    cwd,
                    env,
                    kill_rx,
                    stdin_rx,
                    event_tx.clone(),
                ));
            }
            LoopCommand::ProcWrite { id, data } => {
                // Forward to the child's stdin task; a closed channel (the child
                // exited) silently drops the write — the Lua handle's exit fired.
                if let Some(sink) = proc_stdin.get(&id) {
                    let _ = sink.send(data);
                }
            }
            LoopCommand::SockConnect { id, host, port } => {
                let (close_tx, close_rx) = oneshot::channel();
                let (write_tx, write_rx) = tokio::sync::mpsc::unbounded_channel();
                sock_close.insert(id, close_tx);
                sock_write.insert(id, write_tx);
                tokio::spawn(crate::host::run_socket_connection(
                    id,
                    host,
                    port,
                    close_rx,
                    write_rx,
                    event_tx.clone(),
                ));
            }
            LoopCommand::SockWrite { id, data } => {
                if let Some(sink) = sock_write.get(&id) {
                    let _ = sink.send(data);
                }
            }
            LoopCommand::SockClose { id } => {
                if let Some(close_tx) = sock_close.remove(&id) {
                    let _ = close_tx.send(());
                }
                sock_write.remove(&id);
            }
            LoopCommand::Kill { id } => {
                // Dropping the kill sender (or sending on it) wakes the process
                // task, which terminates the child via `kill_on_drop`.
                if let Some(kill_tx) = procs.remove(&id) {
                    let _ = kill_tx.send(());
                }
                // Drop the stdin sink too (duplex children) so its writer task sees
                // the channel close and shuts the pipe.
                proc_stdin.remove(&id);
            }
            LoopCommand::FsEventStart {
                id,
                path,
                recursive,
            } => {
                // Re-arming an id replaces its watch: drop the old watcher first
                // (a fresh :start on a handle).
                fs_watchers.remove(&id);
                // The internal per-buffer watch (id ≥ BASE) wants a raw, path-less
                // event per change — its consumer re-stats. A Lua `nx.fs.watch` (id <
                // BASE) wants coalesced `{kind, paths}` batches, so it arms the
                // debouncing variant.
                let armed = if id >= crate::INTERNAL_WATCH_BASE {
                    start_fs_watch(id, &path, recursive, event_tx.clone())
                } else {
                    start_fs_watch_coalesced(id, &path, recursive, event_tx.clone())
                };
                match armed {
                    Ok(watcher) => {
                        fs_watchers.insert(id, watcher);
                    }
                    Err(e) => {
                        // The watch couldn't arm (path missing, watch limit). The
                        // async :start already returned 0 to Lua, so the only place
                        // to surface this is the callback's `err` arg — never a
                        // silent drop. (libuv reports it from :start; the async
                        // bridge defers it one hop to the callback.)
                        let _ = event_tx.send(LoopEvent::FsEvent {
                            id,
                            error: Some(e.to_string()),
                            kind: None,
                            paths: Vec::new(),
                        });
                    }
                }
            }
            LoopCommand::FsEventStop { id } => {
                fs_watchers.remove(&id); // dropping the watcher stops it
            }
            LoopCommand::Fs { id, job, local } => {
                // Run the op off the actor's async task and send the typed result back for
                // the server to settle the promise on its one thread. Spawned so concurrent
                // ops don't serialize behind one another. Two paths (see [`FsBackend`]):
                // local disk runs `run_fs_job` on the blocking pool (a quick syscall);
                // native-daemon sends the whole job over `luafs_op` and `await`s the reply
                // (no thread park — it's a tokio request on the link). A `local`-flagged op
                // (the plugin manager's discover / source) always takes the local backend,
                // bypassing the session's remote routing.
                let event_tx = event_tx.clone();
                let fs = if local { &local_fs } else { &fs };
                match fs {
                    FsBackend::Local(lua_fs) => {
                        let lua_fs = lua_fs.clone();
                        tokio::spawn(async move {
                            let result = tokio::task::spawn_blocking(move || {
                                run_fs_job(lua_fs.as_ref(), &job)
                            })
                            .await
                            .unwrap_or_else(|e| {
                                // The blocking task panicked (a `LuaFs` impl should
                                // never panic, but never silently swallow it).
                                Err(FsError {
                                    code: "EIO".to_string(),
                                    message: format!("nx.fs op task failed: {e}"),
                                })
                            });
                            let _ = event_tx.send(LoopEvent::FsResult { id, result });
                        });
                    }
                    FsBackend::Remote(remote) => {
                        let remote = remote.clone();
                        tokio::spawn(async move {
                            let result = remote.run(job).await;
                            let _ = event_tx.send(LoopEvent::FsResult { id, result });
                        });
                    }
                }
            }
            LoopCommand::Http { id, request, local } => {
                // Run the round-trip off the actor's async task (spawned so concurrent
                // fetches don't serialize) and send the typed result back for the server to
                // settle the promise. `HttpBackend::run` offloads the blocking `ureq` call to
                // the blocking pool (native-bare) or awaits the `http_op` leg (native-daemon)
                // — either way the actor is never parked. A `local`-flagged request
                // (`nx.http.fetch_local`) forces the local `ureq` even in a daemon session.
                let event_tx = event_tx.clone();
                let backend = if local {
                    HttpBackend::Local
                } else {
                    http.clone()
                };
                tokio::spawn(async move {
                    let result = backend.run(request).await;
                    let _ = event_tx.send(LoopEvent::HttpResult { id, result });
                });
            }
        }
        // Forget kill channels whose process tasks have closed them (the child
        // exited and the `HostProc` future dropped the receiver) — the leak guard
        // for procs, mirroring the timer prune above.
        procs.retain(|_, tx| !tx.is_closed());
    }
}

/// Arm a native filesystem watch on `path` (inotify/FSEvents/kqueue via `notify`),
/// translating each change into a [`LoopEvent::FsEvent`] for callback `id`. The
/// returned [`RecommendedWatcher`] owns the backend thread; the caller keeps it
/// alive in the watcher map and drops it to stop. `recursive` watches a subtree
/// (libuv's `recursive` flag); the default is the single named path (a file like
/// lualine's `.git/HEAD`, or a directory). The `notify` callback runs on the
/// backend thread and only sends on the channel, so no Lua/editor state crosses
/// the boundary — same discipline as the timer/process tasks.
fn start_fs_watch(
    id: u64,
    path: &str,
    recursive: bool,
    event_tx: UnboundedSender<LoopEvent>,
) -> notify::Result<RecommendedWatcher> {
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let event = match res {
            Ok(event) => event,
            // A backend error mid-watch (e.g. the inotify queue overflowed): report
            // it to the callback's `err` rather than dropping it silently.
            Err(e) => {
                let _ = event_tx.send(LoopEvent::FsEvent {
                    id,
                    error: Some(e.to_string()),
                    kind: None,
                    paths: Vec::new(),
                });
                return;
            }
        };
        if classify_fs_event(&event.kind).is_none() {
            return; // an access/metadata-only event libuv wouldn't report
        }
        // The internal per-buffer watch re-stats a known buffer, so the change
        // class / paths are not carried (it would pay to clone them for nothing).
        let _ = event_tx.send(LoopEvent::FsEvent {
            id,
            error: None,
            kind: None,
            paths: Vec::new(),
        });
    })?;
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher.watch(Path::new(path), mode)?;
    Ok(watcher)
}

/// Arm a native filesystem watch for the Lua `nx.fs.watch` surface: like
/// [`start_fs_watch`], but it carries each change's class and paths, and
/// **coalesces** a burst into one [`LoopEvent::FsEvent`] over a 10 ms window. The
/// `notify` backend thread feeds raw `(kind, paths)` (or a backend error) into an
/// internal channel; a spawned task drains it, accumulating until 10 ms idle, then
/// emits a single deduped batch. (10 ms only — a plugin that wants a longer settle
/// composes `nx.utils.debounce` on top.)
///
/// The task is self-reaping: when the watcher is dropped (`FsEventStop`, a re-arm,
/// or actor shutdown), its callback closure — and the raw sender it holds — drops,
/// the channel closes, and the task ends. So the caller only has to keep the
/// returned watcher alive, exactly like the internal path.
pub(crate) fn start_fs_watch_coalesced(
    id: u64,
    path: &str,
    recursive: bool,
    event_tx: UnboundedSender<LoopEvent>,
) -> notify::Result<RecommendedWatcher> {
    let (raw_tx, mut raw_rx) = unbounded_channel::<RawFsChange>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        match res {
            Ok(event) => {
                if let Some(kind) = classify_fs_kind(&event.kind) {
                    let _ = raw_tx.send(RawFsChange::Change(kind, event.paths));
                }
            }
            // Surface a backend error to the consumer (terminal — same as the arm
            // failure), never a silent drop.
            Err(e) => {
                let _ = raw_tx.send(RawFsChange::Error(e.to_string()));
            }
        }
    })?;
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    watcher.watch(Path::new(path), mode)?;

    const WINDOW: Duration = Duration::from_millis(10);
    tokio::spawn(async move {
        while let Some(first) = raw_rx.recv().await {
            // A backend error ends the watch — forward it and stop draining.
            let (mut kind, mut paths) = match first {
                RawFsChange::Error(msg) => {
                    let _ = event_tx.send(LoopEvent::FsEvent {
                        id,
                        error: Some(msg),
                        kind: None,
                        paths: Vec::new(),
                    });
                    break;
                }
                RawFsChange::Change(k, p) => (k, dedup_paths(Vec::new(), p)),
            };
            // Coalesce everything that arrives within the window into this batch.
            let mut errored = None;
            loop {
                match tokio::time::timeout(WINDOW, raw_rx.recv()).await {
                    Ok(Some(RawFsChange::Change(k, p))) => {
                        // Mixed kinds in one burst coarsen to "modify" (the generic
                        // "something changed" the tree consumer rescans on anyway).
                        if k != kind {
                            kind = "modify";
                        }
                        paths = dedup_paths(paths, p);
                    }
                    Ok(Some(RawFsChange::Error(msg))) => {
                        errored = Some(msg);
                        break;
                    }
                    // Window elapsed (Err) or channel closed (Ok(None)) → flush.
                    _ => break,
                }
            }
            let _ = event_tx.send(LoopEvent::FsEvent {
                id,
                error: None,
                kind: Some(kind),
                paths,
            });
            if let Some(msg) = errored {
                let _ = event_tx.send(LoopEvent::FsEvent {
                    id,
                    error: Some(msg),
                    kind: None,
                    paths: Vec::new(),
                });
                break;
            }
        }
    });
    Ok(watcher)
}

/// A raw `notify` change handed from the backend thread to the coalescing task.
enum RawFsChange {
    Change(&'static str, Vec<PathBuf>),
    Error(String),
}

/// Append `more` to `acc`, skipping paths already present (bursts are small, so a
/// linear check beats a `HashSet`'s allocation), preserving first-seen order.
fn dedup_paths(mut acc: Vec<PathBuf>, more: Vec<PathBuf>) -> Vec<PathBuf> {
    for p in more {
        if !acc.contains(&p) {
            acc.push(p);
        }
    }
    acc
}

/// The `nx.fs.watch` change class for a `notify` [`EventKind`]: `"create"` /
/// `"remove"` / `"rename"` for the structural kinds, `"modify"` for content/metadata
/// edits and the coarse `Any`/`Other` kinds (macOS FSEvents emits those when it
/// can't classify — defaulted to `"modify"` so a real edit is never dropped), and
/// `None` for the pure-access events libuv/Deno don't surface.
fn classify_fs_kind(kind: &EventKind) -> Option<&'static str> {
    use notify::event::ModifyKind;
    match kind {
        EventKind::Create(_) => Some("create"),
        EventKind::Remove(_) => Some("remove"),
        EventKind::Modify(ModifyKind::Name(_)) => Some("rename"),
        EventKind::Access(_) => None,
        _ => Some("modify"),
    }
}

/// Map a `notify` [`EventKind`] onto libuv's two `fs_event` flags — `(change,
/// rename)` — or `None` for the one kind libuv doesn't surface (pure file access).
/// A create / remove / rename is a `rename`; everything else that mutates is a
/// `change`. The coarse `Any` / `Other` kinds (which macOS FSEvents emits when it
/// can't classify) default to `change` rather than being dropped, so a real edit
/// is never silently missed.
fn classify_fs_event(kind: &EventKind) -> Option<(bool, bool)> {
    use notify::event::ModifyKind;
    match kind {
        EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_)) => {
            Some((false, true))
        }
        EventKind::Access(_) => None,
        _ => Some((true, false)),
    }
}
