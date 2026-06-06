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
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_lsp::router::Router;
use async_lsp::{LanguageServer, MainLoop, ServerSocket};
use lsp_types::notification::{LogMessage, PublishDiagnostics, ShowMessage};
use lsp_types::{
    AnnotatedTextEdit, ClientCapabilities, CodeAction, CodeActionCapabilityResolveSupport,
    CodeActionClientCapabilities, CodeActionContext, CodeActionKindLiteralSupport,
    CodeActionLiteralSupport, CodeActionOrCommand, CodeActionParams, CodeActionResponse,
    CompletionClientCapabilities, CompletionItem, CompletionItemCapability,
    CompletionItemCapabilityResolveSupport, CompletionItemKind, CompletionParams,
    CompletionResponse, CompletionTextEdit, Diagnostic, DidChangeConfigurationParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentChangeOperation, DocumentChanges,
    DocumentFormattingClientCapabilities, DocumentFormattingParams, Documentation,
    FormattingOptions, GeneralClientCapabilities, GotoDefinitionParams, GotoDefinitionResponse,
    Hover, HoverContents, HoverParams, InitializeParams, InitializeResult, InitializedParams,
    Location, MarkedString, MarkupKind, MessageType, OneOf, ParameterLabel, Position,
    PositionEncodingKind, PublishDiagnosticsClientCapabilities, Range, ReferenceContext,
    ReferenceParams, RenameClientCapabilities, RenameParams, ServerCapabilities, SignatureHelp,
    SignatureHelpParams, TextDocumentClientCapabilities, TextDocumentContentChangeEvent,
    TextDocumentEdit, TextDocumentIdentifier, TextDocumentItem, TextDocumentPositionParams,
    TextDocumentSyncCapability, TextDocumentSyncClientCapabilities, TextDocumentSyncKind, TextEdit,
    Url, VersionedTextDocumentIdentifier, WorkspaceClientCapabilities, WorkspaceEdit,
    WorkspaceEditClientCapabilities,
};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::log::{LogLevel, LspLog};

/// Identifies one language server instance: a `(name, workspace-root)` pair.
/// `name` is the user-chosen LSP config name (`vim.lsp.config('<name>', …)` /
/// `vim.lsp.enable('<name>')`), arbitrary rather than a fixed filetype. nxvim runs
/// at most one child per key and routes a buffer to its server by it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServerKey {
    pub name: String,
    pub root: PathBuf,
}

/// How to launch a server and how to configure it. The working directory is the
/// key's `root`; `program`/`args` are derived from the resolved Lua config's `cmd`
/// (or the `NXVIM_LSP_CMD` test override). The three JSON payloads are the
/// config's `init_options` / `settings` / `capabilities`, forwarded at the
/// handshake so the server runs *configured*, not on defaults (Phase 2).
#[derive(Clone, Debug, Default)]
pub struct ServerSpawn {
    pub program: String,
    pub args: Vec<String>,
    /// Sent verbatim as `initialization_options` at `initialize` (falling back to
    /// `settings` when absent). `None` when the config sets neither.
    pub init_options: Option<serde_json::Value>,
    /// The `workspace/didChangeConfiguration` payload sent after `initialized`,
    /// and the `initialization_options` fallback. `None` when the config sets none.
    pub settings: Option<serde_json::Value>,
    /// Deep-merged OVER nxvim's base [`client_capabilities`] at `initialize`.
    /// `None` when the config adds none.
    pub capabilities: Option<serde_json::Value>,
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

/// The distilled server capabilities the editor needs: the document-sync kind
/// (full vs incremental) that drives `didChange`, and the per-feature provider
/// bools surfaced to Lua as `client.server_capabilities` (Phase 7b Slice 3).
#[derive(Clone, Debug)]
pub struct ServerCaps {
    pub sync_kind: TextDocumentSyncKind,
    pub providers: ProviderCaps,
}

/// The language-feature providers a server advertised at `initialize`, one bool
/// per feature nxvim implements. Surfaced to Lua as `client.server_capabilities`
/// so an `on_attach` can branch on what the server supports (e.g. only map `K`
/// when `hover`). Each field is the matching protocol `*Provider` reduced to
/// "advertised and not an explicit `false`".
#[derive(Clone, Debug, Default)]
pub struct ProviderCaps {
    pub definition: bool,
    pub declaration: bool,
    pub type_definition: bool,
    pub implementation: bool,
    pub references: bool,
    pub hover: bool,
    pub signature_help: bool,
    pub completion: bool,
    pub document_formatting: bool,
    pub rename: bool,
    pub code_action: bool,
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
    /// A **generic** notification issued by Lua `client:notify(method, params)`
    /// (Phase 5): an arbitrary `method` with raw JSON `params`, fire-and-forget
    /// like the document-sync notes. The `method` must be in the dispatch table
    /// ([`apply_dyn_notify`]); an unknown one is logged and dropped.
    Raw {
        method: String,
        params: serde_json::Value,
    },
}

/// A language-feature request the editor fires; its reply returns later as an
/// [`LspEvent::Reply`] carrying the same [`ReqToken`] (Decision 3). Already in LSP
/// coordinates — the server converts the cursor's byte column to the negotiated
/// encoding before sending, because only it has the buffer text. The manager
/// ferries the request to the matching server's socket and forwards the reply.
#[derive(Debug)]
pub enum LspRequest {
    Definition {
        uri: Url,
        position: Position,
    },
    Declaration {
        uri: Url,
        position: Position,
    },
    TypeDefinition {
        uri: Url,
        position: Position,
    },
    Implementation {
        uri: Url,
        position: Position,
    },
    References {
        uri: Url,
        position: Position,
        include_declaration: bool,
    },
    Hover {
        uri: Url,
        position: Position,
    },
    SignatureHelp {
        uri: Url,
        position: Position,
    },
    Completion {
        uri: Url,
        position: Position,
    },
    /// `textDocument/formatting` — whole-document formatting; the reply is a
    /// `TextEdit[]` (in the negotiated encoding) the editor applies to the buffer.
    Formatting {
        uri: Url,
    },
    /// `textDocument/rename` — the reply is a `WorkspaceEdit` the editor applies
    /// across the open buffers it touches.
    Rename {
        uri: Url,
        position: Position,
        new_name: String,
    },
    /// `textDocument/codeAction` for a range, with the diagnostics there as
    /// context; the reply is the available actions (titles + eager edits).
    CodeAction {
        uri: Url,
        range: Range,
        diagnostics: Vec<Diagnostic>,
    },
    /// `codeAction/resolve` — populate a lazy action's `edit`. The full original
    /// [`CodeAction`] (incl. its `data`) is round-tripped to the server, which
    /// returns it with the `edit` filled in.
    ResolveCodeAction {
        action: Box<CodeAction>,
    },
    /// `completionItem/resolve` — fetch a selected completion item's lazy
    /// `documentation`/`detail`. The original item is round-tripped verbatim as
    /// JSON (the editor holds it as [`CompletionItemData::resolve_data`]); the
    /// manager deserializes it back to a [`CompletionItem`] to send, because a
    /// server matches the resolve against the exact item it issued (rust_analyzer
    /// keys on the `data` blob). Mirrors [`LspRequest::ResolveCodeAction`].
    ResolveCompletion {
        item: serde_json::Value,
    },
    /// A **generic** request issued by Lua `client:request(method, params, …)`
    /// (Phase 5): an arbitrary `method` with raw JSON `params`, whose raw JSON
    /// result routes back to a Lua handler ([`LspReply::Raw`]). This is the seam
    /// the config `handlers` and server-specific commands (`:LspCargoReload`,
    /// `switchSourceHeader`, organize-imports) build on, distinct from the typed
    /// native features above. The `method` must be in the dispatch table
    /// ([`issue_dyn_request`]); an unknown one fails loud rather than no-op.
    Raw {
        method: String,
        params: serde_json::Value,
    },
}

/// The opaque correlation token the editor issues with a request and the manager
/// echoes back on the reply (Decision 3). `kind` distinguishes the feature
/// (definition vs references …) and `generation` is a monotonic counter the
/// editor uses to drop a stale reply (one whose request was superseded, or whose
/// cursor has since moved). The manager never interprets either field; it only
/// ferries them, exactly as `tick` rides along an `ts_highlights` request.
#[derive(Clone, Copy, Debug)]
pub struct ReqToken {
    pub kind: u16,
    pub generation: u64,
    /// The Lua deferred-callback id ([`vim._cb_fns`]) a generic
    /// [`LspRequest::Raw`] (Phase 5 `client:request`) routes its reply to; `0` for
    /// the typed native requests, which dispatch by `kind`/`generation` instead.
    /// The manager only ferries it, exactly as it does `kind`/`generation`.
    pub cb_id: u64,
}

/// The distilled result of an [`LspRequest`]. Each variant is already reduced to
/// what the editor renders, so the protocol's many response shapes never leak
/// past the manager: goto-family/`references` collapse to a flat location list,
/// hover/signatureHelp to plain display lines (markup extracted, never styled
/// here — full markdown rendering is a follow-up).
#[derive(Clone, Debug)]
pub enum LspReply {
    /// Target locations for a goto-family request or `references` (every
    /// `Location`/`Location[]`/`LocationLink[]` shape normalized to one list).
    Locations(Vec<Location>),
    /// Hover contents as plain display lines (empty ⇒ the server had nothing).
    Hover(Vec<String>),
    /// The active signature's label and, when known, its active parameter's text.
    /// Both `None` ⇒ no signature help (no signatures, or the server returned
    /// nothing).
    SignatureHelp {
        signature: Option<String>,
        active_parameter: Option<String>,
    },
    /// Completion candidates (a `CompletionItem[]` and a `CompletionList` both
    /// normalized to this shape). `is_incomplete` ⇒ the server's list is partial,
    /// so the editor re-requests as the typed prefix narrows rather than
    /// filtering the cache. The items' edit ranges stay in the server's negotiated
    /// position encoding for the editor to convert (it owns the buffer text).
    Completion {
        is_incomplete: bool,
        items: Vec<CompletionItemData>,
    },
    /// Whole-document text edits from `textDocument/formatting`, ranges still in
    /// the negotiated position encoding (an empty list ⇒ already formatted).
    Edits(Vec<TextEdit>),
    /// A normalized workspace edit from `textDocument/rename` (an empty list ⇒
    /// the server renamed nothing).
    WorkspaceEdit(WorkspaceEditData),
    /// The code actions available at the cursor — titles for the panel plus each
    /// action's eager edit (`None` ⇒ a lazy action to resolve, or a bare command).
    CodeActions(Vec<CodeActionData>),
    /// The edit a `codeAction/resolve` produced for a lazy action (`None` ⇒ the
    /// resolved action still carried no edit, or the request failed).
    ResolvedCodeAction(Option<WorkspaceEditData>),
    /// The `documentation`/`detail` a `completionItem/resolve` produced for the
    /// selected menu item — `documentation` distilled to plain lines like the
    /// inline field (Phase 1). Either is `None` when the resolved item still
    /// carried nothing there, the item was malformed, or the request failed (the
    /// editor then leaves that field as-is — a docless item stays docless, never
    /// faked).
    ResolvedCompletion {
        documentation: Option<String>,
        detail: Option<String>,
    },
    /// The reply to an [`LspRequest::Raw`] (Phase 5): the server's raw JSON result
    /// (`Ok`) or an error message (`Err`) — an unsupported method, a transport
    /// failure, or the server replying an error. Routed back to the Lua handler as
    /// `(err, result)`, bypassing the editor-feature staleness machinery the typed
    /// replies use (a config command's reply always fires its handler).
    Raw(Result<serde_json::Value, String>),
}

/// A [`WorkspaceEdit`] normalized to per-document text edits: the protocol's
/// `changes` map and the versioned `documentChanges` (with `OneOf`/annotation
/// edits, and edit-vs-resource operations) both collapse to one
/// `(Url, Vec<TextEdit>)` list. File resource operations (create/rename/delete)
/// are **dropped** — the editor applies only to already-open buffers (the
/// unopened-file case is scoped out). Ranges stay in the negotiated encoding for
/// the editor to convert, like the goto/completion normalizations.
pub type WorkspaceEditData = Vec<(Url, Vec<TextEdit>)>;

/// One code action distilled for the editor: its `title` for the panel list, its
/// eager `edit` (a normalized [`WorkspaceEditData`]) when the server returned one,
/// `resolve` — the original [`CodeAction`] to round-trip through
/// `codeAction/resolve` when there is neither an eager edit nor a command (a lazy
/// action) — and `command`, a `workspace/executeCommand` payload to dispatch
/// after the edit (Phase 8). A bare `Command` action lands as `command`-only; a
/// `CodeAction` may carry both an `edit` and a `command`.
#[derive(Clone, Debug)]
pub struct CodeActionData {
    pub title: String,
    pub edit: Option<WorkspaceEditData>,
    pub resolve: Option<Box<CodeAction>>,
    pub command: Option<lsp_types::Command>,
}

/// One completion candidate, distilled from a protocol [`CompletionItem`] to the
/// fields the editor's menu needs. `kind` is the `CompletionItemKind` as a small
/// int (`0` = unspecified) the client maps to an icon. `text_edit` and
/// `additional_text_edits` keep their ranges in the negotiated position encoding;
/// the editor converts them to byte ranges on accept (only it has the buffer
/// text). When `text_edit` is `None`, the editor replaces the completion word
/// (the identifier run left of the cursor) with `insert_text` (else `label`).
#[derive(Clone, Debug)]
pub struct CompletionItemData {
    pub label: String,
    pub kind: u8,
    pub detail: Option<String>,
    pub filter_text: Option<String>,
    pub sort_text: Option<String>,
    pub insert_text: Option<String>,
    pub text_edit: Option<TextEdit>,
    pub additional_text_edits: Vec<TextEdit>,
    /// The item's `documentation` (a plain string or `MarkupContent`) reduced to
    /// plain display lines joined by `\n` (markdown is not styled — same as hover),
    /// with trailing blank lines trimmed. `None` ⇒ the item carried no
    /// documentation *inline*; many servers (rust_analyzer especially) send it only
    /// on `completionItem/resolve` (Phase 2), keyed by [`Self::resolve_data`].
    pub documentation: Option<String>,
    /// The original protocol [`CompletionItem`] serialized verbatim, round-tripped
    /// to `completionItem/resolve` in Phase 2 to fetch the lazy
    /// `documentation`/`detail`. The whole item (not just its `data` blob) is kept
    /// because a server matches the resolve against the exact item it issued —
    /// rust_analyzer rejects a resolve whose `data` it didn't send. `None` only if
    /// the item somehow failed to serialize (it always should). Mirrors
    /// [`CodeActionData::resolve`] round-tripping the original [`CodeAction`].
    pub resolve_data: Option<serde_json::Value>,
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
        /// The raw `InitializeResult` as JSON, passed to the config's `on_init`
        /// hook (Phase 3) so it can read what the server advertised. `Null` if the
        /// result couldn't be serialized.
        init_result: serde_json::Value,
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
    ServerExited {
        key: ServerKey,
        message: String,
        /// The child's exit code, if it exited normally (the first arg of the
        /// config's `on_exit(code, signal, client)` hook, Phase 3). `None` for the
        /// pre-serve handshake failures, where no client was ever registered.
        code: Option<i32>,
        /// The terminating signal on unix, if the child was killed by one; `None`
        /// otherwise (and always on non-unix).
        signal: Option<i32>,
    },
    /// The reply to an [`LspRequest`], tagged with the [`ReqToken`] the editor
    /// issued so it can match the reply to its intent and drop stale ones.
    Reply {
        key: ServerKey,
        token: ReqToken,
        reply: LspReply,
    },
    /// `window/logMessage` / `window/showMessage`, or a manager-level note.
    Log { key: ServerKey, message: String },
}

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

/// Translate an [`LspNotify`] into the corresponding `async-lsp` notification.
/// Send errors are ignored: a dead socket is detected by the main loop ending.
fn apply_notify(socket: &mut ServerSocket, note: LspNotify, log: &LspLog, name: &str) {
    if log.enabled(LogLevel::Debug) {
        log.log(LogLevel::Debug, name, &describe_notify(&note));
    }
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
        // A generic `client:notify` (Phase 5): dispatched by runtime method name
        // through the notification table. An unknown method is logged and dropped
        // (a notification has no reply to carry an error back on).
        LspNotify::Raw { method, params } => {
            apply_dyn_notify(socket, &method, params, log, name);
            return;
        }
    };
}

/// Issue one language-feature [`LspRequest`] on the socket and await its reply,
/// normalizing every goto-family / references response to a flat [`LspReply`].
/// A transport error (a server that died mid-request, an unsupported method) is
/// logged and degraded to an empty location list, so the editor uniformly sees
/// "nothing found" rather than a hang.
async fn issue_request(
    sock: &mut ServerSocket,
    req: LspRequest,
    log: &LspLog,
    name: &str,
) -> LspReply {
    if log.enabled(LogLevel::Debug) {
        log.log(LogLevel::Debug, name, &describe_request(&req));
    }
    match req {
        LspRequest::Definition { uri, position } => LspReply::Locations(goto_locations(
            sock.definition(goto_params(uri, position)).await,
            log,
            name,
        )),
        LspRequest::Declaration { uri, position } => LspReply::Locations(goto_locations(
            sock.declaration(goto_params(uri, position)).await,
            log,
            name,
        )),
        LspRequest::TypeDefinition { uri, position } => LspReply::Locations(goto_locations(
            sock.type_definition(goto_params(uri, position)).await,
            log,
            name,
        )),
        LspRequest::Implementation { uri, position } => LspReply::Locations(goto_locations(
            sock.implementation(goto_params(uri, position)).await,
            log,
            name,
        )),
        LspRequest::References {
            uri,
            position,
            include_declaration,
        } => {
            let params = ReferenceParams {
                text_document_position: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: ReferenceContext {
                    include_declaration,
                },
            };
            let locations = match sock.references(params).await {
                Ok(locs) => locs.unwrap_or_default(),
                Err(e) => {
                    log.log(LogLevel::Warn, name, &format!("references failed: {e}"));
                    Vec::new()
                }
            };
            LspReply::Locations(locations)
        }
        LspRequest::Hover { uri, position } => {
            let params = HoverParams {
                text_document_position_params: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
            };
            hover_reply(sock.hover(params).await, log, name)
        }
        LspRequest::SignatureHelp { uri, position } => {
            let params = SignatureHelpParams {
                context: None,
                text_document_position_params: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
            };
            signature_help_reply(sock.signature_help(params).await, log, name)
        }
        LspRequest::Completion { uri, position } => {
            let params = CompletionParams {
                text_document_position: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            };
            completion_reply(sock.completion(params).await, log, name)
        }
        LspRequest::Formatting { uri } => {
            let params = DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: formatting_options(),
                work_done_progress_params: Default::default(),
            };
            match sock.formatting(params).await {
                Ok(edits) => LspReply::Edits(edits.unwrap_or_default()),
                Err(e) => {
                    log.log(LogLevel::Warn, name, &format!("formatting failed: {e}"));
                    LspReply::Edits(Vec::new())
                }
            }
        }
        LspRequest::Rename {
            uri,
            position,
            new_name,
        } => {
            let params = RenameParams {
                text_document_position: text_document_position(uri, position),
                new_name,
                work_done_progress_params: Default::default(),
            };
            match sock.rename(params).await {
                Ok(Some(edit)) => LspReply::WorkspaceEdit(normalize_workspace_edit(edit)),
                Ok(None) => LspReply::WorkspaceEdit(Vec::new()),
                Err(e) => {
                    log.log(LogLevel::Warn, name, &format!("rename failed: {e}"));
                    LspReply::WorkspaceEdit(Vec::new())
                }
            }
        }
        LspRequest::CodeAction {
            uri,
            range,
            diagnostics,
        } => {
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics,
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            match sock.code_action(params).await {
                Ok(resp) => LspReply::CodeActions(code_actions(resp.unwrap_or_default())),
                Err(e) => {
                    log.log(LogLevel::Warn, name, &format!("codeAction failed: {e}"));
                    LspReply::CodeActions(Vec::new())
                }
            }
        }
        LspRequest::ResolveCodeAction { action } => match sock.code_action_resolve(*action).await {
            Ok(resolved) => {
                LspReply::ResolvedCodeAction(resolved.edit.map(normalize_workspace_edit))
            }
            Err(e) => {
                log.log(
                    LogLevel::Warn,
                    name,
                    &format!("codeAction/resolve failed: {e}"),
                );
                LspReply::ResolvedCodeAction(None)
            }
        },
        LspRequest::ResolveCompletion { item } => {
            resolve_completion_reply(sock, item, log, name).await
        }
        // A generic `client:request` (Phase 5): dispatched by runtime method name
        // through the request table, raw JSON in and out. Unlike the typed
        // requests above, a failure is surfaced to the Lua handler as an `Err`
        // string (not degraded to an empty result) — the config command that
        // issued it decides what to do.
        LspRequest::Raw { method, params } => {
            LspReply::Raw(issue_dyn_request(sock, &method, params, log, name).await)
        }
    }
}

/// Issue a generic, runtime-method request and return its raw JSON result (`Ok`)
/// or an error message (`Err`) for the Lua handler.
///
/// async-lsp's [`ServerSocket::request`] is generic over a compile-time
/// [`lsp_types::request::Request`] whose `METHOD` is a `const &'static str`, so a
/// truly arbitrary runtime method can't be sent through it directly. The
/// [`dyn_requests!`] macro bridges that gap: it generates one zero-sized
/// `Request` type per supported method (all uniform `serde_json::Value` in and
/// out, since the editor only relays the JSON to/from Lua) and a runtime `match`
/// on the method string. An **unknown** method fails loud — it returns an `Err`
/// the handler receives rather than silently no-op'ing — and is a one-line table
/// addition away from being supported.
async fn issue_dyn_request(
    sock: &mut ServerSocket,
    method: &str,
    params: serde_json::Value,
    log: &LspLog,
    name: &str,
) -> Result<serde_json::Value, String> {
    let result = issue_dyn_request_inner(sock, method, params).await;
    if let Err(e) = &result {
        log.log(
            LogLevel::Warn,
            name,
            &format!("client:request {method}: {e}"),
        );
    }
    result
}

/// Generate the request dispatch table: one `Request` impl per `method => Type`
/// row (raw JSON params/result) and the runtime `match` that issues it. Standard
/// LSP methods and server-specific ones (`rust-analyzer/*`, clangd's
/// `switchSourceHeader`, …) live side by side — they only differ by the method
/// string. Add a method by adding a row.
macro_rules! dyn_requests {
    ($($method:literal => $ty:ident),* $(,)?) => {
        $(
            #[allow(non_camel_case_types)]
            enum $ty {}
            impl lsp_types::request::Request for $ty {
                type Params = serde_json::Value;
                type Result = serde_json::Value;
                const METHOD: &'static str = $method;
            }
        )*
        async fn issue_dyn_request_inner(
            sock: &mut ServerSocket,
            method: &str,
            params: serde_json::Value,
        ) -> Result<serde_json::Value, String> {
            match method {
                $( $method => sock.request::<$ty>(params).await.map_err(|e| e.to_string()), )*
                other => Err(format!(
                    "nxvim: client:request: unsupported method '{other}' \
                     (add it to dyn_requests! in nxvim-lsp/src/manager.rs)"
                )),
            }
        }
    };
}

/// Generate the notification dispatch table — the fire-and-forget twin of
/// [`dyn_requests!`].
macro_rules! dyn_notifications {
    ($($method:literal => $ty:ident),* $(,)?) => {
        $(
            #[allow(non_camel_case_types)]
            enum $ty {}
            impl lsp_types::notification::Notification for $ty {
                type Params = serde_json::Value;
                const METHOD: &'static str = $method;
            }
        )*
        /// Send a generic `client:notify` by runtime method name. An unknown
        /// method is logged and dropped (a notification carries no reply).
        fn apply_dyn_notify(
            sock: &mut ServerSocket,
            method: &str,
            params: serde_json::Value,
            log: &LspLog,
            name: &str,
        ) {
            match method {
                $( $method => { let _ = sock.notify::<$ty>(params); } )*
                other => log.log(
                    LogLevel::Warn,
                    name,
                    &format!("client:notify: unsupported method '{other}'"),
                ),
            }
        }
    };
}

// The supported generic-request methods. Every standard LSP request the editor
// doesn't already drive through a typed [`LspRequest`], plus the server-specific
// methods the headline configs reach for via `client:request`. All are relayed
// as raw JSON, so a row is just `"<method>" => <unique-type-name>`.
dyn_requests! {
    "workspace/executeCommand" => req_workspace_executeCommand,
    "workspace/symbol" => req_workspace_symbol,
    "workspaceSymbol/resolve" => req_workspaceSymbol_resolve,
    "workspace/willCreateFiles" => req_workspace_willCreateFiles,
    "workspace/willRenameFiles" => req_workspace_willRenameFiles,
    "workspace/willDeleteFiles" => req_workspace_willDeleteFiles,
    "textDocument/documentSymbol" => req_textDocument_documentSymbol,
    "textDocument/documentHighlight" => req_textDocument_documentHighlight,
    "textDocument/documentLink" => req_textDocument_documentLink,
    "documentLink/resolve" => req_documentLink_resolve,
    "textDocument/foldingRange" => req_textDocument_foldingRange,
    "textDocument/selectionRange" => req_textDocument_selectionRange,
    "textDocument/prepareCallHierarchy" => req_textDocument_prepareCallHierarchy,
    "callHierarchy/incomingCalls" => req_callHierarchy_incomingCalls,
    "callHierarchy/outgoingCalls" => req_callHierarchy_outgoingCalls,
    "textDocument/prepareTypeHierarchy" => req_textDocument_prepareTypeHierarchy,
    "typeHierarchy/supertypes" => req_typeHierarchy_supertypes,
    "typeHierarchy/subtypes" => req_typeHierarchy_subtypes,
    "textDocument/semanticTokens/full" => req_textDocument_semanticTokens_full,
    "textDocument/semanticTokens/full/delta" => req_textDocument_semanticTokens_full_delta,
    "textDocument/semanticTokens/range" => req_textDocument_semanticTokens_range,
    "textDocument/inlayHint" => req_textDocument_inlayHint,
    "inlayHint/resolve" => req_inlayHint_resolve,
    "textDocument/codeLens" => req_textDocument_codeLens,
    "codeLens/resolve" => req_codeLens_resolve,
    "textDocument/documentColor" => req_textDocument_documentColor,
    "textDocument/colorPresentation" => req_textDocument_colorPresentation,
    "textDocument/linkedEditingRange" => req_textDocument_linkedEditingRange,
    "textDocument/moniker" => req_textDocument_moniker,
    "textDocument/prepareRename" => req_textDocument_prepareRename,
    "textDocument/rangeFormatting" => req_textDocument_rangeFormatting,
    "textDocument/onTypeFormatting" => req_textDocument_onTypeFormatting,
    "completionItem/resolve" => req_completionItem_resolve,
    // The typed native features are also reachable generically, for a config that
    // routes them through `client:request` (e.g. a custom `handlers` entry).
    "textDocument/definition" => req_textDocument_definition,
    "textDocument/declaration" => req_textDocument_declaration,
    "textDocument/typeDefinition" => req_textDocument_typeDefinition,
    "textDocument/implementation" => req_textDocument_implementation,
    "textDocument/references" => req_textDocument_references,
    "textDocument/hover" => req_textDocument_hover,
    "textDocument/signatureHelp" => req_textDocument_signatureHelp,
    "textDocument/completion" => req_textDocument_completion,
    "textDocument/formatting" => req_textDocument_formatting,
    "textDocument/rename" => req_textDocument_rename,
    "textDocument/codeAction" => req_textDocument_codeAction,
    "codeAction/resolve" => req_codeAction_resolve,
    // Server-specific methods the headline configs drive via `client:request`.
    "rust-analyzer/reloadWorkspace" => req_rustAnalyzer_reloadWorkspace,
    "rust-analyzer/expandMacro" => req_rustAnalyzer_expandMacro,
    "rust-analyzer/analyzerStatus" => req_rustAnalyzer_analyzerStatus,
    "rust-analyzer/viewSyntaxTree" => req_rustAnalyzer_viewSyntaxTree,
    "rust-analyzer/openCargoToml" => req_rustAnalyzer_openCargoToml,
    "experimental/externalDocs" => req_experimental_externalDocs,
    "textDocument/switchSourceHeader" => req_textDocument_switchSourceHeader,
}

// The supported generic-notification methods.
dyn_notifications! {
    "$/setTrace" => notif_setTrace,
    "$/cancelRequest" => notif_cancelRequest,
    "window/workDoneProgress/cancel" => notif_window_workDoneProgress_cancel,
    "workspace/didChangeWatchedFiles" => notif_workspace_didChangeWatchedFiles,
    "workspace/didChangeWorkspaceFolders" => notif_workspace_didChangeWorkspaceFolders,
    "workspace/didCreateFiles" => notif_workspace_didCreateFiles,
    "workspace/didRenameFiles" => notif_workspace_didRenameFiles,
    "workspace/didDeleteFiles" => notif_workspace_didDeleteFiles,
}

/// The `FormattingOptions` for `textDocument/formatting`. nxvim has no
/// `:set shiftwidth`/`expandtab` yet, so these are fixed: `tab_size: 8` to match
/// the editor's `TABSTOP`, and spaces. Real, option-driven values are a follow-up
/// for when `:set` grows them.
fn formatting_options() -> FormattingOptions {
    FormattingOptions {
        tab_size: 8,
        insert_spaces: true,
        ..Default::default()
    }
}

/// Distill a `textDocument/codeAction` response (a mixed `(Command | CodeAction)[]`)
/// into the editor-facing list: a `CodeAction`'s `title` + normalized eager
/// `edit` + optional `command` (run via `workspace/executeCommand` after the
/// edit); a bare `Command` lands as a `command`-only entry (Phase 8).
fn code_actions(resp: CodeActionResponse) -> Vec<CodeActionData> {
    resp.into_iter()
        .map(|item| match item {
            CodeActionOrCommand::CodeAction(ca) => {
                let title = ca.title.clone();
                let command = ca.command.clone();
                let edit = ca
                    .edit
                    .as_ref()
                    .map(|e| normalize_workspace_edit(e.clone()));
                // With neither an eager edit nor a command, keep the original
                // action to resolve lazily; a command makes it directly applicable.
                let resolve = (edit.is_none() && command.is_none()).then(|| Box::new(ca));
                CodeActionData {
                    title,
                    edit,
                    resolve,
                    command,
                }
            }
            CodeActionOrCommand::Command(cmd) => CodeActionData {
                title: cmd.title.clone(),
                edit: None,
                resolve: None,
                command: Some(cmd),
            },
        })
        .collect()
}

/// Normalize a [`WorkspaceEdit`] to flat per-document [`TextEdit`]s (see
/// [`WorkspaceEditData`]). `documentChanges` (versioned) is preferred when present
/// — collapsing the `OneOf<TextEdit, AnnotatedTextEdit>` and dropping file
/// resource operations — else the plain `changes` map is used.
///
/// `pub` so `nxvim-server` can reuse it for `vim.lsp.util.apply_workspace_edit`
/// (Phase 7): a WorkspaceEdit handed up from Lua normalizes through the exact same
/// path the native rename / code-action replies use.
pub fn normalize_workspace_edit(edit: WorkspaceEdit) -> WorkspaceEditData {
    if let Some(changes) = edit.document_changes {
        return match changes {
            DocumentChanges::Edits(edits) => edits.into_iter().map(text_document_edit).collect(),
            DocumentChanges::Operations(ops) => ops
                .into_iter()
                .filter_map(|op| match op {
                    DocumentChangeOperation::Edit(e) => Some(text_document_edit(e)),
                    // create/rename/delete file ops are scoped out (open buffers only).
                    DocumentChangeOperation::Op(_) => None,
                })
                .collect(),
        };
    }
    edit.changes
        .map(|m| m.into_iter().collect())
        .unwrap_or_default()
}

/// Flatten one [`TextDocumentEdit`] to `(uri, TextEdit[])`, collapsing each
/// `OneOf<TextEdit, AnnotatedTextEdit>` to a plain edit (the change annotation is
/// dropped — nxvim does not surface them).
fn text_document_edit(edit: TextDocumentEdit) -> (Url, Vec<TextEdit>) {
    let edits = edit
        .edits
        .into_iter()
        .map(|oneof| match oneof {
            OneOf::Left(te) => te,
            OneOf::Right(AnnotatedTextEdit { text_edit, .. }) => text_edit,
        })
        .collect();
    (edit.text_document.uri, edits)
}

/// Distill a `textDocument/completion` reply into [`LspReply::Completion`],
/// normalizing the two response shapes — a bare `CompletionItem[]` (always
/// complete) and a `CompletionList` (which carries its own `isIncomplete`) — to
/// one. `None`/an error degrades to an empty, complete list, so the editor
/// uniformly sees "no candidates" rather than a hang.
fn completion_reply(
    result: Result<Option<CompletionResponse>, async_lsp::Error>,
    log: &LspLog,
    name: &str,
) -> LspReply {
    let (is_incomplete, items) = match result {
        Ok(Some(CompletionResponse::Array(items))) => (false, items),
        Ok(Some(CompletionResponse::List(list))) => (list.is_incomplete, list.items),
        Ok(None) => (false, Vec::new()),
        Err(e) => {
            log.log(LogLevel::Warn, name, &format!("completion failed: {e}"));
            (false, Vec::new())
        }
    };
    LspReply::Completion {
        is_incomplete,
        items: items.into_iter().map(completion_item).collect(),
    }
}

/// Reduce a protocol [`CompletionItem`] to the editor-facing [`CompletionItemData`]:
/// keep the label/kind/detail/sort+filter text and insert text, normalize the
/// `CompletionTextEdit` (an `Edit`, or an `InsertAndReplace` collapsed to its
/// `replace` range) plus the `additionalTextEdits` to plain [`TextEdit`]s whose
/// ranges stay in the negotiated encoding, carry any inline `documentation`
/// (markup → plain lines), and preserve the original item for a later
/// `completionItem/resolve` ([`CompletionItemData::resolve_data`], Phase 2).
fn completion_item(item: CompletionItem) -> CompletionItemData {
    // Serialize the whole item up front (before its fields are moved out) for the
    // resolve round-trip; a server matches the resolve against the exact item it
    // issued, so the original is preserved verbatim, not rebuilt from our distill.
    let resolve_data = serde_json::to_value(&item).ok();
    let documentation = item.documentation.and_then(documentation_lines);
    let text_edit = item.text_edit.map(|edit| match edit {
        CompletionTextEdit::Edit(e) => e,
        CompletionTextEdit::InsertAndReplace(ir) => TextEdit {
            range: ir.replace,
            new_text: ir.new_text,
        },
    });
    CompletionItemData {
        label: item.label,
        kind: kind_code(item.kind),
        detail: item.detail,
        filter_text: item.filter_text,
        sort_text: item.sort_text,
        insert_text: item.insert_text,
        text_edit,
        additional_text_edits: item.additional_text_edits.unwrap_or_default(),
        documentation,
        resolve_data,
    }
}

/// Issue a `completionItem/resolve` for the selected menu item and distill the
/// reply to its `documentation`/`detail` ([`LspReply::ResolvedCompletion`]). The
/// `item` is the original completion item as JSON ([`CompletionItemData::resolve_data`]);
/// it is deserialized back to a [`CompletionItem`] to send verbatim. A malformed
/// item, an unsupported method, or a server error degrades to both-`None` (logged),
/// so the editor leaves a docless item docless rather than hang — never a fake doc.
async fn resolve_completion_reply(
    sock: &mut ServerSocket,
    item: serde_json::Value,
    log: &LspLog,
    name: &str,
) -> LspReply {
    let none = LspReply::ResolvedCompletion {
        documentation: None,
        detail: None,
    };
    let item: CompletionItem = match serde_json::from_value(item) {
        Ok(item) => item,
        Err(e) => {
            log.log(
                LogLevel::Warn,
                name,
                &format!("completionItem/resolve: malformed item: {e}"),
            );
            return none;
        }
    };
    match sock.completion_item_resolve(item).await {
        Ok(resolved) => LspReply::ResolvedCompletion {
            documentation: resolved.documentation.and_then(documentation_lines),
            detail: resolved.detail,
        },
        Err(e) => {
            log.log(
                LogLevel::Warn,
                name,
                &format!("completionItem/resolve failed: {e}"),
            );
            none
        }
    }
}

/// Reduce a completion item's `documentation` (a plain string, or a
/// `MarkupContent` whose markdown is rendered as plain lines — same as hover) to
/// its display text, trailing blank lines trimmed. `None` when the result is
/// empty, so a blank documentation block reads as "no docs" rather than an empty
/// preview (it is never *faked* into one — an absent field is simply `None`).
fn documentation_lines(doc: Documentation) -> Option<String> {
    let text = match doc {
        Documentation::String(s) => s,
        Documentation::MarkupContent(mc) => mc.value,
    };
    let lines = markup_lines(text);
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The numeric `CompletionItemKind` (`1`=Text … `25`=TypeParameter), via serde so
/// it tracks the protocol enum without a hand-maintained arm per kind. `0` for an
/// unspecified kind, which the client renders without an icon.
fn kind_code(kind: Option<CompletionItemKind>) -> u8 {
    kind.and_then(|k| serde_json::to_value(k).ok())
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u8
}

/// Distill a `textDocument/hover` reply into plain display lines: extract the
/// markup's text (a `MarkedString`, an array of them joined by blank lines, or a
/// `MarkupContent` value), split into lines, and drop trailing blank lines so the
/// panel isn't padded. `None`/an error degrades to an empty list ("no
/// information"), so the editor never hangs waiting on a feature a server lacks.
fn hover_reply(
    result: Result<Option<Hover>, async_lsp::Error>,
    log: &LspLog,
    name: &str,
) -> LspReply {
    let hover = match result {
        Ok(Some(hover)) => hover,
        Ok(None) => return LspReply::Hover(Vec::new()),
        Err(e) => {
            log.log(LogLevel::Warn, name, &format!("hover failed: {e}"));
            return LspReply::Hover(Vec::new());
        }
    };
    let text = match hover.contents {
        HoverContents::Scalar(ms) => marked_string_text(ms),
        HoverContents::Array(parts) => parts
            .into_iter()
            .map(marked_string_text)
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    };
    LspReply::Hover(markup_lines(text))
}

/// Split a markup/prose block (hover contents, completion `documentation`) into
/// display lines, dropping trailing blank lines so a panel isn't padded. The
/// shared distiller for every markup-to-lines reduction — nxvim renders markdown
/// as plain lines today, so this is a plain `lines()` split (styling is a
/// follow-up, tracked with hover).
fn markup_lines(text: String) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines
}

/// The text of a `MarkedString` (a plain markdown string, or the code of a
/// language-tagged block — the language fence is dropped since hover is rendered
/// as plain lines).
fn marked_string_text(ms: MarkedString) -> String {
    match ms {
        MarkedString::String(s) => s,
        MarkedString::LanguageString(ls) => ls.value,
    }
}

/// Distill a `textDocument/signatureHelp` reply into the active signature's label
/// and active parameter text. The active signature is `activeSignature` (default
/// the first); the active parameter is the signature's own `activeParameter` when
/// present, else the top-level one. `None`/an error/no signatures degrades to a
/// "no signature help" (both fields `None`).
fn signature_help_reply(
    result: Result<Option<SignatureHelp>, async_lsp::Error>,
    log: &LspLog,
    name: &str,
) -> LspReply {
    let none = LspReply::SignatureHelp {
        signature: None,
        active_parameter: None,
    };
    let help = match result {
        Ok(Some(help)) => help,
        Ok(None) => return none,
        Err(e) => {
            log.log(LogLevel::Warn, name, &format!("signatureHelp failed: {e}"));
            return none;
        }
    };
    let active = help.active_signature.unwrap_or(0) as usize;
    let Some(sig) = help
        .signatures
        .get(active)
        .or_else(|| help.signatures.first())
    else {
        return none;
    };
    // A per-signature `activeParameter` (3.16+) overrides the top-level one.
    let param_idx = sig
        .active_parameter
        .or(help.active_parameter)
        .map(|i| i as usize);
    let active_parameter = param_idx
        .and_then(|i| sig.parameters.as_ref()?.get(i))
        .map(|p| parameter_text(&p.label, &sig.label));
    LspReply::SignatureHelp {
        signature: Some(sig.label.clone()),
        active_parameter,
    }
}

/// The display text of a parameter: its label string, or the substring of the
/// signature label at the given offsets. Offsets are UTF-16 code units into the
/// signature label (per LSP); they are sliced on char boundaries here, exact for
/// the common ASCII case and best-effort otherwise (this is display-only).
fn parameter_text(label: &ParameterLabel, signature: &str) -> String {
    match label {
        ParameterLabel::Simple(s) => s.clone(),
        ParameterLabel::LabelOffsets([start, end]) => {
            let (start, end) = (*start as usize, *end as usize);
            let mut unit = 0usize;
            let mut out = String::new();
            for c in signature.chars() {
                if unit >= end {
                    break;
                }
                if unit >= start {
                    out.push(c);
                }
                unit += c.len_utf16();
            }
            out
        }
    }
}

/// Flatten a goto-family reply (definition/declaration/typeDefinition/
/// implementation all share `GotoDefinitionResponse`) into a list of target
/// locations, collapsing the `LocationLink` shape to its selection target. A
/// transport error degrades to an empty list (logged).
fn goto_locations(
    result: Result<Option<GotoDefinitionResponse>, async_lsp::Error>,
    log: &LspLog,
    name: &str,
) -> Vec<Location> {
    match result {
        Ok(None) => Vec::new(),
        Ok(Some(GotoDefinitionResponse::Scalar(loc))) => vec![loc],
        Ok(Some(GotoDefinitionResponse::Array(locs))) => locs,
        Ok(Some(GotoDefinitionResponse::Link(links))) => links
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_selection_range,
            })
            .collect(),
        Err(e) => {
            log.log(LogLevel::Warn, name, &format!("goto request failed: {e}"));
            Vec::new()
        }
    }
}

/// The shared `GotoDefinitionParams` for the goto-family requests (a position in
/// a document, with default progress params).
fn goto_params(uri: Url, position: Position) -> GotoDefinitionParams {
    GotoDefinitionParams {
        text_document_position_params: text_document_position(uri, position),
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    }
}

/// A `(document, position)` pair shared by every position-based request.
fn text_document_position(uri: Url, position: Position) -> TextDocumentPositionParams {
    TextDocumentPositionParams {
        text_document: TextDocumentIdentifier { uri },
        position,
    }
}

/// A one-line summary of an outgoing request for the DEBUG log.
fn describe_request(req: &LspRequest) -> String {
    let (label, pos) = match req {
        LspRequest::Definition { position, .. } => ("definition", position),
        LspRequest::Declaration { position, .. } => ("declaration", position),
        LspRequest::TypeDefinition { position, .. } => ("typeDefinition", position),
        LspRequest::Implementation { position, .. } => ("implementation", position),
        LspRequest::References { position, .. } => ("references", position),
        LspRequest::Hover { position, .. } => ("hover", position),
        LspRequest::SignatureHelp { position, .. } => ("signatureHelp", position),
        LspRequest::Completion { position, .. } => ("completion", position),
        LspRequest::Rename {
            position, new_name, ..
        } => {
            return format!(
                "→ rename '{new_name}' @ {}:{}",
                position.line, position.character
            )
        }
        LspRequest::Formatting { .. } => return "→ formatting".to_string(),
        LspRequest::CodeAction { range, .. } => {
            return format!(
                "→ codeAction @ {}:{}",
                range.start.line, range.start.character
            )
        }
        LspRequest::ResolveCodeAction { action } => {
            return format!("→ codeAction/resolve '{}'", action.title)
        }
        LspRequest::ResolveCompletion { item } => {
            return format!(
                "→ completionItem/resolve '{}'",
                item.get("label").and_then(|l| l.as_str()).unwrap_or("?")
            )
        }
        LspRequest::Raw { method, .. } => return format!("→ {method} (client:request)"),
    };
    format!("→ {label} @ {}:{}", pos.line, pos.character)
}

/// A one-line summary of an outgoing notification for the DEBUG log.
fn describe_notify(note: &LspNotify) -> String {
    match note {
        LspNotify::DidOpen { version, text, .. } => {
            format!("→ didOpen v{version} ({} bytes)", text.len())
        }
        LspNotify::DidChange {
            version, changes, ..
        } => format!("→ didChange v{version} ({} change(s))", changes.len()),
        LspNotify::DidSave { .. } => "→ didSave".to_string(),
        LspNotify::DidClose { .. } => "→ didClose".to_string(),
        LspNotify::Raw { method, .. } => format!("→ {method} (client:notify)"),
    }
}

/// State shared by the client `MainLoop`'s notification handlers: which server
/// this loop belongs to, the channel to forward distilled events on, and the log.
struct ClientState {
    key: ServerKey,
    event_tx: UnboundedSender<LspEvent>,
    log: Arc<LspLog>,
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
    log: Arc<LspLog>,
) -> (MainLoop<Router<ClientState>>, ServerSocket) {
    MainLoop::new_client(|_server| {
        let mut router = Router::new(ClientState { key, event_tx, log });
        router.notification::<PublishDiagnostics>(|st, params| {
            st.log.log(
                LogLevel::Debug,
                &st.key.name,
                &format!(
                    "← publishDiagnostics ({} item(s))",
                    params.diagnostics.len()
                ),
            );
            let _ = st.event_tx.send(LspEvent::Diagnostics {
                key: st.key.clone(),
                uri: params.uri,
                version: params.version,
                diagnostics: params.diagnostics,
            });
            ControlFlow::Continue(())
        });
        // `window/logMessage` is for the log only (not user-facing); route it to
        // the file at the message's mapped severity.
        router.notification::<LogMessage>(|st, params| {
            st.log
                .log(level_of(params.typ), &st.key.name, &params.message);
            ControlFlow::Continue(())
        });
        // `window/showMessage` IS user-facing: log it *and* forward it to the
        // editor's `:messages`.
        router.notification::<ShowMessage>(|st, params| {
            st.log
                .log(level_of(params.typ), &st.key.name, &params.message);
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

/// Map an LSP `window/*Message` severity to a log level (`LOG`, the most verbose,
/// becomes `Debug`).
fn level_of(typ: MessageType) -> LogLevel {
    match typ {
        MessageType::ERROR => LogLevel::Error,
        MessageType::WARNING => LogLevel::Warn,
        MessageType::INFO => LogLevel::Info,
        _ => LogLevel::Debug,
    }
}

/// The client capabilities we advertise at `initialize`: UTF-8 preferred over
/// UTF-16 for positions (Decision 4), document-save notifications, and the edit
/// features Phase 6 needs. Most consequential is
/// `codeAction.codeActionLiteralSupport` — **without it a server returns legacy
/// `Command[]` rather than a `CodeAction` carrying an `edit`**, and "apply the
/// edit" becomes impossible; we also declare `formatting`/`rename` and
/// `workspaceEdit.documentChanges` so servers offer those features, and
/// `completion.completionItem` (`documentationFormat` + `resolveSupport`) so
/// servers send per-item docs / let us resolve them lazily.
/// Split a child's [`std::process::ExitStatus`] into `(code, signal)` for the
/// config's `on_exit(code, signal, client)` hook (Phase 3). `code` is the normal
/// exit code; `signal` is the terminating signal (unix only — always `None`
/// elsewhere). `None`/`None` when the status couldn't be collected.
fn exit_code_signal(status: Option<std::process::ExitStatus>) -> (Option<i32>, Option<i32>) {
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

/// nxvim's base [`client_capabilities`] with the config's `extra` capabilities
/// (a raw JSON value) deep-merged over them — the config wins on any conflict, so
/// it can both extend (add a capability nxvim doesn't advertise) and override
/// (flip a flag). A malformed `extra` that won't round-trip back into
/// [`ClientCapabilities`] is logged and the base is used — loud, not silent, so a
/// bad `capabilities` table is visible rather than mysteriously ignored.
fn merged_client_capabilities(
    extra: Option<&serde_json::Value>,
    log: &LspLog,
    name: &str,
) -> ClientCapabilities {
    let base = client_capabilities();
    let Some(extra) = extra else {
        return base;
    };
    let mut merged = match serde_json::to_value(&base) {
        Ok(v) => v,
        Err(e) => {
            log.log(
                LogLevel::Warn,
                name,
                &format!(
                    "could not serialize base capabilities: {e}; ignoring config capabilities"
                ),
            );
            return base;
        }
    };
    json_merge(&mut merged, extra);
    match serde_json::from_value(merged) {
        Ok(caps) => caps,
        Err(e) => {
            log.log(
                LogLevel::Warn,
                name,
                &format!(
                    "config `capabilities` are not valid client capabilities: {e}; using base"
                ),
            );
            client_capabilities()
        }
    }
}

/// Recursively merge `src` into `dst`: objects merge key-by-key (recursing on
/// shared keys), and any non-object pair replaces `dst` with `src`. The deep-merge
/// `merged_client_capabilities` uses so a nested config capability (e.g. one field
/// under `textDocument.completion`) doesn't clobber its siblings.
fn json_merge(dst: &mut serde_json::Value, src: &serde_json::Value) {
    match (dst, src) {
        (serde_json::Value::Object(d), serde_json::Value::Object(s)) => {
            for (k, v) in s {
                json_merge(d.entry(k.clone()).or_insert(serde_json::Value::Null), v);
            }
        }
        (d, s) => *d = s.clone(),
    }
}

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
            formatting: Some(DocumentFormattingClientCapabilities {
                dynamic_registration: Some(false),
            }),
            rename: Some(RenameClientCapabilities {
                dynamic_registration: Some(false),
                ..Default::default()
            }),
            code_action: Some(CodeActionClientCapabilities {
                code_action_literal_support: Some(CodeActionLiteralSupport {
                    code_action_kind: CodeActionKindLiteralSupport {
                        // The standard kinds; servers fall back gracefully for any
                        // value outside this set, per the protocol.
                        value_set: [
                            "",
                            "quickfix",
                            "refactor",
                            "refactor.extract",
                            "refactor.inline",
                            "refactor.rewrite",
                            "source",
                            "source.organizeImports",
                        ]
                        .iter()
                        .map(|s| s.to_string())
                        .collect(),
                    },
                }),
                // We resolve a lazy action's `edit` on demand (`codeAction/resolve`)
                // and round-trip its `data`, so declare both — else a server that
                // only offers `edit` lazily would withhold it.
                resolve_support: Some(CodeActionCapabilityResolveSupport {
                    properties: vec!["edit".to_string()],
                }),
                data_support: Some(true),
                ..Default::default()
            }),
            // Declare completion-item documentation + resolve support. Most servers
            // — notably rust_analyzer — send completion lists *without* per-item
            // `documentation`/`detail` and expect the client to fetch them lazily
            // per selected item via `completionItem/resolve`; advertising
            // `resolveSupport` for those properties is what unlocks that round-trip
            // (Phase 2), and `documentationFormat` declares we accept markdown (and
            // plaintext) for the docs that do arrive (the markup distiller renders
            // either as plain lines).
            completion: Some(CompletionClientCapabilities {
                completion_item: Some(CompletionItemCapability {
                    documentation_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
                    resolve_support: Some(CompletionItemCapabilityResolveSupport {
                        properties: vec!["documentation".to_string(), "detail".to_string()],
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            // We *do* consume `textDocument/publishDiagnostics` (see the client
            // router), but some servers — notably typescript-language-server —
            // withhold push diagnostics entirely unless the client advertises
            // support here. Declaring it is what turns those servers' diagnostics
            // on; `relatedInformation` lets them attach cross-reference notes.
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                related_information: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
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

/// Reduce the protocol [`ServerCapabilities`] to the per-feature provider bools
/// the editor surfaces as `client.server_capabilities`. Serializing once and
/// probing the camelCase `*Provider` fields keeps all eleven uniform across the
/// protocol's mix of `bool`/`OneOf`/options shapes: a provider counts as
/// advertised when its field is present and not an explicit `false` (an options
/// object — the common case — counts as supported).
fn provider_caps(caps: &ServerCapabilities) -> ProviderCaps {
    let json = serde_json::to_value(caps).unwrap_or(serde_json::Value::Null);
    let present = |key: &str| match json.get(key) {
        Some(serde_json::Value::Bool(b)) => *b,
        None | Some(serde_json::Value::Null) => false,
        Some(_) => true,
    };
    ProviderCaps {
        definition: present("definitionProvider"),
        declaration: present("declarationProvider"),
        type_definition: present("typeDefinitionProvider"),
        implementation: present("implementationProvider"),
        references: present("referencesProvider"),
        hover: present("hoverProvider"),
        signature_help: present("signatureHelpProvider"),
        completion: present("completionProvider"),
        document_formatting: present("documentFormattingProvider"),
        rename: present("renameProvider"),
        code_action: present("codeActionProvider"),
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
