//! A **synchronous**, byte-driven LSP client — the browser edit-host's analogue of
//! the async [`LspManager`](crate::manager) (Phase 6e).
//!
//! The wasm edit-host has no tokio and no `async-lsp` `MainLoop` (Open Decision
//! #6a: no executor in the Worker). It still wants full LSP parity, so this is the
//! manager's machinery — spawn, JSON-RPC framing, id correlation, the initialize
//! handshake, request serialization, reply distillation, the server→client
//! pull/refresh handling — re-expressed as a **tick-driven state machine** that
//! produces and consumes raw bytes instead of awaiting futures:
//!
//! - outbound: the editor calls [`ensure_server`](SyncLspClient::ensure_server) /
//!   [`notify`](SyncLspClient::notify) / [`request`](SyncLspClient::request); each
//!   appends to a [`WireOp`] queue the wasm host drains
//!   ([`take_wire_ops`](SyncLspClient::take_wire_ops)) and forwards to the daemon
//!   over the raw `lsp_spawn` / `lsp_stdin` / `lsp_kill` wire (Phase 3c's LSP leg).
//! - inbound: the daemon's `lsp_stdout` / `lsp_stderr` / `lsp_exited` pushes feed
//!   [`feed_stdout`](SyncLspClient::feed_stdout) / [`exited`](SyncLspClient::exited),
//!   which parse `Content-Length`-framed JSON-RPC and emit the same
//!   [`LspEvent`]s the native manager does, drained by
//!   [`take_events`](SyncLspClient::take_events) and fed to the editor's
//!   `on_lsp_event` — the **identical** synchronous consumer the native build uses.
//!
//! The protocol-facing transforms (capability negotiation in [`crate::client`],
//! reply distillation in [`crate::convert`]) are shared verbatim with the native
//! path — only the *driving* (async loop vs. sync tick) differs. Server children
//! still run on the daemon (`serve_one_lsp`), exactly as in the native split, so a
//! server runs where the project files are while editing stays in the Worker.
//!
//! Simplifications relative to the native manager (recorded, not stubbed): no
//! per-server respawn/backoff circuit breaker — an exited server surfaces
//! [`LspEvent::ServerExited`] and is forgotten, so the editor re-`ensure`s it on the
//! next `FileType` (the breaker is a native-supervisor concern; the browser leans
//! on the editor's re-attach). `process_id` is `None` (the browser has no pid).

use std::collections::HashMap;

use lsp_types::{
    CodeAction, CodeActionResponse, CompletionItem, CompletionResponse, ConfigurationParams,
    DocumentSymbolResponse, GotoDefinitionResponse, Hover, InitializeParams, InitializeResult,
    InlayHint, Location, PublishDiagnosticsParams, SemanticTokensFullDeltaResult,
    SemanticTokensResult, ShowMessageParams, SignatureHelp, TextEdit, Url, WorkspaceEdit,
    WorkspaceSymbolResponse,
};
use serde_json::{json, Value};

use crate::client::{
    configuration_reply, encoding_of, merged_client_capabilities, provider_caps, semantic_legend,
    semantic_tokens_delta, sync_kind_of,
};
use crate::convert::{
    code_actions, completion_reply, document_symbols, documentation_lines, goto_locations,
    hover_reply, inlay_hint, inlay_label_core, normalize_workspace_edit, pad_label,
    signature_help_reply, workspace_symbols,
};
use crate::log::LspLog;
use crate::protocol::{
    LspEvent, LspNotify, LspReply, LspRequest, RefreshKind, ReqToken, SemanticTokensData,
    ServerCaps, ServerKey, ServerSpawn,
};

/// One raw operation the wasm host forwards to the daemon's LSP leg. `Spawn`/`Kill`
/// map to `lsp_spawn`/`lsp_kill` notifications; `Stdin` carries one framed JSON-RPC
/// chunk as an `lsp_stdin` notification. The `id` is this client's per-server wire
/// correlation id (the daemon routes its `lsp_stdout`/`lsp_exited` back by it),
/// minted exactly as [`RemoteLspTransport`](../../nxvim_server) mints its spawn id.
#[derive(Debug, Clone)]
pub enum WireOp {
    Spawn {
        id: u64,
        program: String,
        args: Vec<String>,
        cwd: String,
    },
    Stdin {
        id: u64,
        bytes: Vec<u8>,
    },
    Kill {
        id: u64,
    },
}

/// Where a server is in its lifecycle. Commands issued before the handshake
/// completes are queued and flushed on `Ready` (the sync analogue of the native
/// per-server channel buffering until the serve loop starts).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initializing,
    Ready,
}

/// An outbound command buffered until the server is `Ready`.
enum Outbound {
    Notify(LspNotify),
    Request(ReqToken, LspRequest),
}

/// A request awaiting its JSON-RPC response, keyed by the outgoing message id.
enum Pending {
    /// The `initialize` request — its reply drives the handshake.
    Handshake,
    /// A language-feature request — its reply is distilled per `kind` and
    /// forwarded as [`LspEvent::Reply`] tagged with the editor's `token`.
    Feature(ReqToken, ReqKind),
}

/// Which [`LspRequest`] a [`Pending::Feature`] was, so its response is decoded into
/// the right typed result and distilled by the matching [`crate::convert`] helper.
#[derive(Clone)]
enum ReqKind {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
    /// Carries the requesting document's `uri` — the nested `DocumentSymbol` reply
    /// form has no location of its own, so the distiller needs it (mirrors the
    /// native `document_symbols(&uri, …)`).
    DocumentSymbol(Url),
    WorkspaceSymbol,
    References,
    Hover,
    SignatureHelp,
    Completion,
    Formatting,
    Rename,
    CodeAction,
    ResolveCodeAction,
    ResolveCompletion,
    SemanticTokensFull,
    SemanticTokensDelta,
    InlayHint,
    ResolveInlayHint,
    Raw,
}

/// Per-server state: its wire id, handshake phase, the inbound byte buffer the
/// frame parser drains, the outgoing JSON-RPC id counter and pending-reply map, the
/// config `settings` (to answer `workspace/configuration` pulls), and the
/// pre-handshake outbound queue.
struct ServerState {
    id: u64,
    phase: Phase,
    inbuf: Vec<u8>,
    next_req_id: i64,
    pending: HashMap<i64, Pending>,
    settings: Option<Value>,
    queued: Vec<Outbound>,
}

/// The synchronous LSP client the wasm edit-host holds in place of the native
/// [`LspManager`]. Drives N servers over the daemon's raw `lsp_*` wire.
pub struct SyncLspClient {
    servers: HashMap<ServerKey, ServerState>,
    /// Wire id → key, so an inbound `lsp_stdout`/`lsp_exited` routes to its server.
    by_id: HashMap<u64, ServerKey>,
    next_wire_id: u64,
    wire: Vec<WireOp>,
    events: Vec<LspEvent>,
    /// Silent on wasm — the server's stderr is dropped here (the native path logs
    /// it to a file; the browser has none), and capability negotiation never logs.
    log: LspLog,
}

impl Default for SyncLspClient {
    fn default() -> Self {
        Self::new()
    }
}

impl SyncLspClient {
    pub fn new() -> SyncLspClient {
        SyncLspClient {
            servers: HashMap::new(),
            by_id: HashMap::new(),
            next_wire_id: 1,
            wire: Vec::new(),
            events: Vec::new(),
            log: LspLog::disabled(),
        }
    }

    /// Ensure a server for `key` is started (idempotent: a live server for the same
    /// key is left alone). Mints a wire id, enqueues the `Spawn`, and sends
    /// `initialize` straight away — the daemon processes `lsp_spawn` before the
    /// `lsp_stdin` that follows on the same ordered stream.
    #[allow(deprecated)] // root_uri is the broadest way to convey the workspace root
    pub fn ensure_server(&mut self, key: ServerKey, spawn: ServerSpawn) {
        if self.servers.contains_key(&key) {
            return;
        }
        let wire_id = self.next_wire_id;
        self.next_wire_id += 1;
        let name = key.name.clone();
        self.by_id.insert(wire_id, key.clone());
        self.servers.insert(
            key.clone(),
            ServerState {
                id: wire_id,
                phase: Phase::Initializing,
                inbuf: Vec::new(),
                next_req_id: 1,
                pending: HashMap::new(),
                settings: spawn.settings.clone(),
                queued: Vec::new(),
            },
        );
        self.wire.push(WireOp::Spawn {
            id: wire_id,
            program: spawn.program.clone(),
            args: spawn.args.clone(),
            cwd: key.root.to_string_lossy().into_owned(),
        });
        let init = InitializeParams {
            process_id: None,
            root_uri: Url::from_file_path(&key.root).ok(),
            initialization_options: spawn
                .init_options
                .clone()
                .or_else(|| spawn.settings.clone()),
            capabilities: merged_client_capabilities(spawn.capabilities.as_ref(), &self.log, &name),
            ..Default::default()
        };
        let params = serde_json::to_value(&init).unwrap_or(Value::Null);
        self.send_request(&key, "initialize", params, Pending::Handshake);
    }

    /// Fire-and-forget a document-sync notification (or generic `client:notify`).
    /// Dropped if no server for `key`; buffered until `Ready` otherwise.
    pub fn notify(&mut self, key: ServerKey, note: LspNotify) {
        match self.phase(&key) {
            Some(Phase::Ready) => {
                let (method, params) = notify_wire(note);
                self.send_notification(&key, &method, params);
            }
            Some(Phase::Initializing) => self.queue(&key, Outbound::Notify(note)),
            None => {}
        }
    }

    /// Fire a language-feature request; its reply returns later as an
    /// [`LspEvent::Reply`] carrying `token`. Dropped if no server for `key`;
    /// buffered until `Ready` otherwise.
    pub fn request(&mut self, key: ServerKey, token: ReqToken, req: LspRequest) {
        match self.phase(&key) {
            Some(Phase::Ready) => {
                let (method, params, kind) = request_wire(req);
                self.send_request(&key, &method, params, Pending::Feature(token, kind));
            }
            Some(Phase::Initializing) => self.queue(&key, Outbound::Request(token, req)),
            None => {}
        }
    }

    /// Cleanly stop `key`'s server: `shutdown`/`exit` then kill the child, and
    /// forget it (a later `ensure_server` re-spawns). The reply to `shutdown` is
    /// ignored — we are tearing down regardless.
    pub fn shutdown(&mut self, key: ServerKey) {
        if !self.servers.contains_key(&key) {
            return;
        }
        // Best-effort graceful stop before the kill (mirrors the native serve loop).
        self.send_notification(&key, "exit", json!(null));
        if let Some(state) = self.servers.remove(&key) {
            self.by_id.remove(&state.id);
            self.wire.push(WireOp::Kill { id: state.id });
        }
    }

    /// Feed one `lsp_stdout` push: append the bytes to the server's buffer and
    /// process every complete JSON-RPC frame it now contains.
    pub fn feed_stdout(&mut self, id: u64, bytes: &[u8]) {
        let Some(key) = self.by_id.get(&id).cloned() else {
            return;
        };
        let frames = match self.servers.get_mut(&key) {
            Some(state) => {
                state.inbuf.extend_from_slice(bytes);
                parse_frames(&mut state.inbuf)
            }
            None => return,
        };
        for msg in frames {
            self.handle_message(&key, msg);
        }
    }

    /// Feed one `lsp_stderr` push. The native path drains a server's stderr to the
    /// LSP log file; the browser has no log file, so this is a deliberate no-op
    /// (the bytes are diagnostic, not protocol). Kept so the host has a sink to
    /// call rather than silently dropping the wire method.
    pub fn feed_stderr(&mut self, _id: u64, _bytes: &[u8]) {}

    /// The server (identified by wire `id`) exited or its pipe closed. Surface
    /// [`LspEvent::ServerExited`] and forget it; the editor re-`ensure`s on the next
    /// `FileType` (no auto-respawn — see the module note).
    pub fn exited(&mut self, id: u64, code: Option<i32>, signal: Option<i32>) {
        let Some(key) = self.by_id.remove(&id) else {
            return;
        };
        self.servers.remove(&key);
        self.events.push(LspEvent::ServerExited {
            key,
            message: "language server exited".to_string(),
            code,
            signal,
        });
    }

    /// Drain the outbound wire ops the host forwards to the daemon.
    pub fn take_wire_ops(&mut self) -> Vec<WireOp> {
        std::mem::take(&mut self.wire)
    }

    /// Drain the distilled events the editor feeds to `on_lsp_event`.
    pub fn take_events(&mut self) -> Vec<LspEvent> {
        std::mem::take(&mut self.events)
    }

    // --- internals -------------------------------------------------------------

    fn phase(&self, key: &ServerKey) -> Option<Phase> {
        self.servers.get(key).map(|s| s.phase)
    }

    fn queue(&mut self, key: &ServerKey, ob: Outbound) {
        if let Some(state) = self.servers.get_mut(key) {
            state.queued.push(ob);
        }
    }

    /// Send a JSON-RPC request, recording its pending reply. Two short borrows
    /// (bump the id under the server, then push to the shared wire queue) avoid
    /// aliasing `self.servers` and `self.wire`.
    fn send_request(&mut self, key: &ServerKey, method: &str, params: Value, pending: Pending) {
        let info = self.servers.get_mut(key).map(|state| {
            let rid = state.next_req_id;
            state.next_req_id += 1;
            state.pending.insert(rid, pending);
            (state.id, rid)
        });
        if let Some((wire_id, rid)) = info {
            let body = json!({"jsonrpc": "2.0", "id": rid, "method": method, "params": params});
            self.wire.push(WireOp::Stdin {
                id: wire_id,
                bytes: frame(&body),
            });
        }
    }

    fn send_notification(&mut self, key: &ServerKey, method: &str, params: Value) {
        if let Some(wire_id) = self.servers.get(key).map(|s| s.id) {
            let body = json!({"jsonrpc": "2.0", "method": method, "params": params});
            self.wire.push(WireOp::Stdin {
                id: wire_id,
                bytes: frame(&body),
            });
        }
    }

    fn send_response(&mut self, key: &ServerKey, resp_id: Value, result: Value) {
        if let Some(wire_id) = self.servers.get(key).map(|s| s.id) {
            let body = json!({"jsonrpc": "2.0", "id": resp_id, "result": result});
            self.wire.push(WireOp::Stdin {
                id: wire_id,
                bytes: frame(&body),
            });
        }
    }

    fn handle_message(&mut self, key: &ServerKey, msg: Value) {
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = msg.get("id").cloned();
        match (method, id) {
            // server → client request (method + id): answer it.
            (Some(method), Some(req_id)) => self.on_server_request(key, req_id, &method, &msg),
            // server → client notification (method, no id).
            (Some(method), None) => self.on_server_notification(key, &method, &msg),
            // response to one of our requests (id, no method).
            (None, Some(req_id)) => {
                if let Some(rid) = req_id.as_i64() {
                    let result = match msg.get("error") {
                        Some(err) => Err(error_message(err)),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    self.on_response(key, rid, result);
                }
            }
            (None, None) => {}
        }
    }

    fn on_response(&mut self, key: &ServerKey, rid: i64, result: Result<Value, String>) {
        let pending = self
            .servers
            .get_mut(key)
            .and_then(|s| s.pending.remove(&rid));
        match pending {
            Some(Pending::Handshake) => self.on_init_result(key, result),
            Some(Pending::Feature(token, ReqKind::Raw)) => {
                self.events.push(LspEvent::Reply {
                    key: key.clone(),
                    token,
                    reply: LspReply::Raw(result),
                });
            }
            Some(Pending::Feature(token, kind)) => {
                let reply = distill(kind, result);
                self.events.push(LspEvent::Reply {
                    key: key.clone(),
                    token,
                    reply,
                });
            }
            None => {}
        }
    }

    fn on_init_result(&mut self, key: &ServerKey, result: Result<Value, String>) {
        let init: InitializeResult =
            match result.and_then(|v| serde_json::from_value(v).map_err(|e| e.to_string())) {
                Ok(init) => init,
                Err(e) => {
                    if let Some(state) = self.servers.remove(key) {
                        self.by_id.remove(&state.id);
                    }
                    self.events.push(LspEvent::ServerExited {
                        key: key.clone(),
                        message: format!("initialize failed: {e}"),
                        code: None,
                        signal: None,
                    });
                    return;
                }
            };
        let caps = ServerCaps {
            sync_kind: sync_kind_of(&init.capabilities),
            providers: provider_caps(&init.capabilities),
            legend: semantic_legend(&init.capabilities),
            semantic_tokens_delta: semantic_tokens_delta(&init.capabilities),
        };
        let encoding = encoding_of(&init.capabilities);
        let init_result = serde_json::to_value(&init).unwrap_or(Value::Null);
        self.events.push(LspEvent::Initialized {
            key: key.clone(),
            caps,
            encoding,
            init_result,
        });
        // initialized + (config) didChangeConfiguration, then flush the queue.
        self.send_notification(key, "initialized", json!({}));
        let settings = self.servers.get(key).and_then(|s| s.settings.clone());
        if let Some(settings) = settings {
            self.send_notification(
                key,
                "workspace/didChangeConfiguration",
                json!({ "settings": settings }),
            );
        }
        if let Some(state) = self.servers.get_mut(key) {
            state.phase = Phase::Ready;
        }
        self.flush_queued(key);
    }

    fn flush_queued(&mut self, key: &ServerKey) {
        let queued = self
            .servers
            .get_mut(key)
            .map(|s| std::mem::take(&mut s.queued))
            .unwrap_or_default();
        for ob in queued {
            match ob {
                Outbound::Notify(note) => {
                    let (method, params) = notify_wire(note);
                    self.send_notification(key, &method, params);
                }
                Outbound::Request(token, req) => {
                    let (method, params, kind) = request_wire(req);
                    self.send_request(key, &method, params, Pending::Feature(token, kind));
                }
            }
        }
    }

    fn on_server_request(&mut self, key: &ServerKey, req_id: Value, method: &str, msg: &Value) {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            // The pull model: answer each requested `section` from the config's
            // `settings` (the exact `configuration_reply` the native router uses).
            "workspace/configuration" => {
                let settings = self.servers.get(key).and_then(|s| s.settings.clone());
                let reply = match serde_json::from_value::<ConfigurationParams>(params) {
                    Ok(cfg) => configuration_reply(settings.as_ref(), &cfg),
                    Err(_) => Vec::new(),
                };
                self.send_response(key, req_id, Value::Array(reply));
            }
            "workspace/inlayHint/refresh" => {
                self.events.push(LspEvent::WorkspaceRefresh {
                    key: key.clone(),
                    kind: RefreshKind::InlayHint,
                });
                self.send_response(key, req_id, Value::Null);
            }
            "workspace/semanticTokens/refresh" => {
                self.events.push(LspEvent::WorkspaceRefresh {
                    key: key.clone(),
                    kind: RefreshKind::SemanticTokens,
                });
                self.send_response(key, req_id, Value::Null);
            }
            // Everything else a server may request (registerCapability,
            // workDoneProgress/create, applyEdit, …) is acked with a null result so
            // the server proceeds — matching async-lsp's lenient default.
            _ => self.send_response(key, req_id, Value::Null),
        }
    }

    fn on_server_notification(&mut self, key: &ServerKey, method: &str, msg: &Value) {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "textDocument/publishDiagnostics" => {
                if let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(params) {
                    self.events.push(LspEvent::Diagnostics {
                        key: key.clone(),
                        uri: p.uri,
                        version: p.version,
                        diagnostics: p.diagnostics,
                    });
                }
            }
            // User-facing: forward to the editor's messages (the native path logs
            // it *and* forwards; the browser has no log, so it only forwards).
            "window/showMessage" => {
                if let Ok(p) = serde_json::from_value::<ShowMessageParams>(params) {
                    self.events.push(LspEvent::Log {
                        key: key.clone(),
                        message: p.message,
                    });
                }
            }
            // `window/logMessage`, progress, telemetry, custom notifications: ignore
            // (the log-only sinks the native client has are absent here).
            _ => {}
        }
    }
}

/// Decode a JSON-RPC `result` (or an error → `None`) into the typed `Option<T>` the
/// [`crate::convert`] distillers expect. A malformed or absent result degrades to
/// `None` — the uniform "nothing found" case, exactly as the native dispatch
/// degrades a transport error.
fn decode<T: serde::de::DeserializeOwned>(result: Result<Value, String>) -> Option<T> {
    result
        .ok()
        .and_then(|v| serde_json::from_value::<Option<T>>(v).ok())
        .flatten()
}

/// Distill a typed feature reply, reusing the same [`crate::convert`] transforms the
/// native dispatch does — so a hover/diagnostic/completion looks identical in the
/// browser and on the desktop. (Raw replies are handled by the caller, which has
/// the error string.)
fn distill(kind: ReqKind, result: Result<Value, String>) -> LspReply {
    match kind {
        ReqKind::Definition
        | ReqKind::Declaration
        | ReqKind::TypeDefinition
        | ReqKind::Implementation => {
            LspReply::Locations(goto_locations(decode::<GotoDefinitionResponse>(result)))
        }
        ReqKind::DocumentSymbol(uri) => LspReply::Symbols(document_symbols(
            &uri,
            decode::<DocumentSymbolResponse>(result),
        )),
        ReqKind::WorkspaceSymbol => {
            LspReply::Symbols(workspace_symbols(decode::<WorkspaceSymbolResponse>(result)))
        }
        ReqKind::References => {
            LspReply::Locations(decode::<Vec<Location>>(result).unwrap_or_default())
        }
        ReqKind::Hover => hover_reply(decode::<Hover>(result)),
        ReqKind::SignatureHelp => signature_help_reply(decode::<SignatureHelp>(result)),
        ReqKind::Completion => completion_reply(decode::<CompletionResponse>(result)),
        ReqKind::Formatting => LspReply::Edits(decode::<Vec<TextEdit>>(result).unwrap_or_default()),
        ReqKind::Rename => LspReply::WorkspaceEdit(
            decode::<WorkspaceEdit>(result)
                .map(normalize_workspace_edit)
                .unwrap_or_default(),
        ),
        ReqKind::CodeAction => LspReply::CodeActions(code_actions(
            decode::<CodeActionResponse>(result).unwrap_or_default(),
        )),
        ReqKind::ResolveCodeAction => LspReply::ResolvedCodeAction(
            decode::<CodeAction>(result)
                .and_then(|a| a.edit)
                .map(normalize_workspace_edit),
        ),
        ReqKind::ResolveCompletion => match decode::<CompletionItem>(result) {
            Some(item) => LspReply::ResolvedCompletion {
                documentation: item.documentation.and_then(documentation_lines),
                detail: item.detail,
            },
            None => LspReply::ResolvedCompletion {
                documentation: None,
                detail: None,
            },
        },
        ReqKind::SemanticTokensFull => {
            LspReply::SemanticTokens(match decode::<SemanticTokensResult>(result) {
                Some(SemanticTokensResult::Tokens(t)) => SemanticTokensData::Full {
                    result_id: t.result_id,
                    tokens: t.data,
                },
                Some(SemanticTokensResult::Partial(p)) => SemanticTokensData::Full {
                    result_id: None,
                    tokens: p.data,
                },
                None => empty_semantic_tokens(),
            })
        }
        ReqKind::SemanticTokensDelta => {
            LspReply::SemanticTokens(match decode::<SemanticTokensFullDeltaResult>(result) {
                Some(SemanticTokensFullDeltaResult::TokensDelta(d)) => SemanticTokensData::Delta {
                    result_id: d.result_id,
                    edits: d.edits,
                },
                Some(SemanticTokensFullDeltaResult::PartialTokensDelta { edits }) => {
                    SemanticTokensData::Delta {
                        result_id: None,
                        edits,
                    }
                }
                Some(SemanticTokensFullDeltaResult::Tokens(t)) => SemanticTokensData::Full {
                    result_id: t.result_id,
                    tokens: t.data,
                },
                None => empty_semantic_tokens(),
            })
        }
        ReqKind::InlayHint => LspReply::InlayHints(
            decode::<Vec<InlayHint>>(result)
                .unwrap_or_default()
                .iter()
                .map(inlay_hint)
                .collect(),
        ),
        ReqKind::ResolveInlayHint => match decode::<InlayHint>(result) {
            Some(hint) => {
                let core = inlay_label_core(&hint);
                let label = (!core.is_empty()).then(|| pad_label(&core, &hint));
                LspReply::ResolvedInlayHint { label }
            }
            None => LspReply::ResolvedInlayHint { label: None },
        },
        // Handled by the caller (needs the transport error string).
        ReqKind::Raw => LspReply::Raw(Ok(Value::Null)),
    }
}

/// The "server classified nothing" reply (a full set, no tokens, no `result_id`),
/// matching the native dispatch's `empty_semantic_tokens`.
fn empty_semantic_tokens() -> SemanticTokensData {
    SemanticTokensData::Full {
        result_id: None,
        tokens: Vec::new(),
    }
}

/// Translate an [`LspNotify`] into its JSON-RPC `(method, params)`. The position
/// coordinates are already in the negotiated encoding (the editor converts before
/// issuing), so this is a pure serialization — the sync twin of
/// [`crate::dispatch::apply_notify`].
fn notify_wire(note: LspNotify) -> (String, Value) {
    match note {
        LspNotify::DidOpen {
            uri,
            language_id,
            version,
            text,
        } => (
            "textDocument/didOpen".into(),
            json!({"textDocument": {
                "uri": uri, "languageId": language_id, "version": version, "text": text,
            }}),
        ),
        LspNotify::DidChange {
            uri,
            version,
            changes,
        } => (
            "textDocument/didChange".into(),
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": changes,
            }),
        ),
        LspNotify::DidSave { uri, text } => (
            "textDocument/didSave".into(),
            json!({"textDocument": {"uri": uri}, "text": text}),
        ),
        LspNotify::DidClose { uri } => (
            "textDocument/didClose".into(),
            json!({"textDocument": {"uri": uri}}),
        ),
        // A generic `client:notify` — the method/params are already raw JSON.
        LspNotify::Raw { method, params } => (method, params),
    }
}

/// Translate an [`LspRequest`] into its JSON-RPC `(method, params, kind)` — the sync
/// twin of [`crate::dispatch::issue_request`]'s param construction. `kind` records
/// which distiller to apply to the response.
fn request_wire(req: LspRequest) -> (String, Value, ReqKind) {
    match req {
        LspRequest::Definition { uri, position } => (
            "textDocument/definition".into(),
            pos_params(&uri, &position),
            ReqKind::Definition,
        ),
        LspRequest::Declaration { uri, position } => (
            "textDocument/declaration".into(),
            pos_params(&uri, &position),
            ReqKind::Declaration,
        ),
        LspRequest::TypeDefinition { uri, position } => (
            "textDocument/typeDefinition".into(),
            pos_params(&uri, &position),
            ReqKind::TypeDefinition,
        ),
        LspRequest::Implementation { uri, position } => (
            "textDocument/implementation".into(),
            pos_params(&uri, &position),
            ReqKind::Implementation,
        ),
        LspRequest::DocumentSymbol { uri } => (
            "textDocument/documentSymbol".into(),
            json!({"textDocument": {"uri": uri.clone()}}),
            ReqKind::DocumentSymbol(uri),
        ),
        LspRequest::WorkspaceSymbol { query } => (
            "workspace/symbol".into(),
            json!({"query": query}),
            ReqKind::WorkspaceSymbol,
        ),
        LspRequest::References {
            uri,
            position,
            include_declaration,
        } => (
            "textDocument/references".into(),
            json!({
                "textDocument": {"uri": uri},
                "position": position,
                "context": {"includeDeclaration": include_declaration},
            }),
            ReqKind::References,
        ),
        LspRequest::Hover { uri, position } => (
            "textDocument/hover".into(),
            pos_params(&uri, &position),
            ReqKind::Hover,
        ),
        LspRequest::SignatureHelp { uri, position } => (
            "textDocument/signatureHelp".into(),
            pos_params(&uri, &position),
            ReqKind::SignatureHelp,
        ),
        LspRequest::Completion { uri, position } => (
            "textDocument/completion".into(),
            pos_params(&uri, &position),
            ReqKind::Completion,
        ),
        LspRequest::Formatting {
            uri,
            tab_size,
            insert_spaces,
        } => (
            "textDocument/formatting".into(),
            json!({
                "textDocument": {"uri": uri},
                "options": {"tabSize": tab_size, "insertSpaces": insert_spaces},
            }),
            ReqKind::Formatting,
        ),
        LspRequest::Rename {
            uri,
            position,
            new_name,
        } => (
            "textDocument/rename".into(),
            json!({
                "textDocument": {"uri": uri},
                "position": position,
                "newName": new_name,
            }),
            ReqKind::Rename,
        ),
        LspRequest::CodeAction {
            uri,
            range,
            diagnostics,
        } => (
            "textDocument/codeAction".into(),
            json!({
                "textDocument": {"uri": uri},
                "range": range,
                "context": {"diagnostics": diagnostics},
            }),
            ReqKind::CodeAction,
        ),
        LspRequest::ResolveCodeAction { action } => (
            "codeAction/resolve".into(),
            serde_json::to_value(*action).unwrap_or(Value::Null),
            ReqKind::ResolveCodeAction,
        ),
        LspRequest::ResolveCompletion { item } => (
            "completionItem/resolve".into(),
            item,
            ReqKind::ResolveCompletion,
        ),
        LspRequest::SemanticTokensFull { uri } => (
            "textDocument/semanticTokens/full".into(),
            json!({"textDocument": {"uri": uri}}),
            ReqKind::SemanticTokensFull,
        ),
        LspRequest::SemanticTokensDelta {
            uri,
            previous_result_id,
        } => (
            "textDocument/semanticTokens/full/delta".into(),
            json!({
                "textDocument": {"uri": uri},
                "previousResultId": previous_result_id,
            }),
            ReqKind::SemanticTokensDelta,
        ),
        LspRequest::InlayHint { uri, range } => (
            "textDocument/inlayHint".into(),
            json!({"textDocument": {"uri": uri}, "range": range}),
            ReqKind::InlayHint,
        ),
        LspRequest::ResolveInlayHint { hint } => {
            ("inlayHint/resolve".into(), hint, ReqKind::ResolveInlayHint)
        }
        // A generic `client:request` — the method/params are already raw JSON.
        LspRequest::Raw { method, params } => (method, params, ReqKind::Raw),
    }
}

/// `{textDocument: {uri}, position}` — the shared body of every position-anchored
/// request (the JSON `TextDocumentPositionParams` serializes to, with the optional
/// progress tokens omitted, exactly as the typed params do).
fn pos_params(uri: &Url, position: &lsp_types::Position) -> Value {
    json!({"textDocument": {"uri": uri}, "position": position})
}

/// Frame one JSON-RPC message with its `Content-Length` header for the wire.
fn frame(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).unwrap_or_default();
    let mut out = format!("Content-Length: {}\r\n\r\n", json.len()).into_bytes();
    out.extend_from_slice(&json);
    out
}

/// Drain every complete `Content-Length`-framed JSON-RPC message from `inbuf`,
/// leaving any trailing partial frame buffered for the next chunk. A malformed
/// header is skipped (past its terminator) rather than stalling the stream.
fn parse_frames(inbuf: &mut Vec<u8>) -> Vec<Value> {
    let mut out = Vec::new();
    loop {
        let Some(hdr_end) = find_subsequence(inbuf, b"\r\n\r\n") else {
            break;
        };
        let body_start = hdr_end + 4;
        let Some(len) = parse_content_length(&inbuf[..hdr_end]) else {
            inbuf.drain(..body_start); // skip the unparseable header, keep going
            continue;
        };
        if inbuf.len() < body_start + len {
            break; // body not fully arrived yet
        }
        let body: Vec<u8> = inbuf[body_start..body_start + len].to_vec();
        inbuf.drain(..body_start + len);
        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            out.push(v);
        }
    }
    out
}

/// The `Content-Length` value from a JSON-RPC frame header block (case-insensitive
/// field name, per the LSP base protocol).
fn parse_content_length(header: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    for line in text.split("\r\n") {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("content-length") {
            return value.trim().parse().ok();
        }
    }
    None
}

/// The index of the first occurrence of `needle` in `haystack`.
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The `message` field of a JSON-RPC error object, for the `Err` of a generic
/// `client:request` reply.
fn error_message(err: &Value) -> String {
    err.get("message")
        .and_then(Value::as_str)
        .unwrap_or("lsp error")
        .to_string()
}
