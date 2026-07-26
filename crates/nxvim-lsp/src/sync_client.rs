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
    CompletionItem, CompletionResponse, ConfigurationParams, DocumentSymbolResponse, FoldingRange,
    GotoDefinitionResponse, Hover, InitializeResult, InlayHint, Location, PublishDiagnosticsParams,
    SemanticTokensFullDeltaResult, SemanticTokensResult, ShowMessageParams, SignatureHelp,
    TextEdit, Url, WorkspaceEdit, WorkspaceSymbolResponse,
};
use serde_json::{json, Value};

use crate::client::{configuration_reply, init_params, read_init_result};
use crate::convert::{
    code_actions_value, completion_reply, document_symbols, folding_ranges, goto_locations,
    hover_reply, inlay_hint, normalize_workspace_edit_value, resolved_completion,
    resolved_inlay_hint, semantic_tokens_delta_data, semantic_tokens_full, signature_help_reply,
    try_normalize_workspace_edit_value, workspace_symbols,
};
use crate::log::LspLog;
use crate::protocol::{
    ApplyEditOutcome, LspEvent, LspNotify, LspReply, LspRequest, RefreshKind, ReqToken, ServerKey,
    ServerSpawn,
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
    FoldingRange,
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
    /// In-flight `workspace/applyEdit` requests, keyed by the id handed to the editor
    /// on [`LspEvent::ApplyEdit`]: the server that asked, and its JSON-RPC request id
    /// to answer on. The editor answers a tick or more later
    /// ([`Self::apply_edit_response`]) — the one inbound request that can't be
    /// answered from here, because only the editor knows whether the edit landed.
    pending_apply: HashMap<u64, (ServerKey, Value)>,
    next_apply_id: u64,
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
            pending_apply: HashMap::new(),
            next_apply_id: 1,
            log: LspLog::disabled(),
        }
    }

    /// Ensure a server for `key` is started (idempotent: a live server for the same
    /// key is left alone). Mints a wire id, enqueues the `Spawn`, and sends
    /// `initialize` straight away — the daemon processes `lsp_spawn` before the
    /// `lsp_stdin` that follows on the same ordered stream.
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
        let init = init_params(&key.root, &spawn, None, &self.log, &name);
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
        // Best-effort graceful stop before the kill, mirroring the native serve
        // loop's `shutdown` request → `exit` notification. Fire-and-forget: the
        // request id is minted like any other, but the whole server state is
        // removed below, so a late reply is dropped with the connection.
        if let Some(state) = self.servers.get_mut(&key) {
            let rid = state.next_req_id;
            state.next_req_id += 1;
            let wire_id = state.id;
            let body = json!({"jsonrpc": "2.0", "id": rid, "method": "shutdown", "params": null});
            self.wire.push(WireOp::Stdin {
                id: wire_id,
                bytes: frame(&body),
            });
        }
        self.send_notification(&key, "exit", json!(null));
        if let Some(state) = self.servers.remove(&key) {
            self.by_id.remove(&state.id);
            self.wire.push(WireOp::Kill { id: state.id });
            self.fail_pending(&key, state, "language server shut down");
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
        if let Some(state) = self.servers.remove(&key) {
            self.fail_pending(&key, state, "language server exited");
        }
        self.events.push(LspEvent::ServerExited {
            key,
            message: "language server exited".to_string(),
            code,
            signal,
        });
    }

    /// Answer a server→client `workspace/applyEdit` the editor received as
    /// [`LspEvent::ApplyEdit`] — the wasm twin of `LspManager::apply_edit_response`.
    /// The server has been blocked on this since it asked; framing the response is
    /// what unblocks it. Unknown `id` (a server that exited meanwhile, or a double
    /// answer) is a no-op — its request died with the connection.
    pub fn apply_edit_response(&mut self, id: u64, outcome: ApplyEditOutcome) {
        let Some((key, req_id)) = self.pending_apply.remove(&id) else {
            return;
        };
        let mut result = json!({ "applied": outcome.applied });
        if let Some(map) = result.as_object_mut() {
            if let Some(reason) = outcome.failure_reason {
                map.insert("failureReason".to_string(), Value::String(reason));
            }
            if let Some(index) = outcome.failed_change {
                map.insert("failedChange".to_string(), Value::from(index));
            }
        }
        self.send_response(&key, req_id, result);
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
        // Borrow `method`/`id` out of the owned `msg` rather than cloning them up
        // front — only the server-request branch needs an owned `id`.
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            // server → client request (method + id): answer it.
            (Some(method), Some(req_id)) => {
                self.on_server_request(key, req_id.clone(), method, &msg)
            }
            // server → client notification (method, no id).
            (Some(method), None) => self.on_server_notification(key, method, &msg),
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
                        // The child is alive (it *answered* `initialize`, however
                        // malformed) — kill it, or the daemon holds a zombie
                        // server no one will ever talk to again.
                        self.wire.push(WireOp::Kill { id: state.id });
                        self.fail_pending(key, state, "initialize failed");
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
        let (caps, encoding, init_result) = read_init_result(&init);
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

    /// Resolve every in-flight (and still-queued) request of a dying/removed server
    /// with the degraded reply its native twin produces when a socket dies
    /// mid-request (the detached `issue_request` gets an `Err`, distilled to the
    /// uniform empty case) — so no [`ReqToken`] is ever dropped on the floor: a
    /// typed reply clears its feature's pending state, and a generic
    /// `client:request` fires its Lua handler with the error rather than leaking
    /// the deferred callback forever.
    fn fail_pending(&mut self, key: &ServerKey, state: ServerState, reason: &str) {
        // Its inbound `workspace/applyEdit`s die with it too: the response would go
        // to a closed pipe, and leaving the entry would strand it forever (the ids
        // are never reused, so a later answer for one is simply dropped).
        self.pending_apply.retain(|_, (k, _)| k != key);
        let failed = |kind: ReqKind| match kind {
            // `distill` leaves `Raw` to the caller (it needs the error string).
            ReqKind::Raw => LspReply::Raw(Err(reason.to_string())),
            kind => distill(kind, Err(reason.to_string())),
        };
        for pending in state.pending.into_values() {
            if let Pending::Feature(token, kind) = pending {
                self.events.push(LspEvent::Reply {
                    key: key.clone(),
                    token,
                    reply: failed(kind),
                });
            }
        }
        for ob in state.queued {
            if let Outbound::Request(token, req) = ob {
                let (_, _, kind) = request_wire(req);
                self.events.push(LspEvent::Reply {
                    key: key.clone(),
                    token,
                    reply: failed(kind),
                });
            }
        }
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
            // `workspace/applyEdit`: the server asks the *editor* to apply an edit it
            // authored (how a refactor delivered as a `command` lands). The answer
            // can't be minted here — only the editor knows whether the edit reached
            // its buffers — so stash the request and hand the normalized edit up;
            // `apply_edit_response` frames the reply when the editor answers. A
            // malformed payload is refused loud rather than acked as applied.
            "workspace/applyEdit" => {
                // Normalized from the **raw** value: the typed form has already lost
                // its text edits' `annotationId`s, which decide what nxvim asks about.
                // An edit that doesn't parse is refused loud rather than degraded to an
                // empty one — which would be answered `applied: true`, a success for
                // something that never reached a buffer. The native router refuses the
                // same way, so both legs give a server the same answer.
                let label = params
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let edit = params.get("edit").cloned().unwrap_or_default();
                match try_normalize_workspace_edit_value(&edit) {
                    Ok(changes) => {
                        let id = self.next_apply_id;
                        self.next_apply_id += 1;
                        self.pending_apply.insert(id, (key.clone(), req_id));
                        self.events.push(LspEvent::ApplyEdit {
                            key: key.clone(),
                            id,
                            label,
                            changes,
                        });
                    }
                    Err(reason) => self.send_response(
                        key,
                        req_id,
                        json!({ "applied": false, "failureReason": reason }),
                    ),
                }
            }
            // Everything else a server may request (registerCapability,
            // workDoneProgress/create, …) is acked with a null result so the server
            // proceeds — matching async-lsp's lenient default.
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
        // The three edit-carrying replies normalize from the **raw** value the wire
        // already handed us: the typed form has lost its text edits' `annotationId`s,
        // which decide whether nxvim asks before applying (see
        // `normalize_workspace_edit_value`). Nothing to re-request — on this leg the
        // JSON never left.
        ReqKind::Rename => LspReply::WorkspaceEdit(
            result
                .ok()
                .filter(|r| !r.is_null())
                .as_ref()
                .map(normalize_workspace_edit_value)
                .unwrap_or_default(),
        ),
        ReqKind::CodeAction => {
            LspReply::CodeActions(code_actions_value(result.unwrap_or_default()))
        }
        ReqKind::ResolveCodeAction => LspReply::ResolvedCodeAction(
            result
                .ok()
                .as_ref()
                .and_then(|r| r.get("edit"))
                .filter(|e| !e.is_null())
                .map(normalize_workspace_edit_value),
        ),
        ReqKind::ResolveCompletion => resolved_completion(decode::<CompletionItem>(result)),
        ReqKind::SemanticTokensFull => {
            LspReply::SemanticTokens(semantic_tokens_full(decode::<SemanticTokensResult>(result)))
        }
        ReqKind::SemanticTokensDelta => {
            LspReply::SemanticTokens(semantic_tokens_delta_data(decode::<
                SemanticTokensFullDeltaResult,
            >(result)))
        }
        ReqKind::InlayHint => LspReply::InlayHints(
            decode::<Vec<InlayHint>>(result)
                .unwrap_or_default()
                .iter()
                .map(inlay_hint)
                .collect(),
        ),
        ReqKind::ResolveInlayHint => resolved_inlay_hint(decode::<InlayHint>(result).as_ref()),
        ReqKind::FoldingRange => LspReply::Folds(folding_ranges(
            decode::<Vec<FoldingRange>>(result).unwrap_or_default(),
        )),
        // Handled by the caller (needs the transport error string).
        ReqKind::Raw => LspReply::Raw(Ok(Value::Null)),
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
            only,
        } => {
            // `only` is omitted when empty — an empty list would ask for *no* kinds.
            let mut context = json!({ "diagnostics": diagnostics });
            if !only.is_empty() {
                context["only"] = json!(only);
            }
            (
                "textDocument/codeAction".into(),
                json!({
                    "textDocument": {"uri": uri},
                    "range": range,
                    "context": context,
                }),
                ReqKind::CodeAction,
            )
        }
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
        LspRequest::FoldingRange { uri } => (
            "textDocument/foldingRange".into(),
            json!({"textDocument": {"uri": uri}}),
            ReqKind::FoldingRange,
        ),
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
    use std::io::Write;
    let json = serde_json::to_vec(body).unwrap_or_default();
    // Pre-size for the body plus a generous header allowance (the fixed
    // `Content-Length: \r\n\r\n` text plus the length's decimal digits), so the
    // header writes and the body append never reallocate.
    let mut out = Vec::with_capacity(json.len() + 32);
    let _ = write!(out, "Content-Length: {}\r\n\r\n", json.len());
    out.extend_from_slice(&json);
    out
}

/// The largest `Content-Length` we will buffer for a single JSON-RPC frame
/// (256 MiB). A trustworthy language server never frames anything near this; an
/// announced length past it is a corrupt/hostile header, so the frame is dropped
/// (loudly, past its body if it arrives) rather than letting `inbuf` grow without
/// bound waiting for bytes that may never come. Generous enough for any real
/// payload (a whole-file `didOpen`, a large `semanticTokens` set).
const MAX_FRAME_LEN: usize = 256 * 1024 * 1024;

/// The largest run of bytes we will buffer *without* a complete `\r\n\r\n` header
/// terminator in sight (64 KiB). A real LSP frame header is tiny (a `Content-Length`
/// line, maybe a `Content-Type` — under ~100 bytes), so a stream that piles up past
/// this with no terminator is corrupt or hostile (or the trailing bytes of a frame
/// we refused for being over-long): the buffer is dropped rather than allowed to
/// grow without bound waiting for a terminator that may never come. Without this cap
/// the [`MAX_FRAME_LEN`] body limit alone leaves a memory-exhaustion hole — a server
/// that never emits `\r\n\r\n` (or dribbles a never-terminating header line) makes
/// `inbuf` grow unboundedly, since the body limit only applies *after* a header is
/// parsed.
const MAX_HEADER_LEN: usize = 64 * 1024;

/// Drain every complete `Content-Length`-framed JSON-RPC message from `inbuf`,
/// leaving any trailing partial frame buffered for the next chunk. A malformed
/// header is skipped (past its terminator) rather than stalling the stream, an
/// absurd `Content-Length` (> [`MAX_FRAME_LEN`]) drops the frame rather than
/// buffering unboundedly, and an un-terminated header run past [`MAX_HEADER_LEN`]
/// drops the buffer rather than growing without bound.
fn parse_frames(inbuf: &mut Vec<u8>) -> Vec<Value> {
    let mut out = Vec::new();
    // Walk a cursor over the buffer and drain everything consumed *once* at the
    // end — a per-frame `drain` would shift the whole remaining buffer for every
    // frame (quadratic over a chunk that carries many messages).
    let mut pos = 0usize;
    loop {
        let tail = &inbuf[pos..];
        let Some(hdr_end) = find_subsequence(tail, b"\r\n\r\n") else {
            // No complete header block buffered. Real headers are tiny, so an
            // un-terminated run past the cap is corrupt/hostile input (or the
            // headerless remnant of an over-long frame skipped below): drop it so
            // `inbuf` can't grow without bound waiting on a terminator. A genuine
            // partial header well under the cap stays buffered for the next chunk.
            if tail.len() > MAX_HEADER_LEN {
                pos = inbuf.len();
            }
            break;
        };
        let body_start = pos + hdr_end + 4;
        let Some(len) = parse_content_length(&tail[..hdr_end]) else {
            pos = body_start; // skip the unparseable header, keep going
            continue;
        };
        // An over-long frame is a protocol error: never grow `inbuf` to hold it.
        // Skip past the header now; the body (when/if it arrives) is headerless, so
        // it fails the header search above and is dropped by the [`MAX_HEADER_LEN`]
        // guard rather than wedging the stream on a length we refuse to honor.
        if len > MAX_FRAME_LEN {
            pos = body_start;
            continue;
        }
        let body_end = body_start + len;
        if inbuf.len() < body_end {
            break; // body not fully arrived yet (bounded by MAX_FRAME_LEN)
        }
        // Parse straight from the buffer slice (no intermediate copy).
        if let Ok(v) = serde_json::from_slice::<Value>(&inbuf[body_start..body_end]) {
            out.push(v);
        }
        pos = body_end;
    }
    inbuf.drain(..pos);
    out
}

/// The `Content-Length` value from a JSON-RPC frame header block (case-insensitive
/// field name, per the LSP base protocol). Other header lines (`Content-Type`, or
/// a line with no colon) are skipped — only a missing/unparseable `Content-Length`
/// yields `None`.
fn parse_content_length(header: &[u8]) -> Option<usize> {
    let text = std::str::from_utf8(header).ok()?;
    for line in text.split("\r\n") {
        let Some((name, value)) = line.split_once(':') else {
            continue; // not a `name: value` line — skip, don't abandon the scan
        };
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
