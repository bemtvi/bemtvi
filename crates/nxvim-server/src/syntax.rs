//! The server's client to the treesitter syntax **process**.
//!
//! Tree-sitter parsing runs in a separate, crash-isolated child process
//! (`nxvim --__ts-worker`, implemented by `nxvim-ts`). This module spawns and
//! **supervises** it: if the child dies — even by a grammar segfault — it is
//! respawned with backoff, guarded by a circuit breaker, and the editor is never
//! blocked or affected.
//!
//! The link is one-way advisory: the server sends `ts_open`/`ts_edit`/`ts_view`
//! commands (never awaiting), and worker `ts_highlights`/`ts_error`
//! notifications arrive as [`SyntaxEvent`]s the server loop selects on. The
//! supervisor runs as its own task, so respawns are invisible to the editor.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use nxvim_rpc::{connect, Incoming};
use rmpv::Value;
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Environment override for the worker executable. Defaults to the current
/// binary (so the real `nxvim` re-invokes itself). Tests point this at the built
/// `nxvim` binary, since their own test executable is `current_exe()`.
const WORKER_ENV: &str = "NXVIM_TS_WORKER";
/// Internal flag selecting worker mode (mirrors `nxvim`'s `main.rs`).
const WORKER_FLAG: &str = "--__ts-worker";

/// A command for the worker: a pre-built RPC notification.
struct Cmd {
    method: &'static str,
    params: Vec<Value>,
}

/// Something that happened on the worker link, delivered to the server loop.
pub enum SyntaxEvent {
    /// A fresh worker process came up (initial spawn or a respawn). The server
    /// must re-`open` its buffers from full text.
    Restarted,
    /// A notification from the worker (`ts_highlights` / `ts_error`).
    Notification { method: String, params: Vec<Value> },
}

/// Handle the server holds to talk to the syntax process. The supervisor task is
/// spawned lazily on the first [`SyntaxClient::ensure_started`], so buffers with
/// no known grammar never spawn a process.
pub struct SyntaxClient {
    cmd_tx: UnboundedSender<Cmd>,
    /// Spawn materials, taken when the supervisor is first started.
    start: Option<StartKit>,
    started: bool,
}

struct StartKit {
    program: PathBuf,
    cmd_rx: UnboundedReceiver<Cmd>,
    event_tx: UnboundedSender<SyntaxEvent>,
}

impl SyntaxClient {
    /// Create the client and the receiver the server loop selects on. No process
    /// is spawned until [`SyntaxClient::ensure_started`].
    pub fn new() -> (SyntaxClient, UnboundedReceiver<SyntaxEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let program = std::env::var(WORKER_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nxvim")));
        let client = SyntaxClient {
            cmd_tx,
            start: Some(StartKit {
                program,
                cmd_rx,
                event_tx,
            }),
            started: false,
        };
        (client, event_rx)
    }

    /// Spawn the supervisor task if it isn't running yet.
    pub fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        if let Some(kit) = self.start.take() {
            tokio::spawn(supervise(kit.program, kit.cmd_rx, kit.event_tx));
            self.started = true;
        }
    }

    /// `ts_open`: (re)initialize a buffer from full text.
    pub fn open(
        &self,
        buffer: u64,
        tick: u64,
        language: &str,
        text: &str,
        first: usize,
        last: usize,
    ) {
        self.send(
            "ts_open",
            vec![Value::Map(vec![
                kv_u64("buffer", buffer),
                kv_u64("tick", tick),
                (Value::from("language"), Value::from(language)),
                (Value::from("text"), Value::from(text)),
                kv_u64("first_line", first as u64),
                kv_u64("last_line", last as u64),
            ])],
        );
    }

    /// `ts_edit`: apply a batch of edit deltas.
    pub fn edit(&self, buffer: u64, tick: u64, edits: Value, first: usize, last: usize) {
        self.send(
            "ts_edit",
            vec![Value::Map(vec![
                kv_u64("buffer", buffer),
                kv_u64("tick", tick),
                (Value::from("edits"), edits),
                kv_u64("first_line", first as u64),
                kv_u64("last_line", last as u64),
            ])],
        );
    }

    /// `ts_view`: viewport moved, no text change.
    pub fn view(&self, buffer: u64, first: usize, last: usize) {
        self.send(
            "ts_view",
            vec![Value::Map(vec![
                kv_u64("buffer", buffer),
                kv_u64("first_line", first as u64),
                kv_u64("last_line", last as u64),
            ])],
        );
    }

    fn send(&self, method: &'static str, params: Vec<Value>) {
        let _ = self.cmd_tx.send(Cmd { method, params });
    }
}

fn kv_u64(key: &'static str, value: u64) -> (Value, Value) {
    (Value::from(key), Value::from(value))
}

/// Supervise the worker process for the life of the server: spawn it, pump
/// commands to it and notifications back, and respawn it whenever it dies. A
/// circuit breaker stops a crash-loop (a poison grammar) from burning CPU.
async fn supervise(
    program: PathBuf,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    event_tx: UnboundedSender<SyntaxEvent>,
) {
    // Recent restart timestamps, for the breaker.
    let mut crashes: VecDeque<Instant> = VecDeque::new();
    const WINDOW: Duration = Duration::from_secs(10);
    const MAX_CRASHES: usize = 3;
    const COOLDOWN: Duration = Duration::from_secs(30);
    // The initial spawn needs no re-open: the server's first sync already sends
    // `ts_open` (buffered until the child connects). Only *re*-spawns must ask the
    // server to re-open, so the first full-text send isn't done twice.
    let mut first_spawn = true;

    loop {
        let mut child = match spawn(&program) {
            Ok(c) => c,
            Err(_) => {
                // Couldn't even spawn; wait and retry (server may be shutting down).
                tokio::time::sleep(Duration::from_secs(1)).await;
                if cmd_rx.is_closed() {
                    return;
                }
                continue;
            }
        };
        let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
            return;
        };
        let (rpc, mut incoming) = connect(stdout, stdin);

        // A *re*-spawned process has no buffers; ask the server to re-open. The
        // very first spawn is already covered by the server's initial sync.
        if !first_spawn && event_tx.send(SyntaxEvent::Restarted).is_err() {
            return;
        }
        first_spawn = false;

        // Pump until the child dies or the server drops its command sender.
        let server_gone = loop {
            tokio::select! {
                cmd = cmd_rx.recv() => match cmd {
                    Some(cmd) => rpc.notify(cmd.method, cmd.params),
                    None => break true, // server shutting down
                },
                msg = incoming.recv() => match msg {
                    Some(Incoming::Notification { method, params }) => {
                        if event_tx.send(SyntaxEvent::Notification { method, params }).is_err() {
                            break true;
                        }
                    }
                    Some(_) => {}
                    None => break false, // child closed its pipe (died)
                },
                status = child.wait() => {
                    let _ = status; // child exited (e.g. segfault/abort)
                    break false;
                }
            }
        };
        // Best-effort reap so we don't leak a zombie.
        let _ = child.start_kill();
        if server_gone || cmd_rx.is_closed() {
            return;
        }

        // Circuit breaker: back off, and cool down hard on a crash storm.
        let now = Instant::now();
        crashes.push_back(now);
        while crashes
            .front()
            .is_some_and(|t| now.duration_since(*t) > WINDOW)
        {
            crashes.pop_front();
        }
        if crashes.len() >= MAX_CRASHES {
            tokio::time::sleep(COOLDOWN).await;
            crashes.clear();
        } else {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

fn spawn(program: &PathBuf) -> std::io::Result<tokio::process::Child> {
    Command::new(program)
        .arg(WORKER_FLAG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
}
