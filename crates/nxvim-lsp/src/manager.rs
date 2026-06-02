//! [`LspManager`]: spawn, supervise, and route N language servers, bridging each
//! to the editor's single [`LspCommand`]/[`LspEvent`] channel pair.
//!
//! Shape mirrors `nxvim-server`'s `SyntaxClient`/`supervise`/`run_worker_once`:
//! a lazily-spawned supervisor task, per-child supervision with an
//! escalating-backoff circuit breaker, and a fire-and-forget command path so the
//! editor thread is never blocked. The differences are intrinsic to LSP: there
//! are many children (one per [`ServerKey`]) rather than one worker, and each is
//! driven by an [`async_lsp`] client `MainLoop` that owns its JSON-RPC framing
//! and id correlation, so we ferry typed [`LspNotify`]/[`LspEvent`] values rather
//! than raw msgpack notifications.

use std::collections::{HashMap, VecDeque};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use async_lsp::router::Router;
use async_lsp::{LanguageServer, MainLoop, ServerSocket};
use lsp_types::notification::{LogMessage, PublishDiagnostics, ShowMessage};
use lsp_types::{
    ClientCapabilities, Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, DidSaveTextDocumentParams, GeneralClientCapabilities,
    InitializeParams, InitializeResult, InitializedParams, PositionEncodingKind,
    ServerCapabilities, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
    TextDocumentIdentifier, TextDocumentItem, TextDocumentSyncCapability,
    TextDocumentSyncClientCapabilities, TextDocumentSyncKind, Url, VersionedTextDocumentIdentifier,
};
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Identifies one language server instance: a `(language, workspace-root)` pair.
/// nxvim runs at most one child per key and routes a buffer to its server by it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub language: &'static str,
    pub root: PathBuf,
}

/// How to launch a server: the program and its arguments. The working directory
/// is the key's `root`. Derived from the built-in config table (or the
/// `NXVIM_LSP_CMD` test override) by the server.
#[derive(Clone, Debug)]
pub struct ServerSpawn {
    pub program: String,
    pub args: Vec<String>,
}

/// The position encoding negotiated at `initialize` (Decision 4). nxvim columns
/// are byte offsets, so [`PositionEncoding::Utf8`] makes the conversion the
/// identity; the others need column math, applied by the server (which owns the
/// buffer text), never here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

/// The distilled server capabilities the editor needs. Grows as later phases add
/// features; Phase 1 needs only the document-sync kind (full vs incremental).
#[derive(Clone, Debug)]
pub struct ServerCaps {
    pub sync_kind: TextDocumentSyncKind,
}

/// A fire-and-forget document-sync notification, already in LSP coordinates. The
/// server does all byte↔position conversion before sending; the manager just
/// ferries to the matching server's `async-lsp` socket.
#[derive(Debug)]
pub enum LspNotify {
    DidOpen {
        uri: Url,
        language_id: String,
        version: i32,
        text: String,
    },
    DidChange {
        uri: Url,
        version: i32,
        changes: Vec<TextDocumentContentChangeEvent>,
    },
    DidSave {
        uri: Url,
        text: Option<String>,
    },
    DidClose {
        uri: Url,
    },
}

/// Server → editor, delivered to the main loop's `select!`. The distilled events
/// the editor acts on; lifecycle/framing/id-correlation are handled inside the
/// manager by `async-lsp` and never surface here.
#[derive(Debug)]
pub enum LspEvent {
    /// A server completed (or re-completed, after a respawn) its handshake. The
    /// editor records the encoding/caps and re-`didOpen`s its buffers — this
    /// doubles as the restart signal, the way `SyntaxEvent::Restarted` does.
    Initialized {
        key: ServerKey,
        caps: ServerCaps,
        encoding: PositionEncoding,
    },
    /// `textDocument/publishDiagnostics` for a document.
    Diagnostics {
        key: ServerKey,
        uri: Url,
        version: Option<i32>,
        diagnostics: Vec<Diagnostic>,
    },
    /// The child exited or its pipe closed (it will be respawned per the breaker,
    /// or stay down once the breaker gives up). Surfaced so the editor can tell
    /// the user instead of leaving buffers silently un-serviced.
    ServerExited { key: ServerKey, message: String },
    /// `window/logMessage` / `window/showMessage`, or a manager-level note.
    Log { key: ServerKey, message: String },
}

/// Commands from the editor to the supervisor, routed to per-server tasks by key.
enum LspCommand {
    Ensure { key: ServerKey, spawn: ServerSpawn },
    Notify { key: ServerKey, note: LspNotify },
    Shutdown { key: ServerKey },
}

/// A message from the supervisor to one per-server task.
enum ServerMsg {
    Notify(LspNotify),
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
                tokio::spawn(run_server(key, spawn, rx, event_tx.clone()));
            }
            LspCommand::Notify { key, note } => {
                if let Some(tx) = servers.get(&key) {
                    let _ = tx.send(ServerMsg::Notify(note));
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
) {
    // Recent failure timestamps drive the breaker (a window of repeated crashes
    // ⇒ persistently broken: stop respawning).
    let mut failures: VecDeque<Instant> = VecDeque::new();
    const WINDOW: Duration = Duration::from_secs(10);
    const GIVE_UP: usize = 5;
    const BASE_BACKOFF: Duration = Duration::from_millis(200);
    const MAX_BACKOFF: Duration = Duration::from_secs(5);

    loop {
        match run_server_once(&key, &spawn, &mut rx, &event_tx).await {
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
            let _ = event_tx.send(LspEvent::Log {
                key: key.clone(),
                message: format!("lsp: {} kept failing to start; giving up", spawn.program),
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
) -> ServerOutcome {
    let mut child = match Command::new(&spawn.program)
        .args(&spawn.args)
        .current_dir(&key.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = event_tx.send(LspEvent::ServerExited {
                key: key.clone(),
                message: format!("failed to spawn {}: {e}", spawn.program),
            });
            return ServerOutcome::Failed;
        }
    };
    let (Some(stdout), Some(stdin)) = (child.stdout.take(), child.stdin.take()) else {
        let _ = child.start_kill();
        let _ = child.wait().await;
        let _ = event_tx.send(LspEvent::ServerExited {
            key: key.clone(),
            message: "server stdio pipes unexpectedly missing".to_string(),
        });
        return ServerOutcome::Failed;
    };

    // `async-lsp`'s `MainLoop` drives the `futures` AsyncRead/Write; bridge the
    // tokio child pipes with tokio-util's compat shims. Input is the child's
    // stdout (server→client), output its stdin (client→server).
    let (mainloop, mut socket) = new_client(key.clone(), event_tx.clone());
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
        capabilities: client_capabilities(),
        ..Default::default()
    };
    let init_result = tokio::select! {
        res = socket.initialize(init) => res,
        _ = &mut mainloop_fut => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = event_tx.send(LspEvent::ServerExited {
                key: key.clone(),
                message: "server exited during initialize".to_string(),
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
            let _ = event_tx.send(LspEvent::ServerExited {
                key: key.clone(),
                message: format!("initialize failed: {e}"),
            });
            return ServerOutcome::Failed;
        }
    };
    let _ = socket.initialized(InitializedParams {});
    let _ = event_tx.send(LspEvent::Initialized {
        key: key.clone(),
        caps: ServerCaps {
            sync_kind: sync_kind_of(&init_result.capabilities),
        },
        encoding: encoding_of(&init_result.capabilities),
    });

    // Serve: ferry document-sync notifications to the socket until told to stop
    // or the child dies.
    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(ServerMsg::Notify(note)) => apply_notify(&mut socket, note),
                // Explicit shutdown, or the manager dropped our sender: tear down.
                Some(ServerMsg::Shutdown) | None => {
                    let _ = socket.shutdown(()).await;
                    let _ = socket.exit(());
                    mainloop_fut.abort();
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    return ServerOutcome::Shutdown;
                }
            },
            _ = &mut mainloop_fut => {
                // The loop ended: the child closed its pipe or exited.
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = event_tx.send(LspEvent::ServerExited {
                    key: key.clone(),
                    message: "language server exited".to_string(),
                });
                return ServerOutcome::Failed;
            }
        }
    }
}

/// Translate an [`LspNotify`] into the corresponding `async-lsp` notification.
/// Send errors are ignored: a dead socket is detected by the main loop ending.
fn apply_notify(socket: &mut ServerSocket, note: LspNotify) {
    let _ = match note {
        LspNotify::DidOpen {
            uri,
            language_id,
            version,
            text,
        } => socket.did_open(DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri,
                language_id,
                version,
                text,
            },
        }),
        LspNotify::DidChange {
            uri,
            version,
            changes,
        } => socket.did_change(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri, version },
            content_changes: changes,
        }),
        LspNotify::DidSave { uri, text } => socket.did_save(DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
            text,
        }),
        LspNotify::DidClose { uri } => socket.did_close(DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri },
        }),
    };
}

/// State shared by the client `MainLoop`'s notification handlers: which server
/// this loop belongs to, and the channel to forward distilled events on.
struct ClientState {
    key: ServerKey,
    event_tx: UnboundedSender<LspEvent>,
}

/// Build the `async-lsp` client `MainLoop` and its `ServerSocket`. The bare
/// [`Router`] is the service: the client only *receives* notifications
/// (diagnostics, log/show messages) whose handlers are trivial and panic-free,
/// so the concurrency/catch-unwind middleware a server needs is unnecessary
/// here. Unhandled server→client requests get a method-not-found response, which
/// language servers tolerate.
fn new_client(
    key: ServerKey,
    event_tx: UnboundedSender<LspEvent>,
) -> (MainLoop<Router<ClientState>>, ServerSocket) {
    MainLoop::new_client(|_server| {
        let mut router = Router::new(ClientState { key, event_tx });
        router.notification::<PublishDiagnostics>(|st, params| {
            let _ = st.event_tx.send(LspEvent::Diagnostics {
                key: st.key.clone(),
                uri: params.uri,
                version: params.version,
                diagnostics: params.diagnostics,
            });
            ControlFlow::Continue(())
        });
        router.notification::<LogMessage>(|st, params| {
            let _ = st.event_tx.send(LspEvent::Log {
                key: st.key.clone(),
                message: params.message,
            });
            ControlFlow::Continue(())
        });
        router.notification::<ShowMessage>(|st, params| {
            let _ = st.event_tx.send(LspEvent::Log {
                key: st.key.clone(),
                message: params.message,
            });
            ControlFlow::Continue(())
        });
        // Be lenient about everything else a server emits (progress, telemetry,
        // custom notifications/events): ignore rather than break the loop.
        router.unhandled_notification(|_st, _notif| ControlFlow::Continue(()));
        router.unhandled_event(|_st, _event| ControlFlow::Continue(()));
        router
    })
}

/// The client capabilities we advertise at `initialize`: UTF-8 preferred over
/// UTF-16 for positions (Decision 4), and document-save notifications.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            position_encodings: Some(vec![
                PositionEncodingKind::UTF8,
                PositionEncodingKind::UTF16,
            ]),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            synchronization: Some(TextDocumentSyncClientCapabilities {
                did_save: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The position encoding the server chose (LSP defaults to UTF-16 when the
/// server says nothing).
fn encoding_of(caps: &ServerCapabilities) -> PositionEncoding {
    match caps.position_encoding.as_ref().map(|e| e.as_str()) {
        Some("utf-8") => PositionEncoding::Utf8,
        Some("utf-32") => PositionEncoding::Utf32,
        _ => PositionEncoding::Utf16,
    }
}

/// The document-sync kind the server wants (full text, incremental deltas, or
/// none). Defaults to `NONE` when unspecified, so we never push changes a server
/// didn't ask for.
fn sync_kind_of(caps: &ServerCapabilities) -> TextDocumentSyncKind {
    match &caps.text_document_sync {
        Some(TextDocumentSyncCapability::Kind(kind)) => *kind,
        Some(TextDocumentSyncCapability::Options(opts)) => {
            opts.change.unwrap_or(TextDocumentSyncKind::NONE)
        }
        None => TextDocumentSyncKind::NONE,
    }
}
