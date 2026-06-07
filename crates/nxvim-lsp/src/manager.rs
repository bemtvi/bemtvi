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
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_lsp::LanguageServer;
use lsp_types::{
    DidChangeConfigurationParams, InitializeParams, InitializeResult, InitializedParams, Url,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::client::{
    encoding_of, exit_code_signal, merged_client_capabilities, new_client, provider_caps,
    sync_kind_of,
};
use crate::dispatch::{apply_notify, issue_request};
use crate::log::{LogLevel, LspLog};
use crate::protocol::{
    LspEvent, LspNotify, LspRequest, ReqToken, ServerCaps, ServerKey, ServerSpawn,
};

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
    Shutdown {
        key: ServerKey,
    },
}

/// A message from the supervisor to one per-server task.
enum ServerMsg {
    Notify(LspNotify),
    Request(ReqToken, LspRequest),
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
}

impl LspManager {
    /// Create the manager and the receiver the server loop selects on. No task is
    /// spawned and no process launched until [`LspManager::ensure_server`].
    pub fn new() -> (LspManager, UnboundedReceiver<LspEvent>) {
        let (cmd_tx, cmd_rx) = unbounded_channel();
        let (event_tx, event_rx) = unbounded_channel();
        let manager = LspManager {
            cmd_tx,
            start: Some((cmd_rx, event_tx)),
            started: false,
        };
        (manager, event_rx)
    }

    /// Spawn the supervisor task if it isn't running yet.
    fn ensure_started(&mut self) {
        if self.started {
            return;
        }
        if let Some((cmd_rx, event_tx)) = self.start.take() {
            tokio::spawn(run_supervisor(cmd_rx, event_tx));
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
                tokio::spawn(run_server(key, spawn, rx, event_tx.clone(), log.clone()));
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
/// `nxvim-server`'s `supervise`, scoped to a single child.
async fn run_server(
    key: ServerKey,
    spawn: ServerSpawn,
    mut rx: UnboundedReceiver<ServerMsg>,
    event_tx: UnboundedSender<LspEvent>,
    log: Arc<LspLog>,
) {
    // Recent failure timestamps drive the breaker (a window of repeated crashes
    // ⇒ persistently broken: stop respawning).
    let mut failures: VecDeque<Instant> = VecDeque::new();
    const WINDOW: Duration = Duration::from_secs(10);
    const GIVE_UP: usize = 5;
    const BASE_BACKOFF: Duration = Duration::from_millis(200);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    loop {
        match run_server_once(&key, &spawn, &mut rx, &event_tx, &log).await {
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
            // Idle, still draining commands so the manager's sends never error,
            // until the manager drops us.
            while rx.recv().await.is_some() {}
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
) -> ServerOutcome {
    let name = key.name.as_str();
    log.log(
        LogLevel::Info,
        name,
        &format!("starting {} in {}", spawn.program, key.root.display()),
    );
    let mut child = match Command::new(&spawn.program)
        .args(&spawn.args)
        .current_dir(&key.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Captured (not null'd) so the server's stderr — panics, RA_LOG output —
        // reaches the log; a reader task below drains it so the pipe never blocks.
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
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
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        log.log(
            LogLevel::Error,
            name,
            "server stdio pipes unexpectedly missing",
        );
        let _ = event_tx.send(LspEvent::ServerExited {
            key: key.clone(),
            message: "server stdio pipes unexpectedly missing".to_string(),
            code: None,
            signal: None,
        });
        return ServerOutcome::Failed;
    };
    // Drain the server's stderr into the log, one line at a time, until it closes
    // (server exit). Each line is logged at WARN so it shows at the default level.
    if let Some(stderr) = child.stderr.take() {
        let log = log.clone();
        let name = key.name.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                log.log(LogLevel::Warn, &name, &format!("stderr: {line}"));
            }
        });
    }

    // `async-lsp`'s `MainLoop` drives the `futures` AsyncRead/Write; bridge the
    // tokio child pipes with tokio-util's compat shims. Input is the child's
    // stdout (server→client), output its stdin (client→server).
    let (mainloop, mut socket) = new_client(key.clone(), event_tx.clone(), log.clone());
    let mut mainloop_fut = tokio::spawn(async move {
        mainloop
            .run_buffered(stdout.compat(), stdin.compat_write())
            .await
    });

    // Initialize. Race the handshake against the loop ending, so a server that
    // dies mid-handshake is reported rather than hanging the await.
    // `root_uri` is deprecated in favor of `workspace_folders`, but it is still
    // the most broadly honored way to tell a server its workspace root, so we set
    // it deliberately.
    #[allow(deprecated)]
    let init = InitializeParams {
        process_id: Some(std::process::id()),
        root_uri: Url::from_file_path(&key.root).ok(),
        // The config's `init_options`, or `settings` as the fallback (neovim's
        // behavior): a server that reads only `initialization_options` still sees
        // what a `settings`-only config configured.
        initialization_options: spawn
            .init_options
            .clone()
            .or_else(|| spawn.settings.clone()),
        // nxvim's base capabilities with the config's `capabilities` deep-merged
        // over them (config wins) — so a config that advertises extra capabilities
        // (snippet support, extra code-action kinds, …) is honored.
        capabilities: merged_client_capabilities(spawn.capabilities.as_ref(), log, name),
        ..Default::default()
    };
    let init_result = tokio::select! {
        res = socket.initialize(init) => res,
        _ = &mut mainloop_fut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
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
            let _ = child.start_kill();
            let _ = child.wait().await;
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
    let encoding = encoding_of(&init_result.capabilities);
    let sync_kind = sync_kind_of(&init_result.capabilities);
    let providers = provider_caps(&init_result.capabilities);
    log.log(
        LogLevel::Info,
        name,
        &format!("initialized: encoding={encoding:?}, sync={sync_kind:?}"),
    );
    let _ = event_tx.send(LspEvent::Initialized {
        key: key.clone(),
        caps: ServerCaps {
            sync_kind,
            providers,
        },
        encoding,
        // The raw result for the config's `on_init` hook (Phase 3); `Null` if it
        // somehow won't serialize (it always should).
        init_result: serde_json::to_value(&init_result).unwrap_or(serde_json::Value::Null),
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
                // Explicit shutdown, or the manager dropped our sender: tear down.
                Some(ServerMsg::Shutdown) | None => {
                    log.log(LogLevel::Info, name, "shutting down");
                    let _ = socket.shutdown(()).await;
                    let _ = socket.exit(());
                    mainloop_fut.abort();
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return ServerOutcome::Shutdown;
                }
            },
            _ = &mut mainloop_fut => {
                // The loop ended: the child closed its pipe or exited. Capture the
                // exit status for the config's `on_exit(code, signal, client)` hook
                // (Phase 3) — this is the one exit path with a registered client.
                let _ = child.start_kill();
                let (code, signal) = exit_code_signal(child.wait().await.ok());
                log.log(LogLevel::Warn, name, "language server exited");
                let _ = event_tx.send(LspEvent::ServerExited {
                    key: key.clone(),
                    message: "language server exited".to_string(),
                    code,
                    signal,
                });
                return ServerOutcome::Failed;
            }
        }
    }
}
