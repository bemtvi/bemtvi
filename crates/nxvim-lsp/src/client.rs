//! The `async-lsp` client side: building the per-server `MainLoop`, the client
//! capabilities advertised at `initialize`, and reading back what the server
//! chose (position encoding, sync kind, feature providers).
//!
//! The client only *receives* notifications (diagnostics, log/show messages); its
//! handlers forward distilled [`LspEvent`]s on the manager's channel. The
//! capability negotiation here is what makes servers run *configured* and offer
//! the features Phase 6+ relies on — most consequentially
//! `codeAction.codeActionLiteralSupport`, without which a server returns legacy
//! `Command[]` and "apply the edit" becomes impossible.

#[cfg(feature = "native")]
use std::collections::HashMap;
#[cfg(feature = "native")]
use std::future::ready;
#[cfg(feature = "native")]
use std::ops::ControlFlow;
#[cfg(feature = "native")]
use std::sync::Arc;

#[cfg(feature = "native")]
use async_lsp::router::Router;
#[cfg(feature = "native")]
use async_lsp::{MainLoop, ServerSocket};
#[cfg(feature = "native")]
use lsp_types::notification::{LogMessage, Progress, PublishDiagnostics, ShowMessage};
#[cfg(feature = "native")]
use lsp_types::request::{
    InlayHintRefreshRequest, RegisterCapability, SemanticTokensRefresh, UnregisterCapability,
    WorkDoneProgressCreate, WorkspaceConfiguration, WorkspaceFoldersRequest,
};
#[cfg(feature = "native")]
use lsp_types::{ApplyWorkspaceEditResponse, ProgressParamsValue};
use lsp_types::{
    ChangeAnnotationWorkspaceEditClientCapabilities, ClientCapabilities,
    CodeActionCapabilityResolveSupport, CodeActionClientCapabilities, CodeActionKindLiteralSupport,
    CodeActionLiteralSupport, CompletionClientCapabilities, CompletionItemCapability,
    CompletionItemCapabilityResolveSupport, ConfigurationParams,
    DidChangeConfigurationClientCapabilities, DidChangeWatchedFilesClientCapabilities,
    DocumentFormattingClientCapabilities, FailureHandlingKind, FoldingRangeClientCapabilities,
    GeneralClientCapabilities, HoverClientCapabilities, InitializeParams, InitializeResult,
    InlayHintClientCapabilities, InlayHintResolveClientCapabilities,
    InlayHintWorkspaceClientCapabilities, MarkupKind, MessageType, PositionEncodingKind,
    PublishDiagnosticsClientCapabilities, RenameClientCapabilities, ResourceOperationKind,
    SemanticTokenModifier, SemanticTokenType, SemanticTokensClientCapabilities,
    SemanticTokensClientCapabilitiesRequests, SemanticTokensFullOptions,
    SemanticTokensWorkspaceClientCapabilities, ServerCapabilities, TextDocumentClientCapabilities,
    TextDocumentSyncCapability, TextDocumentSyncClientCapabilities, TextDocumentSyncKind,
    TokenFormat, Url, WindowClientCapabilities, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities, WorkspaceFolder,
};
#[cfg(feature = "native")]
use tokio::sync::mpsc::UnboundedSender;
#[cfg(feature = "native")]
use tokio::sync::oneshot;

#[cfg(feature = "native")]
use crate::convert::try_normalize_workspace_edit_value;

/// `workspace/applyEdit` with a **raw JSON** params type — see the handler below.
#[cfg(feature = "native")]
enum RawApplyWorkspaceEdit {}
#[cfg(feature = "native")]
impl lsp_types::request::Request for RawApplyWorkspaceEdit {
    type Params = serde_json::Value;
    type Result = ApplyWorkspaceEditResponse;
    const METHOD: &'static str = "workspace/applyEdit";
}
use crate::log::{LogLevel, LspLog};
// Pure helpers (always compiled) return these; the async router/handshake items
// (gated below) use `LspEvent`/`RefreshKind`/`ServerKey`.
#[cfg(feature = "native")]
use crate::protocol::{
    progress_token, progress_update, ApplyEditOutcome, CapabilityRegistration, LspEvent,
    RefreshKind, ServerKey,
};
use crate::protocol::{PositionEncoding, ProviderCaps, SemanticLegend, ServerCaps, ServerSpawn};

/// State shared by the client `MainLoop`'s notification handlers: which server
/// this loop belongs to, the channel to forward distilled events on, and the log.
#[cfg(feature = "native")]
pub(crate) struct ClientState {
    key: ServerKey,
    event_tx: UnboundedSender<LspEvent>,
    log: Arc<LspLog>,
    /// The config's `settings` JSON, kept to answer the server's pull-model
    /// `workspace/configuration` requests (lua_ls/gopls read their config this way,
    /// returning the requested `section` slice). `None` when the config set none.
    settings: Option<serde_json::Value>,
    /// In-flight `workspace/applyEdit` requests: the server is blocked on each until
    /// the *editor* says whether it applied the edit, which happens a tick or more
    /// later. The request handler parks on the receiver; [`ApplyEditDone`] (emitted
    /// onto the loop by the manager when the editor answers) resolves the sender.
    pending_apply: HashMap<u64, oneshot::Sender<ApplyEditOutcome>>,
    /// Source of the [`LspEvent::ApplyEdit`] ids, which are per-client and opaque to
    /// the editor (they stand in for the JSON-RPC request id).
    next_apply_id: u64,
}

/// The editor's answer to a `workspace/applyEdit`, routed back into the client's
/// `MainLoop` as an `async-lsp` custom event (`ServerSocket::emit`) so it reaches the
/// [`ClientState`] the parked request handler left its sender in. The loop drives
/// request futures in a `FuturesUnordered` *alongside* its event and incoming arms,
/// so a handler parked on the editor never stalls the server's other traffic.
#[cfg(feature = "native")]
pub(crate) struct ApplyEditDone {
    pub id: u64,
    pub outcome: ApplyEditOutcome,
}

/// Build the `async-lsp` client `MainLoop` and its `ServerSocket`. The bare
/// [`Router`] is the service: the client's handlers are trivial and panic-free, so
/// the concurrency/catch-unwind middleware a server needs is unnecessary here.
///
/// Server→client *requests* are answered here too, and the modelled ones are the
/// difference between a working feature and a broken one: `workspace/configuration`
/// (a pull-only server runs on defaults without it), the inlay-hint / semantic-token
/// refreshes, and `workspace/applyEdit` (every refactor delivered as a `command`).
/// Anything still unmodelled falls to async-lsp's method-not-found, which servers
/// tolerate for the optional rest (`client/registerCapability`, work-done progress).
#[cfg(feature = "native")]
pub(crate) fn new_client(
    key: ServerKey,
    event_tx: UnboundedSender<LspEvent>,
    log: Arc<LspLog>,
    settings: Option<serde_json::Value>,
) -> (MainLoop<Router<ClientState>>, ServerSocket) {
    MainLoop::new_client(|_server| {
        let mut router = Router::new(ClientState {
            key,
            event_tx,
            log,
            settings,
            pending_apply: HashMap::new(),
            next_apply_id: 1,
        });
        // `workspace/configuration` (pull model): the server asks for its config by
        // `section`; answer each item with that dotted path into the config's
        // `settings` (or null when unset). This is how lua_ls/gopls actually read
        // their options — a `settings`-configured server that *only* pulls (lua_ls
        // ignores the `didChangeConfiguration` push for its hint options) otherwise
        // runs on defaults, so e.g. inlay hints never turn on.
        router.request::<WorkspaceConfiguration, _>(|st: &mut ClientState, params| {
            let result = configuration_reply(st.settings.as_ref(), &params);
            ready(Ok(result))
        });
        // `workspace/inlayHint/refresh` / `workspace/semanticTokens/refresh`: the
        // server recomputed and wants the client to re-query. Forward it so the
        // editor re-issues the whole-buffer request for this server's buffers, then
        // ack. Without this, a server that produces hints/tokens asynchronously (and
        // signals readiness only via refresh) never has them fetched.
        router.request::<InlayHintRefreshRequest, _>(|st: &mut ClientState, ()| {
            let _ = st.event_tx.send(LspEvent::WorkspaceRefresh {
                key: st.key.clone(),
                kind: RefreshKind::InlayHint,
            });
            ready(Ok(()))
        });
        router.request::<SemanticTokensRefresh, _>(|st: &mut ClientState, ()| {
            let _ = st.event_tx.send(LspEvent::WorkspaceRefresh {
                key: st.key.clone(),
                kind: RefreshKind::SemanticTokens,
            });
            ready(Ok(()))
        });
        // `workspace/applyEdit`: the server asks the *editor* to apply an edit it
        // authored — how a refactor delivered as a `command` actually lands (the
        // `executeCommand` reply carries nothing). Forward the normalized edit and
        // park until the editor answers, so the `applied` flag we return is the real
        // outcome rather than an optimistic ack. Left unhandled this fell to
        // async-lsp's method-not-found default and the refactor failed outright.
        // Registered with a **raw** params type (same method, `serde_json::Value`):
        // `lsp-types` drops a text edit's `annotationId` on the way in, and that id is
        // what decides whether nxvim asks the user before applying.
        router.request::<RawApplyWorkspaceEdit, _>(|st: &mut ClientState, params| {
            let label = params
                .get("label")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let edit = params.get("edit").cloned().unwrap_or_default();
            // An edit we cannot read is refused, loud, rather than degraded to an empty
            // one — an empty edit is indistinguishable from "the server sent no
            // changes" and would be answered `applied: true`, i.e. a success for
            // something that never reached a buffer. The wasm client refuses the same
            // way, so both legs give a server the same answer.
            let parked = match try_normalize_workspace_edit_value(&edit) {
                Err(reason) => {
                    st.log.log(
                        LogLevel::Warn,
                        &st.key.name,
                        &format!("applyEdit: {reason}"),
                    );
                    Err(reason)
                }
                Ok(changes) => {
                    let id = st.next_apply_id;
                    st.next_apply_id += 1;
                    let (tx, rx) = oneshot::channel();
                    st.pending_apply.insert(id, tx);
                    let sent = st.event_tx.send(LspEvent::ApplyEdit {
                        key: st.key.clone(),
                        id,
                        label,
                        changes,
                    });
                    // The editor is gone (shutting down): drop the sender so the await
                    // below resolves immediately to "not applied" instead of hanging
                    // the server.
                    if sent.is_err() {
                        st.pending_apply.remove(&id);
                    }
                    Ok(rx)
                }
            };
            async move {
                let outcome = match parked {
                    // A dropped sender (editor gone, or the server torn down mid-apply)
                    // is a truthful "we did not apply it", never a fake success.
                    Ok(rx) => rx.await.unwrap_or_else(|_| ApplyEditOutcome {
                        applied: false,
                        failure_reason: Some("editor did not answer the edit".to_string()),
                        failed_change: None,
                    }),
                    Err(reason) => ApplyEditOutcome {
                        applied: false,
                        failure_reason: Some(reason),
                        failed_change: None,
                    },
                };
                Ok(ApplyWorkspaceEditResponse {
                    applied: outcome.applied,
                    failure_reason: outcome.failure_reason,
                    failed_change: outcome.failed_change,
                })
            }
        });
        // The editor's answer, emitted onto this loop by the manager. Resolving the
        // sender completes the parked request handler, which frames the response.
        router.event::<ApplyEditDone>(|st: &mut ClientState, done| {
            if let Some(tx) = st.pending_apply.remove(&done.id) {
                let _ = tx.send(done.outcome);
            }
            ControlFlow::Continue(())
        });
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
        // `window/workDoneProgress/create`: the server asking permission to report on
        // a token it just minted. It MUST be acked — this is not one of the optional
        // requests a server shrugs off. gopls reads the reply, and async-lsp's
        // method-not-found default made it conclude the client cannot do progress, so
        // it sent no `$/progress` at all: the whole chain below received nothing while
        // every layer of it worked. (The wasm `SyncLspClient` acks every unmodelled
        // request with `null` and so was never affected — the two legs had silently
        // drifted, which is exactly what the tier-1 rule forbids.)
        //
        // The token is not recorded: nxvim keys tasks off the token the `$/progress`
        // itself carries, which covers both server-minted (`create`) and client-minted
        // (`workDoneToken` on a request) tokens with one path.
        router.request::<WorkDoneProgressCreate, _>(|_st: &mut ClientState, _params| ready(Ok(())));
        // `workspace/workspaceFolders`: the server pulling the folder set (the twin of
        // the `workspaceFolders` we push at `initialize`). Answered from the key's root
        // through the SHARED [`workspace_folders`] helper, so the pull and the push
        // can't disagree. Declaring `workspace.workspaceFolders` without answering this
        // would be worse than not declaring it: a server that pulls would get
        // method-not-found and fall back to *no* workspace at all.
        router.request::<WorkspaceFoldersRequest, _>(|st: &mut ClientState, ()| {
            ready(Ok(workspace_folders(&st.key.root)))
        });
        // `client/registerCapability` / `client/unregisterCapability`: the dynamic half
        // of capability negotiation. Forwarded to the editor (and on to
        // `nx.lsp._register_capability`) and acked — an ack is required, and until this
        // existed the request fell to async-lsp's method-not-found, which is what a
        // server reads as "this client cannot do dynamic registration": ruff logs
        // "automatic configuration reloading will not be available" and every server
        // that watches files gives up on `workspace/didChangeWatchedFiles`, serving
        // stale results after any change made outside the editor.
        router.request::<RegisterCapability, _>(|st: &mut ClientState, params| {
            let registrations = params
                .registrations
                .into_iter()
                .map(|r| CapabilityRegistration {
                    id: r.id,
                    method: r.method,
                    register_options: r.register_options.unwrap_or(serde_json::Value::Null),
                })
                .collect();
            let _ = st.event_tx.send(LspEvent::RegisterCapability {
                key: st.key.clone(),
                registrations,
            });
            ready(Ok(()))
        });
        router.request::<UnregisterCapability, _>(|st: &mut ClientState, params| {
            let ids = params
                .unregisterations
                .into_iter()
                .map(|r| r.id)
                .collect::<Vec<_>>();
            let _ = st.event_tx.send(LspEvent::UnregisterCapability {
                key: st.key.clone(),
                ids,
            });
            ready(Ok(()))
        });
        // `$/progress`: the server reporting a long-running task (indexing, loading
        // a workspace). Forwarded as the editor's flattened update — the token
        // normalized and the payload decoded by the SHARED helpers, so this leg and
        // the wasm `SyncLspClient` cannot drift on either. The editor holds the
        // per-token state (a `report` means "patch what I sent"; see
        // `ProgressUpdate`), so nothing is interpreted here.
        //
        // `ProgressParamsValue` is untagged with a single `WorkDone` variant today;
        // matched (not unwrapped) so a future non-work-done progress kind is a
        // compile error to handle rather than a silent misread.
        router.notification::<Progress>(|st, params| {
            let ProgressParamsValue::WorkDone(value) = &params.value;
            let _ = st.event_tx.send(LspEvent::Progress {
                key: st.key.clone(),
                token: progress_token(&params.token),
                update: progress_update(value),
            });
            ControlFlow::Continue(())
        });
        // Be lenient about everything else a server emits (telemetry, custom
        // notifications/events): ignore rather than break the loop.
        router.unhandled_notification(|_st, _notif| ControlFlow::Continue(()));
        router.unhandled_event(|_st, _event| ControlFlow::Continue(()));
        router
    })
}

/// Build the `workspace/configuration` reply: one value per requested item, each
/// the item's dotted `section` path resolved against the config's `settings` (the
/// whole settings when the section is empty, `null` when the path is unset or no
/// settings were configured). The pull-model analogue of the `didChangeConfiguration`
/// push — neovim resolves each item the same way against `config.settings`.
pub(crate) fn configuration_reply(
    settings: Option<&serde_json::Value>,
    params: &ConfigurationParams,
) -> Vec<serde_json::Value> {
    params
        .items
        .iter()
        .map(|item| match settings {
            Some(s) => config_section(s, item.section.as_deref().unwrap_or("")),
            None => serde_json::Value::Null,
        })
        .collect()
}

/// Resolve a dotted config `section` (e.g. `"Lua.hint"`) to its value within
/// `settings`: an empty section returns the whole table; a path that runs off a
/// missing key returns `null` (the server then uses its default for that key).
fn config_section(settings: &serde_json::Value, section: &str) -> serde_json::Value {
    if section.is_empty() {
        return settings.clone();
    }
    let mut cur = settings;
    for part in section.split('.') {
        match cur.get(part) {
            Some(v) => cur = v,
            None => return serde_json::Value::Null,
        }
    }
    cur.clone()
}

/// Map an LSP `window/*Message` severity to a log level (`LOG`, the most verbose,
/// becomes `Debug`). Only the native client's `window/*Message` router calls it.
#[cfg_attr(not(feature = "native"), allow(dead_code))]
fn level_of(typ: MessageType) -> LogLevel {
    match typ {
        MessageType::ERROR => LogLevel::Error,
        MessageType::WARNING => LogLevel::Warn,
        MessageType::INFO => LogLevel::Info,
        _ => LogLevel::Debug,
    }
}

/// nxvim's base [`client_capabilities`] with the config's `extra` capabilities
/// (a raw JSON value) deep-merged over them — the config wins on any conflict, so
/// it can both extend (add a capability nxvim doesn't advertise) and override
/// (flip a flag). A malformed `extra` that won't round-trip back into
/// [`ClientCapabilities`] is logged and the base is used — loud, not silent, so a
/// bad `capabilities` table is visible rather than mysteriously ignored.
pub(crate) fn merged_client_capabilities(
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

/// The client capabilities we advertise at `initialize`: UTF-8 preferred over
/// UTF-16 for positions (Decision 4), document-save notifications, and the edit
/// features Phase 6 needs. Most consequential is
/// `codeAction.codeActionLiteralSupport` — **without it a server returns legacy
/// `Command[]` rather than a `CodeAction` carrying an `edit`**, and "apply the
/// edit" becomes impossible; we also declare `formatting`/`rename` and
/// `workspaceEdit.documentChanges` so servers offer those features, and
/// `completion.completionItem` (`documentationFormat` + `resolveSupport`) so
/// servers send per-item docs / let us resolve them lazily.
///
/// `window.workDoneProgress` belongs to the same family of "declare it or the
/// feature simply never happens" flags: a conforming server sends **no**
/// `$/progress` at all unless the client has advertised that it handles it. gopls
/// stays completely silent about its workspace load without this, so the whole
/// progress chain — store, mirror, `LspProgress`, statusline — receives nothing and
/// looks like a bug several layers away from the actual cause.
fn client_capabilities() -> ClientCapabilities {
    ClientCapabilities {
        window: Some(WindowClientCapabilities {
            work_done_progress: Some(true),
            ..Default::default()
        }),
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
            // Declare that we accept **markdown** hover contents (preferred) as well as
            // plaintext. This is load-bearing for syntax-highlighted hovers: pyright /
            // basedpyright (and others) default to *plaintext* hover unless the client
            // advertises `contentFormat`, returning a bare `def f() -> None` with no
            // ```lang fence. With markdown declared, the signature comes back fenced
            // (```python … ```), which the hover float renders as a `markdown` buffer —
            // tree-sitter's markdown injection colours the fenced code natively, and the
            // wasm edit-host's client-side `spansForFencedMarkdown` colours it on the web
            // build. Without this the fence never exists, so there is nothing to colour.
            hover: Some(HoverClientCapabilities {
                content_format: Some(vec![MarkupKind::Markdown, MarkupKind::PlainText]),
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
            // Declare semantic-tokens support so a server that gates the feature on
            // the client publishes its `legend` and answers `semanticTokens/full`
            // (ADR 0001 bridge #2). We request the `full` token set (whole document)
            // and its `full/delta` refinement (Phase 2) — once a buffer has cached a
            // `resultId`, the editor sends `full/delta` so the server can ship a diff
            // instead of the whole array. `range` is still unwired. The
            // type/modifier vocabularies are the LSP standard sets; a server is free
            // to publish a legend that references others (the decode falls back to
            // the raw name). `formats: [relative]` is the only token encoding the
            // protocol defines. `augments_syntax_tokens` is true: we paint semantic
            // tokens *over* the treesitter floor, not instead of it.
            semantic_tokens: Some(SemanticTokensClientCapabilities {
                dynamic_registration: Some(false),
                requests: SemanticTokensClientCapabilitiesRequests {
                    range: Some(false),
                    full: Some(SemanticTokensFullOptions::Delta { delta: Some(true) }),
                },
                token_types: standard_token_types(),
                token_modifiers: standard_token_modifiers(),
                formats: vec![TokenFormat::RELATIVE],
                overlapping_token_support: Some(false),
                multiline_token_support: Some(false),
                augments_syntax_tokens: Some(true),
                ..Default::default()
            }),
            // Declare inlay-hint support so a server that gates the feature on the
            // client answers `textDocument/inlayHint`. `resolveSupport` lists the
            // properties we can fetch lazily via `inlayHint/resolve` — declared so a
            // server may ship a bare label and fill the rest on demand (the resolve
            // round-trip itself is Phase 2; until then an unresolved hint shows its
            // eager label).
            inlay_hint: Some(InlayHintClientCapabilities {
                dynamic_registration: Some(false),
                resolve_support: Some(InlayHintResolveClientCapabilities {
                    properties: vec![
                        "label.location".to_string(),
                        "label.tooltip".to_string(),
                        "tooltip".to_string(),
                    ],
                }),
            }),
            // Declare folding-range support so a server answers
            // `textDocument/foldingRange` — the LSP fold source. We fold whole lines
            // (no `lineFoldingOnly` is fine), so the defaults suffice.
            folding_range: Some(FoldingRangeClientCapabilities {
                dynamic_registration: Some(false),
                line_folding_only: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                // The file operations a `documentChanges` edit may interleave with
                // its text edits. Declared because a server may withhold a refactor
                // whose edit needs one (an "extract to new file" is a `create` plus
                // the edits that fill it); nxvim applies them in the order sent.
                resource_operations: Some(vec![
                    ResourceOperationKind::Create,
                    ResourceOperationKind::Rename,
                    ResourceOperationKind::Delete,
                ]),
                // What actually happens when one change of an edit fails: the ones
                // before it stay applied, the ones after it are dropped — the
                // protocol's `abort`. Declared because it is exactly what nxvim
                // guarantees (the changes run strictly in order, one file operation at
                // a time), and declaring it is what gives the response's `failedChange`
                // index meaning. NOT `transactional`: a `delete` cannot be rolled back
                // without a backup, so promising all-or-nothing would be a lie a server
                // might act on.
                failure_handling: Some(FailureHandlingKind::Abort),
                // `changeAnnotations`: a server may split one edit into named groups
                // and mark some `needsConfirmation`, which nxvim asks about before
                // applying (the group is accepted or declined whole). Declared because
                // a server checks it before bothering to annotate — and because not
                // declaring it while *also* ignoring the flag would silently apply
                // exactly the changes the server wanted a human to look at.
                // `groupsOnLabel`: one question per label, not per change.
                change_annotation_support: Some(ChangeAnnotationWorkspaceEditClientCapabilities {
                    groups_on_label: Some(true),
                }),
                ..Default::default()
            }),
            // We honor server→client `workspace/applyEdit`. This is how a refactor
            // delivered as a `command` reaches the buffer at all — the
            // `executeCommand` reply is empty and the edit arrives as this push — so
            // a server that gates on the capability (rather than sending it blind,
            // as gopls does) would otherwise silently do nothing.
            apply_edit: Some(true),
            // We answer `workspace/configuration` (the pull model) from the config's
            // `settings` — declaring it is what makes a pull-only server (lua_ls,
            // gopls) read its options instead of running on defaults. Without it,
            // lua_ls never enables inlay hints regardless of the `settings` we push.
            configuration: Some(true),
            // We honor the server→client refresh requests by re-querying, so declare
            // support: a server that computes inlay hints / semantic tokens
            // asynchronously signals readiness this way, and won't bother (or will
            // produce nothing) unless the client advertises it can refresh.
            inlay_hint: Some(InlayHintWorkspaceClientCapabilities {
                refresh_support: Some(true),
            }),
            semantic_tokens: Some(SemanticTokensWorkspaceClientCapabilities {
                refresh_support: Some(true),
            }),
            // We send the workspace root as a `workspaceFolders` array at `initialize`
            // *and* answer the server→client `workspace/workspaceFolders` pull. Both
            // are needed: `rootUri` is deprecated and pyright/basedpyright ignore it
            // outright, creating their synthetic `<default workspace root>` and then
            // reporting `File or directory "/<default workspace root>" does not exist`
            // for a workspace that is perfectly fine — the whole analysis runs against
            // nothing. nxvim has exactly one folder per client (the key's root; a
            // second root is a second client), so the set never changes and no
            // `didChangeWorkspaceFolders` is ever sent.
            workspace_folders: Some(true),
            // Dynamic registration, declared **only** for the two `workspace/*`
            // capabilities nxvim genuinely honors (`client/registerCapability` is
            // answered in both clients; the registration is forwarded to
            // `nx.lsp._register_capability`, which arms the watches):
            //
            //   * `didChangeConfiguration` — the "automatic configuration reloading"
            //     ruff and friends warn about when it is missing.
            //   * `didChangeWatchedFiles` — without it a server never learns about a
            //     file changed outside the editor (a `git checkout`, a generated file)
            //     and serves stale results with no way to notice.
            //
            // Every OTHER capability keeps `dynamicRegistration: false` on purpose: a
            // server may deliver a feature *only* through a registration it thinks the
            // client honors, so claiming support we don't implement would turn a
            // working static feature into a silently missing one.
            did_change_configuration: Some(DidChangeConfigurationClientCapabilities {
                dynamic_registration: Some(true),
            }),
            did_change_watched_files: Some(DidChangeWatchedFilesClientCapabilities {
                dynamic_registration: Some(true),
                // `RelativePattern` (a `{ baseUri, pattern }` glob rather than a bare
                // string): declared because we resolve it — the Lua watcher joins the
                // base to the pattern before matching.
                relative_pattern_support: Some(true),
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// The one [`WorkspaceFolder`] a client serves: its key's `root`, named by the
/// directory's last component (what an editor shows in a folder list; pyright echoes
/// it in diagnostics). `None` when the root won't convert to a `file://` URL.
///
/// Shared by the `initialize` params and the server→client `workspace/workspaceFolders`
/// pull, so both spell the same folder — a server that reads one and then the other
/// must not see them disagree.
pub(crate) fn workspace_folders(root: &std::path::Path) -> Option<Vec<WorkspaceFolder>> {
    let uri = Url::from_file_path(root).ok()?;
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.to_string_lossy().into_owned());
    Some(vec![WorkspaceFolder { uri, name }])
}

/// The `initialize` request params shared by the async manager and the sync wasm
/// client: the workspace `root` as **both** `workspaceFolders` and the deprecated
/// `root_uri`, the config's `init_options` (falling back to `settings`, neovim's
/// behavior), and nxvim's base capabilities with the config's `capabilities`
/// deep-merged over them. `process_id` is the only thing that differs between the
/// paths — `Some(pid)` natively, `None` in the browser.
///
/// Both root spellings go out because servers split on which they read: older ones
/// only know `rootUri`, while pyright/basedpyright dropped it and use
/// `workspaceFolders` alone — given only `rootUri` they fall back to a synthetic
/// `<default workspace root>` and report `File or directory
/// "/<default workspace root>" does not exist`. Sending both is what neovim does,
/// and they can't disagree: one root per client.
#[allow(deprecated)] // root_uri is what a pre-workspaceFolders server still reads
pub(crate) fn init_params(
    root: &std::path::Path,
    spawn: &ServerSpawn,
    process_id: Option<u32>,
    log: &LspLog,
    name: &str,
) -> InitializeParams {
    InitializeParams {
        process_id,
        root_uri: Url::from_file_path(root).ok(),
        workspace_folders: workspace_folders(root),
        initialization_options: spawn
            .init_options
            .clone()
            .or_else(|| spawn.settings.clone()),
        capabilities: merged_client_capabilities(spawn.capabilities.as_ref(), log, name),
        ..Default::default()
    }
}

/// Read an `initialize` result into the editor-facing trio both dispatch paths
/// forward on [`LspEvent::Initialized`]: the distilled [`ServerCaps`], the
/// negotiated [`PositionEncoding`], and the raw result JSON (for the config's
/// `on_init` hook; `Null` if it somehow won't serialize). Shared so the async
/// manager and the sync wasm client distill the handshake identically.
pub(crate) fn read_init_result(
    init: &InitializeResult,
) -> (ServerCaps, PositionEncoding, serde_json::Value) {
    let caps = ServerCaps {
        sync_kind: sync_kind_of(&init.capabilities),
        providers: provider_caps(&init.capabilities),
        legend: semantic_legend(&init.capabilities),
        semantic_tokens_delta: semantic_tokens_delta(&init.capabilities),
    };
    let encoding = encoding_of(&init.capabilities);
    let raw = serde_json::to_value(init).unwrap_or(serde_json::Value::Null);
    (caps, encoding, raw)
}

/// The position encoding the server chose (LSP defaults to UTF-16 when the
/// server says nothing).
pub(crate) fn encoding_of(caps: &ServerCapabilities) -> PositionEncoding {
    match caps.position_encoding.as_ref().map(|e| e.as_str()) {
        Some("utf-8") => PositionEncoding::Utf8,
        Some("utf-32") => PositionEncoding::Utf32,
        _ => PositionEncoding::Utf16,
    }
}

/// Reduce the protocol [`ServerCapabilities`] to the per-feature provider bools
/// the editor surfaces as `client.server_capabilities`. Serializing once and
/// probing the camelCase `*Provider` fields keeps all fifteen uniform across the
/// protocol's mix of `bool`/`OneOf`/options shapes: a provider counts as
/// advertised when its field is present and not an explicit `false` (an options
/// object — the common case — counts as supported).
pub(crate) fn provider_caps(caps: &ServerCapabilities) -> ProviderCaps {
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
        // The advertised signature-help trigger/retrigger characters (e.g. `(` / `,`),
        // flattened — what an opt-in auto-trigger fires on. Read off the typed options
        // object rather than the JSON probe above (which only yields presence).
        signature_trigger_chars: caps
            .signature_help_provider
            .as_ref()
            .map(|o| {
                let mut chars = o.trigger_characters.clone().unwrap_or_default();
                if let Some(retrigger) = &o.retrigger_characters {
                    chars.extend(retrigger.iter().cloned());
                }
                chars
            })
            .unwrap_or_default(),
        completion: present("completionProvider"),
        document_formatting: present("documentFormattingProvider"),
        rename: present("renameProvider"),
        code_action: present("codeActionProvider"),
        semantic_tokens: present("semanticTokensProvider"),
        inlay_hints: present("inlayHintProvider"),
        folding_range: present("foldingRangeProvider"),
        document_symbol: present("documentSymbolProvider"),
        workspace_symbol: present("workspaceSymbolProvider"),
    }
}

/// The standard LSP semantic token *types* nxvim understands, advertised in the
/// client capability. A server may publish a legend referencing types outside
/// this set; the decode still maps them by raw name (an unknown type just won't
/// resolve a highlight group). Kept in protocol order for readability only — the
/// legend, not this list, fixes a server's indices.
fn standard_token_types() -> Vec<SemanticTokenType> {
    vec![
        SemanticTokenType::NAMESPACE,
        SemanticTokenType::TYPE,
        SemanticTokenType::CLASS,
        SemanticTokenType::ENUM,
        SemanticTokenType::INTERFACE,
        SemanticTokenType::STRUCT,
        SemanticTokenType::TYPE_PARAMETER,
        SemanticTokenType::PARAMETER,
        SemanticTokenType::VARIABLE,
        SemanticTokenType::PROPERTY,
        SemanticTokenType::ENUM_MEMBER,
        SemanticTokenType::EVENT,
        SemanticTokenType::FUNCTION,
        SemanticTokenType::METHOD,
        SemanticTokenType::MACRO,
        SemanticTokenType::KEYWORD,
        SemanticTokenType::MODIFIER,
        SemanticTokenType::COMMENT,
        SemanticTokenType::STRING,
        SemanticTokenType::NUMBER,
        SemanticTokenType::REGEXP,
        SemanticTokenType::OPERATOR,
        SemanticTokenType::DECORATOR,
    ]
}

/// The standard LSP semantic token *modifiers* nxvim understands. As with the
/// types, a server's legend may reference others; those fall back to the raw name.
fn standard_token_modifiers() -> Vec<SemanticTokenModifier> {
    vec![
        SemanticTokenModifier::DECLARATION,
        SemanticTokenModifier::DEFINITION,
        SemanticTokenModifier::READONLY,
        SemanticTokenModifier::STATIC,
        SemanticTokenModifier::DEPRECATED,
        SemanticTokenModifier::ABSTRACT,
        SemanticTokenModifier::ASYNC,
        SemanticTokenModifier::MODIFICATION,
        SemanticTokenModifier::DOCUMENTATION,
        SemanticTokenModifier::DEFAULT_LIBRARY,
    ]
}

/// The server's `semanticTokensProvider.legend` distilled to plain string arrays,
/// or `None` when the server advertises no semantic-tokens provider. The integer
/// indices a `semanticTokens/full` reply carries are positions into these arrays,
/// so the editor needs them to decode (Decision 4 keeps the encoding-aware
/// conversion editor-side; the legend rides along the same `Initialized` path).
pub(crate) fn semantic_legend(caps: &ServerCapabilities) -> Option<SemanticLegend> {
    use lsp_types::SemanticTokensServerCapabilities as Cap;
    let legend = match caps.semantic_tokens_provider.as_ref()? {
        Cap::SemanticTokensOptions(opts) => &opts.legend,
        Cap::SemanticTokensRegistrationOptions(opts) => &opts.semantic_tokens_options.legend,
    };
    Some(SemanticLegend {
        token_types: legend
            .token_types
            .iter()
            .map(|t| t.as_str().to_string())
            .collect(),
        token_modifiers: legend
            .token_modifiers
            .iter()
            .map(|m| m.as_str().to_string())
            .collect(),
    })
}

/// Whether the server advertised `semanticTokensProvider.full.delta == true` — it
/// can answer `semanticTokens/full/delta` with diffs (Phase 2). `false` when the
/// provider is absent, or its `full` is a bare `true` / has `delta != true` (the
/// editor then always re-requests the whole `full` set). `full/delta` to a server
/// that didn't advertise it would error and loop, so this gates the delta path.
pub(crate) fn semantic_tokens_delta(caps: &ServerCapabilities) -> bool {
    use lsp_types::SemanticTokensFullOptions as Full;
    use lsp_types::SemanticTokensServerCapabilities as Cap;
    let opts = match caps.semantic_tokens_provider.as_ref() {
        Some(Cap::SemanticTokensOptions(opts)) => opts,
        Some(Cap::SemanticTokensRegistrationOptions(opts)) => &opts.semantic_tokens_options,
        None => return false,
    };
    matches!(opts.full, Some(Full::Delta { delta: Some(true) }))
}

/// The document-sync kind the server wants (full text, incremental deltas, or
/// none). Defaults to `NONE` when unspecified, so we never push changes a server
/// didn't ask for.
pub(crate) fn sync_kind_of(caps: &ServerCapabilities) -> TextDocumentSyncKind {
    match &caps.text_document_sync {
        Some(TextDocumentSyncCapability::Kind(kind)) => *kind,
        Some(TextDocumentSyncCapability::Options(opts)) => {
            opts.change.unwrap_or(TextDocumentSyncKind::NONE)
        }
        None => TextDocumentSyncKind::NONE,
    }
}

/// Split a child's [`std::process::ExitStatus`] into `(code, signal)` for the
/// config's `on_exit(code, signal, client)` hook (Phase 3). `code` is the normal
/// exit code; `signal` is the terminating signal (unix only — always `None`
/// elsewhere). `None`/`None` when the status couldn't be collected.
#[cfg(feature = "native")]
pub(crate) fn exit_code_signal(
    status: Option<std::process::ExitStatus>,
) -> (Option<i32>, Option<i32>) {
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
