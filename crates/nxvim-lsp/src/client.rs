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

use std::ops::ControlFlow;
use std::sync::Arc;

use async_lsp::router::Router;
use async_lsp::{MainLoop, ServerSocket};
use lsp_types::notification::{LogMessage, PublishDiagnostics, ShowMessage};
use lsp_types::{
    ClientCapabilities, CodeActionCapabilityResolveSupport, CodeActionClientCapabilities,
    CodeActionKindLiteralSupport, CodeActionLiteralSupport, CompletionClientCapabilities,
    CompletionItemCapability, CompletionItemCapabilityResolveSupport,
    DocumentFormattingClientCapabilities, GeneralClientCapabilities, MarkupKind, MessageType,
    PositionEncodingKind, PublishDiagnosticsClientCapabilities, RenameClientCapabilities,
    ServerCapabilities, TextDocumentClientCapabilities, TextDocumentSyncCapability,
    TextDocumentSyncClientCapabilities, TextDocumentSyncKind, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities,
};
use tokio::sync::mpsc::UnboundedSender;

use crate::log::{LogLevel, LspLog};
use crate::protocol::{LspEvent, PositionEncoding, ProviderCaps, ServerKey};

/// State shared by the client `MainLoop`'s notification handlers: which server
/// this loop belongs to, the channel to forward distilled events on, and the log.
pub(crate) struct ClientState {
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
pub(crate) fn new_client(
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
pub(crate) fn encoding_of(caps: &ServerCapabilities) -> PositionEncoding {
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
        completion: present("completionProvider"),
        document_formatting: present("documentFormattingProvider"),
        rename: present("renameProvider"),
        code_action: present("codeActionProvider"),
    }
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
