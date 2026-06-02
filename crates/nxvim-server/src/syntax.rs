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
    /// The supervisor gave up: the worker failed to spawn or kept crashing past
    /// the breaker's limit, so syntax is permanently down for this server's life.
    /// Sent once so the server can tell the user instead of leaving buffers
    /// silently un-highlighted.
    Disabled,
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

    /// `ts_close`: the editor deleted a buffer; drop the worker's state for it.
    pub fn close(&self, buffer: u64) {
        self.send("ts_close", vec![Value::Map(vec![kv_u64("buffer", buffer)])]);
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
/// circuit breaker stops a crash-loop (a poison grammar) — or a missing/broken
/// worker binary that won't even spawn — from burning CPU forever.
async fn supervise(
    program: PathBuf,
    mut cmd_rx: UnboundedReceiver<Cmd>,
    event_tx: UnboundedSender<SyntaxEvent>,
) {
    // Recent *failure* timestamps drive the breaker. A failure is a spawn error,
    // a missing stdio pipe, or a crash of a live child — all the ways a worker
    // lifetime can end other than the server shutting down.
    let mut failures: VecDeque<Instant> = VecDeque::new();
    const WINDOW: Duration = Duration::from_secs(10);
    // Failures within `WINDOW` past this count mean the worker is persistently
    // broken (a poison grammar or a missing binary): give up rather than respawn
    // forever. Backoff between respawns escalates with the windowed failure
    // count, so even below the give-up line we never hammer.
    const GIVE_UP: usize = 5;
    const BASE_BACKOFF: Duration = Duration::from_millis(200);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    // The initial spawn needs no re-open: the server's first sync already sends
    // `ts_open` (buffered until the child connects). Only *re*-spawns must ask the
    // server to re-open, so the first full-text send isn't done twice.
    let mut first_spawn = true;

    loop {
        // Run one worker lifetime. `true` ⇒ the server is shutting down (clean
        // exit); `false` ⇒ a worker failure that should trip the breaker.
        if run_worker_once(&program, &mut cmd_rx, &event_tx, &mut first_spawn).await {
            return;
        }
        if cmd_rx.is_closed() {
            return;
        }

        // Breaker: record the failure and age out anything older than the window.
        let now = Instant::now();
        failures.push_back(now);
        while failures
            .front()
            .is_some_and(|t| now.duration_since(*t) > WINDOW)
        {
            failures.pop_front();
        }

        if failures.len() >= GIVE_UP {
            // Persistent failure. Stop respawning, tell the server once so it can
            // surface "syntax unavailable", then idle — still draining commands so
            // the client's `send`s never error — until the server shuts down.
            // Syntax comes back on the next server start.
            let _ = event_tx.send(SyntaxEvent::Disabled);
            while cmd_rx.recv().await.is_some() {}
            return;
        }

        // Escalating backoff: 200ms, 400, 800, … doubling with the windowed
        // failure count, capped, so a healthy worker (window long since drained)
        // restarts promptly while a flapping one is throttled.
        let backoff = (BASE_BACKOFF * (1u32 << (failures.len() - 1))).min(MAX_BACKOFF);
        tokio::time::sleep(backoff).await;
    }
}

/// One worker lifetime: spawn, pump commands/notifications until the child dies
/// or the server drops its command sender, then reap. Returns `true` only when
/// the *server* is shutting down (a clean exit); every worker-side failure —
/// spawn error, missing stdio pipe, crash — returns `false` so the supervisor's
/// breaker decides whether to respawn.
async fn run_worker_once(
    program: &PathBuf,
    cmd_rx: &mut UnboundedReceiver<Cmd>,
    event_tx: &UnboundedSender<SyntaxEvent>,
    first_spawn: &mut bool,
) -> bool {
    let mut child = match spawn(program) {
        Ok(c) => c,
        Err(_) => return false, // couldn't spawn → breaker
    };
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        // Pipes unexpectedly missing: reap and treat as a failure so the breaker
        // retries, rather than permanently disabling syntax.
        let _ = child.start_kill();
        let _ = child.wait().await;
        return false;
    };
    let (rpc, mut incoming) = connect(stdout, stdin);

    // A *re*-spawned process has no buffers; ask the server to re-open. The very
    // first spawn is already covered by the server's initial sync.
    if !*first_spawn && event_tx.send(SyntaxEvent::Restarted).is_err() {
        return true; // server gone
    }
    *first_spawn = false;

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

    // Deterministic reap: SIGKILL then await the exit, so we don't leak a zombie
    // or depend on `kill_on_drop` timing.
    let _ = child.start_kill();
    let _ = child.wait().await;
    server_gone
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
