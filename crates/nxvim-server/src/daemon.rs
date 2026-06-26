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
//! | edit-host → daemon | `fs_chdir [path]`        | `["ok", canonical]` (a `:cd` target's resolved dir), or an `E344` RPC error |
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
//! ## The LSP leg (`lsp_*` — long-lived bidirectional pipes)
//!
//! A language server is neither run-to-completion (the `proc_*` leg) nor
//! request/response (`fs_*`): it is a *long-lived child whose stdio is a raw
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
use nxvim_lua::LuaFs;
use nxvim_rpc::{connect, Incoming, Rpc};

use crate::evloop::LoopEvent;
use crate::host::{HostProc, ProcEvents, ProcSpec, StdHostProc};
use crate::remote_config::{decode_config_bundle, RemoteConfigBundle};

const FS_READ: &str = "fs_read";
const FS_WRITE: &str = "fs_write";
// `:cd` in a daemon session: resolve + validate a directory on the daemon and reply with
// its canonical path (request/response, like a read). Pure — it does NOT chdir the daemon
// *process* (one daemon serves many concurrent sessions; a process-global cwd would
// corrupt the others), so the edit-host owns the logical cwd in `DirState` and resolves
// its own relative paths against it. See `docs/plans/2026-06-23-remote-cwd.md`.
const FS_CHDIR: &str = "fs_chdir";
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

// The terminal leg (`term_*`): a *streaming* PTY per buffer — the web `:terminal`
// (Phase 7). Unlike the run-to-completion process leg above, a terminal stays open
// for its whole life with raw PTY bytes flowing both ways: the edit-host pushes
// keystrokes/resizes in, the daemon streams the child's output back. The daemon runs
// the real PTY via the native [`TerminalManager`](crate::terminal::native::TerminalManager)
// (the same engine a local `:terminal` uses); the browser owns the vt100 emulation.
const TERM_OPEN: &str = "term_open";
const TERM_WRITE: &str = "term_write";
const TERM_RESIZE: &str = "term_resize";
const TERM_KILL: &str = "term_kill";
const TERM_DATA: &str = "term_data";
const TERM_EXIT: &str = "term_exit";

// The LSP leg: a *long-lived bidirectional pipe* per language server. Unlike every
// other leg (run-to-completion `proc_*`, request/response `fs_*`), a
// language server's stdio stays open for its whole life, with JSON-RPC flowing both
// ways and stdout consumed incrementally — so the wire streams raw stdin/stdout/stderr
// chunks correlated by a per-spawn `id`, never a single buffered result.
const LSP_SPAWN: &str = "lsp_spawn"; // edit-host → daemon: [id, program, args, cwd]
const LSP_STDIN: &str = "lsp_stdin"; // edit-host → daemon: [id, bytes]
const LSP_KILL: &str = "lsp_kill"; // edit-host → daemon: [id]
const LSP_STDOUT: &str = "lsp_stdout"; // daemon → edit-host: [id, bytes]
const LSP_STDERR: &str = "lsp_stderr"; // daemon → edit-host: [id, bytes]
const LSP_EXITED: &str = "lsp_exited"; // daemon → edit-host: [id, code?, signal?]

// The Lua-`nx.fs` off-tick op leg (`luafs_op`): a request/response per **high-level**
// `nx.fs.*` op (`readdir` / `read_text` / `write` / `copy{recursive}` / …) — the ONE fs
// path both the native-daemon edit-host (via [`RemoteFsJobs`]) and the wasm edit-host (over
// WebTransport) use. It carries a whole [`FsJob`](nxvim_lua::FsJob) and runs it through
// [`run_fs_job`](nxvim_lua::run_fs_job) on the daemon — so a compound op (a recursive copy /
// remove) decomposes into local syscalls daemon-side rather than a round-trip per step. The
// request is a map (`{ op, path, … }`), the reply the `["ok", <fs-value>] | ["err", code,
// message]` envelope `nxvim_lua::fswire` encodes. (The retired low-level per-`LuaFs`-op
// `luafs` leg, which backed the removed synchronous `vim.fn` fs builtins, is gone.)
const LUAFS_OP: &str = "luafs_op";

// The Lua-`nx.fs.watch` streaming leg (`luafs_watch`): the wasm route for the streaming watch
// (Phase 3b of the off-tick plan). DISTINCT from the buffer-reconcile `fs_watch` leg (a coarse
// single-path stat-poll keyed by path): this is a recursive, change-classified watch keyed by a
// stream `id`, reusing the native event-loop actor's coalescing watcher
// ([`start_fs_watch_coalesced`](crate::evloop::start_fs_watch_coalesced)). The edit-host arms /
// disarms by notification; the daemon pushes change batches / a terminal error back.
const LUAFS_WATCH: &str = "luafs_watch"; // edit-host → daemon: [id, path, recursive]
const LUAFS_UNWATCH: &str = "luafs_unwatch"; // edit-host → daemon: [id]
const LUAFS_CHANGE: &str = "luafs_change"; // daemon → edit-host: [id, kind, [path, …]]
const LUAFS_WATCH_ERR: &str = "luafs_watch_err"; // daemon → edit-host: [id, message]

// The config leg (`config_*`): a single request/response that ships the daemon's
// whole config surface — its `config_dir`, `runtimepath`, and every source file under
// those roots — so a remote session loads the *daemon's* config + plugins (fetched,
// materialized locally, then run locally), not the client's. One round trip; the
// daemon walks the tree daemon-side and the edit-host mirrors it onto a local cache.
// See `docs/plans/2026-06-23-remote-config-and-plugins.md`.
//
// | direction | method | reply |
// | edit-host → daemon | `config_bundle []` | `[config_dir?, [runtimepath…], [[abspath, bytes], …], [ts_lang…]]`, or a loud error |
//
// `ts_lang…` is the daemon's installed tree-sitter parser languages; the client
// auto-installs the same set locally (parsers are native artifacts, never fetched).
const CONFIG_BUNDLE: &str = "config_bundle";

/// What the daemon reports back about one child, demuxed off the wire and handed to
/// the [`RemoteHostProc::run`] future waiting on that spawn's `id`. Mirrors the two
/// [`ProcEvents`] reports the future then re-emits to the editor.
enum DaemonEvent {
    /// The child is running (or failed to spawn — `None` pid).
    Spawned(Option<u32>),
    /// A streaming child emitted a batch of stdout lines (`nx.run_stream`'s streamed stdout).
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
                // The daemon only spawns processes — it arms no timers, no filesystem
                // watches, and no `nx.fs` ops (the luafs leg has its own handler) — so
                // no other variant can reach here.
                LoopEvent::Timer { .. }
                | LoopEvent::FsEvent { .. }
                | LoopEvent::FsResult { .. } => {}
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

/// The terminal leg's connection-agnostic core: drives the `term_*` wire over a
/// pre-built shared [`Rpc`] + its own demuxed inbound stream — the streaming sibling of
/// [`serve_proc_daemon_on`]. The single-stdio daemon multiplexer ([`run_daemon_io`]) fans
/// one connection across every leg's `*_on`; this leg owns a native
/// [`TerminalManager`](crate::terminal::native::TerminalManager) (the same PTY engine a
/// local `:terminal` uses) and bridges it to the wire: incoming `term_open`/`term_write`/
/// `term_resize`/`term_kill` notifications drive the manager, and the children's
/// [`TermEvent`](crate::terminal::native::TermEvent) output/exit stream back as
/// `term_data`/`term_exit` pushes the browser feeds to its own vt100 emulator. The buffer
/// id (`BufferId(u64)`) is the per-terminal key, carried verbatim on the wire.
pub async fn serve_term_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
    use crate::terminal::native::{TermCommand, TermEvent, TerminalManager};
    use nxvim_core::BufferId;

    let (mut terminals, mut term_events) = TerminalManager::new();

    // One forwarder turns the children's `TermEvent`s — the same events the local run
    // loop's `on_term_events` arm consumes — into wire notifications back to the browser.
    let reply = rpc.clone();
    tokio::spawn(async move {
        while let Some(ev) = term_events.recv().await {
            match ev {
                // Data goes over the *backpressured* stream channel: when the wire is
                // behind (browser slow / QUIC congested), this `await` blocks, so we
                // stop draining `term_events`, the bounded event channel fills, the PTY
                // reader blocks, and the child is throttled — no unbounded backlog, so a
                // `^C` actually stops the output. Exit stays on the control channel so it
                // is delivered promptly even behind a backed-up data stream.
                TermEvent::Data { buf, bytes } => {
                    reply
                        .notify_stream(TERM_DATA, vec![Value::from(buf.0), Value::Binary(bytes)])
                        .await
                }
                TermEvent::Exit { buf, code } => reply.notify(
                    TERM_EXIT,
                    vec![Value::from(buf.0), Value::from(code as i64)],
                ),
            }
        }
    });

    while let Some(msg) = incoming.recv().await {
        let Incoming::Notification { method, params } = msg else {
            continue; // the edit-host drives the daemon with notifications only
        };
        match method.as_str() {
            TERM_OPEN => {
                if let Some(cmd) = decode_term_open(params) {
                    terminals.send(cmd);
                }
            }
            TERM_WRITE => {
                let buf = params.first().and_then(Value::as_u64);
                let bytes = params.get(1).and_then(|v| match v {
                    Value::Binary(b) => Some(b.clone()),
                    Value::String(s) => Some(s.as_bytes().to_vec()),
                    _ => None,
                });
                if let (Some(buf), Some(bytes)) = (buf, bytes) {
                    terminals.send(TermCommand::Write {
                        buf: BufferId(buf),
                        bytes,
                    });
                }
            }
            TERM_RESIZE => {
                let buf = params.first().and_then(Value::as_u64);
                let rows = params.get(1).and_then(Value::as_u64);
                let cols = params.get(2).and_then(Value::as_u64);
                if let (Some(buf), Some(rows), Some(cols)) = (buf, rows, cols) {
                    terminals.send(TermCommand::Resize {
                        buf: BufferId(buf),
                        rows: rows as u16,
                        cols: cols as u16,
                    });
                }
            }
            TERM_KILL => {
                if let Some(buf) = params.first().and_then(Value::as_u64) {
                    terminals.send(TermCommand::Kill { buf: BufferId(buf) });
                }
            }
            _ => {}
        }
    }
    Ok(())
}

/// `term_open` params → a [`TermCommand::Open`]: `[buf(u64), argv([str]), cwd(str|nil),
/// rows(u64), cols(u64)]`. Returns `None` (the open is dropped) on a malformed frame —
/// the peer is the same build, so this only guards against a truncated message.
fn decode_term_open(params: Vec<Value>) -> Option<crate::terminal::native::TermCommand> {
    use crate::terminal::native::TermCommand;
    use nxvim_core::BufferId;

    let buf = params.first().and_then(Value::as_u64)?;
    let argv = match params.get(1) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let cwd = params.get(2).and_then(Value::as_str).map(str::to_string);
    let rows = params.get(3).and_then(Value::as_u64)? as u16;
    let cols = params.get(4).and_then(Value::as_u64)? as u16;
    Some(TermCommand::Open {
        buf: BufferId(buf),
        argv,
        cwd,
        rows,
        cols,
    })
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
    /// An existing file's bytes plus its stat at read time — load the bytes into the buffer
    /// (a replica of the remote) and stamp the [`FileStat`] as the `disk` baseline, so the
    /// buffer counts as read-from-disk (fires `BufReadPost`, not `BufNewFile`) and the watch
    /// leg's later `fs_changed` pushes compare against an accurate snapshot. `None` if the
    /// daemon couldn't stat the (still readable) file — a rare degrade to a size-only baseline.
    File(Vec<u8>, Option<FileStat>),
    /// The path doesn't exist yet — open an empty new-file buffer named for it (the
    /// `:e newfile` case), so a first `:w` would create it.
    New,
    /// The path is a **directory** — open it as the in-window file explorer. `path` is
    /// the daemon's *canonical* directory path (so `../`/descend navigation is unambiguous
    /// on the edit-host side); `entries` are its immediate, unsorted entries (the edit-host
    /// renders the listing via [`nxvim_core::dir_listing`] for the explorer plugin).
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

    /// Resolve + validate a `:cd` target on the daemon, resolving to its canonical absolute
    /// path (or a loud `E344` error if it isn't a directory) — the off-tick half of
    /// remote `:cd` (`docs/plans/2026-06-23-remote-cwd.md`). Pure: the daemon does not chdir
    /// its process (it serves many sessions), so the edit-host installs the returned path
    /// into its own [`DirState`](crate). The default fails loud — a backend with no remote
    /// `:cd` support must say so at runtime, not silently succeed (the no-silent-stub rule).
    fn chdir(&self, _path: String) -> Pin<Box<dyn Future<Output = io::Result<String>> + Send>> {
        Box::pin(async {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote :cd is not supported by this filesystem backend",
            ))
        })
    }

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

    fn chdir(&self, path: String) -> Pin<Box<dyn Future<Output = io::Result<String>> + Send>> {
        let rpc = self.rpc.clone();
        Box::pin(async move {
            match rpc.request(FS_CHDIR, vec![Value::from(path)]).await {
                Ok(Value::Array(a)) => decode_fs_chdir(&a),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_chdir: malformed reply",
                )),
                // A daemon error reply carries the `E344` text (a missing/!dir target) or
                // a transport failure; either is surfaced loud, never a silent no-move.
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
        Some("file") => {
            let stat = a.get(2).and_then(decode_stat);
            match a.get_mut(1).map(|v| std::mem::replace(v, Value::Nil)) {
                Some(Value::Binary(bytes)) => Ok(FsRead::File(bytes, stat)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "fs_read: file reply missing bytes",
                )),
            }
        }
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

/// Decode an `fs_chdir` ok reply (`["ok", canonical]`) to the canonical directory path.
/// A daemon *error* reply (the `E344` text) never reaches here — `nxvim_rpc` surfaces it
/// as the `request` future's `Err`, which [`RemoteHostFs::chdir`] maps straight through.
fn decode_fs_chdir(a: &[Value]) -> io::Result<String> {
    match (
        a.first().and_then(Value::as_str),
        a.get(1).and_then(Value::as_str),
    ) {
        (Some("ok"), Some(path)) => Ok(path.to_owned()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs_chdir: malformed ok reply",
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
                            FS_CHDIR => serve_chdir(&*fs, &params),
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
        Ok(FsRead::File(bytes, stat)) => Ok(Value::Array(vec![
            Value::from("file"),
            Value::Binary(bytes),
            stat.as_ref().map_or(Value::Nil, encode_stat),
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

/// Serve one `fs_chdir [path]` against `fs`: resolve a `:cd` target on the daemon and
/// reply `["ok", canonical]` with its canonical absolute path, or a loud `E344` error if
/// it isn't a readable directory. Pure — no process `chdir` (the daemon serves many
/// sessions in one process), so this only *resolves and validates*; the edit-host owns
/// the logical cwd. An empty path means `:cd` with no argument → the daemon's `$HOME`; a
/// leading `~` expands against the daemon's home (Unix `:cd` semantics, resolved on the
/// remote where it belongs). Directory-ness is checked through the [`HostFs`] seam
/// (`read_dir` succeeds only for a directory), so a fake test backend behaves identically.
fn serve_chdir(fs: &dyn HostFs, params: &[Value]) -> Result<Value, Value> {
    let Some(arg) = params.first().and_then(Value::as_str) else {
        return Err(Value::from("fs_chdir: missing path"));
    };
    let target = expand_remote_cd_arg(arg);
    // `read_dir` is the directory check: it succeeds only for a directory and fails
    // (NotFound / NotADirectory) for anything else — exactly vim's `E344` condition.
    match fs.read_dir(&target) {
        Ok(_) => {
            // The canonical absolute path (symlinks resolved on the daemon) is what the
            // edit-host stores + reports, so `:pwd` shows the real remote directory.
            let canon = fs.canonicalize(&target).unwrap_or(target);
            Ok(Value::Array(vec![
                Value::from("ok"),
                Value::from(canon.to_string_lossy().into_owned()),
            ]))
        }
        Err(e) => Err(Value::from(format!(
            "E344: Can't change directory to \"{}\": {e}",
            target.display()
        ))),
    }
}

/// Expand a `:cd` argument on the **daemon** side: an empty arg → `$HOME` (Unix `:cd` with
/// no directory), a leading `~` / `~/…` → the daemon's home dir, anything else verbatim
/// (the edit-host already absolutized relative paths against its `DirState`, so what
/// arrives is absolute or `~`-prefixed). Mirrors the edit-host's local `expand_cd_arg`,
/// but rooted at the *daemon's* `$HOME` — the home `~` must mean on the remote.
fn expand_remote_cd_arg(arg: &str) -> PathBuf {
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if arg.is_empty() {
        return home().unwrap_or_else(|| PathBuf::from("/"));
    }
    if let Some(rest) = arg.strip_prefix('~') {
        if rest.is_empty() {
            return home().unwrap_or_else(|| PathBuf::from(arg));
        }
        if let Some(rest) = rest.strip_prefix('/') {
            if let Some(h) = home() {
                return h.join(rest);
            }
        }
    }
    PathBuf::from(arg)
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
            // Stat the file we just read so the edit-host stamps an accurate `disk`
            // baseline (and so an existing file fires `BufReadPost`, not `BufNewFile`).
            Ok(FsRead::File(bytes, fs.stat(path)))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(FsRead::New),
        Err(e) => Err(e),
    }
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

// ----- the Lua-filesystem leg (`luafs_op`) ----------------------------------------
//
// `RemoteFsJobs` (the edit-host side) is how a **native-daemon** session runs async
// `nx.fs`: the event-loop actor hands a whole [`FsJob`](nxvim_lua::FsJob) here, it
// crosses in ONE `luafs_op` request, and the daemon runs it through
// [`run_fs_job`](nxvim_lua::run_fs_job) against its [`StdLuaFs`](nxvim_lua::StdLuaFs)
// (decomposing any compound op daemon-side, so a recursive copy is one round-trip, not a
// chatter of per-op calls). The wasm edit-host forwards the identical `luafs_op` request
// over WebTransport — one leg, one shape. Unlike the retired per-op `RemoteLuaFs` bridge
// this parks no thread: the actor `await`s the reply on the shared link runtime.

/// One queued fs job on the link thread: the whole [`FsJob`](nxvim_lua::FsJob) and the
/// tokio oneshot the awaiting actor parks on for the typed result. Async (a tokio channel),
/// because the caller is the event-loop actor's task, not a synchronous editor-thread call.
type FsJobReq = (
    nxvim_lua::FsJob,
    tokio::sync::oneshot::Sender<Result<nxvim_lua::FsValue, nxvim_lua::FsError>>,
);

/// The `luafs_op` leg's job server: pull each whole [`FsJob`](nxvim_lua::FsJob) off
/// `req_rx`, send it as one `luafs_op` request over `rpc`, decode the reply through the
/// shared [`fswire`](nxvim_lua) codec, and deliver the typed result to the awaiting actor.
async fn run_fs_jobs(rpc: Rpc, mut req_rx: UnboundedReceiver<FsJobReq>) {
    while let Some((job, reply_tx)) = req_rx.recv().await {
        let result = match rpc
            .request(LUAFS_OP, vec![nxvim_lua::fs_job_to_value(&job)])
            .await
        {
            Ok(v) => nxvim_lua::fs_result_from_value(&v),
            // A transport failure (daemon gone) rejects the promise loud — never a panic.
            Err(e) => Err(nxvim_lua::FsError {
                code: "EIO".to_string(),
                message: format!("nx.fs: daemon error: {e}"),
            }),
        };
        let _ = reply_tx.send(result);
    }
}

/// The edit-host side of the `luafs_op` leg for a **native-daemon** session — the actor
/// sends a whole [`FsJob`](nxvim_lua::FsJob) here and `await`s its typed result. Holds a
/// tokio sender to the shared link runtime's [`run_fs_jobs`]; `Clone` so each `nx.fs` op
/// can be driven concurrently, `Send + Sync` so it rides [`ServerInit`](crate::ServerInit)
/// onto the server thread.
#[derive(Clone)]
pub struct RemoteFsJobs {
    req_tx: UnboundedSender<FsJobReq>,
}

impl RemoteFsJobs {
    /// Connect to a daemon over `reader`/`writer` as a standalone leg, spawning a
    /// dedicated link thread (its own current-thread runtime + the RPC link) that runs
    /// [`run_fs_jobs`]. The multiplexed [`connect_daemon`] builds a `RemoteFsJobs`
    /// directly instead (sharing one link across all legs); this single-leg form is for
    /// driving the `luafs_op` leg in isolation (tests).
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteFsJobs
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (req_tx, req_rx) = unbounded_channel::<FsJobReq>();
        std::thread::spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                // A runtime we can't build means the link is dead on arrival; `req_rx`
                // drops, so every `run` sees the channel closed and rejects loudly.
                Err(_) => return,
            };
            rt.block_on(async move {
                let (rpc, mut incoming) = connect(reader, writer);
                // The luafs_op leg has no daemon→edit-host pushes; drain so the connection
                // isn't torn down (dropping the receiver would).
                tokio::spawn(async move { while incoming.recv().await.is_some() {} });
                run_fs_jobs(rpc, req_rx).await;
            });
        });
        RemoteFsJobs { req_tx }
    }

    /// Send `job` to the daemon over `luafs_op` and `await` the typed result. Off the
    /// editor tick (the caller is the actor's async task), so this is a tokio await, not a
    /// thread park; a dropped link rejects loud.
    pub async fn run(
        &self,
        job: nxvim_lua::FsJob,
    ) -> Result<nxvim_lua::FsValue, nxvim_lua::FsError> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.req_tx.send((job, reply_tx)).is_err() {
            return Err(nxvim_lua::FsError {
                code: "ENOTCONN".to_string(),
                message: "nx.fs: daemon link is gone".to_string(),
            });
        }
        reply_rx.await.unwrap_or_else(|_| {
            Err(nxvim_lua::FsError {
                code: "EIO".to_string(),
                message: "nx.fs: daemon link dropped the request".to_string(),
            })
        })
    }
}

/// Run one high-level `nx.fs` op (the `luafs_op` leg): decode the request map into an
/// [`FsJob`](nxvim_lua::FsJob), run it through [`run_fs_job`](nxvim_lua::run_fs_job) against
/// the daemon's `fs`, and shape the `["ok", <fs-value>] | ["err", code, message]` reply.
/// A request that doesn't decode is an `["err", "EWIRE", …]` reply (fail loud — never a
/// silent empty result). Compound ops (recursive copy/remove) decompose into local syscalls
/// inside `run_fs_job`, so this is one wire round-trip regardless of the op's fan-out.
fn serve_fs_op(fs: &dyn LuaFs, params: &[Value]) -> Value {
    let Some(req) = params.first() else {
        return nxvim_lua::fs_result_to_value(&Err(nxvim_lua::FsError {
            code: "EWIRE".to_string(),
            message: "luafs_op: request has no job".to_string(),
        }));
    };
    match nxvim_lua::fs_job_from_value(req) {
        Ok(job) => nxvim_lua::fs_result_to_value(&nxvim_lua::run_fs_job(fs, &job)),
        Err(message) => nxvim_lua::fs_result_to_value(&Err(nxvim_lua::FsError {
            code: "EWIRE".to_string(),
            message,
        })),
    }
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
/// the `*_on` split). Serves the low-level `luafs` request (one `LuaFs` op) **and** the
/// high-level `luafs_op` request (a whole [`FsJob`](nxvim_lua::FsJob) run through
/// [`run_fs_job`](nxvim_lua::run_fs_job) — the wasm `nx.fs` leg, Phase 2) over the shared
/// [`Rpc`] + its demuxed stream. Both share the one `StdLuaFs`, and both offload to the
/// blocking pool so a slow fs call can't stall the reader.
pub async fn serve_luafs_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
    fs: Box<dyn LuaFs + Send + Sync>,
) -> anyhow::Result<()> {
    let fs: Arc<dyn LuaFs + Send + Sync> = Arc::from(fs);
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, params } = msg {
            if method == LUAFS_OP {
                let fs = fs.clone();
                let rpc = rpc.clone();
                tokio::spawn(async move {
                    let reply =
                        match tokio::task::spawn_blocking(move || serve_fs_op(&*fs, &params)).await
                        {
                            Ok(v) => Ok(v),
                            Err(e) => Err(Value::from(format!("luafs_op: join error: {e}"))),
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

/// The `nx.fs.watch` streaming leg's connection-agnostic core. Arms a recursive,
/// change-classified watch per stream `id` (reusing the event-loop actor's coalescing watcher,
/// [`start_fs_watch_coalesced`](crate::evloop::start_fs_watch_coalesced) — the same 10 ms-coalesced
/// `notify` backend the native `nx.fs.watch` rides) and pushes each batch back as `luafs_change
/// [id, kind, paths]` / a terminal `luafs_watch_err [id, message]`. The edit-host arms / disarms
/// by notification (`luafs_watch` / `luafs_unwatch`); there is no reply, so a stray request is
/// answered with an error. Watchers are kept alive in a per-`id` map (dropping one stops its
/// backend thread); the leg ends when the edit-host hangs up.
pub async fn serve_luafs_watch_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
    // The coalescing watcher emits `LoopEvent::FsEvent` (the native actor's shape); we forward
    // each into RPC pushes. One shared channel for all watches — `id` tags every event.
    let (ev_tx, mut ev_rx) = unbounded_channel::<LoopEvent>();
    let mut watchers: HashMap<u64, notify::RecommendedWatcher> = HashMap::new();
    loop {
        tokio::select! {
            msg = incoming.recv() => {
                let Some(msg) = msg else { break }; // the edit-host hung up
                match msg {
                    Incoming::Notification { method, params } => match method.as_str() {
                        LUAFS_WATCH => {
                            let id = params.first().and_then(Value::as_u64).unwrap_or(0);
                            let path = params.get(1).and_then(Value::as_str).unwrap_or("").to_string();
                            let recursive = params.get(2).and_then(Value::as_bool).unwrap_or(false);
                            match crate::evloop::start_fs_watch_coalesced(
                                id, &path, recursive, ev_tx.clone(),
                            ) {
                                Ok(w) => { watchers.insert(id, w); }
                                // Arm failure (bad path / watch limit) is terminal for this
                                // stream — push it loud, exactly as the native arm rejects.
                                Err(e) => rpc.notify(
                                    LUAFS_WATCH_ERR,
                                    vec![Value::from(id), Value::from(e.to_string())],
                                ),
                            }
                        }
                        // Dropping the watcher stops its backend thread (and the coalescing task).
                        LUAFS_UNWATCH => {
                            let id = params.first().and_then(Value::as_u64).unwrap_or(0);
                            watchers.remove(&id);
                        }
                        _ => {}
                    },
                    // The leg speaks only notifications; a request is a protocol error.
                    Incoming::Request { id, .. } => rpc.respond(
                        id,
                        Err(Value::from("luafs_watch leg takes notifications, not requests")),
                    ),
                }
            }
            Some(ev) = ev_rx.recv() => {
                if let LoopEvent::FsEvent { id, error, kind, paths } = ev {
                    match error {
                        Some(msg) => rpc.notify(
                            LUAFS_WATCH_ERR,
                            vec![Value::from(id), Value::from(msg)],
                        ),
                        None => {
                            let plist = paths
                                .into_iter()
                                .map(|p| Value::from(p.to_string_lossy().into_owned()))
                                .collect();
                            rpc.notify(
                                LUAFS_CHANGE,
                                vec![
                                    Value::from(id),
                                    Value::from(kind.unwrap_or("modify")),
                                    Value::Array(plist),
                                ],
                            );
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

// ============================================================================
// The config leg (daemon side)
// ============================================================================

/// Run the daemon end of the *config* wire over `reader`/`writer`, answering
/// `config_bundle` requests with this machine's config surface (see [`CONFIG_BUNDLE`]).
/// Returns when the connection closes. The per-leg wrapper the tests drive over a
/// private duplex; the real binary routes the `config_` namespace into
/// [`serve_config_daemon_on`] through the [`run_daemon_io`](crate::run_daemon_io) mux.
pub async fn serve_config_daemon<R, W>(reader: R, writer: W) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, incoming) = connect(reader, writer);
    serve_config_daemon_on(rpc, incoming).await
}

/// The config leg's connection-agnostic core: answer each `config_bundle` request by
/// walking the daemon's config tree ([`crate::collect_config_bundle`]) and replying with
/// the encoded bundle, or a loud error. The leg carries no notifications and holds no
/// state, so it is a plain request loop (unlike the stateful `fs_*`/`proc_*` legs).
pub async fn serve_config_daemon_on(
    rpc: Rpc,
    mut incoming: UnboundedReceiver<Incoming>,
) -> anyhow::Result<()> {
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, .. } = msg {
            let reply = match method.as_str() {
                CONFIG_BUNDLE => serve_config_bundle(),
                other => Err(Value::from(format!("unknown method: {other}"))),
            };
            rpc.respond(id, reply);
        }
        // The config leg has no notifications; ignore anything else.
    }
    Ok(())
}

/// Walk the daemon's config surface and project it onto the `config_bundle` wire shape,
/// or a loud error reply (a failed walk is never a silently-empty bundle).
fn serve_config_bundle() -> Result<Value, Value> {
    match crate::collect_config_bundle() {
        Ok((config_dir, runtimepath, files, ts_languages)) => Ok(encode_config_bundle(
            config_dir,
            runtimepath,
            files,
            ts_languages,
            // The daemon's process cwd, to seed the edit-host's `DirState` so a remote
            // session's `:pwd` / `getcwd` / `:cd` operate on the daemon's directory
            // (`docs/plans/2026-06-23-remote-cwd.md`). `None` if it can't be read — the
            // edit-host then falls back to its own local cwd.
            std::env::current_dir().ok(),
        )),
        Err(e) => Err(Value::from(format!("config_bundle: {e}"))),
    }
}

/// `[config_dir?, [runtimepath…], [[abspath, bytes], …], [ts_lang…], cwd?]` — the bundle
/// on the wire ([`decode_config_bundle`] is the inverse). Paths are the daemon's absolute
/// paths; the edit-host rebases them onto its local cache. `cwd` is the daemon's working
/// directory (a trailing field an older peer omits → the edit-host keeps its local cwd).
fn encode_config_bundle(
    config_dir: Option<PathBuf>,
    runtimepath: Vec<PathBuf>,
    files: Vec<(PathBuf, Vec<u8>)>,
    ts_languages: Vec<String>,
    cwd: Option<PathBuf>,
) -> Value {
    let path_str = |p: PathBuf| Value::from(p.to_string_lossy().into_owned());
    Value::Array(vec![
        config_dir.map_or(Value::Nil, &path_str),
        Value::Array(runtimepath.into_iter().map(&path_str).collect()),
        Value::Array(
            files
                .into_iter()
                .map(|(p, bytes)| Value::Array(vec![path_str(p), Value::Binary(bytes)]))
                .collect(),
        ),
        Value::Array(ts_languages.into_iter().map(Value::from).collect()),
        cwd.map_or(Value::Nil, &path_str),
    ])
}

// ============================================================================
// The edit-host-side multiplexer
// ============================================================================
//
// Each `Remote*::connect` above opens its own connection — fine for the per-leg tests,
// where each leg gets a private duplex, but the real edit-host talks to *one* daemon
// over *one* transport. `connect_daemon` is the symmetric counterpart of the daemon's
// `run_daemon_io` multiplexer: it `connect`s once and hands back all four seams sharing
// that single link, so one `ServerInit` populates `host_fs_async` / `host_proc` /
// `lsp_transport` / `fs_jobs` from a single `--daemon` child.
//
// Two properties make this a clean router, not a rework (both verified in the code):
// the daemon→edit-host *notifications* split into disjoint method namespaces
// (`proc_spawned`/`proc_exited`, `fs_changed`, `lsp_stdout`/`lsp_stderr`/`lsp_exited`),
// and request *responses* (`fs_read`/`fs_write`/`luafs_op`) are msgid-routed
// *inside* [`Rpc`] and never surface as an [`Incoming`] — so one demux over the shared
// `incoming` covers every leg, and concurrent writes from all legs serialize through
// `Rpc`'s single out-channel.

/// The edit-host side of the config leg: a [`Rpc`] handle that fetches the daemon's
/// config surface with one `config_bundle` request. Shares the single daemon link like
/// the other seams (see [`connect_daemon`]); [`RemoteConfig::connect`] is the per-leg
/// constructor the tests drive over a private duplex.
pub struct RemoteConfig {
    rpc: Rpc,
}

impl RemoteConfig {
    /// Connect to a daemon's config leg over `reader`/`writer` (its own link — the
    /// per-leg path the tests use; the real edit-host builds [`RemoteConfig`] inline in
    /// [`serve_daemon_link`] over the shared link). The leg carries no notifications,
    /// but the inbound stream must still be drained or the reader backpressures —
    /// dropping it would tear the connection down — so a task drains it to EOF.
    pub fn connect<R, W>(reader: R, writer: W) -> RemoteConfig
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (rpc, mut incoming) = connect(reader, writer);
        tokio::spawn(async move { while incoming.recv().await.is_some() {} });
        RemoteConfig { rpc }
    }

    /// Fetch the daemon's config surface (one `config_bundle` round trip). A transport
    /// failure or a malformed reply is a loud error — never a silently-empty bundle that
    /// would look like "the remote has no config".
    pub async fn fetch(&self) -> io::Result<RemoteConfigBundle> {
        match self.rpc.request(CONFIG_BUNDLE, vec![]).await {
            // The decode (shape validation) lives in `remote_config` so the wasm edit-host
            // shares it; a mismatch is a loud `InvalidData` error here.
            Ok(v) => {
                decode_config_bundle(v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            }
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }
}

/// The five edit-host seams of one daemon connection, all sharing a single link (see
/// [`connect_daemon`]). Each field drops straight into the matching
/// [`ServerInit`](crate::ServerInit) slot — except [`config`](Self::config), which the
/// session fetches *before* building `ServerInit` to derive the local config roots.
pub struct DaemonClient {
    /// The async filesystem seam (`fs_read`/`fs_write`) + the `fs_changed` watch push.
    pub host_fs: RemoteHostFs,
    /// The event-routed process seam (the async `vim.system` / `jobstart` / `:!`).
    pub host_proc: RemoteHostProc,
    /// The streaming-pipe LSP seam (`lsp_*`).
    pub lsp_transport: RemoteLspTransport,
    /// The async `nx.fs` seam (`luafs_op`) — whole-job, decomposed daemon-side. The
    /// event-loop actor `await`s it off the editor tick (no thread park).
    pub fs_jobs: RemoteFsJobs,
    /// The config seam (`config_bundle`) — fetched once at session start to mirror the
    /// daemon's config + plugins onto a local cache (Phase 2).
    pub config: RemoteConfig,
}

/// Connect to a single daemon over `reader`/`writer` and return all four edit-host
/// seams sharing that one link — the edit-host-side multiplexer (the symmetric twin of
/// the daemon's [`run_daemon_io`](crate::run_daemon_io)). The transport is any
/// [`AsyncRead`]/[`AsyncWrite`] pair: the real `--daemon` binary's stdio (how
/// `daemon_stdio.rs` drives it), an in-process duplex, or the QUIC stream of the future
/// listener.
///
/// **Why a dedicated link thread.** The connection runs on its *own* OS thread + a
/// current-thread runtime — not the server runtime — so the wire I/O is driven off the
/// server's thread. On this one shared thread we run the [`run_fs_jobs`] job server (the
/// `nx.fs` `luafs_op` leg) and the single [`run_client_demux`] that fans every
/// daemon→edit-host notification to the right leg. Every seam
/// (`host_fs`/`host_proc`/`lsp_transport`/`fs_jobs`) holds a clone of the shared [`Rpc`]
/// (or a channel to a job server on this thread) and issues its requests from the server
/// runtime; the actual wire I/O always happens here. No leg parks the editor thread —
/// `nx.fs` is `await`ed off the tick by the event-loop actor, the async legs are
/// fire-and-forget request/response.
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

/// Build the four edit-host seams over an already-connected `(rpc, incoming)` pair,
/// hand the [`DaemonClient`] out on `client_tx`, then drive the link — the [`run_fs_jobs`]
/// job server (the `nx.fs` `luafs_op` leg) plus the single [`run_client_demux`] — until the
/// daemon hangs up. This is the transport-agnostic heart of [`connect_daemon`]; the QUIC
/// connector ([`crate::quic::connect_quic`]) calls it with the halves of a QUIC bidi stream,
/// the link thread keeping the endpoint + connection alive in scope. Runs on the dedicated
/// link thread's runtime (so the wire is driven off the server's thread — see
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

    // The `nx.fs` (`luafs_op`) leg: the job channel its seam sends whole `FsJob`s onto and
    // its job server (below) pulls from.
    let (fs_jobs_tx, fs_jobs_rx) = unbounded_channel::<FsJobReq>();

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
        lsp_transport: RemoteLspTransport {
            rpc: rpc.clone(),
            inflight: lsp_inflight.clone(),
            next_id: AtomicU64::new(1),
        },
        fs_jobs: RemoteFsJobs { req_tx: fs_jobs_tx },
        // The config leg shares the link's `Rpc`; its `config_bundle` responses are
        // msgid-routed inside `Rpc` (never an `Incoming`), so the existing demux drains
        // the inbound stream — no extra wiring needed here.
        config: RemoteConfig { rpc: rpc.clone() },
    };
    // Hand the seams out before serving; if the caller already dropped, there's
    // nothing to drive.
    if client_tx.send(client).is_err() {
        return;
    }

    // The `nx.fs` job server rides this shared runtime as a task; the demux is the main
    // future. Both share the one `incoming`/`Rpc`.
    tokio::spawn(run_fs_jobs(rpc.clone(), fs_jobs_rx));
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
