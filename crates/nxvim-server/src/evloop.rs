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
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

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
    Spawn {
        id: u64,
        argv: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    },
    /// Terminate the async child running under `id` (a no-op if it already
    /// exited). The child is terminated via `kill_on_drop`, and its `on_exit`
    /// still fires with `code = -1` (the signal is not honored — see
    /// [`LoopOp::Kill`](nxvim_lua::LoopOp::Kill)).
    Kill { id: u64 },
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
    /// A child spawned via [`LoopCommand::Spawn`] exited; carries the result its
    /// `on_exit` callback receives (`code = -1` on spawn failure or a kill).
    ProcessExit {
        id: u64,
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
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
    started: bool,
}

impl EventLoop {
    /// Create the event loop and the receiver the server loop selects on. No task
    /// is spawned until the first [`EventLoop::send`].
    pub fn new() -> (EventLoop, UnboundedReceiver<LoopEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let evloop = EventLoop {
            cmd_tx,
            start: Some((cmd_rx, event_tx)),
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
            tokio::spawn(run_evloop(cmd_rx, event_tx));
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
) {
    // Live timer tasks and the per-process kill channels, keyed by callback id.
    let mut timers: HashMap<u64, JoinHandle<()>> = HashMap::new();
    let mut procs: HashMap<u64, oneshot::Sender<()>> = HashMap::new();
    while let Some(cmd) = cmd_rx.recv().await {
        // Drop handles whose tasks have finished, so a long run of one-shot timers
        // / processes can't accumulate dead entries.
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
            LoopCommand::Spawn { id, argv, cwd, env } => {
                let (kill_tx, kill_rx) = oneshot::channel();
                procs.insert(id, kill_tx);
                let event_tx = event_tx.clone();
                tokio::spawn(run_process(id, argv, cwd, env, kill_rx, event_tx));
            }
            LoopCommand::Kill { id } => {
                // Dropping the kill sender (or sending on it) wakes the process
                // task, which terminates the child via `kill_on_drop`.
                if let Some(kill_tx) = procs.remove(&id) {
                    let _ = kill_tx.send(());
                }
            }
        }
        // Forget kill channels whose process tasks have closed them (the child
        // exited and `run_process` dropped the receiver) — the leak guard for
        // procs, mirroring the timer prune above.
        procs.retain(|_, tx| !tx.is_closed());
    }
}

/// Run one child process to completion (or until killed) and report it. Spawns
/// `argv` with piped stdout/stderr and `kill_on_drop`, sends [`LoopEvent::ProcessSpawned`]
/// with its pid, then races the child's completion against the kill signal: on a
/// natural exit it reports the real status and captured output; on a kill it lets
/// the output future drop (terminating the child) and reports `code = -1`. Either
/// way exactly one [`LoopEvent::ProcessExit`] is sent, so the one-shot `on_exit`
/// callback always fires and is dropped (never leaked).
async fn run_process(
    id: u64,
    argv: Vec<String>,
    cwd: Option<String>,
    env: Vec<(String, String)>,
    mut kill_rx: oneshot::Receiver<()>,
    event_tx: UnboundedSender<LoopEvent>,
) {
    let Some((program, args)) = argv.split_first() else {
        let _ = event_tx.send(LoopEvent::ProcessSpawned { id, pid: None });
        let _ = event_tx.send(LoopEvent::ProcessExit {
            id,
            code: -1,
            stdout: Vec::new(),
            stderr: b"vim.system: cmd must be a non-empty list".to_vec(),
        });
        return;
    };
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    for (k, v) in env {
        command.env(k, v);
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            // Mirror the blocking `vim._system`: a missing tool degrades to
            // `code = -1` with the message on stderr rather than raising, so an
            // async `vim.system` can never break a config on a machine lacking it.
            let _ = event_tx.send(LoopEvent::ProcessSpawned { id, pid: None });
            let _ = event_tx.send(LoopEvent::ProcessExit {
                id,
                code: -1,
                stdout: Vec::new(),
                stderr: format!("vim.system: failed to spawn {program}: {e}").into_bytes(),
            });
            return;
        }
    };
    let _ = event_tx.send(LoopEvent::ProcessSpawned {
        id,
        pid: child.id(),
    });
    let exit = tokio::select! {
        result = child.wait_with_output() => match result {
            Ok(out) => LoopEvent::ProcessExit {
                id,
                code: out.status.code().unwrap_or(-1),
                stdout: out.stdout,
                stderr: out.stderr,
            },
            Err(e) => LoopEvent::ProcessExit {
                id,
                code: -1,
                stdout: Vec::new(),
                stderr: e.to_string().into_bytes(),
            },
        },
        _ = &mut kill_rx => LoopEvent::ProcessExit {
            // The `wait_with_output` future is dropped here, dropping the child,
            // whose `kill_on_drop` terminates it. `on_exit` still fires (code -1).
            id,
            code: -1,
            stdout: Vec::new(),
            stderr: b"vim.system: process killed".to_vec(),
        },
    };
    let _ = event_tx.send(exit);
}
