//! The host process seam: the interface the server spawns child processes through.
//!
//! This is the process-side companion to core's [`HostFs`](nxvim_core::HostFs) —
//! the daemon's *other* half (see
//! `docs/plans/2026-06-09-edit-host-and-browser-lua.md` → Phase 3). Where `HostFs`
//! is **synchronous** and lives in core (buffer I/O is on the editing path's
//! terms), `HostProc` is **async + event-routing** and lives *here*, in the
//! server: a process runs for wall-clock time and reports its pid and exit back as
//! [`LoopEvent`]s on the event-loop channel, never as a return value. It is
//! consumed by the async server (the [`EventLoop`](crate::evloop::EventLoop)
//! actor), not by core, which stays pure and synchronous.
//!
//! [`StdHostProc`] is the default: it spawns real local processes via
//! `tokio::process`. A daemon-backed implementation (Phase 3's full split)
//! forwards the spawn over the wire and relays the daemon's pid/exit back as the
//! same events — the event-loop actor never knows which it is talking to.
//!
//! **Scope (Phase 3b).** This seam backs the one-shot `vim.system` / `jobstart` /
//! `:!` path only — the single spawn site whose shape (run-to-completion, pid then
//! exit as events) matches this contract. LSP servers are long-lived bidirectional
//! raw pipes, a different shape, so they ride their own dedicated daemon leg
//! ([`RemoteLspTransport`](crate::RemoteLspTransport) over the `lsp_*` wire in
//! [`daemon`](crate::daemon)) rather than this one. The clipboard shell-outs (a
//! synchronous `Clipboard` provider) still keep their own spawning — folding them in
//! is a later slice whose shape is matched to the daemon wire protocol rather than
//! guessed ahead of it (the same discipline that scoped the `HostFs` seam).

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;

use tokio::process::Command;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::evloop::LoopEvent;

/// A child process to run: program + args (no shell), the environment to run it
/// in, and the bytes to feed its stdin. Mirrors [`LoopCommand::Spawn`]'s payload —
/// the actor builds one of these from a `Spawn` and hands it to the [`HostProc`].
///
/// [`LoopCommand::Spawn`]: crate::evloop::LoopCommand::Spawn
pub struct ProcSpec {
    /// Program then arguments; the first element is the executable.
    pub argv: Vec<String>,
    /// Working directory to run in, or the server's cwd when `None`.
    pub cwd: Option<String>,
    /// Extra environment variables, applied over the inherited environment.
    pub env: Vec<(String, String)>,
    /// Bytes written to the child's stdin then closed (empty feeds no input, and
    /// the child gets `/dev/null` rather than a pipe).
    pub stdin: Vec<u8>,
    /// Stream stdout incrementally: emit newline-delimited batches as
    /// [`ProcEvents::stdout`] as they arrive (the picker's streaming sources via
    /// `nx.run_stream`'s streamed stdout), and report empty stdout with the exit. When
    /// `false` the child runs to completion and its whole stdout is captured into
    /// the single [`ProcEvents::exited`] — the original `vim.system` behavior.
    pub stream: bool,
}

/// The handle a [`HostProc::run`] reports a child's progress through: the pid
/// shortly after spawn, then exactly one exit. It wraps the event-loop channel and
/// the callback `id` so an implementation never touches the server's internal
/// event enum — it just calls [`spawned`](ProcEvents::spawned) then
/// [`exited`](ProcEvents::exited). `exited` consumes the handle, so the
/// exactly-one-exit contract is enforced by the type: a second report won't
/// compile.
pub struct ProcEvents {
    id: u64,
    tx: UnboundedSender<LoopEvent>,
}

impl ProcEvents {
    /// Build the handle for callback `id`, reporting over the event-loop channel
    /// `tx`. Crate-internal: only the [`EventLoop`](crate::evloop::EventLoop) actor
    /// mints these, one per spawn, from a [`LoopCommand::Spawn`].
    ///
    /// [`LoopCommand::Spawn`]: crate::evloop::LoopCommand::Spawn
    pub(crate) fn new(id: u64, tx: UnboundedSender<LoopEvent>) -> ProcEvents {
        ProcEvents { id, tx }
    }

    /// Report the running child's pid (`None` if the spawn failed). Lets the
    /// `vim.system` handle expose a real pid shortly after the call returns.
    pub fn spawned(&self, pid: Option<u32>) {
        let _ = self.tx.send(LoopEvent::ProcessSpawned { id: self.id, pid });
    }

    /// Report a batch of stdout `lines` from a streaming child (`stream = true`).
    /// Fires zero or more times before [`exited`](ProcEvents::exited); takes
    /// `&self` (unlike `exited`) so it can be called repeatedly.
    pub fn stdout(&self, lines: Vec<String>) {
        let _ = self
            .tx
            .send(LoopEvent::ProcessStdout { id: self.id, lines });
    }

    /// Report the child's exit — its status code and captured output (`code = -1`
    /// on a spawn failure or a kill). Consumes the handle so it fires once.
    pub fn exited(self, code: i32, stdout: Vec<u8>, stderr: Vec<u8>) {
        let _ = self.tx.send(LoopEvent::ProcessExit {
            id: self.id,
            code,
            stdout,
            stderr,
        });
    }
}

/// The process-spawning seam the server's event loop runs children through.
///
/// One method, [`HostProc::run`], owns a child's whole lifecycle: spawn, report the
/// pid, feed stdin, then report the exit (or a kill) — exactly the run-to-completion
/// shape `vim.system` needs. It returns a boxed future (rather than being an
/// `async fn`) so the trait stays object-safe for `dyn HostProc`, matching the
/// `Box<dyn HostFs>` dependency-injection style the rest of the server uses without
/// pulling in an async-trait dependency. The future is `Send + 'static` so the
/// actor can `tokio::spawn` it; an implementation that needs shared state clones it
/// (behind an `Arc`) into the future rather than borrowing `self`.
pub trait HostProc: Send + Sync {
    /// Run one child to completion (or until killed) and report it via `events`.
    ///
    /// The contract the actor relies on: report the pid with
    /// [`ProcEvents::spawned`], then exactly one exit with [`ProcEvents::exited`]
    /// (`code = -1` on a spawn failure or a kill), so the one-shot `on_exit`
    /// callback always fires and is never leaked. A signal on `kill` terminates the
    /// child early.
    fn run(
        &self,
        spec: ProcSpec,
        kill: oneshot::Receiver<()>,
        events: ProcEvents,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>>;
}

/// The default [`HostProc`]: real local child processes via `tokio::process`.
/// Every front end uses this today; the daemon-backed implementation arrives with
/// the edit-host split.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdHostProc;

impl HostProc for StdHostProc {
    fn run(
        &self,
        spec: ProcSpec,
        kill: oneshot::Receiver<()>,
        events: ProcEvents,
    ) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        Box::pin(run_local_process(spec, kill, events))
    }
}

/// Run one local child process to completion (or until killed) and report it.
/// Spawns `spec.argv` with piped stdout/stderr and `kill_on_drop`, sends
/// [`LoopEvent::ProcessSpawned`] with its pid, then races the child's completion
/// against the kill signal: on a natural exit it reports the real status and
/// captured output; on a kill it lets the output future drop (terminating the
/// child) and reports `code = -1`. Either way exactly one
/// [`LoopEvent::ProcessExit`] is sent, so the one-shot `on_exit` callback always
/// fires and is dropped (never leaked).
async fn run_local_process(spec: ProcSpec, mut kill_rx: oneshot::Receiver<()>, events: ProcEvents) {
    let ProcSpec {
        argv,
        cwd,
        env,
        stdin,
        stream,
    } = spec;
    let Some((program, args)) = argv.split_first() else {
        events.spawned(None);
        events.exited(
            -1,
            Vec::new(),
            b"vim.system: cmd must be a non-empty list".to_vec(),
        );
        return;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        // A child fed stdin gets a real pipe (closed after the bytes are written,
        // so it sees EOF); one with no input keeps `/dev/null` so a process that
        // reads stdin doesn't block waiting on a tty.
        .stdin(if stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Detach the child from the editor's controlling terminal by putting it in its
    // own session (setsid). A session with no controlling terminal makes any
    // `/dev/tty` access fail, so an interactive tool — git/ssh prompting for a
    // password, a CLI drawing a progress bar — can never read or write the
    // terminal nxvim's TUI is painting. Without this, such a tool scribbles its
    // prompt over the screen and blocks on a tty read that never arrives; with it,
    // the tool errors instead, and that error rides the stderr pipe we capture and
    // surface through the editor's own UI. stdin stays null/closed and stdout/stderr
    // stay piped, so all of the child's I/O still flows through channels we own.
    #[cfg(unix)]
    {
        // SAFETY: `pre_exec` runs in the forked child before `execvp`, where only
        // async-signal-safe calls are permitted — `setsid(2)` is one. A freshly
        // forked child is never a process-group leader, so the call succeeds; we
        // ignore the result defensively either way (a failure leaves the child
        // attached, no worse than before).
        unsafe {
            command.pre_exec(|| {
                extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            });
        }
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Mirror the blocking `nx._system`: a missing tool degrades to
            // `code = -1` with the message on stderr rather than raising, so an
            // async `vim.system` can never break a config on a machine lacking it.
            events.spawned(None);
            events.exited(
                -1,
                Vec::new(),
                format!("vim.system: failed to spawn {program}: {e}").into_bytes(),
            );
            return;
        }
    };
    events.spawned(child.id());
    // Feed stdin (if any) from a detached task and close it, so the write runs
    // concurrently with reading stdout/stderr — a child that echoes a large input
    // back would otherwise deadlock (both sides blocked on full pipes).
    if !stdin.is_empty() {
        if let Some(mut sink) = child.stdin.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = sink.write_all(&stdin).await;
                let _ = sink.shutdown().await; // close → the child reads EOF
            });
        }
    }
    if stream {
        stream_local_process(child, kill_rx, events).await;
        return;
    }
    let (code, stdout, stderr) = tokio::select! {
        result = child.wait_with_output() => match result {
            Ok(out) => (out.status.code().unwrap_or(-1), out.stdout, out.stderr),
            Err(e) => (-1, Vec::new(), e.to_string().into_bytes()),
        },
        _ = &mut kill_rx => (
            // The `wait_with_output` future is dropped here, dropping the child,
            // whose `kill_on_drop` terminates it. `on_exit` still fires (code -1).
            -1,
            Vec::new(),
            b"vim.system: process killed".to_vec(),
        ),
    };
    events.exited(code, stdout, stderr);
}

/// Run a **streaming** child: read its stdout line by line, emitting newline-
/// delimited batches as [`ProcEvents::stdout`] as they arrive, while stderr is
/// collected for the final exit. Races against the kill signal at every read; on a
/// kill the child is dropped (terminated via `kill_on_drop`) and the exit reports
/// `code = -1`. Exactly one [`ProcEvents::exited`] is always sent (with empty
/// stdout — the output was already streamed), so `on_exit` never leaks.
async fn stream_local_process(
    mut child: tokio::process::Child,
    mut kill_rx: oneshot::Receiver<()>,
    events: ProcEvents,
) {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

    // Collect stderr concurrently in a detached task so a child that writes a lot
    // of stderr can't deadlock against the stdout reader (both pipes full).
    let stderr_buf = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::new()));
    if let Some(mut err) = child.stderr.take() {
        let sink = stderr_buf.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = err.read_to_end(&mut buf).await;
            *sink.lock().await = buf;
        });
    }

    // Stream stdout in batches: each newline-terminated line is pushed, and the
    // accumulated batch is flushed whenever a read yields (natural OS-read
    // batching) or it grows large, keeping the redraw cadence sane on big lists.
    let mut killed = false;
    if let Some(out) = child.stdout.take() {
        let mut reader = BufReader::new(out);
        let mut line = String::new();
        let mut batch: Vec<String> = Vec::new();
        loop {
            line.clear();
            tokio::select! {
                read = reader.read_line(&mut line) => match read {
                    Ok(0) => break, // EOF — the child closed stdout
                    Ok(_) => {
                        batch.push(line.trim_end_matches(['\n', '\r']).to_string());
                        if batch.len() >= 512 {
                            events.stdout(std::mem::take(&mut batch));
                        }
                    }
                    Err(_) => break,
                },
                _ = &mut kill_rx => { killed = true; break; }
            }
        }
        if !batch.is_empty() {
            events.stdout(batch);
        }
    }

    let code = if killed {
        -1
    } else {
        tokio::select! {
            status = child.wait() => status.ok().and_then(|s| s.code()).unwrap_or(-1),
            _ = &mut kill_rx => -1,
        }
    };
    let stderr = std::mem::take(&mut *stderr_buf.lock().await);
    events.exited(code, Vec::new(), stderr);
}

/// Run a **duplex** child (`nx.process.open`) — the keystone for an in-Lua client
/// that frames its own protocol (DAP / LSP-style Content-Length JSON) over a
/// long-lived pipe. Unlike [`run_local_process`] it keeps stdin **open** (fed
/// incrementally from `stdin_rx`, each `nx.process` handle `:write`) and streams
/// stdout/stderr back as **raw, un-split byte chunks** ([`LoopEvent::ProcOut`]) so
/// the caller owns the framing — never the newline batching the picker's
/// `nx.run_stream` does. Exactly one [`LoopEvent::ProcExit`] is sent (`code = -1`
/// on a spawn failure or a kill), after which the Lua handle is dead.
pub async fn run_duplex_process(
    id: u64,
    argv: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    mut kill_rx: oneshot::Receiver<()>,
    mut stdin_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    event_tx: UnboundedSender<LoopEvent>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Some((program, args)) = argv.split_first() else {
        let _ = event_tx.send(LoopEvent::ProcExit { id, code: -1 });
        return;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Detach from the editor's controlling terminal (see `run_local_process`): a
    // debug adapter must never read/write the tty nxvim is painting.
    #[cfg(unix)]
    {
        // SAFETY: `setsid(2)` is async-signal-safe and a fresh fork is never a
        // group leader; ignore the result defensively (see `run_local_process`).
        unsafe {
            command.pre_exec(|| {
                extern "C" {
                    fn setsid() -> i32;
                }
                setsid();
                Ok(())
            });
        }
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Surface the spawn failure on the child's stderr stream (so the Lua
            // client sees *why*), then the loud exit — never a silent drop.
            let _ = event_tx.send(LoopEvent::ProcOut {
                id,
                data: format!("nx.process: failed to spawn {program}: {e}").into_bytes(),
                stderr: true,
            });
            let _ = event_tx.send(LoopEvent::ProcExit { id, code: -1 });
            return;
        }
    };

    // Feed stdin from `stdin_rx` in a detached task: each `:write` chunk is written
    // and flushed; when the channel closes (the handle killed / the actor dropped
    // the sink) the pipe is shut so the child reads EOF.
    if let Some(mut sink) = child.stdin.take() {
        tokio::spawn(async move {
            while let Some(chunk) = stdin_rx.recv().await {
                if sink.write_all(&chunk).await.is_err() || sink.flush().await.is_err() {
                    break;
                }
            }
            let _ = sink.shutdown().await;
        });
    }

    // Stream stderr as raw chunks from a detached task (so a chatty stderr can't
    // deadlock the stdout reader against a full pipe).
    if let Some(mut err) = child.stderr.take() {
        let tx = event_tx.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 8192];
            loop {
                match err.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx
                            .send(LoopEvent::ProcOut {
                                id,
                                data: buf[..n].to_vec(),
                                stderr: true,
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });
    }

    // Read stdout raw, racing each read against the kill signal.
    let mut killed = false;
    if let Some(mut out) = child.stdout.take() {
        let mut buf = [0u8; 8192];
        loop {
            tokio::select! {
                read = out.read(&mut buf) => match read {
                    Ok(0) | Err(_) => break, // EOF — child closed stdout
                    Ok(n) => {
                        if event_tx.send(LoopEvent::ProcOut {
                            id,
                            data: buf[..n].to_vec(),
                            stderr: false,
                        }).is_err() {
                            return; // server gone
                        }
                    }
                },
                _ = &mut kill_rx => { killed = true; break; }
            }
        }
    }

    let code = if killed {
        -1
    } else {
        tokio::select! {
            status = child.wait() => status.ok().and_then(|s| s.code()).unwrap_or(-1),
            _ = &mut kill_rx => -1,
        }
    };
    let _ = event_tx.send(LoopEvent::ProcExit { id, code });
}

/// Open a TCP client connection (`nx.socket.connect`) — the duplex sibling of
/// [`run_duplex_process`] for an adapter that speaks over a socket (a DAP
/// `type = "server"` adapter) rather than stdio. On a successful connect it sends
/// [`LoopEvent::SockConnected`], then streams inbound bytes as
/// [`LoopEvent::SockData`] (raw, un-framed) while draining `write_rx` to the socket,
/// until EOF / an I/O error / a requested close — then exactly one
/// [`LoopEvent::SockClosed`] (`error` set on a connect/I-O failure).
pub async fn run_socket_connection(
    id: u64,
    host: String,
    port: u16,
    mut close_rx: oneshot::Receiver<()>,
    mut write_rx: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    event_tx: UnboundedSender<LoopEvent>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let stream = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx.send(LoopEvent::SockClosed {
                id,
                error: Some(format!("nx.socket: connect to {host}:{port} failed: {e}")),
            });
            return;
        }
    };
    if event_tx.send(LoopEvent::SockConnected { id }).is_err() {
        return; // server gone
    }
    let (mut read_half, mut write_half) = stream.into_split();

    // Drain queued writes to the socket from a detached task (concurrent with the
    // read loop, so a large write can't deadlock against a full receive buffer).
    tokio::spawn(async move {
        while let Some(chunk) = write_rx.recv().await {
            if write_half.write_all(&chunk).await.is_err() || write_half.flush().await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    // Read raw inbound bytes, racing each read against the close signal.
    let mut buf = [0u8; 8192];
    let error;
    loop {
        tokio::select! {
            read = read_half.read(&mut buf) => match read {
                Ok(0) => { error = None; break; } // clean EOF (peer closed)
                Ok(n) => {
                    if event_tx.send(LoopEvent::SockData { id, data: buf[..n].to_vec() }).is_err() {
                        return; // server gone
                    }
                }
                Err(e) => { error = Some(format!("nx.socket: read error: {e}")); break; }
            },
            _ = &mut close_rx => { error = None; break; } // requested close
        }
    }
    let _ = event_tx.send(LoopEvent::SockClosed { id, error });
}
