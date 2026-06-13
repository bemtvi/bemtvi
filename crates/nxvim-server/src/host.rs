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
//! exit as events) already matches this contract. The clipboard shell-outs (a
//! synchronous `Clipboard` provider) and the LSP servers (long-lived, bidirectional
//! raw-pipe transport in `nxvim-lsp`) keep their own spawning for now; folding them
//! in is a later slice whose shape is matched to the daemon wire protocol rather
//! than guessed ahead of it (the same discipline that scoped the `HostFs` seam).

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
