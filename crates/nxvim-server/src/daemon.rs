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
//! | edit-host → daemon | `fs_read [path]`         | `["file", bytes]` / `["new"]`, or an RPC error |
//! | edit-host → daemon | `fs_write [path, bytes]` | `["ok", stat?]`, or an RPC error                |
//!
//! `serve_fs_daemon` reads an existing file (`file`) or reports a not-yet-existing one
//! as a new-file buffer (`new`); a directory or any other read error comes back as a
//! loud RPC error (remote directory/explorer open is a later sub-slice). `fs_write`
//! does the atomic write through the same sync [`HostFs`] and replies with the new
//! [`FileStat`](nxvim_core::FileStat) (so the edit-host can stamp its `disk` snapshot
//! without a remote stat round-trip), or a loud error.
//!
//! **The save path is off-tick, like the read** (`docs/plans/…` → Phase 3e, *the save
//! slice*): core does *not* write through the sync [`HostFs`](nxvim_core::HostFs) in a
//! daemon session — it snapshots the buffer at command time and enqueues a
//! [`PendingSave`](nxvim_core::PendingSave); the server pushes those bytes over
//! `fs_write` off the editor tick and finalizes the buffer's saved-state only on the
//! daemon's ack, so a slow remote write never freezes typing. (`:edit` / `:read` and
//! remote directory listing still use the sync [`HostFs`], on local disk, for now.)

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

use nxvim_core::{FileStat, HostFs};
use nxvim_rpc::{connect, Incoming, Rpc};

use crate::evloop::LoopEvent;
use crate::host::{HostProc, ProcEvents, ProcSpec, StdHostProc};

const FS_READ: &str = "fs_read";
const FS_WRITE: &str = "fs_write";

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

/// What a daemon `fs_read` resolves a path to, for an initial buffer open. A read
/// error (a directory, a permission failure, a dead connection) is *not* one of
/// these — it surfaces as an `Err` the server echoes loudly, never a silent empty
/// buffer.
pub enum FsRead {
    /// An existing file's bytes — load them into the buffer (a replica of the remote).
    File(Vec<u8>),
    /// The path doesn't exist yet — open an empty new-file buffer named for it (the
    /// `:e newfile` case), so a first `:w` would create it.
    New,
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
/// dependency. Scoped to `read` for the initial-open slice; `stat` / `write` /
/// `read_dir` join it as the save and explorer paths cross the wire.
pub trait HostFsAsync: Send + Sync {
    /// Fetch `path`'s contents for an initial buffer open (or report it new).
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
}

/// A [`HostFsAsync`] that reads files from a remote daemon over the wire. `read`
/// issues an `fs_read` request and awaits the reply — a file read is naturally
/// request/response, so (unlike [`RemoteHostProc`]) there is no per-call demux:
/// [`nxvim_rpc`] routes each response to its awaiting `request` by msgid.
pub struct RemoteHostFs {
    rpc: Rpc,
}

impl RemoteHostFs {
    /// Connect to a daemon over `reader`/`writer`. The daemon sends only `fs_read`
    /// *responses* (which `nxvim_rpc` routes internally), never notifications, so a
    /// tiny drain task keeps the `Incoming` stream consumed — dropping the receiver
    /// would tear the connection down. RPC tasks live on the runtime this is called
    /// from, as for any [`nxvim_rpc::connect`].
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteHostFs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, mut incoming) = connect(reader, writer);
        tokio::spawn(async move { while incoming.recv().await.is_some() {} });
        RemoteHostFs { rpc }
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
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_read: unknown reply tag",
        )),
    }
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
    while let Some(msg) = incoming.recv().await {
        let Incoming::Request {
            id,
            method,
            mut params,
        } = msg
        else {
            continue; // the edit-host drives the fs daemon with requests only
        };
        let reply = match method.as_str() {
            FS_READ => serve_read(&*fs, &params),
            FS_WRITE => serve_write(&*fs, &mut params),
            other => Err(Value::from(format!("unknown method: {other}"))),
        };
        rpc.respond(id, reply);
    }
    Ok(())
}

/// Serve one `fs_read [path]` against `fs`, projecting [`classify`]'s result onto the
/// `["file", bytes]` / `["new"]` wire shape (or a loud error reply).
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
        Err(e) => Err(Value::from(e.to_string())),
    }
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
/// directory is a loud error — remote explorer open is a later slice; a `NotFound`
/// is the legitimate new-file case; any other read error propagates loudly.
fn classify(fs: &dyn HostFs, path: &Path) -> io::Result<FsRead> {
    if fs.read_dir(path).is_ok() {
        return Err(io::Error::other("remote directory open not yet supported"));
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
