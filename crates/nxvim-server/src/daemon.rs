//! The daemon wire protocol for the edit-host split (process + filesystem + blocking system).
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
//!
//! ## The blocking-system leg (`sys_run` — a blocking bridge)
//!
//! The *synchronous* `vim.system(...):wait()` (an LSP `root_dir` shelling out to `cargo
//! metadata`) must run **where the project files are** — the daemon — but unlike the
//! async `vim.system` (which rides the process leg above off-tick) the caller needs the
//! result *inline* on the Lua tick: it has no value to hand back later. So this leg is a
//! **blocking bridge** — request/response on the wire, but the edit-host parks its Lua
//! thread on the reply, with the wire's RPC tasks on their *own* OS thread so the parked
//! thread can't starve the reader carrying that reply (Open Decision #5's residual note):
//!
//! | direction | method | reply |
//! | --- | --- | --- |
//! | edit-host → daemon | `sys_run [argv, cwd?, env]` | `[code, stdout, stderr, pid?]`, or an RPC error |
//!
//! [`RemoteBlockingSystem`] (the edit-host side, a [`BlockingSystem`]) owns that dedicated
//! link thread; `serve_sys_daemon` runs each request through the *same*
//! [`StdBlockingSystem`](nxvim_lua::StdBlockingSystem) the local editor uses, on a
//! blocking-pool thread, so a process behaves identically run here or across the wire.
//!
//! ## The LSP leg (`lsp_*` — long-lived bidirectional pipes)
//!
//! A language server is neither run-to-completion (the `proc_*` leg) nor
//! request/response (`fs_*`/`sys_run`): it is a *long-lived child whose stdio is a raw
//! bidirectional pipe*, JSON-RPC flowing both ways for the server's whole life and
//! stdout consumed incrementally. So this leg streams the pipe itself — raw stdin/stdout/
//! stderr chunks correlated by a per-spawn `id`:
//!
//! | direction | method | params |
//! | --- | --- | --- |
//! | edit-host → daemon | `lsp_spawn` | `[id, program, args, cwd]` |
//! | edit-host → daemon | `lsp_stdin` | `[id, bytes]` |
//! | edit-host → daemon | `lsp_kill`  | `[id]` |
//! | daemon → edit-host | `lsp_stdout` | `[id, bytes]` |
//! | daemon → edit-host | `lsp_stderr` | `[id, bytes]` |
//! | daemon → edit-host | `lsp_exited` | `[id, code?, signal?]` |
//!
//! [`RemoteLspTransport`] (the edit-host side, an [`LspTransport`]) hands the
//! [`LspManager`](nxvim_lsp::LspManager) a [`LspChannel`] whose stdout/stderr are fed by
//! demuxed `lsp_stdout`/`lsp_stderr` chunks and whose stdin is pumped onto the wire as
//! `lsp_stdin` — so the manager drives its `async-lsp` loop unchanged, never knowing the
//! server runs across the network. `serve_lsp_daemon` spawns the actual child (the *same*
//! `tokio::process` machinery the local transport uses) and streams its pipes back; it
//! joins the stdout/stderr pumps before signaling `lsp_exited`, so no trailing output is
//! lost to the exit.

use std::collections::HashMap;
use std::future::Future;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, UNIX_EPOCH};

use rmpv::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use nxvim_core::{DirEntry, FileStat, HostFs};
use nxvim_lsp::{LspChannel, LspProcess, LspTransport, ServerSpawn};
use nxvim_lua::{BlockingSystem, FileKind, LuaDirEntry, LuaFs, LuaStat, SystemOutput, SystemSpec};
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
const PROC_STDOUT: &str = "proc_stdout";
const PROC_EXITED: &str = "proc_exited";

// The blocking-system leg (`sys_run`): a request/response shell-out that runs to
// completion on the daemon. Distinct from the process leg above — that one is
// event-routed (the async `vim.system` / `jobstart`), this one blocks the edit-host's
// Lua thread on the reply (the synchronous `vim.system(...):wait()`).
const SYS_RUN: &str = "sys_run";

// The LSP leg: a *long-lived bidirectional pipe* per language server. Unlike every
// other leg (run-to-completion `proc_*`, request/response `fs_*`/`sys_run`), a
// language server's stdio stays open for its whole life, with JSON-RPC flowing both
// ways and stdout consumed incrementally — so the wire streams raw stdin/stdout/stderr
// chunks correlated by a per-spawn `id`, never a single buffered result.
const LSP_SPAWN: &str = "lsp_spawn"; // edit-host → daemon: [id, program, args, cwd]
const LSP_STDIN: &str = "lsp_stdin"; // edit-host → daemon: [id, bytes]
const LSP_KILL: &str = "lsp_kill"; // edit-host → daemon: [id]
const LSP_STDOUT: &str = "lsp_stdout"; // daemon → edit-host: [id, bytes]
const LSP_STDERR: &str = "lsp_stderr"; // daemon → edit-host: [id, bytes]
const LSP_EXITED: &str = "lsp_exited"; // daemon → edit-host: [id, code?, signal?]

// The Lua-filesystem leg (`luafs`): a request/response per project-facing `vim.uv.fs_*`
// / `vim.fn` fs call, run to completion on the daemon. Like the sys leg it is a blocking
// bridge — the edit-host's Lua thread parks on the reply (the calls are synchronous) —
// but it carries the whole fs surface under one method, demuxed by an op tag in the
// request. The whole `["op", args…]` request maps to `["ok", payload] | ["err", msg]`.
const LUAFS: &str = "luafs";

/// What the daemon reports back about one child, demuxed off the wire and handed to
/// the [`RemoteHostProc::run`] future waiting on that spawn's `id`. Mirrors the two
/// [`ProcEvents`] reports the future then re-emits to the editor.
enum DaemonEvent {
    /// The child is running (or failed to spawn — `None` pid).
    Spawned(Option<u32>),
    /// A streaming child emitted a batch of stdout lines (`nx.spawn`'s `on_stdout`).
    Stdout(Vec<String>),
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
                        Some(DaemonEvent::Stdout(lines)) => {
                            if let Some(e) = &events {
                                e.stdout(lines);
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
            PROC_STDOUT => {
                if let Some((id, ev)) = decode_stdout(params) {
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
    let (rpc, incoming) = connect(reader, writer);
    serve_proc_daemon_on(rpc, incoming).await
}

/// The process leg's connection-agnostic core: drives the `proc_*` wire over a
/// pre-built shared [`Rpc`] + its own demuxed inbound stream. The single-stdio
/// daemon multiplexer ([`run_daemon_io`]) fans one connection across every leg's
/// `*_on`; [`serve_daemon`] is the standalone wrapper (its own connection) the
/// per-leg tests drive.
pub async fn serve_proc_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
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
                LoopEvent::ProcessStdout { id, lines } => reply.notify(
                    PROC_STDOUT,
                    vec![
                        Value::from(id),
                        Value::Array(lines.into_iter().map(Value::from).collect()),
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
        stream,
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
        Value::from(stream),
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
    // `stream` (6th param) may be absent from an older peer's frame — default
    // false (the one-shot `vim.system` shape).
    let stream = params.get(5).and_then(Value::as_bool).unwrap_or(false);
    Some((
        id,
        ProcSpec {
            argv,
            cwd,
            env,
            stdin,
            stream,
        },
    ))
}

/// `proc_spawned` params → `(id, Spawned)`. A nil/absent pid means the spawn failed.
fn decode_spawned(params: &[Value]) -> Option<(u64, DaemonEvent)> {
    let id = params.first()?.as_u64()?;
    let pid = params.get(1).and_then(Value::as_u64).map(|p| p as u32);
    Some((id, DaemonEvent::Spawned(pid)))
}

/// `proc_stdout` params → `(id, Stdout)` — a streaming child's batch of stdout lines.
fn decode_stdout(mut params: Vec<Value>) -> Option<(u64, DaemonEvent)> {
    let id = params.first()?.as_u64()?;
    let lines = match params.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
        Some(Value::Array(a)) => a
            .into_iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect(),
        _ => Vec::new(),
    };
    Some((id, DaemonEvent::Stdout(lines)))
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
    let (rpc, incoming) = connect(reader, writer);
    serve_fs_daemon_on(rpc, incoming, fs).await
}

/// The filesystem + watch leg's connection-agnostic core (see [`serve_proc_daemon_on`]
/// for why the `*_on` split exists). Drives `fs_read`/`fs_write`/`fs_watch` over a
/// shared [`Rpc`] + its demuxed inbound stream.
pub async fn serve_fs_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
    fs: Box<dyn HostFs + Send>,
) -> anyhow::Result<()> {
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

// ===== the blocking-system leg (`sys_run`) ==================================
//
// `vim.system(...):wait()` (the *synchronous* form) shells out **on the edit-host's
// Lua thread** and needs its result inline — an LSP `root_dir` running `cargo
// metadata` must run *where the project files are* (the daemon), but the caller can't
// go off-tick the way a buffer open does (it has no value to hand back later). So this
// leg is a **blocking bridge**: the edit-host parks its Lua thread on the reply, and —
// crucially — the wire's RPC tasks live on their *own* OS thread + runtime, so the
// parked thread can never starve the reader carrying its reply (the deadlock trap the
// plan's Open Decision #5 residual note calls out). The wire itself is a plain
// request/response, like the fs read:
//
// | direction | method | reply |
// | --- | --- | --- |
// | edit-host → daemon | `sys_run [argv, cwd?, env]` | `[code, stdout, stderr, pid?]`, or an RPC error |
//
// `serve_sys_daemon` runs each request through the *same* [`StdBlockingSystem`] the
// local editor uses today (on a blocking pool thread, so a long shell-out can't stall
// the reader), so a process behaves identically whether it ran here or across the wire.

/// A [`BlockingSystem`] that runs the synchronous `vim.system(...):wait()` shell-out on
/// a remote daemon instead of locally — the edit-host side of the `sys_run` leg.
///
/// The blocking bridge: [`run`](BlockingSystem::run) hands the spec to a **dedicated
/// link thread** (which owns the wire and its own current-thread runtime) over a plain
/// `std` channel, then parks the calling (Lua) thread on the reply. Parking with a
/// `std` channel — not a tokio primitive — is deliberate: `nx._system` runs *inside*
/// the server's tokio runtime, where a tokio `blocking_recv` would panic; a `std` recv
/// just parks the OS thread, and the link thread (a different thread entirely) is free
/// to drive the wire that delivers the reply.
///
/// `Send` (it holds only a `std::sync::mpsc::Sender`) so it rides
/// [`ServerInit`](crate::ServerInit) onto the server thread, where it is rebuilt into
/// the editor's `Rc<dyn BlockingSystem>`.
pub struct RemoteBlockingSystem {
    /// Into the link thread: a spec to run plus the one-shot reply channel the caller
    /// parks on. A **tokio** sender (not `std`) so the job-server can `await` it on the
    /// shared link runtime — the same channel feeds the dedicated-thread single-leg
    /// [`connect`](Self::connect) and the multiplexed [`connect_daemon`].
    req_tx: UnboundedSender<SysJob>,
}

/// One blocking shell-out queued onto the link thread: the spec, and the `std` channel
/// the parked editor thread waits on for its [`SystemOutput`]. The reply stays `std`
/// (not tokio) because the editor thread parks on it from *inside* the server runtime,
/// where a tokio recv would panic — a plain OS-thread park is what's wanted.
type SysJob = (SystemSpec, std::sync::mpsc::Sender<SystemOutput>);

/// The sys leg's job server: pull each queued shell-out off `req_rx` and drive its
/// `sys_run` request to completion over `rpc`, delivering the result to the parked
/// caller. Runs on whichever runtime drives it — the single-leg [`RemoteBlockingSystem::
/// connect`]'s dedicated thread, or the shared [`connect_daemon`] link thread — so the
/// editor thread (parked on the `std` reply) is never the one polling the reply.
async fn run_sys_jobs(rpc: Rpc, mut req_rx: UnboundedReceiver<SysJob>) {
    while let Some((spec, reply_tx)) = req_rx.recv().await {
        let out = match rpc.request(SYS_RUN, encode_sys_run(&spec)).await {
            Ok(v) => decode_sys_output(v),
            // A transport failure (daemon gone) degrades loudly to a `code = -1` result,
            // never a panic — `vim.system` callers rely on a value.
            Err(e) => SystemOutput::failed(format!("vim.system: daemon error: {e}")),
        };
        // The receiver is gone only if the caller was itself dropped mid-call; nothing
        // to deliver to, so discard.
        let _ = reply_tx.send(out);
    }
}

impl RemoteBlockingSystem {
    /// Connect to a daemon over `reader`/`writer`, spawning the dedicated link thread.
    /// That thread builds its **own** current-thread runtime, opens the RPC link there,
    /// and serves jobs one at a time (a blocking shell-out is serial by nature — the
    /// edit-host is parked until it returns), driving each `sys_run` request to
    /// completion on its runtime so the parked editor thread isn't the one that has to
    /// poll the reply. (The multiplexed [`connect_daemon`] runs [`run_sys_jobs`] as a
    /// task on the *shared* link runtime instead of owning a thread here.)
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteBlockingSystem
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (req_tx, req_rx) = unbounded_channel::<SysJob>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                // A runtime we can't build means the link is dead on arrival; the
                // `req_rx` is dropped, so every `run` sees the channel closed and
                // degrades loudly rather than hanging.
                Err(_) => return,
            };
            rt.block_on(async move {
                let (rpc, mut incoming) = connect(reader, writer);
                // Drain the incoming stream so the connection isn't torn down (dropping
                // the receiver would). The sys leg has no daemon→edit-host pushes, so
                // this only ever observes EOF — but the receiver must stay alive and
                // consumed for the link to live.
                tokio::spawn(async move { while incoming.recv().await.is_some() {} });
                run_sys_jobs(rpc, req_rx).await;
            });
        });
        RemoteBlockingSystem { req_tx }
    }
}

impl BlockingSystem for RemoteBlockingSystem {
    fn run(&self, spec: SystemSpec) -> SystemOutput {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self.req_tx.send((spec, reply_tx)).is_err() {
            return SystemOutput::failed("vim.system: daemon link is gone");
        }
        // Park the editor thread until the link thread delivers the daemon's reply. This
        // is the blocking bridge: `nx._system` is synchronous (the LSP `root_dir`
        // caller needs the value inline), so the tick blocks here, the same as a local
        // spawn blocks on `wait`. A `std` recv parks the OS thread without caring that
        // it sits inside the server's tokio runtime; the link's reader is on its own
        // thread, free to read the reply that unblocks us.
        reply_rx
            .recv()
            .unwrap_or_else(|_| SystemOutput::failed("vim.system: daemon link dropped the request"))
    }
}

/// `sys_run` request params: `[argv, cwd?, env]`, with `env` an array of `[k, v]`
/// pairs. The inverse of [`decode_sys_run`].
fn encode_sys_run(spec: &SystemSpec) -> Vec<Value> {
    let cmd = Value::Array(spec.cmd.iter().map(|s| Value::from(s.clone())).collect());
    let cwd = spec.cwd.clone().map_or(Value::Nil, Value::from);
    let env = Value::Array(
        spec.env
            .iter()
            .map(|(k, v)| Value::Array(vec![Value::from(k.clone()), Value::from(v.clone())]))
            .collect(),
    );
    vec![cmd, cwd, env]
}

/// `sys_run` request params → a [`SystemSpec`]. Tolerant of a malformed frame (a peer
/// is the same build): a missing argv yields an empty `cmd`, which the backend reports
/// as the "non-empty list" degrade.
fn decode_sys_run(params: &[Value]) -> SystemSpec {
    let cmd = params
        .first()
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let cwd = params.get(1).and_then(Value::as_str).map(str::to_string);
    let env = params
        .get(2)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let p = p.as_array()?;
                    Some((
                        p.first()?.as_str()?.to_string(),
                        p.get(1)?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    SystemSpec { cmd, cwd, env }
}

/// `[code, stdout, stderr, pid?]` reply → a [`SystemOutput`]. The inverse of
/// [`encode_sys_output`]; a malformed reply degrades loudly (`code = -1`).
fn decode_sys_output(v: Value) -> SystemOutput {
    let Value::Array(a) = v else {
        return SystemOutput::failed("sys_run: malformed reply");
    };
    let code = a.first().and_then(Value::as_i64).unwrap_or(-1) as i32;
    let stdout = a.get(1).map(value_bytes).unwrap_or_default();
    let stderr = a.get(2).map(value_bytes).unwrap_or_default();
    let pid = a.get(3).and_then(Value::as_u64).map(|p| p as u32);
    SystemOutput {
        code,
        stdout,
        stderr,
        pid,
    }
}

/// Raw bytes out of a wire value — `Binary` (the encoding we send) or, defensively, a
/// `String`. Anything else is empty.
fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Binary(b) => b.clone(),
        Value::String(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

/// `[code, stdout, stderr, pid?]` reply for a [`SystemOutput`]. `stdout`/`stderr` ride
/// as binary so non-UTF-8 output survives.
fn encode_sys_output(out: &SystemOutput) -> Value {
    Value::Array(vec![
        Value::from(out.code),
        Value::Binary(out.stdout.clone()),
        Value::Binary(out.stderr.clone()),
        out.pid.map_or(Value::Nil, Value::from),
    ])
}

/// Run the daemon end of the *blocking-system* wire over `reader`/`writer`, serving
/// `sys_run` requests through `sys` (the daemon's real backend —
/// [`StdBlockingSystem`](nxvim_lua::StdBlockingSystem) in the binary, a fake in tests).
/// Each run is offloaded to a blocking-pool thread so a long shell-out can't stall the
/// reader (the edit-host can have at most one blocking run in flight — it's parked —
/// but the offload keeps `incoming` responsive regardless). Returns when the connection
/// closes.
pub async fn serve_sys_daemon<R, W>(
    reader: R,
    writer: W,
    sys: Box<dyn BlockingSystem + Send + Sync>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect(reader, writer);
    serve_sys_daemon_on(rpc, incoming, sys).await
}

/// The blocking-system leg's connection-agnostic core (see [`serve_proc_daemon_on`]
/// for the `*_on` split). Serves `sys_run` over a shared [`Rpc`] + its demuxed stream.
pub async fn serve_sys_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
    sys: Box<dyn BlockingSystem + Send + Sync>,
) -> anyhow::Result<()> {
    let sys: Arc<dyn BlockingSystem + Send + Sync> = Arc::from(sys);
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, params } = msg {
            if method == SYS_RUN {
                let sys = sys.clone();
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let spec = decode_sys_run(&params);
                    let reply = match tokio::task::spawn_blocking(move || sys.run(spec)).await {
                        Ok(out) => Ok(encode_sys_output(&out)),
                        Err(e) => Err(Value::from(format!("sys_run: join error: {e}"))),
                    };
                    rpc.respond(id, reply);
                });
            } else {
                rpc.respond(id, Err(Value::from(format!("unknown method: {method}"))));
            }
        }
    }
    Ok(())
}

// ===== the LSP leg (long-lived bidirectional pipes) ===========================

/// Per-server routing on the edit-host side, keyed by the per-spawn `id`: where the
/// demux delivers a server's stdout/stderr chunks and its eventual exit. The
/// `stdout_tx`/`stderr_tx` feed the [`ChannelReader`]s the manager reads; dropping
/// them (on exit, or a dead link) is what EOFs those readers.
struct LspInflight {
    stdout_tx: UnboundedSender<Vec<u8>>,
    stderr_tx: UnboundedSender<Vec<u8>>,
    exit_tx: oneshot::Sender<(Option<i32>, Option<i32>)>,
}

/// The table of live servers awaiting their daemon reports: `id` → its routing. The
/// demux forwards each `lsp_stdout`/`lsp_stderr` to the matching sinks and fires
/// `exit_tx` (removing the entry) on `lsp_exited`.
type LspInflightMap = Arc<Mutex<HashMap<u64, LspInflight>>>;

/// An [`LspTransport`] that runs language servers on a remote daemon instead of
/// locally: each [`spawn`](LspTransport::spawn) tunnels the server's stdio over the
/// wire to a [`serve_lsp_daemon`] holding the actual child, so the
/// [`LspManager`](nxvim_lsp::LspManager) drives its `async-lsp` loop unchanged. The
/// drop-in for [`LocalLspTransport`](nxvim_lsp::LocalLspTransport) on the edit-host
/// side of the split — the long-lived bidirectional-pipe analogue of
/// [`RemoteHostProc`]'s run-to-completion path.
pub struct RemoteLspTransport {
    rpc: Rpc,
    inflight: LspInflightMap,
    /// Per-spawn correlation id minted here; the demux routes purely by it.
    next_id: AtomicU64,
}

impl RemoteLspTransport {
    /// Connect to a daemon over `reader`/`writer` (a duplex, or ssh stdio). Spawns
    /// the demux task that fans the daemon's stdout/stderr/exit out to per-server
    /// sinks; call it from within a tokio runtime (its RPC tasks live there).
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteLspTransport
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, incoming) = connect(reader, writer);
        let inflight: LspInflightMap = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(run_lsp_demux(incoming, inflight.clone()));
        RemoteLspTransport {
            rpc,
            inflight,
            next_id: AtomicU64::new(1),
        }
    }
}

impl LspTransport for RemoteLspTransport {
    fn spawn(
        &self,
        spec: &ServerSpawn,
        root: &Path,
    ) -> Pin<Box<dyn Future<Output = io::Result<LspChannel>> + Send>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let rpc = self.rpc.clone();
        let inflight = self.inflight.clone();
        let program = spec.program.clone();
        let args = spec.args.clone();
        let cwd = root.to_string_lossy().into_owned();
        Box::pin(async move {
            let (stdout_tx, stdout_rx) = unbounded_channel::<Vec<u8>>();
            let (stderr_tx, stderr_rx) = unbounded_channel::<Vec<u8>>();
            let (exit_tx, exit_rx) = oneshot::channel();
            // Register *before* the spawn request so a fast reply can't race ahead of
            // its sinks (mirrors [`RemoteHostProc::run`]).
            inflight.lock().unwrap().insert(
                id,
                LspInflight {
                    stdout_tx,
                    stderr_tx,
                    exit_tx,
                },
            );
            // client → server: the manager writes JSON-RPC into `stdin_writer`; a pump
            // reads the other end of the duplex and forwards each chunk as `lsp_stdin`.
            let (stdin_writer, stdin_reader) = tokio::io::duplex(1 << 16);
            tokio::spawn(pump_lsp_stdin(id, stdin_reader, rpc.clone()));
            rpc.notify(
                LSP_SPAWN,
                vec![
                    Value::from(id),
                    Value::from(program),
                    Value::Array(args.into_iter().map(Value::from).collect()),
                    Value::from(cwd),
                ],
            );
            Ok(LspChannel {
                stdout: Box::pin(ChannelReader::new(stdout_rx)),
                stdin: Box::pin(stdin_writer),
                stderr: Some(Box::pin(ChannelReader::new(stderr_rx))),
                process: Box::new(RemoteLspProcess { id, rpc, exit_rx }),
            })
        })
    }
}

/// The edit-host-side [`LspProcess`]: terminate the remote server (`lsp_kill`) and
/// await its exit, which the demux fires off `lsp_exited`. A dropped daemon link
/// drops the `exit_tx`, so `wait` resolves to `(None, None)` rather than hanging.
struct RemoteLspProcess {
    id: u64,
    rpc: Rpc,
    exit_rx: oneshot::Receiver<(Option<i32>, Option<i32>)>,
}

impl LspProcess for RemoteLspProcess {
    fn start_kill(&mut self) {
        self.rpc.notify(LSP_KILL, vec![Value::from(self.id)]);
    }

    fn wait(self: Box<Self>) -> nxvim_lsp::ExitFuture {
        Box::pin(async move { self.exit_rx.await.unwrap_or((None, None)) })
    }
}

/// Pump the daemon's per-server stdout/stderr/exit off the wire and route each to the
/// server it belongs to. On teardown (`incoming` ends) it clears [`LspInflightMap`],
/// dropping every sink (EOF the readers) and every `exit_tx` (so each waiting server
/// reports `(None, None)` rather than hanging).
async fn run_lsp_demux(mut incoming: UnboundedReceiver<Incoming>, inflight: LspInflightMap) {
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        route_lsp_notification(&inflight, &method, params);
    }
    inflight.lock().unwrap().clear();
}

/// Route one daemon→edit-host LSP notification (`lsp_stdout` / `lsp_stderr` /
/// `lsp_exited`) to the server it belongs to by `id`. Factored out of [`run_lsp_demux`]
/// so the multiplexed [`connect_daemon`] demux — which fans *all* legs off one shared
/// `incoming` — reuses the exact same routing. A method that isn't an LSP push is a
/// no-op. (`stdout`/`stderr` chunks queue onto the unbounded sinks in wire order, so the
/// `lsp_exited` remove-and-drop can't strand trailing output: the reader drains the
/// queued chunks before observing the sink's EOF.)
fn route_lsp_notification(inflight: &LspInflightMap, method: &str, params: Vec<Value>) {
    match method {
        LSP_STDOUT => {
            if let Some((id, bytes)) = decode_id_bytes(params) {
                if let Some(inf) = inflight.lock().unwrap().get(&id) {
                    let _ = inf.stdout_tx.send(bytes);
                }
            }
        }
        LSP_STDERR => {
            if let Some((id, bytes)) = decode_id_bytes(params) {
                if let Some(inf) = inflight.lock().unwrap().get(&id) {
                    let _ = inf.stderr_tx.send(bytes);
                }
            }
        }
        LSP_EXITED => {
            if let Some((id, code, signal)) = decode_lsp_exited(&params) {
                if let Some(inf) = inflight.lock().unwrap().remove(&id) {
                    let _ = inf.exit_tx.send((code, signal));
                }
            }
        }
        _ => {}
    }
}

/// Forward everything the manager writes to a server's stdin onto the wire as
/// `lsp_stdin` chunks, until the duplex closes (the manager's loop ended).
async fn pump_lsp_stdin(id: u64, mut reader: DuplexStream, rpc: Rpc) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => rpc.notify(
                LSP_STDIN,
                vec![Value::from(id), Value::Binary(buf[..n].to_vec())],
            ),
        }
    }
}

/// A [`tokio::io::AsyncRead`] fed by an unbounded channel of byte chunks — the bridge
/// from the demux (which receives discrete `lsp_stdout`/`lsp_stderr` *messages*) to the
/// streaming reader the manager's `async-lsp` loop expects. Buffers one chunk across
/// reads; a closed channel reads as EOF.
struct ChannelReader {
    rx: UnboundedReceiver<Vec<u8>>,
    chunk: Vec<u8>,
    pos: usize,
}

impl ChannelReader {
    fn new(rx: UnboundedReceiver<Vec<u8>>) -> ChannelReader {
        ChannelReader {
            rx,
            chunk: Vec::new(),
            pos: 0,
        }
    }
}

impl AsyncRead for ChannelReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.pos < this.chunk.len() {
                let n = std::cmp::min(buf.remaining(), this.chunk.len() - this.pos);
                buf.put_slice(&this.chunk[this.pos..this.pos + n]);
                this.pos += n;
                return Poll::Ready(Ok(()));
            }
            match this.rx.poll_recv(cx) {
                Poll::Ready(Some(chunk)) => {
                    if chunk.is_empty() {
                        continue; // a stray empty chunk would falsely read as EOF
                    }
                    this.chunk = chunk;
                    this.pos = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())), // sinks dropped → EOF
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Run the daemon end of the LSP wire over `reader`/`writer`: spawn the language
/// servers a far edit-host asks for and stream their stdio back. Returns when the
/// connection closes. Each child runs through the *same* `tokio::process` machinery
/// the local transport uses, so a server behaves identically remote and local.
pub async fn serve_lsp_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect(reader, writer);
    serve_lsp_daemon_on(rpc, incoming).await
}

/// The LSP leg's connection-agnostic core (see [`serve_proc_daemon_on`] for the `*_on`
/// split). Streams the `lsp_*` raw bidirectional pipe over a shared [`Rpc`] + its
/// demuxed inbound stream.
pub async fn serve_lsp_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
    // Per-child stdin channels and kill signals, keyed by the edit-host's spawn id, so
    // `lsp_stdin`/`lsp_kill` can reach the running child (mirrors the process leg's maps).
    let mut stdins: HashMap<u64, UnboundedSender<Vec<u8>>> = HashMap::new();
    let mut kills: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the edit-host drives the daemon with notifications only
        };
        match method.as_str() {
            LSP_SPAWN => {
                if let Some((id, program, args, cwd)) = decode_lsp_spawn(params) {
                    let (stdin_tx, stdin_rx) = unbounded_channel::<Vec<u8>>();
                    let (kill_tx, kill_rx) = oneshot::channel();
                    stdins.insert(id, stdin_tx);
                    kills.insert(id, kill_tx);
                    tokio::spawn(serve_one_lsp(
                        id,
                        program,
                        args,
                        cwd,
                        stdin_rx,
                        kill_rx,
                        rpc.clone(),
                    ));
                }
            }
            LSP_STDIN => {
                if let Some((id, bytes)) = decode_id_bytes(params) {
                    if let Some(tx) = stdins.get(&id) {
                        let _ = tx.send(bytes);
                    }
                }
            }
            LSP_KILL => {
                if let Some(id) = params.first().and_then(Value::as_u64) {
                    if let Some(kill_tx) = kills.remove(&id) {
                        let _ = kill_tx.send(());
                    }
                }
            }
            _ => {}
        }
        // Forget channels whose child tasks have closed them (the child exited), the
        // same leak guard the process leg applies.
        stdins.retain(|_, tx| !tx.is_closed());
        kills.retain(|_, tx| !tx.is_closed());
    }
    Ok(())
}

/// Run one language server to completion (or until killed) on the daemon, streaming its
/// stdout/stderr onto the wire and feeding its stdin from `stdin_rx`. Joins the
/// stdout/stderr pumps *before* sending `lsp_exited`, so the edit-host (which EOFs its
/// reader on exit) never loses trailing output.
async fn serve_one_lsp(
    id: u64,
    program: String,
    args: Vec<String>,
    cwd: String,
    mut stdin_rx: UnboundedReceiver<Vec<u8>>,
    mut kill_rx: oneshot::Receiver<()>,
    rpc: Rpc,
) {
    let mut command = tokio::process::Command::new(&program);
    command
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !cwd.is_empty() {
        command.current_dir(&cwd);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_e) => {
            // A spawn failure reports a bare exit (no code) — the edit-host's reader
            // EOFs and the manager reports the failure during initialize, the same way
            // a local spawn error does.
            rpc.notify(LSP_EXITED, vec![Value::from(id), Value::Nil, Value::Nil]);
            return;
        }
    };
    let mut stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut out_handle =
        stdout.map(|out| tokio::spawn(pump_child_output(out, id, LSP_STDOUT, rpc.clone())));
    let mut err_handle =
        stderr.map(|err| tokio::spawn(pump_child_output(err, id, LSP_STDERR, rpc.clone())));
    let stdin_task = tokio::spawn(async move {
        if let Some(sink) = stdin.as_mut() {
            while let Some(bytes) = stdin_rx.recv().await {
                if sink.write_all(&bytes).await.is_err() || sink.flush().await.is_err() {
                    break;
                }
            }
            let _ = sink.shutdown().await; // close → the server reads EOF
        }
    });
    let mut killed = false;
    let status = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            // Disable the arm once fired (re-polling a consumed oneshot busy-loops);
            // the child still exits via `child.wait()` after the kill takes effect.
            _ = &mut kill_rx, if !killed => {
                killed = true;
                let _ = child.start_kill();
            }
        }
    };
    // Flush all stdout/stderr onto the wire *before* signaling exit.
    if let Some(h) = out_handle.take() {
        let _ = h.await;
    }
    if let Some(h) = err_handle.take() {
        let _ = h.await;
    }
    stdin_task.abort();
    let (code, signal) = lsp_exit_code_signal(status);
    rpc.notify(
        LSP_EXITED,
        vec![
            Value::from(id),
            code.map_or(Value::Nil, Value::from),
            signal.map_or(Value::Nil, Value::from),
        ],
    );
}

/// Stream a child's stdout (or stderr) onto the wire as `method` chunks until it
/// closes (the child exited). Stops on the first read error or EOF.
async fn pump_child_output<R>(mut src: R, id: u64, method: &'static str, rpc: Rpc)
where
    R: AsyncRead + Unpin,
{
    let mut buf = [0u8; 8192];
    loop {
        match src.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => rpc.notify(
                method,
                vec![Value::from(id), Value::Binary(buf[..n].to_vec())],
            ),
        }
    }
}

/// Split a child's [`std::process::ExitStatus`] into `(code, signal)` for the
/// `lsp_exited` wire (the daemon-side analogue of `nxvim-lsp`'s `exit_code_signal`).
fn lsp_exit_code_signal(status: Option<std::process::ExitStatus>) -> (Option<i32>, Option<i32>) {
    let Some(status) = status else {
        return (None, None);
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        (status.code(), status.signal())
    }
    #[cfg(not(unix))]
    {
        (status.code(), None)
    }
}

/// `lsp_spawn` params → `(id, program, args, cwd)`, or `None` on a malformed frame.
fn decode_lsp_spawn(mut params: Vec<Value>) -> Option<(u64, String, Vec<String>, String)> {
    if params.len() < 4 {
        return None;
    }
    let id = params[0].as_u64()?;
    let program = params[1].as_str()?.to_string();
    let args = match std::mem::replace(&mut params[2], Value::Nil) {
        Value::Array(a) => a
            .into_iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => return None,
    };
    let cwd = params[3].as_str().unwrap_or("").to_string();
    Some((id, program, args, cwd))
}

/// `[id, bytes]` → `(id, bytes)`, moving the (potentially large) payload out. Used by
/// both `lsp_stdin` (daemon side) and `lsp_stdout`/`lsp_stderr` (edit-host side).
fn decode_id_bytes(mut params: Vec<Value>) -> Option<(u64, Vec<u8>)> {
    if params.len() < 2 {
        return None;
    }
    let id = params[0].as_u64()?;
    let bytes = match std::mem::replace(&mut params[1], Value::Nil) {
        Value::Binary(b) => b,
        Value::String(s) => s.into_bytes(),
        _ => Vec::new(),
    };
    Some((id, bytes))
}

/// `lsp_exited` params → `(id, code?, signal?)`. A nil code/signal stays `None`.
fn decode_lsp_exited(params: &[Value]) -> Option<(u64, Option<i32>, Option<i32>)> {
    let id = params.first()?.as_u64()?;
    let code = params.get(1).and_then(Value::as_i64).map(|c| c as i32);
    let signal = params.get(2).and_then(Value::as_i64).map(|s| s as i32);
    Some((id, code, signal))
}

// ----- the Lua-filesystem leg (`luafs`) -------------------------------------------
//
// `RemoteLuaFs` (the edit-host side, a [`LuaFs`]) is the fs analogue of
// [`RemoteBlockingSystem`]: a dedicated link thread owns the wire and its own
// current-thread runtime, each call hands its `["op", args…]` request to that thread
// over a `std` channel and **parks the Lua thread** on the reply. `serve_luafs_daemon`
// runs each op through the daemon's real [`StdLuaFs`](nxvim_lua::StdLuaFs) on a blocking
// pool thread, so the daemon — not the edit-host — owns the open-fd table the `i64`
// tokens index, and a plugin reads the *remote* project byte-for-byte.

/// One queued fs request on the link thread: the `["op", args…]` params and the `std`
/// channel the parked editor thread waits on for the reply (`Ok` value or a transport
/// error string). The reply stays `std` for the same reason as [`SysJob`] — the editor
/// thread parks on it from inside the server runtime.
type LuaFsJob = (Vec<Value>, std::sync::mpsc::Sender<Result<Value, String>>);

/// The luafs leg's job server: pull each queued `["op", args…]` request off `req_rx`,
/// drive it to completion over `rpc`, and deliver the raw reply (or a transport-error
/// string) to the parked caller. Runs on whichever runtime drives it — the single-leg
/// dedicated thread or the shared [`connect_daemon`] link runtime (mirrors
/// [`run_sys_jobs`]).
async fn run_luafs_jobs(rpc: Rpc, mut req_rx: UnboundedReceiver<LuaFsJob>) {
    while let Some((params, reply_tx)) = req_rx.recv().await {
        let reply = rpc.request(LUAFS, params).await.map_err(|e| e.to_string());
        let _ = reply_tx.send(reply);
    }
}

/// A [`LuaFs`] that runs the project-facing Lua fs surface on a remote daemon instead of
/// locally — the edit-host side of the `luafs` leg. The blocking bridge mirrors
/// [`RemoteBlockingSystem`]: synchronous calls park the editor thread on the daemon
/// reply, with the wire's RPC tasks on their own thread so the park can't deadlock.
///
/// `Send` (it holds only a tokio [`UnboundedSender`]) so it rides
/// [`ServerInit`](crate::ServerInit) onto the server thread, where it is rebuilt into the
/// editor's `Rc<dyn LuaFs>`.
pub struct RemoteLuaFs {
    req_tx: UnboundedSender<LuaFsJob>,
}

impl RemoteLuaFs {
    /// Connect to a daemon over `reader`/`writer`, spawning the dedicated link thread
    /// (its own current-thread runtime + the RPC link). Calls are serial by nature (the
    /// edit-host parks until each returns), so the thread serves one job at a time,
    /// driving each `luafs` request to completion on its runtime. (The multiplexed
    /// [`connect_daemon`] runs [`run_luafs_jobs`] on the *shared* link runtime instead.)
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteLuaFs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (req_tx, req_rx) = unbounded_channel::<LuaFsJob>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                // A runtime we can't build means the link is dead on arrival; `req_rx`
                // drops, so every `call` sees the channel closed and degrades loudly.
                Err(_) => return,
            };
            rt.block_on(async move {
                let (rpc, mut incoming) = connect(reader, writer);
                // The luafs leg has no daemon→edit-host pushes; drain so the connection
                // isn't torn down (dropping the receiver would).
                tokio::spawn(async move { while incoming.recv().await.is_some() {} });
                run_luafs_jobs(rpc, req_rx).await;
            });
        });
        RemoteLuaFs { req_tx }
    }

    /// Send `params` (an `["op", args…]` request) to the link thread and park on the
    /// reply, decoding the daemon's `["ok", payload] | ["err", msg]` envelope back into an
    /// `io::Result`. A dropped link / transport failure degrades to a loud `io::Error`.
    fn call(&self, params: Vec<Value>) -> io::Result<Value> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        if self.req_tx.send((params, reply_tx)).is_err() {
            return Err(io::Error::other("luafs: daemon link is gone"));
        }
        match reply_rx.recv() {
            Ok(Ok(v)) => decode_luafs_reply(v),
            Ok(Err(e)) => Err(io::Error::other(format!("luafs: daemon error: {e}"))),
            Err(_) => Err(io::Error::other("luafs: daemon link dropped the request")),
        }
    }
}

impl LuaFs for RemoteLuaFs {
    fn open(&self, path: &str, flags: &str, mode: u32) -> io::Result<i64> {
        self.call(vec![
            Value::from("open"),
            Value::from(path.to_string()),
            Value::from(flags.to_string()),
            Value::from(mode as u64),
        ])
        .map(|v| v.as_i64().unwrap_or(-1))
    }

    fn close(&self, fd: i64) -> io::Result<()> {
        self.call(vec![Value::from("close"), Value::from(fd)])
            .map(|_| ())
    }

    fn read(&self, fd: i64, size: usize, offset: Option<i64>) -> io::Result<Vec<u8>> {
        self.call(vec![
            Value::from("read"),
            Value::from(fd),
            Value::from(size as u64),
            offset.map_or(Value::Nil, Value::from),
        ])
        .map(|v| value_bytes(&v))
    }

    fn write(&self, fd: i64, data: &[u8], offset: Option<i64>) -> io::Result<usize> {
        self.call(vec![
            Value::from("write"),
            Value::from(fd),
            Value::Binary(data.to_vec()),
            offset.map_or(Value::Nil, Value::from),
        ])
        .map(|v| v.as_u64().unwrap_or(0) as usize)
    }

    fn fstat(&self, fd: i64) -> io::Result<LuaStat> {
        self.call(vec![Value::from("fstat"), Value::from(fd)])
            .and_then(|v| decode_lua_stat(&v))
    }

    fn stat(&self, path: &str) -> io::Result<LuaStat> {
        self.call(vec![Value::from("stat"), Value::from(path.to_string())])
            .and_then(|v| decode_lua_stat(&v))
    }

    fn lstat(&self, path: &str) -> io::Result<LuaStat> {
        self.call(vec![Value::from("lstat"), Value::from(path.to_string())])
            .and_then(|v| decode_lua_stat(&v))
    }

    fn scandir(&self, path: &str) -> io::Result<Vec<LuaDirEntry>> {
        self.call(vec![Value::from("scandir"), Value::from(path.to_string())])
            .map(|v| decode_lua_entries(&v))
    }

    fn mkdir(&self, path: &str, mode: u32, recursive: bool) -> io::Result<()> {
        self.call(vec![
            Value::from("mkdir"),
            Value::from(path.to_string()),
            Value::from(mode as u64),
            Value::from(recursive),
        ])
        .map(|_| ())
    }

    fn rmdir(&self, path: &str) -> io::Result<()> {
        self.call(vec![Value::from("rmdir"), Value::from(path.to_string())])
            .map(|_| ())
    }

    fn unlink(&self, path: &str) -> io::Result<()> {
        self.call(vec![Value::from("unlink"), Value::from(path.to_string())])
            .map(|_| ())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.call(vec![
            Value::from("rename"),
            Value::from(from.to_string()),
            Value::from(to.to_string()),
        ])
        .map(|_| ())
    }

    fn copyfile(&self, src: &str, dest: &str, excl: bool) -> io::Result<()> {
        self.call(vec![
            Value::from("copyfile"),
            Value::from(src.to_string()),
            Value::from(dest.to_string()),
            Value::from(excl),
        ])
        .map(|_| ())
    }

    fn utime(&self, path: &str, atime: f64, mtime: f64) -> io::Result<()> {
        self.call(vec![
            Value::from("utime"),
            Value::from(path.to_string()),
            Value::F64(atime),
            Value::F64(mtime),
        ])
        .map(|_| ())
    }

    fn access(&self, path: &str, modes: &str) -> bool {
        // Never errors (libuv semantics); a transport failure degrades to `false`.
        self.call(vec![
            Value::from("access"),
            Value::from(path.to_string()),
            Value::from(modes.to_string()),
        ])
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    }

    fn realpath(&self, path: &str) -> io::Result<String> {
        self.call(vec![Value::from("realpath"), Value::from(path.to_string())])
            .and_then(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| io::Error::other("luafs: malformed realpath reply"))
            })
    }

    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        self.call(vec![
            Value::from("read_file"),
            Value::from(path.to_string()),
        ])
        .map(|v| value_bytes(&v))
    }

    fn which(&self, name: &str) -> Option<String> {
        // Never errors; a miss (or transport failure) is `None`.
        self.call(vec![Value::from("which"), Value::from(name.to_string())])
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
    }
}

/// Decode the daemon's `["ok", payload] | ["err", msg]` envelope into an `io::Result`.
fn decode_luafs_reply(v: Value) -> io::Result<Value> {
    let Value::Array(a) = &v else {
        return Err(io::Error::other("luafs: malformed reply"));
    };
    match a.first().and_then(Value::as_str) {
        Some("ok") => Ok(a.get(1).cloned().unwrap_or(Value::Nil)),
        Some("err") => Err(io::Error::other(
            a.get(1)
                .and_then(Value::as_str)
                .unwrap_or("luafs error")
                .to_string(),
        )),
        _ => Err(io::Error::other("luafs: malformed reply tag")),
    }
}

/// `LuaStat` → its flat wire array: `[kind, size, mode, mtime_sec, mtime_nsec,
/// atime_sec, atime_nsec, ino, uid, gid, nlink, dev]`. A `None` time has a `Nil` sec.
fn encode_lua_stat(st: &LuaStat) -> Value {
    let time = |t: Option<(i64, u32)>| match t {
        Some((sec, nsec)) => (Value::from(sec), Value::from(nsec as u64)),
        None => (Value::Nil, Value::from(0u64)),
    };
    let (mts, mtn) = time(st.mtime);
    let (ats, atn) = time(st.atime);
    Value::Array(vec![
        Value::from(st.kind.as_str()),
        Value::from(st.size),
        Value::from(st.mode as u64),
        mts,
        mtn,
        ats,
        atn,
        Value::from(st.ino),
        Value::from(st.uid as u64),
        Value::from(st.gid as u64),
        Value::from(st.nlink),
        Value::from(st.dev),
    ])
}

/// The inverse of [`encode_stat`]; a malformed array is a loud error.
fn decode_lua_stat(v: &Value) -> io::Result<LuaStat> {
    let a = v
        .as_array()
        .ok_or_else(|| io::Error::other("luafs: malformed stat reply"))?;
    let kind = FileKind::from_wire(a.first().and_then(Value::as_str).unwrap_or("file"));
    let u64_at = |i: usize| a.get(i).and_then(Value::as_u64).unwrap_or(0);
    let time = |s: usize, n: usize| {
        a.get(s)
            .and_then(Value::as_i64)
            .map(|sec| (sec, a.get(n).and_then(Value::as_u64).unwrap_or(0) as u32))
    };
    Ok(LuaStat {
        kind,
        size: u64_at(1),
        mode: u64_at(2) as u32,
        mtime: time(3, 4),
        atime: time(5, 6),
        ino: u64_at(7),
        uid: u64_at(8) as u32,
        gid: u64_at(9) as u32,
        nlink: u64_at(10),
        dev: u64_at(11),
    })
}

/// Directory entries → an array of `[name, kind]` pairs.
fn encode_lua_entries(entries: &[LuaDirEntry]) -> Value {
    Value::Array(
        entries
            .iter()
            .map(|e| {
                Value::Array(vec![
                    Value::from(e.name.clone()),
                    Value::from(e.kind.as_str()),
                ])
            })
            .collect(),
    )
}

/// The inverse of [`encode_dir_entries`]; a malformed entry is skipped.
fn decode_lua_entries(v: &Value) -> Vec<LuaDirEntry> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let e = e.as_array()?;
                    Some(LuaDirEntry {
                        name: e.first()?.as_str()?.to_string(),
                        kind: FileKind::from_wire(
                            e.get(1).and_then(Value::as_str).unwrap_or("file"),
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Run one `["op", args…]` request through `fs` and shape the `["ok", payload] |
/// ["err", msg]` envelope. The op set is the whole [`LuaFs`] surface; `access`/`which`
/// never error, so they always report `ok`.
fn serve_luafs_op(fs: &dyn LuaFs, params: &[Value]) -> Value {
    let op = params.first().and_then(Value::as_str).unwrap_or("");
    let s = |i: usize| {
        params
            .get(i)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let i64_at = |i: usize| params.get(i).and_then(Value::as_i64).unwrap_or(0);
    let u32_at = |i: usize| params.get(i).and_then(Value::as_u64).unwrap_or(0) as u32;
    let opt_off = |i: usize| params.get(i).and_then(Value::as_i64);
    let bool_at = |i: usize| params.get(i).and_then(Value::as_bool).unwrap_or(false);
    let f64_at = |i: usize| params.get(i).and_then(Value::as_f64).unwrap_or(0.0);

    // `access` / `which` are infallible (libuv semantics) — report them directly.
    match op {
        "access" => return luafs_ok(Value::from(fs.access(&s(1), &s(2)))),
        "which" => return luafs_ok(fs.which(&s(1)).map_or(Value::Nil, Value::from)),
        _ => {}
    }

    let result: io::Result<Value> = match op {
        "open" => fs.open(&s(1), &s(2), u32_at(3)).map(Value::from),
        "close" => fs.close(i64_at(1)).map(|_| Value::Nil),
        "read" => fs
            .read(i64_at(1), u32_at(2) as usize, opt_off(3))
            .map(Value::Binary),
        "write" => fs
            .write(i64_at(1), &value_bytes(&params[2]), opt_off(3))
            .map(|n| Value::from(n as u64)),
        "fstat" => fs.fstat(i64_at(1)).map(|st| encode_lua_stat(&st)),
        "stat" => fs.stat(&s(1)).map(|st| encode_lua_stat(&st)),
        "lstat" => fs.lstat(&s(1)).map(|st| encode_lua_stat(&st)),
        "scandir" => fs.scandir(&s(1)).map(|e| encode_lua_entries(&e)),
        "mkdir" => fs.mkdir(&s(1), u32_at(2), bool_at(3)).map(|_| Value::Nil),
        "rmdir" => fs.rmdir(&s(1)).map(|_| Value::Nil),
        "unlink" => fs.unlink(&s(1)).map(|_| Value::Nil),
        "rename" => fs.rename(&s(1), &s(2)).map(|_| Value::Nil),
        "copyfile" => fs.copyfile(&s(1), &s(2), bool_at(3)).map(|_| Value::Nil),
        "utime" => fs.utime(&s(1), f64_at(2), f64_at(3)).map(|_| Value::Nil),
        "realpath" => fs.realpath(&s(1)).map(Value::from),
        "read_file" => fs.read_file(&s(1)).map(Value::Binary),
        other => Err(io::Error::other(format!("luafs: unknown op '{other}'"))),
    };
    match result {
        Ok(payload) => luafs_ok(payload),
        Err(e) => Value::Array(vec![Value::from("err"), Value::from(e.to_string())]),
    }
}

/// Wrap a payload in the success envelope.
fn luafs_ok(payload: Value) -> Value {
    Value::Array(vec![Value::from("ok"), payload])
}

/// Run the daemon end of the *Lua-filesystem* wire over `reader`/`writer`, serving
/// `luafs` requests through `fs` (the daemon's real backend —
/// [`StdLuaFs`](nxvim_lua::StdLuaFs) in the binary, a virtual fs in tests). Each op is
/// offloaded to a blocking-pool thread so a slow fs call can't stall the reader; the
/// `fs` is shared (it owns the open-fd table the `i64` tokens index, so an `fs_open`
/// here is read back by a later `fs_read`). Returns when the connection closes.
pub async fn serve_luafs_daemon<R, W>(
    reader: R,
    writer: W,
    fs: Box<dyn LuaFs + Send + Sync>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect(reader, writer);
    serve_luafs_daemon_on(rpc, incoming, fs).await
}

/// The Lua-visible-fs leg's connection-agnostic core (see [`serve_proc_daemon_on`] for
/// the `*_on` split). Serves the `luafs` request over a shared [`Rpc`] + its demuxed stream.
pub async fn serve_luafs_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
    fs: Box<dyn LuaFs + Send + Sync>,
) -> anyhow::Result<()> {
    let fs: Arc<dyn LuaFs + Send + Sync> = Arc::from(fs);
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, params } = msg {
            if method == LUAFS {
                let fs = fs.clone();
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let reply =
                        match tokio::task::spawn_blocking(move || serve_luafs_op(&*fs, &params))
                            .await
                        {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Value::from(format!("luafs: join error: {e}"))),
                        };
                    rpc.respond(id, reply);
                });
            } else {
                rpc.respond(id, Err(Value::from(format!("unknown method: {method}"))));
            }
        }
    }
    Ok(())
}

// ============================================================================
// The edit-host-side multiplexer
// ============================================================================
//
// Each `Remote*::connect` above opens its own connection — fine for the per-leg tests,
// where each leg gets a private duplex, but the real edit-host talks to *one* daemon
// over *one* transport. `connect_daemon` is the symmetric counterpart of the daemon's
// `run_daemon_io` multiplexer: it `connect`s once and hands back all five seams sharing
// that single link, so one `ServerInit` populates `host_fs_async` / `host_proc` /
// `blocking_system` / `lsp_transport` / `lua_fs` from a single `--daemon` child.
//
// Two properties make this a clean router, not a rework (both verified in the code):
// the daemon→edit-host *notifications* split into disjoint method namespaces
// (`proc_spawned`/`proc_exited`, `fs_changed`, `lsp_stdout`/`lsp_stderr`/`lsp_exited`),
// and request *responses* (`fs_read`/`fs_write`/`sys_run`/`luafs`) are msgid-routed
// *inside* [`Rpc`] and never surface as an [`Incoming`] — so one demux over the shared
// `incoming` covers every leg, and concurrent writes from all legs serialize through
// `Rpc`'s single out-channel.

/// The five edit-host seams of one daemon connection, all sharing a single link (see
/// [`connect_daemon`]). Each field drops straight into the matching
/// [`ServerInit`](crate::ServerInit) slot.
pub struct DaemonClient {
    /// The async filesystem seam (`fs_read`/`fs_write`) + the `fs_changed` watch push.
    pub host_fs: RemoteHostFs,
    /// The event-routed process seam (the async `vim.system` / `jobstart` / `:!`).
    pub host_proc: RemoteHostProc,
    /// The blocking-bridge `sys_run` seam (synchronous `vim.system(...):wait()`).
    pub blocking_system: RemoteBlockingSystem,
    /// The streaming-pipe LSP seam (`lsp_*`).
    pub lsp_transport: RemoteLspTransport,
    /// The blocking-bridge `luafs` seam (project-facing `vim.uv.fs_*` / `vim.fn` fs).
    pub lua_fs: RemoteLuaFs,
}

/// Connect to a single daemon over `reader`/`writer` and return all five edit-host
/// seams sharing that one link — the edit-host-side multiplexer (the symmetric twin of
/// the daemon's [`run_daemon_io`](crate::run_daemon_io)). The transport is any
/// [`AsyncRead`]/[`AsyncWrite`] pair: the real `--daemon` binary's stdio (how
/// `daemon_stdio.rs` drives it), an in-process duplex, or the QUIC stream of the future
/// listener.
///
/// **Why a dedicated link thread.** The connection runs on its *own* OS thread + a
/// current-thread runtime — not the server runtime — because the two blocking bridges
/// (`sys_run`, `luafs`) park the editor/Lua thread on a `std` reply channel, and that
/// parked thread *is* the server runtime; the wire must be driven elsewhere or the park
/// would starve the reader carrying its own reply (the deadlock trap from Open
/// Decision #5). On this one shared thread we run both blocking-bridge job servers
/// ([`run_sys_jobs`] / [`run_luafs_jobs`]) and the single [`run_client_demux`] that fans
/// every daemon→edit-host notification to the right leg — collapsing what used to be a
/// separate link thread per bridge plus a demux task per async leg onto one connection.
/// The async legs (`host_fs`/`host_proc`/`lsp_transport`) hold clones of the shared
/// [`Rpc`] and issue their requests/notifications from the server runtime; the actual
/// wire I/O always happens here.
pub fn connect_daemon<R, W>(reader: R, writer: W) -> DaemonClient
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // The link thread builds the seams (it owns the `Rpc` the async legs clone) and
    // hands the `DaemonClient` back out; a `std` channel lets a non-async caller block
    // briefly for it. Everything in `DaemonClient` is `Send`.
    let (client_tx, client_rx) = std::sync::mpsc::channel::<DaemonClient>();

    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            // A runtime we can't build leaves `client_tx` dropped; the caller's `recv`
            // errors and `connect_daemon` fails loud (a basic OS-capability failure, not
            // a recoverable daemon condition).
            Err(_) => return,
        };
        rt.block_on(async move {
            let (rpc, incoming) = connect(reader, writer);
            serve_daemon_link(rpc, incoming, client_tx).await;
        });
    });

    client_rx
        .recv()
        .expect("connect_daemon: the link thread could not build a tokio runtime")
}

/// Build the five edit-host seams over an already-connected `(rpc, incoming)` pair,
/// hand the [`DaemonClient`] out on `client_tx`, then drive the link — the two
/// blocking-bridge job servers ([`run_sys_jobs`] / [`run_luafs_jobs`]) plus the single
/// [`run_client_demux`] — until the daemon hangs up. This is the transport-agnostic
/// heart of [`connect_daemon`]; the QUIC connector ([`crate::quic::connect_quic`]) calls
/// it with the halves of a QUIC bidi stream, the link thread keeping the endpoint +
/// connection alive in scope. Must run on the dedicated link thread's runtime (the
/// blocking bridges park the editor thread on a `std` reply channel — see
/// [`connect_daemon`]).
pub(crate) async fn serve_daemon_link(
    rpc: Rpc,
    incoming: UnboundedReceiver<Incoming>,
    client_tx: std::sync::mpsc::Sender<DaemonClient>,
) {
    // Notification-routed legs: shared state the demux forwards into.
    let proc_inflight: Inflight = Arc::new(Mutex::new(HashMap::new()));
    let lsp_inflight: LspInflightMap = Arc::new(Mutex::new(HashMap::new()));
    let (watch_tx, watch_rx) = unbounded_channel::<WatchEvent>();

    // Blocking-bridge legs: the job channels their seams send onto and their job
    // servers (below) pull from.
    let (sys_tx, sys_rx) = unbounded_channel::<SysJob>();
    let (luafs_tx, luafs_rx) = unbounded_channel::<LuaFsJob>();

    let client = DaemonClient {
        host_fs: RemoteHostFs {
            rpc: rpc.clone(),
            watch_rx: Mutex::new(Some(watch_rx)),
        },
        host_proc: RemoteHostProc {
            rpc: rpc.clone(),
            inflight: proc_inflight.clone(),
            next_id: AtomicU64::new(1),
        },
        blocking_system: RemoteBlockingSystem { req_tx: sys_tx },
        lsp_transport: RemoteLspTransport {
            rpc: rpc.clone(),
            inflight: lsp_inflight.clone(),
            next_id: AtomicU64::new(1),
        },
        lua_fs: RemoteLuaFs { req_tx: luafs_tx },
    };
    // Hand the seams out before serving; if the caller already dropped, there's
    // nothing to drive.
    if client_tx.send(client).is_err() {
        return;
    }

    // The two blocking bridges ride this shared runtime as tasks (each was its
    // own thread+runtime in the single-leg `connect`); the demux is the main
    // future. All three share the one `incoming`/`Rpc`.
    tokio::spawn(run_sys_jobs(rpc.clone(), sys_rx));
    tokio::spawn(run_luafs_jobs(rpc.clone(), luafs_rx));
    run_client_demux(incoming, proc_inflight, lsp_inflight, watch_tx).await;
}

/// The one demux for every daemon→edit-host notification, fanning each to its leg by
/// method: `proc_spawned`/`proc_exited` to the in-flight spawn, `fs_changed` to the
/// watch channel, and the LSP pushes to [`route_lsp_notification`] (which ignores any
/// non-LSP method, so unknown notifications drop). Request *responses* never arrive here
/// — [`Rpc`] msgid-routes them internally. On EOF (the daemon hung up) it clears the
/// proc + LSP maps so every waiting child reports its synthesized exit instead of
/// hanging, and drops `watch_tx` to end the server's watch arm.
async fn run_client_demux(
    mut incoming: UnboundedReceiver<Incoming>,
    proc_inflight: Inflight,
    lsp_inflight: LspInflightMap,
    watch_tx: UnboundedSender<WatchEvent>,
) {
    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the daemon speaks only notifications; ignore stray requests
        };
        match method.as_str() {
            PROC_SPAWNED => {
                if let Some((id, ev)) = decode_spawned(&params) {
                    forward(&proc_inflight, id, ev);
                }
            }
            PROC_STDOUT => {
                if let Some((id, ev)) = decode_stdout(params) {
                    forward(&proc_inflight, id, ev);
                }
            }
            PROC_EXITED => {
                if let Some((id, ev)) = decode_exited(params) {
                    forward(&proc_inflight, id, ev);
                }
            }
            FS_CHANGED => {
                if let Some(ev) = decode_fs_changed(params) {
                    // The server may not have taken the watch receiver yet at startup; a
                    // send that finds no receiver is harmlessly dropped.
                    let _ = watch_tx.send(ev);
                }
            }
            // Everything else routes through the LSP helper, which handles the three
            // `lsp_*` pushes and no-ops any other (e.g. unknown) method.
            other => route_lsp_notification(&lsp_inflight, other, params),
        }
    }
    proc_inflight.lock().unwrap().clear();
    lsp_inflight.lock().unwrap().clear();
}
