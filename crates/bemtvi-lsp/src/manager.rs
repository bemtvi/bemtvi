//! [`LspManager`]: spawn, supervise, and route N language servers, bridging each
//! to the editor's single `LspCommand`/[`LspEvent`] channel pair.
//!
//! Shape: a lazily-spawned supervisor task, per-child supervision with an
//! escalating-backoff circuit breaker, and a fire-and-forget command path so the
//! editor thread is never blocked. There are many children (one per
//! [`ServerKey`]), and each is
//! driven by an [`async_lsp`] client `MainLoop` that owns its JSON-RPC framing
//! and id correlation, so we ferry typed [`LspNotify`]/[`LspEvent`] values rather
//! than raw msgpack notifications.
//!
//! This module is the orchestration only. The editor↔manager data types live in
//! [`crate::protocol`]; turning an [`LspRequest`]/[`LspNotify`] into an
//! `async-lsp` call lives in [`crate::dispatch`]; distilling responses back to
//! an [`LspReply`](crate::protocol::LspReply) lives in [`crate::convert`]; the
//! client `MainLoop` and capability negotiation live in [`crate::client`].

use std::collections::{HashMap, VecDeque};
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use async_lsp::LanguageServer;
use lsp_types::{DidChangeConfigurationParams, InitializeResult, InitializedParams};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader, ReadBuf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::client::{init_params, new_client, read_init_result, ApplyEditDone};
use crate::dispatch::{apply_notify, issue_request};
use crate::log::{LogLevel, LspLog};
use crate::protocol::{
    ApplyEditOutcome, LspEvent, LspNotify, LspRequest, ReqToken, ServerKey, ServerSpawn,
};
use crate::transport::{LocalLspTransport, LspTransport};

/// Commands from the editor to the supervisor, routed to per-server tasks by key.
enum LspCommand {
    Ensure {
        key: ServerKey,
        spawn: ServerSpawn,
    },
    Notify {
        key: ServerKey,
        note: LspNotify,
    },
    Request {
        key: ServerKey,
        token: ReqToken,
        req: LspRequest,
    },
    /// The editor's answer to a server→client `workspace/applyEdit` (the one inbound
    /// request bemtvi answers asynchronously — it has to reach the buffers first).
    ApplyEditResponse {
        key: ServerKey,
        id: u64,
        outcome: ApplyEditOutcome,
    },
    Shutdown {
        key: ServerKey,
    },
}

/// A message from the supervisor to one per-server task.
enum ServerMsg {
    Notify(LspNotify),
    Request(ReqToken, LspRequest),
    ApplyEditResponse(u64, ApplyEditOutcome),
    Shutdown,
}

/// Why a single server lifetime ended.
enum ServerOutcome {
    /// Asked to stop (clean shutdown, or the manager went away). Do not respawn.
    Shutdown,
    /// The child died unexpectedly. The breaker decides whether to respawn.
    Failed,
}

/// Handle the server holds to drive all language servers. Cheap to construct;
/// the supervisor task is spawned lazily on the first [`LspManager::ensure_server`],
/// so a session that never touches a configured filetype spawns nothing.
pub struct LspManager {
    cmd_tx: UnboundedSender<LspCommand>,
    /// Taken when the supervisor is first started.
    start: Option<(UnboundedReceiver<LspCommand>, UnboundedSender<LspEvent>)>,
    started: bool,
    /// How servers are spawned — a real local child by default, or a daemon-backed
    /// transport injected for the edit-host split. Shared with every per-server
    /// task the supervisor spawns.
    transport: Arc<dyn LspTransport>,
}

impl LspManager {
    /// Create the manager and the receiver the server loop selects on, spawning
    /// servers as **local** children. No task is spawned and no process launched
    /// until [`LspManager::ensure_server`].
    pub fn new() -> (LspManager, UnboundedReceiver<LspEvent>) {
        Self::with_transport(Arc::new(LocalLspTransport))
    }

    /// Like [`LspManager::new`], but with an injected [`LspTransport`] — the
    /// edit-host split passes a daemon-backed one so language servers run on the
    /// remote (`docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3).
    pub fn with_transport(
        transport: Arc<dyn LspTransport>,
    ) -> (LspManager, UnboundedReceiver<LspEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let manager = LspManager {
            cmd_tx,
            start: Some((cmd_rx, event_tx)),
            started: false,
            transport,
        };
        (manager, event_rx)
    }

    /// Spawn the supervisor task if it isn't running yet.
    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        if let Some((cmd_rx, event_tx)) = self.start.take() {
            tokio::spawn(run_supervisor(cmd_rx, event_tx, self.transport.clone()));
            self.started = true;
        }
    }

    /// Ensure a server for `key` is started (idempotent: a running server for the
    /// same key is left alone).
    pub fn ensure_server(&mut self, key: ServerKey, spawn: ServerSpawn) {
        self.ensure_started();
        let _ = self.cmd_tx.send(LspCommand::Ensure { key, spawn });
    }

    /// Fire-and-forget a document-sync notification to `key`'s server (dropped if
    /// no such server is running).
    pub fn notify(&self, key: ServerKey, note: LspNotify) {
        let _ = self.cmd_tx.send(LspCommand::Notify { key, note });
    }

    /// Fire a language-feature request at `key`'s server; its reply returns later
    /// as an [`LspEvent::Reply`] carrying `token`. Fire-and-forget like
    /// [`LspManager::notify`] — the editor never awaits the round-trip (Decision
    /// 3). Dropped if no such server is running.
    pub fn request(&self, key: ServerKey, token: ReqToken, req: LspRequest) {
        let _ = self.cmd_tx.send(LspCommand::Request { key, token, req });
    }

    /// Answer a server→client `workspace/applyEdit` the editor received as
    /// [`LspEvent::ApplyEdit`] — the server has been blocked on it since. `id` is the
    /// one that came with the event. Fire-and-forget like the rest; dropped if the
    /// server has since exited (its request died with it).
    pub fn apply_edit_response(&self, key: ServerKey, id: u64, outcome: ApplyEditOutcome) {
        let _ = self
            .cmd_tx
            .send(LspCommand::ApplyEditResponse { key, id, outcome });
    }

    /// Cleanly `shutdown`/`exit` `key`'s server and forget it.
    pub fn shutdown(&self, key: ServerKey) {
        let _ = self.cmd_tx.send(LspCommand::Shutdown { key });
    }
}

/// Route editor commands to per-server tasks, spawning a task on the first
/// `Ensure` for a key. Lives for the manager's life; ends when the editor drops
/// the command sender (server shutdown).
async fn run_supervisor(
    mut cmd_rx: UnboundedReceiver<LspCommand>,
    event_tx: UnboundedSender<LspEvent>,
    transport: Arc<dyn LspTransport>,
) {
    // Created here (not in `LspManager::new`) so the log file is opened lazily —
    // only once a configured filetype is actually opened, never for an
    // LSP-free session.
    let log = Arc::new(LspLog::from_env());
    let mut servers: HashMap<ServerKey, UnboundedSender<ServerMsg>> = HashMap::new();
    while let Some(cmd) = cmd_rx.recv().await {
        match cmd {
            LspCommand::Ensure { key, spawn } => {
                // Already live? Leave it. A task that exited (clean shutdown or a
                // breaker give-up) has dropped its receiver, so `is_closed()` lets
                // a later `Ensure` start a fresh one.
                if servers.get(&key).is_some_and(|tx| !tx.is_closed()) {
                    continue;
                }
                let (tx, rx) = unbounded_channel();
                servers.insert(key.clone(), tx);
                tokio::spawn(run_server(
                    key,
                    spawn,
                    rx,
                    event_tx.clone(),
                    log.clone(),
                    transport.clone(),
                ));
            }
            LspCommand::Notify { key, note } => {
                if let Some(tx) = servers.get(&key) {
                    let _ = tx.send(ServerMsg::Notify(note));
                }
            }
            LspCommand::Request { key, token, req } => {
                if let Some(tx) = servers.get(&key) {
                    let _ = tx.send(ServerMsg::Request(token, req));
                }
            }
            LspCommand::ApplyEditResponse { key, id, outcome } => {
                if let Some(tx) = servers.get(&key) {
                    let _ = tx.send(ServerMsg::ApplyEditResponse(id, outcome));
                }
            }
            LspCommand::Shutdown { key } => {
                if let Some(tx) = servers.remove(&key) {
                    let _ = tx.send(ServerMsg::Shutdown);
                }
            }
        }
    }
}

/// Supervise one server for its life: run a lifetime, and on unexpected death
/// respawn with escalating backoff until a circuit breaker gives up. Mirrors
/// `bemtvi-server`'s `supervise`, scoped to a single child.
async fn run_server(
    key: ServerKey,
    spawn: ServerSpawn,
    mut rx: UnboundedReceiver<ServerMsg>,
    event_tx: UnboundedSender<LspEvent>,
    log: Arc<LspLog>,
    transport: Arc<dyn LspTransport>,
) {
    // Recent failure timestamps drive the breaker (a window of repeated crashes
    // ⇒ persistently broken: stop respawning).
    let mut failures: VecDeque<Instant> = VecDeque::new();
    const WINDOW: Duration = Duration::from_secs(10);
    const GIVE_UP: usize = 5;
    const BASE_BACKOFF: Duration = Duration::from_millis(200);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    loop {
        match run_server_once(&key, &spawn, &mut rx, &event_tx, &log, &transport).await {
            ServerOutcome::Shutdown => return,
            ServerOutcome::Failed => {}
        }
        // Manager gone (no more commands will ever come): stop quietly.
        if rx.is_closed() {
            return;
        }

        let now = Instant::now();
        failures.push_back(now);
        while failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > WINDOW)
        {
            failures.pop_front();
        }

        if failures.len() >= GIVE_UP {
            let message = format!("{} kept failing to start; giving up", spawn.program);
            log.log(LogLevel::Error, &key.name, &message);
            let _ = event_tx.send(LspEvent::Log {
                key: key.clone(),
                message: format!("lsp: {message}"),
            });
            // Return — *dropping the receiver* — rather than idling on it: the
            // supervisor's respawn check is `tx.is_closed()`, so a still-open
            // channel here would make every later `Ensure` for this key a no-op
            // (the server could never be started again without a full shutdown).
            // Sends to the closed channel are fire-and-forget and their errors
            // ignored, so nothing needs us to keep draining.
            return;
        }

        let backoff = (BASE_BACKOFF * (1u32 << (failures.len() - 1))).min(MAX_BACKOFF);
        tokio::time::sleep(backoff).await;
    }
}

/// One server lifetime: spawn the child, drive its `async-lsp` client loop,
/// complete the handshake (emitting [`LspEvent::Initialized`]), then ferry
/// commands to it until it is asked to stop or its pipe closes. Returns
/// [`ServerOutcome::Failed`] on any unexpected death so the breaker can decide.
async fn run_server_once(
    key: &ServerKey,
    spawn: &ServerSpawn,
    rx: &mut UnboundedReceiver<ServerMsg>,
    event_tx: &UnboundedSender<LspEvent>,
    log: &Arc<LspLog>,
    transport: &Arc<dyn LspTransport>,
) -> ServerOutcome {
    let name = key.name.as_str();
    log.log(
        LogLevel::Info,
        name,
        &format!(
            "starting {} (root {}, cwd {})",
            spawn.program,
            key.root_label(),
            spawn
                .cwd
                .as_deref()
                .map_or_else(|| "<inherited>".to_string(), |c| c.display().to_string()),
        ),
    );
    // Spawn through the transport: a real local child by default, or a daemon
    // tunnel for the edit-host split. Either way it hands back the server's stdio
    // streams and a kill/wait handle — the loop below is identical regardless.
    let channel = match transport.spawn(spawn).await {
        Ok(channel) => channel,
        Err(e) => {
            let message = format!("failed to spawn {}: {e}", spawn.program);
            log.log(LogLevel::Error, name, &message);
            let _ = event_tx.send(LspEvent::ServerExited {
                key: key.clone(),
                message,
                code: None,
                signal: None,
            });
            return ServerOutcome::Failed;
        }
    };
    let crate::transport::LspChannel {
        stdout,
        stdin,
        stderr,
        mut process,
    } = channel;
    // Drain the server's stderr into the log, one line at a time, until it closes
    // (server exit). Each line is logged at WARN so it shows at the default level.
    // Drained as raw bytes with a lossy decode, NOT `lines()`: stderr is not
    // guaranteed UTF-8 (binary logging, raw panic dumps), and `next_line()`
    // *errors* on an undecodable line — ending the loop used to abandon the pipe,
    // whose closed read end then killed the server on its next stderr write
    // (SIGPIPE / `eprintln!` panic). The channel is purely diagnostic, so junk on
    // it must never take the server down; keep draining to EOF.
    if let Some(stderr) = stderr {
        let log = log.clone();
        let name = key.name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            // One line at a time, capped at `MAX_STDERR_LINE` bytes per read: a
            // server that spews megabytes with no newline must not make this
            // buffer grow to match. `take` caps each iteration's read (a line
            // beyond the cap is logged truncated, and its remainder streams on
            // the next iterations); `Ok(0)` still only means pipe EOF.
            let mut buf = Vec::with_capacity(MAX_STDERR_LINE);
            loop {
                buf.clear();
                let n = (&mut reader)
                    .take(MAX_STDERR_LINE as u64)
                    .read_until(b'\n', &mut buf)
                    .await;
                // EOF (server exit), or the pipe itself broke.
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let truncated = buf.last() != Some(&b'\n');
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim_end_matches(['\n', '\r']);
                        if truncated {
                            log.log(
                                LogLevel::Warn,
                                &name,
                                &format!(
                                    "stderr: {line} [line exceeds {MAX_STDERR_LINE} bytes, truncated]"
                                ),
                            );
                        } else {
                            log.log(LogLevel::Warn, &name, &format!("stderr: {line}"));
                        }
                    }
                }
            }
        });
    }

    // `async-lsp`'s `MainLoop` drives the `futures` AsyncRead/Write; bridge the
    // transport's stdio streams with tokio-util's compat shims. Input is the
    // server's stdout (server→client), output its stdin (client→server).
    let (mainloop, mut socket) = new_client(
        key.clone(),
        event_tx.clone(),
        log.clone(),
        spawn.settings.clone(),
    );
    let mut mainloop_fut = tokio::spawn(async move {
        // `FramingGuard` validates the server's `Content-Length` announcements
        // *before* `async-lsp` can allocate for them (its `Message::read` has no
        // cap of its own), so a corrupt or hostile server cannot OOM the editor
        // with a single huge frame.
        mainloop
            .run_buffered(FramingGuard::new(stdout).compat(), stdin.compat_write())
            .await
    });

    // Initialize. Race the handshake against the loop ending, so a server that
    // dies mid-handshake is reported rather than hanging the await.
    let init = init_params(
        key.root.as_deref(),
        spawn,
        Some(std::process::id()),
        log,
        name,
    );
    let init_result = tokio::select! {
        res = socket.initialize(init) => res,
        _ = &mut mainloop_fut => {
            process.start_kill();
            let _ = process.wait().await;
            log.log(LogLevel::Error, name, "server exited during initialize");
            let _ = event_tx.send(LspEvent::ServerExited {
                key: key.clone(),
                message: "server exited during initialize".to_string(),
                code: None,
                signal: None,
            });
            return ServerOutcome::Failed;
        }
    };
    let init_result: InitializeResult = match init_result {
        Ok(r) => r,
        Err(e) => {
            mainloop_fut.abort();
            process.start_kill();
            let _ = process.wait().await;
            log.log(LogLevel::Error, name, &format!("initialize failed: {e}"));
            let _ = event_tx.send(LspEvent::ServerExited {
                key: key.clone(),
                message: format!("initialize failed: {e}"),
                code: None,
                signal: None,
            });
            return ServerOutcome::Failed;
        }
    };
    let _ = socket.initialized(InitializedParams {});
    // Push the config's `settings` once the server is ready to accept them. Many
    // servers (re)read their configuration only on `didChangeConfiguration`, so
    // this is what makes a `settings`-configured server actually run configured.
    if let Some(settings) = spawn.settings.clone() {
        let _ = socket.did_change_configuration(DidChangeConfigurationParams { settings });
    }
    // Distill the handshake into the editor-facing caps/encoding/raw trio (shared
    // with the sync wasm client). The raw result rides along for the config's
    // `on_init` hook (Phase 3).
    let (caps, encoding, raw_init) = read_init_result(&init_result);
    log.log(
        LogLevel::Info,
        name,
        &format!(
            "initialized: encoding={encoding:?}, sync={:?}",
            caps.sync_kind
        ),
    );
    let _ = event_tx.send(LspEvent::Initialized {
        key: key.clone(),
        caps,
        encoding,
        init_result: raw_init,
    });

    // Serve: ferry document-sync notifications to the socket until told to stop
    // or the child dies.
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(ServerMsg::Notify(note)) => apply_notify(&mut socket, note, log, name),
                // A language-feature request: clone the socket and await the reply
                // on a detached task, so a slow round-trip never stalls this serve
                // loop (further sync/requests keep flowing) and the editor never
                // blocks (Decision 3). The resolved value is forwarded as a Reply
                // event tagged with the editor's token; staleness is the editor's
                // call, not ours.
                Some(ServerMsg::Request(token, req)) => {
                    let mut sock = socket.clone();
                    let tx = event_tx.clone();
                    let key = key.clone();
                    let log = log.clone();
                    tokio::spawn(async move {
                        let reply = issue_request(&mut sock, req, &log, &key.name).await;
                        let _ = tx.send(LspEvent::Reply { key, token, reply });
                    });
                }
                // The editor answered a `workspace/applyEdit`: hand the outcome to
                // the client loop as a custom event, where it resolves the parked
                // request handler that frames the response. Routed through the loop
                // (rather than answered here) because the pending map lives in the
                // router's state, which only the loop can touch.
                Some(ServerMsg::ApplyEditResponse(id, outcome)) => {
                    let _ = socket.emit(ApplyEditDone { id, outcome });
                }
                // Explicit shutdown, or the manager dropped our sender: tear down.
                Some(ServerMsg::Shutdown) | None => {
                    log.log(LogLevel::Info, name, "shutting down");
                    let _ = socket.shutdown(()).await;
                    let _ = socket.exit(());
                    mainloop_fut.abort();
                    process.start_kill();
                    let _ = process.wait().await;
                    return ServerOutcome::Shutdown;
                }
            },
            res = &mut mainloop_fut => {
                // The loop ended: the child closed its pipe, exited, or the
                // stream failed (a framing-guard rejection, a corrupt frame).
                // Capture the exit status for the config's `on_exit(code,
                // signal, client)` hook (Phase 3) — this is the one exit path
                // with a registered client. The loop's own error is surfaced in
                // the message so a guard rejection is never a silent mystery.
                let reason = match res {
                    Ok(Ok(())) => "language server exited".to_string(),
                    Ok(Err(e)) => format!("language server stream failed: {e}"),
                    Err(e) => format!("language server main loop task panicked: {e}"),
                };
                process.start_kill();
                let (code, signal) = process.wait().await;
                log.log(LogLevel::Warn, name, &reason);
                let _ = event_tx.send(LspEvent::ServerExited {
                    key: key.clone(),
                    message: reason,
                    code,
                    signal,
                });
                return ServerOutcome::Failed;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Framing guard — bound what a server's stdout can make us hold
// ---------------------------------------------------------------------------

/// Largest `Content-Length` a language server may announce (256 MiB) — the
/// native-leg mirror of the wasm leg's `sync_client` frame cap. A trustworthy
/// server never frames anything near this; an announced length past it is a
/// corrupt/hostile header, so the connection fails (loudly) rather than letting
/// `async-lsp` allocate for it.
const MAX_FRAME_LEN: usize = 256 * 1024 * 1024;

/// Largest header line the guard passes before failing the connection (64 KiB) —
/// the mirror of the wasm leg's `MAX_HEADER_LEN`. Real LSP frame headers are
/// ~100 bytes; a line that never terminates must not grow `async-lsp`'s header
/// `String` without bound.
const MAX_HEADER_LEN: usize = 64 * 1024;

/// Largest single line buffered while draining a server's stderr to the log
/// (8 KiB). A line longer than this is logged truncated; memory stays bounded
/// no matter how much the server spews.
const MAX_STDERR_LINE: usize = 8 * 1024;

const CONTENT_LENGTH_NAME: &[u8] = b"content-length";

/// A [`tokio::io::AsyncRead`] guard over a language server's stdout that
/// validates the LSP framing *before* `async-lsp` can act on it.
///
/// `async-lsp` 0.2.4's `Message::read` allocates `vec![0u8; content_len]`
/// straight from the parsed `Content-Length` header — with no cap of its own —
/// so a corrupt or hostile server announcing an enormous frame OOM-aborts the
/// whole editor before the body even starts to arrive. This guard scans the
/// bytes between the server and `MainLoop::run_buffered`: an announced
/// `Content-Length` past [`MAX_FRAME_LEN`], or a header line longer than
/// [`MAX_HEADER_LEN`], fails the stream with an I/O error, which ends the main
/// loop (and kills the connection) loudly — the same fail-closed policy as the
/// wasm leg's `sync_client` frame caps.
struct FramingGuard<R> {
    inner: R,
    phase: Phase,
    scan: HeaderScan,
    /// The `Content-Length` of the frame whose headers are being read
    /// (last-announced wins, like `async-lsp`). `None` until a line announces
    /// one.
    announced: Option<usize>,
    /// Body bytes left to pass through before the next header section; only
    /// meaningful in [`Phase::Body`]. `usize::MAX` when the frame had no
    /// parseable `Content-Length` — `async-lsp` rejects such a frame itself, the
    /// guard just passes the bytes through.
    body_remaining: usize,
}

/// Whether the guard is reading frame headers or passing a frame body through.
#[derive(PartialEq, Eq)]
enum Phase {
    Headers,
    Body,
}

/// One header line's scan state: match the `Content-Length` name
/// (case-insensitive), then accumulate its decimal value. Reset at each line
/// end.
struct HeaderScan {
    /// Bytes of [`CONTENT_LENGTH_NAME`] matched so far. `None` once a mismatch
    /// makes this line incapable of being a Content-Length line (skip to its
    /// end).
    name: Option<usize>,
    /// The value phase (only meaningful once the name matched).
    value: ValuePhase,
    /// The announced value accumulated so far (saturating).
    value_so_far: u64,
    /// Bytes in the current line, excluding its trailing `\r\n`; bounds a line
    /// that never terminates.
    line_len: usize,
    /// Saw a `\r` whose following byte is still unknown (the `\n` of a line
    /// terminator, or a stray byte).
    pending_cr: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValuePhase {
    /// The `content-length` name just completed; the next byte should be `:`.
    ExpectColon,
    /// Seen `:`; skipping spaces before the digits.
    SkipSpaces,
    /// Accumulating decimal digits.
    Digits,
    /// A byte made this line's value unparseable; skip to the line end.
    Dead,
}

impl HeaderScan {
    fn new() -> Self {
        Self {
            name: Some(0),
            value: ValuePhase::Dead,
            value_so_far: 0,
            line_len: 0,
            pending_cr: false,
        }
    }

    /// Feed one non-newline byte of the current header line.
    fn scan_byte(&mut self, b: u8) -> io::Result<()> {
        self.line_len += 1;
        if self.line_len > MAX_HEADER_LEN {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("lsp: header line exceeds the {MAX_HEADER_LEN} byte cap"),
            ));
        }
        match self.name {
            Some(matched) if matched < CONTENT_LENGTH_NAME.len() => {
                if CONTENT_LENGTH_NAME[matched].eq_ignore_ascii_case(&b) {
                    let matched = matched + 1;
                    self.name = Some(matched);
                    if matched == CONTENT_LENGTH_NAME.len() {
                        // The name is complete; the value follows this line's `:`.
                        self.value = ValuePhase::ExpectColon;
                    }
                } else {
                    // Not a Content-Length line: ignore the rest of it.
                    self.name = None;
                    self.value = ValuePhase::Dead;
                }
            }
            _ => match self.value {
                ValuePhase::ExpectColon => {
                    if b == b':' {
                        self.value = ValuePhase::SkipSpaces;
                    } else {
                        self.value = ValuePhase::Dead;
                    }
                }
                ValuePhase::SkipSpaces => {
                    if b == b' ' {
                        // keep skipping
                    } else if b.is_ascii_digit() {
                        self.value = ValuePhase::Digits;
                        self.value_so_far = u64::from(b - b'0');
                    } else {
                        self.value = ValuePhase::Dead;
                    }
                }
                ValuePhase::Digits => {
                    if b.is_ascii_digit() {
                        self.value_so_far = self
                            .value_so_far
                            .saturating_mul(10)
                            .saturating_add(u64::from(b - b'0'));
                        if self.value_so_far > MAX_FRAME_LEN as u64 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "lsp: Content-Length exceeds the {MAX_FRAME_LEN} byte frame cap"
                                ),
                            ));
                        }
                    } else {
                        // Trailing junk after the digits: not a parseable value.
                        self.value = ValuePhase::Dead;
                    }
                }
                ValuePhase::Dead => {}
            },
        }
        Ok(())
    }

    /// The value this line announced, if it ended as a well-formed
    /// `Content-Length` line.
    fn announced(&self) -> Option<usize> {
        match self.value {
            ValuePhase::Digits => Some(self.value_so_far as usize),
            _ => None,
        }
    }
}

impl<R> FramingGuard<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            phase: Phase::Headers,
            scan: HeaderScan::new(),
            announced: None,
            body_remaining: usize::MAX,
        }
    }

    /// Scan a chunk of bytes the inner reader produced, updating the framing
    /// state and failing the connection on an oversized announcement.
    fn scan(&mut self, chunk: &[u8]) -> io::Result<()> {
        for &b in chunk {
            // Body bytes are payload, never headers: count them down before
            // touching the scan state.
            if self.phase == Phase::Body {
                match self.body_remaining {
                    usize::MAX => continue, // no announced length: pass through
                    0 => {
                        // The announced body is fully consumed; this byte opens
                        // the next header section.
                        self.phase = Phase::Headers;
                        self.scan = HeaderScan::new();
                        self.announced = None;
                    }
                    _ => {
                        self.body_remaining -= 1;
                        continue;
                    }
                }
            }
            if let Some(remaining) = scan_header_byte(&mut self.scan, &mut self.announced, b)? {
                // The blank line ended the header section.
                self.phase = Phase::Body;
                self.body_remaining = remaining;
                self.scan = HeaderScan::new();
                self.announced = None;
            }
        }
        Ok(())
    }
}

/// Feed one byte of a header section, stashing a line's announced value into
/// `announced`. Returns `Some(remaining)` when the blank line ends the section:
/// the frame's body length, or `usize::MAX` to pass through when no
/// `Content-Length` was announced (`async-lsp` rejects such a frame itself).
fn scan_header_byte(
    scan: &mut HeaderScan,
    announced: &mut Option<usize>,
    b: u8,
) -> io::Result<Option<usize>> {
    match b {
        b'\r' => {
            scan.pending_cr = true;
            Ok(None)
        }
        b'\n' => {
            if scan.line_len == 0 {
                // Blank line (or a bare `\n`): the header section is over.
                Ok(Some(announced.unwrap_or(usize::MAX)))
            } else {
                if scan.pending_cr {
                    // A properly terminated line: keep its announced value for
                    // the (last-wins) body length. A bare-`\n` line is rejected
                    // by `async-lsp` itself, so its value is not stashed.
                    if let Some(v) = scan.announced() {
                        *announced = Some(v);
                    }
                }
                *scan = HeaderScan::new();
                Ok(None)
            }
        }
        b => {
            if scan.pending_cr {
                // A `\r` not followed by `\n`: an ordinary byte (counts toward
                // the line bound, kills the value phase).
                scan.scan_byte(b'\r')?;
                scan.pending_cr = false;
            }
            scan.scan_byte(b)?;
            Ok(None)
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for FramingGuard<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = self.get_mut();
        let start = buf.filled().len();
        let poll = Pin::new(&mut me.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = poll {
            if let Err(e) = me.scan(&buf.filled()[start..]) {
                return Poll::Ready(Err(e));
            }
        }
        poll
    }
}

