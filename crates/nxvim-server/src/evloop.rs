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
    },
    /// Terminate the async child running under `id` (a no-op if it already
    /// exited). The child is terminated via `kill_on_drop`, and its `on_exit`
    /// still fires with `code = -1` (the signal is not honored — see
    /// [`LoopOp::Kill`](nxvim_lua::LoopOp::Kill)).
    Kill { id: u64 },
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
    started: bool,
}

impl EventLoop {
    /// Create the event loop and the receiver the server loop selects on. Spawns
    /// child processes through `host_proc`. No task is spawned until the first
    /// [`EventLoop::send`].
    pub fn new(host_proc: Arc<dyn HostProc>) -> (EventLoop, UnboundedReceiver<LoopEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let evloop = EventLoop {
            cmd_tx,
            start: Some((cmd_rx, event_tx)),
            host_proc,
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
            tokio::spawn(run_evloop(cmd_rx, event_tx, self.host_proc.clone()));
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
) {
    // Live timer tasks and the per-process kill channels, keyed by callback id.
    let mut timers: HashMap<u64, JoinHandle<()>> = HashMap::new();
    let mut procs: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    // Live filesystem watchers (`notify`), keyed by callback id. Each owns its
    // native backend thread; dropping it (on `FsEventStop`, a re-arm, or actor
    // shutdown) stops the watch.
    let mut fs_watchers: HashMap<u64, RecommendedWatcher> = HashMap::new();
    while let Some(cmd) = cmd_rx.recv().await {
        // Drop handles whose tasks have finished, so a long run of one-shot timers
        // / processes can't accumulate dead entries. (fs watchers are dropped
        // explicitly on `FsEventStop`, not pruned here.)
        timers.retain(|_, h| !h.is_finished());
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
                tokio::spawn(host_proc.run(spec, kill_rx, events));
            }
            LoopCommand::Kill { id } => {
                // Dropping the kill sender (or sending on it) wakes the process
                // task, which terminates the child via `kill_on_drop`.
                if let Some(kill_tx) = procs.remove(&id) {
                    let _ = kill_tx.send(());
                }
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
fn start_fs_watch_coalesced(
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
