//! The daemon wire protocol for the edit-host split (process half).
//!
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3 moves the
//! network boundary *below* the editor: the edit-host (core + Lua + treesitter)
//! runs **local** for a zero-round-trip keystroke path, and only fs + process +
//! watch — the lag-tolerant work — run on a remote **daemon**. This module is the
//! process leg of that wire: the daemon-side server ([`serve_daemon`]) that runs
//! children on behalf of a far edit-host, and the edit-host-side [`RemoteHostProc`]
//! that forwards spawns to it instead of running them locally.
//!
//! It is the first slice of the *full split* to actually carry traffic over a
//! wire, kept to the process seam because [`HostProc`] is already async +
//! event-routed (pid then exit come back as separate events, not a return value),
//! so it maps onto a wire with no impedance mismatch — unlike core's *synchronous*
//! [`HostFs`](nxvim_core::HostFs), whose remote backing has to become an off-tick
//! fetch (a later slice). The transport is any [`AsyncRead`]/[`AsyncWrite`] pair:
//! an in-process `tokio::io::duplex` (how the tests drive it), or — in the real
//! split — ssh stdio to `nxvim --daemon`.
//!
//! ## The wire
//!
//! Four notifications over nxvim's own msgpack-RPC ([`nxvim_rpc`]), correlated by a
//! per-spawn `id` the edit-host mints and the daemon echoes back. Notifications
//! (not request/response) because a child's life is two events at different times —
//! `spawned` then `exited` — which a single reply can't model:
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

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rmpv::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use nxvim_rpc::{connect, Incoming, Rpc};

use crate::evloop::LoopEvent;
use crate::host::{HostProc, ProcEvents, ProcSpec, StdHostProc};

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
