//! The daemon wire protocol for the edit-host split (process + filesystem).
//!
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3 moves the
//! network boundary *below* the editor: the edit-host (core + Lua + treesitter)
//! runs **local** for a zero-round-trip keystroke path, and only fs + process +
//! watch — the lag-tolerant work — run on a remote **daemon**. This module holds
//! both legs of that wire — the daemon-side servers ([`serve_daemon`],
//! [`serve_fs_daemon`]) and the edit-host-side clients ([`RemoteHostProc`],
//! [`RemoteHostFs`]) — over any [`AsyncRead`]/[`AsyncWrite`] transport: an
//! in-process `tokio::io::duplex` (how the tests drive it), or — in the real split —
//! ssh stdio to `nxvim --daemon`.
//!
//! ## The process leg (notifications)
//!
//! [`HostProc`] is already async + event-routed (pid then exit come back as separate
//! events, not a return value), so it maps onto a wire with no impedance mismatch.
//! Four notifications correlated by a per-spawn `id` the edit-host mints and the
//! daemon echoes back — notifications (not request/response) because a child's life
//! is two events at different times, which a single reply can't model:
//!
//! | direction | method | params |
//! | --- | --- | --- |
//! | edit-host → daemon | `proc_spawn` | `[id, argv, cwd?, env, stdin]` |
//! | edit-host → daemon | `proc_kill`  | `[id]` |
//! | daemon → edit-host | `proc_spawned` | `[id, pid?]` |
//! | daemon → edit-host | `proc_exited`  | `[id, code, stdout, stderr]` |
//!
//! The daemon runs each child through the *same* [`StdHostProc`] the local server
//! uses today — it relays that machinery's [`LoopEvent`]s straight onto the wire —
//! so a process behaves identically whether it ran here or across the network.
//!
//! ## The filesystem leg (request/response)
//!
//! Core's [`HostFs`](nxvim_core::HostFs) is *synchronous* — a daemon-backed read
//! can't block the single editor thread on the network (the latency thesis) — so the
//! remote fs is **not** that sync trait. It is a small *async* seam, [`HostFsAsync`],
//! the server consumes **off the editor tick**: it fetches a buffer's bytes over the
//! wire, then hands core a populated replica via `Editor::load_str` (the in-memory
//! open the web build already uses). Unlike the process leg, a file read is naturally
//! request/response, so this needs no `id`/demux — `nxvim_rpc`'s `request` routes the
//! reply directly:
//!
//! | direction | method | reply |
//! | --- | --- | --- |
//! | edit-host → daemon | `fs_read [path]`         | `["file", bytes]` / `["new"]` / `["dir", path, entries]`, or an RPC error |
//! | edit-host → daemon | `fs_write [path, bytes]` | `["ok", stat?]`, or an RPC error                |
//!
//! `serve_fs_daemon` reads an existing file (`file`), reports a not-yet-existing one as a
//! new-file buffer (`new`), or lists a directory (`dir` — the remote explorer, Phase 3g:
//! the daemon's canonical path plus its raw `[is_dir, name]` entries, which the edit-host
//! sorts and renders); any other read error comes back as a loud RPC error. `fs_write`
//! does the atomic write through the same sync [`HostFs`] and replies with the new
//! [`FileStat`](nxvim_core::FileStat) (so the edit-host can stamp its `disk` snapshot
//! without a remote stat round-trip), or a loud error.
//!
//! **The save path is off-tick, like the read** (`docs/plans/…` → Phase 3e, *the save
//! slice*): core does *not* write through the sync [`HostFs`](nxvim_core::HostFs) in a
//! daemon session — it snapshots the buffer at command time and enqueues a
//! [`PendingSave`](nxvim_core::PendingSave); the server pushes those bytes over
//! `fs_write` off the editor tick and finalizes the buffer's saved-state only on the
//! daemon's ack, so a slow remote write never freezes typing. (`:read` still uses the
//! sync [`HostFs`], on local disk, for now.)
//!
//! ## The watch leg (`HostWatch` — server push)
//!
//! Only the daemon can watch a remote file, so it **owns change detection**: the
//! edit-host arms a watch per open file-backed buffer and the daemon pushes a change.
//! Unlike the read/write requests, a change is a server-initiated *notification* (the
//! one daemon→edit-host push on the fs leg), so it can't be a reply:
//!
//! | direction | method | params |
//! | --- | --- | --- |
//! | edit-host → daemon | `fs_watch [path]`   | arm a watch on `path` |
//! | edit-host → daemon | `fs_unwatch [path]` | drop the watch |
//! | daemon → edit-host | `fs_changed [path, stat?]` | `path` changed (nil stat = vanished) |
//!
//! `serve_fs_daemon` baselines each watched path's stat at `fs_watch` time and re-stats
//! on a coarse [`WATCH_POLL`] interval (the daemon is the lag-tolerant leg), pushing
//! `fs_changed` whenever one drifts. A successful `fs_write` refreshes the baseline so
//! the edit-host's **own** save doesn't echo back as an external change. The edit-host
//! turns each push into a [`WatchEvent`] the server reconciles off the editor tick (the
//! `FileChangedShell` round-trip; a reload re-fetches over `fs_read`) — the remote
//! analogue of the local per-buffer file watch.

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_rpc::{connect, Incoming, Rpc};

use crate::evloop::LoopEvent;
use crate::host::{HostProc, ProcEvents, ProcSpec, StdHostProc};

const FS_READ: &str = "fs_read";
const FS_WRITE: &str = "fs_write";
// The watch leg (`HostWatch`): the edit-host arms/disarms watches on the daemon, the
// daemon pushes a change. Server-*push* — the only daemon→edit-host *notification* on
// the fs leg (reads/writes are request/response).
const FS_WATCH: &str = "fs_watch";
const FS_UNWATCH: &str = "fs_unwatch";
const FS_CHANGED: &str = "fs_changed";

/// How often the daemon re-stats its watched paths. The daemon is the lag-tolerant
/// leg (the whole reason the watch lives here, not on the editor tick), so a coarse
/// poll is fine — it owns change detection and the edit-host only reacts to a push.
const WATCH_POLL: Duration = Duration::from_millis(200);

// Wire method names. Kept as constants so the two halves can never drift on a typo.
const PROC_SPAWN: &str = "proc_spawn";
const PROC_KILL: &str = "proc_kill";
const PROC_SPAWNED: &str = "proc_spawned";
const PROC_EXITED: &str = "proc_exited";

/// What the daemon reports back about one child, demuxed off the wire and handed to
/// the [`RemoteHostProc::run`] future waiting on that spawn's `id`. Mirrors the two
/// [`ProcEvents`] reports the future then re-emits to the editor.
enum DaemonEvent {
    /// The child is running (or failed to spawn — `None` pid).
    Spawned(Option<u32>),
    /// The child exited (`code = -1` on spawn failure or a kill).
    Exited {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
}

/// The table of spawns awaiting their daemon reports: `id` → the channel into the
/// [`RemoteHostProc::run`] future driving that child. The demux task forwards each
/// `proc_spawned` / `proc_exited` to the matching sender; the future removes its own
/// entry when the child exits.
type Inflight = Arc<Mutex<HashMap<u64, UnboundedSender<DaemonEvent>>>>;

/// A [`HostProc`] that runs children on a remote daemon instead of locally: each
/// [`run`](HostProc::run) forwards the spawn over the wire and relays the daemon's
/// pid/exit back to the editor's [`ProcEvents`], so the event-loop actor that drives
/// it never knows the process ran across a network. The drop-in for
/// [`StdHostProc`](crate::host::StdHostProc) on the edit-host side of the split.
///
/// `Send + Sync` (it holds only the cloneable [`Rpc`] handle, a shared map, and an
/// id counter) so it rides [`ServerInit`](crate::ServerInit) onto the server thread
/// and is shared across spawns by the actor, exactly as the local host is.
pub struct RemoteHostProc {
    rpc: Rpc,
    inflight: Inflight,
    /// Per-spawn correlation id minted here (not the editor's callback id, which
    /// never needs to cross the wire — the demux routes purely by this).
    next_id: AtomicU64,
}

impl RemoteHostProc {
    /// Connect to a daemon over `reader`/`writer` (a duplex, or ssh stdio). Spawns
    /// the demux task that fans the daemon's replies out to in-flight spawns; its
    /// RPC reader/writer tasks live on the runtime this is called from (the same
    /// arrangement [`nxvim_rpc::connect`] makes for any client), so call it from
    /// within a tokio runtime.
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHostProc
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, incoming) = connect(reader, writer);
        let inflight: Inflight = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(run_demux(incoming, inflight.clone()));
        RemoteHostProc {
            rpc,
            inflight,
            next_id: AtomicU64::new(1),
        }
    }
}

impl HostProc for RemoteHostProc {
    fn run(
        &self,
        spec: ProcSpec,
        mut kill: oneshot::Receiver<()>,
        events: ProcEvents,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let rpc = self.rpc.clone();
        let inflight = self.inflight.clone();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move {
            // Register *before* the spawn request so the daemon's reply can never
            // race ahead of a receiver to land in.
            let (tx, mut rx) = unbounded_channel::<DaemonEvent>();
            inflight.lock().unwrap().insert(id, tx);
            rpc.notify(PROC_SPAWN, encode_spawn(id, spec));

            // Hold `events` in an Option so the `&self` `spawned` calls and the
            // self-consuming `exited` call coexist (and `exited` fires exactly once).
            let mut events = Some(events);
            let mut killed = false;
            loop {
                tokio::select! {
                    // Once kill has fired, disable this arm: re-polling a consumed
                    // oneshot returns instantly and would busy-loop. The child still
                    // exits via the daemon's `proc_exited` (code -1), keeping the
                    // exactly-one-exit contract.
                    _ = &mut kill, if !killed => {
                        killed = true;
                        rpc.notify(PROC_KILL, vec![Value::from(id)]);
                    }
                    ev = rx.recv() => match ev {
                        Some(DaemonEvent::Spawned(pid)) => {
                            if let Some(e) = &events {
                                e.spawned(pid);
                            }
                        }
                        Some(DaemonEvent::Exited { code, stdout, stderr }) => {
                            if let Some(e) = events.take() {
                                e.exited(code, stdout, stderr);
                            }
                            break;
                        }
                        // The demux dropped our sender: the daemon connection died.
                        // Synthesize an exit so the editor's one-shot `on_exit`
                        // always fires and is never leaked.
                        None => {
                            if let Some(e) = events.take() {
                                e.exited(-1, Vec::new(), b"daemon connection closed".to_vec());
                            }
                            break;
                        }
                    }
                }
            }
            inflight.lock().unwrap().remove(&id);
        })
    }
}

/// Pump the daemon's replies off the wire and forward each to the spawn it belongs
/// to. On connection teardown (`incoming` ends) it clears [`Inflight`], dropping
/// every pending sender so each waiting [`RemoteHostProc::run`] future observes the
/// EOF and reports a `-1` exit rather than hanging on a child that will never report.
async fn run_demux(mut incoming: UnboundedReceiver<Incoming>, inflight: Inflight) {
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        match method.as_str() {
            PROC_SPAWNED => {
                if let Some((id, ev)) = decode_spawned(&params) {
                    forward(&inflight, id, ev);
                }
            }
            PROC_EXITED => {
                if let Some((id, ev)) = decode_exited(params) {
                    forward(&inflight, id, ev);
                }
            }
            _ => {}
        }
    }
    inflight.lock().unwrap().clear();
}

/// Deliver `ev` to the spawn registered under `id` (a no-op if it already exited and
/// removed itself — a late or duplicate report is harmless).
fn forward(inflight: &Inflight, id: u64, ev: DaemonEvent) {
    if let Some(tx) = inflight.lock().unwrap().get(&id) {
        let _ = tx.send(ev);
    }
}

/// Run the daemon end of the wire over `reader`/`writer`: spawn the children a far
/// edit-host asks for and stream their pid/exit back. Returns when the connection
/// closes (the edit-host hung up). Each child runs through [`StdHostProc`] — the
/// exact local-spawn machinery — and its [`LoopEvent`]s are relayed onto the wire, so
/// `vim.system` / `jobstart` / `:!` behave identically remote and local.
pub async fn serve_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, mut incoming) = connect(reader, writer);

    // One forwarder turns the children's `LoopEvent`s — the same events the local
    // event-loop actor consumes — into wire notifications back to the edit-host.
    let (ev_tx, mut ev_rx) = unbounded_channel::<LoopEvent>();
    let reply = rpc.clone();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                LoopEvent::ProcessSpawned { id, pid } => reply.notify(
                    PROC_SPAWNED,
                    vec![
                        Value::from(id),
                        pid.map_or(Value::Nil, |p| Value::from(p as u64)),
                    ],
                ),
                LoopEvent::ProcessExit {
                    id,
                    code,
                    stdout,
                    stderr,
                } => reply.notify(
                    PROC_EXITED,
                    vec![
                        Value::from(id),
                        Value::from(code as i64),
                        Value::Binary(stdout),
                        Value::Binary(stderr),
                    ],
                ),
                // The daemon only spawns processes — it arms no timers and no
                // filesystem watches — so no other variant can reach here.
                LoopEvent::Timer { .. } | LoopEvent::FsEvent { .. } => {}
            }
        }
    });

    let host = StdHostProc;
    // Per-child kill channels, keyed by the edit-host's spawn id, so a `proc_kill`
    // can reach the running child (mirrors the event-loop actor's `procs` map).
    let mut kills: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the edit-host drives the daemon with notifications only
        };
        match method.as_str() {
            PROC_SPAWN => {
                if let Some((id, spec)) = decode_spawn(params) {
                    let (kill_tx, kill_rx) = oneshot::channel();
                    kills.insert(id, kill_tx);
                    let events = ProcEvents::new(id, ev_tx.clone());
                    tokio::spawn(host.run(spec, kill_rx, events));
                }
            }
            PROC_KILL => {
                if let Some(id) = params.first().and_then(Value::as_u64) {
                    if let Some(kill_tx) = kills.remove(&id) {
                        let _ = kill_tx.send(());
                    }
                }
            }
            _ => {}
        }
        // Forget kill channels whose child tasks have closed them (the child exited),
        // the same leak guard the event-loop actor applies to its `procs` map.
        kills.retain(|_, tx| !tx.is_closed());
    }
    Ok(())
}

/// `ProcSpec` → `proc_spawn` params. Consumes the spec so `stdin` (potentially
/// large) moves onto the wire rather than copying.
fn encode_spawn(id: u64, spec: ProcSpec) -> Vec<Value> {
    let ProcSpec {
        argv,
        cwd,
        env,
        stdin,
    } = spec;
    vec![
        Value::from(id),
        Value::Array(argv.into_iter().map(Value::from).collect()),
        cwd.map_or(Value::Nil, Value::from),
        Value::Array(
            env.into_iter()
                .map(|(k, v)| Value::Array(vec![Value::from(k), Value::from(v)]))
                .collect(),
        ),
        Value::Binary(stdin),
    ]
}

/// `proc_spawn` params → `(id, ProcSpec)`, or `None` on a malformed frame (which the
/// daemon simply drops — a peer is the same build, so this only guards against
/// corruption). Moves `stdin` / `argv` out rather than cloning.
fn decode_spawn(mut params: Vec<Value>) -> Option<(u64, ProcSpec)> {
    if params.len() < 5 {
        return None;
    }
    let id = params[0].as_u64()?;
    let argv = match std::mem::replace(&mut params[1], Value::Nil) {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => return None,
    };
    let cwd = params[2].as_str().map(str::to_string);
    let env = match std::mem::replace(&mut params[3], Value::Nil) {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|pair| match pair {
                Value::Array(kv) => {
                    let k = kv.first()?.as_str()?.to_string();
                    let v = kv.get(1)?.as_str()?.to_string();
                    Some((k, v))
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let stdin = match std::mem::replace(&mut params[4], Value::Nil) {
        Value::Binary(b) => b,
        _ => Vec::new(),
    };
    Some((
        id,
        ProcSpec {
            argv,
            cwd,
            env,
            stdin,
        },
    ))
}

/// `proc_spawned` params → `(id, Spawned)`. A nil/absent pid means the spawn failed.
fn decode_spawned(params: &[Value]) -> Option<(u64, DaemonEvent)> {
    let id = params.first()?.as_u64()?;
    let pid = params.get(1).and_then(Value::as_u64).map(|p| p as u32);
    Some((id, DaemonEvent::Spawned(pid)))
}

/// `proc_exited` params → `(id, Exited)`. Moves the captured output out of `params`.
fn decode_exited(mut params: Vec<Value>) -> Option<(u64, DaemonEvent)> {
    if params.len() < 4 {
        return None;
    }
    let id = params[0].as_u64()?;
    let code = params[1].as_i64().unwrap_or(-1) as i32;
    let stdout = match std::mem::replace(&mut params[2], Value::Nil) {
        Value::Binary(b) => b,
        _ => Vec::new(),
    };
    let stderr = match std::mem::replace(&mut params[3], Value::Nil) {
        Value::Binary(b) => b,
        _ => Vec::new(),
    };
    Some((
        id,
        DaemonEvent::Exited {
            code,
            stdout,
            stderr,
        },
    ))
}

// ===== the filesystem leg =====================================================

/// What a daemon `fs_read` resolves a path to — a file's bytes, a new-file marker, or a
/// directory listing. A genuine read error (a permission failure, a dead connection) is
/// *not* one of these — it surfaces as an `Err` the server echoes loudly, never a silent
/// empty buffer.
pub enum FsRead {
    /// An existing file's bytes — load them into the buffer (a replica of the remote).
    File(Vec<u8>),
    /// The path doesn't exist yet — open an empty new-file buffer named for it (the
    /// `:e newfile` case), so a first `:w` would create it.
    New,
    /// The path is a **directory** — open it as the in-window file explorer (Phase 3g).
    /// `path` is the daemon's *canonical* directory path (so `../`/descend navigation is
    /// unambiguous on the edit-host side); `entries` are its immediate, unsorted entries
    /// (the edit-host sorts them via [`Buffer::from_dir_entries`](nxvim_core::Buffer)).
    Dir {
        path: String,
        entries: Vec<DirEntry>,
    },
}

/// The **async** filesystem seam the server fetches buffer contents through, off the
/// editor tick — the daemon/remote analog of core's *synchronous*
/// [`HostFs`](nxvim_core::HostFs). Where the sync trait reads local disk at the open
/// call (and must, since it runs on the single editor thread), this returns a future
/// the server awaits *off-tick* and then hands core populated bytes, so a slow remote
/// read never freezes typing. [`RemoteHostFs`] is the over-the-wire implementation;
/// a test can supply a fake.
///
/// Object-safe (returns a boxed `Send` future, no `async fn`) to match the
/// `Box<dyn …>` DI style the rest of the server uses without an `async-trait`
/// dependency. `read` resolves the path to a file, a new-file marker, or a directory
/// listing (the [`FsRead`] variants — so it covers buffer opens *and* the remote
/// explorer); `write` is the save path.
pub trait HostFsAsync: Send + Sync {
    /// Fetch `path` for a buffer open: its bytes (a file), a new-file marker (absent), or
    /// the directory listing (the remote explorer) — whichever the path resolves to.
    fn read(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<FsRead>> + Send>>;

    /// Atomically write `bytes` to `path` (the off-tick `:w`). Resolves to the file's
    /// new [`FileStat`] on success — which the editor stamps as its `disk` baseline so
    /// a later change check doesn't false-positive on our own write — or a loud error
    /// (a failed write is never silently dropped; the contract is that the editor's
    /// saved-state clears *only* on this ack). `None` stat means the write succeeded
    /// but the daemon could not stat the result.
    fn write(
        &self,
        path: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<FileStat>>> + Send>>;

    /// Arm a remote watch on `path` (the `HostWatch` leg): the daemon stats it now as
    /// the baseline and pushes a [`WatchEvent`] each time it changes thereafter.
    /// Fire-and-forget — the change comes back asynchronously via [`Self::take_watch_events`].
    /// The default is a no-op (an impl with no remote, e.g. a local fake that never
    /// pushes).
    fn watch(&self, _path: String) {}

    /// Disarm the remote watch on `path` (the buffer closed / lost its file). The
    /// default is a no-op, matching [`Self::watch`].
    fn unwatch(&self, _path: String) {}

    /// Take the receiver of server-pushed [`WatchEvent`]s — the edit-host side of the
    /// `HostWatch` leg. Returns `Some` exactly once (the first call) for an impl that
    /// pushes ([`RemoteHostFs`]); `None` for one that never watches. The server wires
    /// the receiver as a `select!` arm and reconciles each push off the editor tick.
    fn take_watch_events(&self) -> Option<UnboundedReceiver<WatchEvent>> {
        None
    }
}

/// A server-pushed file change from the daemon's watch leg (the `fs_changed`
/// notification): the watched `path` and its new [`FileStat`] (`None` = the file
/// vanished on the daemon). The edit-host turns it into a `FileChangedShell` reconcile
/// off the editor tick — the remote analogue of the local per-buffer file watch.
pub struct WatchEvent {
    /// The watched path that changed (as the edit-host armed it — the buffer's name).
    pub path: String,
    /// The file's new stat, or `None` if it vanished (drives the `"deleted"` reason).
    pub stat: Option<FileStat>,
}

/// A [`HostFsAsync`] that reads files from a remote daemon over the wire. `read`
/// issues an `fs_read` request and awaits the reply — a file read is naturally
/// request/response, so (unlike [`RemoteHostProc`]) there is no per-call demux:
/// [`nxvim_rpc`] routes each response to its awaiting `request` by msgid.
pub struct RemoteHostFs {
    rpc: Rpc,
    /// The receiver of `fs_changed` pushes, handed to the server once via
    /// [`HostFsAsync::take_watch_events`]. Behind a `Mutex<Option<…>>` because the
    /// trait method is `&self` and the receiver can only be taken out once.
    watch_rx: Mutex<Option<UnboundedReceiver<WatchEvent>>>,
}

impl RemoteHostFs {
    /// Connect to a daemon over `reader`/`writer`. The daemon sends `fs_read` /
    /// `fs_write` *responses* (which `nxvim_rpc` routes internally) and `fs_changed`
    /// *notifications* (the watch leg); a drain task consumes the `Incoming` stream —
    /// dropping the receiver would tear the connection down — and forwards each
    /// `fs_changed` to the watch channel the server drains. RPC tasks live on the
    /// runtime this is called from, as for any [`nxvim_rpc::connect`].
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHostFs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, mut incoming) = connect(reader, writer);
        let (watch_tx, watch_rx) = unbounded_channel::<WatchEvent>();
        tokio::spawn(async move {
            while let Some(msg) = incoming.recv().await {
                if let Incoming::Notification { method, params } = msg {
                    if method == FS_CHANGED {
                        if let Some(ev) = decode_fs_changed(params) {
                            // The server may not have taken the receiver yet at startup;
                            // a send that finds no receiver is harmlessly dropped.
                            let _ = watch_tx.send(ev);
                        }
                    }
                }
            }
        });
        RemoteHostFs {
            rpc,
            watch_rx: Mutex::new(Some(watch_rx)),
        }
    }
}

impl HostFsAsync for RemoteHostFs {
    fn read(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<FsRead>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc.request(FS_READ, vec![Value::from(path)]).await {
                Ok(Value::Array(mut a)) => decode_fs_read(&mut a),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_read: malformed reply",
                )),
                // A transport failure (daemon gone) is a loud read error, not a
                // silent empty buffer.
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn write(
        &self,
        path: String,
        bytes: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = io::Result<Option<FileStat>>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc
                .request(FS_WRITE, vec![Value::from(path), Value::Binary(bytes)])
                .await
            {
                Ok(Value::Array(a)) => decode_fs_write(&a),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_write: malformed reply",
                )),
                // A daemon error (permission, transport gone) is a loud write
                // failure the editor surfaces — the saved-state never clears on it.
                Err(e) => Err(io::Error::other(e.to_string())),
            }
        })
    }

    fn watch(&self, path: String) {
        self.rpc.notify(FS_WATCH, vec![Value::from(path)]);
    }

    fn unwatch(&self, path: String) {
        self.rpc.notify(FS_UNWATCH, vec![Value::from(path)]);
    }

    fn take_watch_events(&self) -> Option<UnboundedReceiver<WatchEvent>> {
        self.watch_rx.lock().unwrap().take()
    }
}

/// `fs_changed [path, stat?]` → [`WatchEvent`]; `None` on a malformed frame (dropped —
/// a peer is the same build). A nil/absent stat means the file vanished.
fn decode_fs_changed(params: Vec<Value>) -> Option<WatchEvent> {
    let path = params.first()?.as_str()?.to_string();
    let stat = params.get(1).and_then(decode_stat);
    Some(WatchEvent { path, stat })
}

/// `["file", bytes]` / `["new"]` → [`FsRead`]; anything else is a malformed reply.
fn decode_fs_read(a: &mut [Value]) -> io::Result<FsRead> {
    match a.first().and_then(Value::as_str) {
        Some("file") => match a.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
            Some(Value::Binary(bytes)) => Ok(FsRead::File(bytes)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fs_read: file reply missing bytes",
            )),
        },
        Some("new") => Ok(FsRead::New),
        Some("dir") => {
            let path = a
                .get(1)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let entries = match a.get_mut(2).map(|v| std::mem::replace(v, Value::Nil)) {
                Some(Value::Array(items)) => {
                    items.into_iter().filter_map(decode_dir_entry).collect()
                }
                _ => Vec::new(),
            };
            Ok(FsRead::Dir { path, entries })
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_read: unknown reply tag",
        )),
    }
}

/// One `[is_dir, name]` wire pair → a [`DirEntry`]; `None` on a malformed pair (dropped
/// — a peer is the same build, so this only guards corruption).
fn decode_dir_entry(v: Value) -> Option<DirEntry> {
    let a = v.as_array()?;
    let is_dir = a.first()?.as_bool()?;
    let name = a.get(1)?.as_str()?.to_string();
    Some(DirEntry { is_dir, name })
}

/// `["ok", stat?]` → the post-write [`FileStat`] (or `None`); any other tag is a
/// malformed reply. A daemon *error* never reaches here — it comes back as the RPC
/// `Err` arm in [`RemoteHostFs::write`], a loud failure, not an `["ok", …]`.
fn decode_fs_write(a: &[Value]) -> io::Result<Option<FileStat>> {
    match a.first().and_then(Value::as_str) {
        Some("ok") => Ok(a.get(1).and_then(decode_stat)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_write: unknown reply tag",
        )),
    }
}

/// A [`FileStat`] on the wire: `[secs, nanos, size]`, where `secs`/`nanos` are the
/// mtime as a duration past the Unix epoch (a `nil` mtime — platform reports none —
/// becomes a nil `secs`). Kept self-contained so both legs agree on the shape.
fn encode_stat(stat: &FileStat) -> Value {
    let (secs, nanos) = match stat.mtime.and_then(|t| t.duration_since(UNIX_EPOCH).ok()) {
        Some(d) => (Value::from(d.as_secs()), Value::from(d.subsec_nanos())),
        None => (Value::Nil, Value::from(0u32)),
    };
    Value::Array(vec![secs, nanos, Value::from(stat.size)])
}

/// Inverse of [`encode_stat`]: `[secs, nanos, size]` → [`FileStat`], or `None` if the
/// value isn't a well-formed stat triple (so a missing/garbled stat degrades to "no
/// baseline" rather than erroring the whole write).
fn decode_stat(v: &Value) -> Option<FileStat> {
    let a = v.as_array()?;
    let size = a.get(2)?.as_u64()?;
    let mtime = match a.first() {
        Some(secs) if !secs.is_nil() => {
            let secs = secs.as_u64()?;
            let nanos = a.get(1).and_then(Value::as_u64).unwrap_or(0) as u32;
            Some(UNIX_EPOCH + Duration::new(secs, nanos))
        }
        _ => None,
    };
    Some(FileStat { mtime, size })
}

/// Run the daemon end of the *filesystem* wire over `reader`/`writer`, serving
/// `fs_read` requests from `fs` (the daemon's real backend — [`StdHostFs`] in the
/// binary, a fake in tests). Returns when the connection closes. Reads run inline
/// (the daemon serves one request at a time); an initial open is a single fetch, so
/// no concurrency is needed yet.
///
/// [`StdHostFs`]: nxvim_core::StdHostFs
pub async fn serve_fs_daemon<R, W>(
    reader: R,
    writer: W,
    fs: Box<dyn HostFs + Send>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, mut incoming) = connect(reader, writer);
    // The watch leg (`HostWatch`): watched path → last-seen stat. The daemon *owns*
    // change detection — the edit-host arms a watch (`fs_watch`) and only reacts to a
    // push, so it never stats the remote disk itself. A coarse poll (the daemon is the
    // lag-tolerant leg) re-stats each watched path and pushes `fs_changed` on a diff.
    let mut watches: HashMap<PathBuf, Option<FileStat>> = HashMap::new();
    let mut poll = tokio::time::interval(WATCH_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            msg = incoming.recv() => {
                let Some(msg) = msg else { break }; // the edit-host hung up
                match msg {
                    Incoming::Request { id, method, mut params } => {
                        let reply = match method.as_str() {
                            FS_READ => serve_read(&*fs, &params),
                            FS_WRITE => {
                                let reply = serve_write(&*fs, &mut params);
                                // Self-suppress: a successful write changed the file, but
                                // it was the edit-host's *own* `:w` — refresh the watch
                                // baseline so the poll doesn't push it back as an external
                                // change. Same task as the poll, so no race (it can't tick
                                // mid-write). `serve_write` only takes the *bytes* out of
                                // `params`, so the path is still readable here.
                                if reply.is_ok() {
                                    if let Some(path) =
                                        params.first().and_then(Value::as_str).map(PathBuf::from)
                                    {
                                        if let Some(slot) = watches.get_mut(&path) {
                                            *slot = fs.stat(&path);
                                        }
                                    }
                                }
                                reply
                            }
                            other => Err(Value::from(format!("unknown method: {other}"))),
                        };
                        rpc.respond(id, reply);
                    }
                    // The watch leg's arm/disarm — notifications, not requests (there is
                    // no reply; the change comes back later as `fs_changed`).
                    Incoming::Notification { method, params } => match method.as_str() {
                        FS_WATCH => {
                            if let Some(path) =
                                params.first().and_then(Value::as_str).map(PathBuf::from)
                            {
                                // Baseline the current stat so the very next poll doesn't
                                // misfire on a file that hasn't changed since the open.
                                let stat = fs.stat(&path);
                                watches.insert(path, stat);
                            }
                        }
                        FS_UNWATCH => {
                            if let Some(path) =
                                params.first().and_then(Value::as_str).map(PathBuf::from)
                            {
                                watches.remove(&path);
                            }
                        }
                        _ => {}
                    },
                }
            }
            // Re-stat the watched paths and push any that drifted from their baseline.
            // Disabled while nothing is watched, so an idle fs daemon does no work.
            _ = poll.tick(), if !watches.is_empty() => {
                for (path, last) in watches.iter_mut() {
                    let now = fs.stat(path);
                    if now != *last {
                        *last = now;
                        rpc.notify(
                            FS_CHANGED,
                            vec![
                                Value::from(path.to_string_lossy().into_owned()),
                                now.map_or(Value::Nil, |s| encode_stat(&s)),
                            ],
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Serve one `fs_read [path]` against `fs`, projecting [`classify`]'s result onto the
/// `["file", bytes]` / `["new"]` / `["dir", path, entries]` wire shape (or a loud error
/// reply).
fn serve_read(fs: &dyn HostFs, params: &[Value]) -> Result<Value, Value> {
    let Some(path) = params.first().and_then(Value::as_str).map(PathBuf::from) else {
        return Err(Value::from("fs_read: missing path"));
    };
    match classify(fs, &path) {
        Ok(FsRead::File(bytes)) => Ok(Value::Array(vec![
            Value::from("file"),
            Value::Binary(bytes),
        ])),
        Ok(FsRead::New) => Ok(Value::Array(vec![Value::from("new")])),
        Ok(FsRead::Dir { path, entries }) => Ok(Value::Array(vec![
            Value::from("dir"),
            Value::from(path),
            encode_dir_entries(entries),
        ])),
        Err(e) => Err(Value::from(e.to_string())),
    }
}

/// `[[is_dir, name], …]` — a directory's entries on the wire. The edit-host sorts and
/// renders them; the daemon only reports the raw `(is_dir, name)` pairs.
fn encode_dir_entries(entries: Vec<DirEntry>) -> Value {
    Value::Array(
        entries
            .into_iter()
            .map(|e| Value::Array(vec![Value::from(e.is_dir), Value::from(e.name)]))
            .collect(),
    )
}

/// Serve one `fs_write [path, bytes]` against `fs`: do the atomic write through the
/// same sync [`HostFs`] the local server uses, then re-stat so the reply carries the
/// new [`FileStat`] the edit-host stamps as its `disk` baseline. A write failure is a
/// loud error reply — the edit-host's saved-state clears *only* on the `["ok", …]`.
fn serve_write(fs: &dyn HostFs, params: &mut [Value]) -> Result<Value, Value> {
    let Some(path) = params.first().and_then(Value::as_str).map(PathBuf::from) else {
        return Err(Value::from("fs_write: missing path"));
    };
    let bytes = match params.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
        Some(Value::Binary(b)) => b,
        _ => return Err(Value::from("fs_write: missing bytes")),
    };
    match fs.write_atomic(&path, &bytes) {
        Ok(()) => {
            let stat = fs
                .stat(&path)
                .map(|s| encode_stat(&s))
                .unwrap_or(Value::Nil);
            Ok(Value::Array(vec![Value::from("ok"), stat]))
        }
        Err(e) => Err(Value::from(e.to_string())),
    }
}

/// Resolve `path` against `fs` to a [`FsRead`], using only the sync [`HostFs`]
/// surface (so a fake test backend and the real disk behave identically). A readable
/// directory becomes a `Dir` listing (the remote explorer); a `NotFound` is the
/// legitimate new-file case; any other read error propagates loudly.
fn classify(fs: &dyn HostFs, path: &Path) -> io::Result<FsRead> {
    if let Ok(entries) = fs.read_dir(path) {
        // A directory: list it for the remote explorer (Phase 3g). Canonicalize so the
        // edit-host's `../`/descend navigation is unambiguous; fall back to the given
        // path if it can't be resolved.
        let dir = fs.canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        return Ok(FsRead::Dir {
            path: dir.to_string_lossy().into_owned(),
            entries,
        });
    }
    match fs.open_read(path) {
        Ok(mut reader) => {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            Ok(FsRead::File(bytes))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FsRead::New),
        Err(e) => Err(e),
    }
}
