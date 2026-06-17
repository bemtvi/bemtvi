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
use lsp_types::notification::{LogMessage, PublishDiagnostics, ShowMessage};
#[cfg(feature = "native")]
use lsp_types::request::{InlayHintRefreshRequest, SemanticTokensRefresh, WorkspaceConfiguration};
use lsp_types::{
    ClientCapabilities, CodeActionCapabilityResolveSupport, CodeActionClientCapabilities,
    CodeActionKindLiteralSupport, CodeActionLiteralSupport, CompletionClientCapabilities,
    CompletionItemCapability, CompletionItemCapabilityResolveSupport, ConfigurationParams,
    DocumentFormattingClientCapabilities, GeneralClientCapabilities, InlayHintClientCapabilities,
    InlayHintResolveClientCapabilities, InlayHintWorkspaceClientCapabilities, MarkupKind,
    MessageType, PositionEncodingKind, PublishDiagnosticsClientCapabilities,
    RenameClientCapabilities, SemanticTokenModifier, SemanticTokenType,
    SemanticTokensClientCapabilities, SemanticTokensClientCapabilitiesRequests,
    SemanticTokensFullOptions, SemanticTokensWorkspaceClientCapabilities, ServerCapabilities,
    TextDocumentClientCapabilities, TextDocumentSyncCapability, TextDocumentSyncClientCapabilities,
    TextDocumentSyncKind, TokenFormat, WorkspaceClientCapabilities,
    WorkspaceEditClientCapabilities,
};
#[cfg(feature = "native")]
use tokio::sync::mpsc::UnboundedSender;

use crate::log::{LogLevel, LspLog};
// Pure helpers (always compiled) return these; the async router/handshake items
// (gated below) use `LspEvent`/`RefreshKind`/`ServerKey`.
#[cfg(feature = "native")]
use crate::protocol::{LspEvent, RefreshKind, ServerKey};
use crate::protocol::{PositionEncoding, ProviderCaps, SemanticLegend};

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
}

/// Build the `async-lsp` client `MainLoop` and its `ServerSocket`. The bare
/// [`Router`] is the service: the client only *receives* notifications
/// (diagnostics, log/show messages) whose handlers are trivial and panic-free,
/// so the concurrency/catch-unwind middleware a server needs is unnecessary
/// here. Unhandled server→client requests get a method-not-found response, which
/// language servers tolerate.
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
            ..Default::default()
        }),
        workspace: Some(WorkspaceClientCapabilities {
            workspace_edit: Some(WorkspaceEditClientCapabilities {
                document_changes: Some(true),
                ..Default::default()
            }),
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
/// probing the camelCase `*Provider` fields keeps all thirteen uniform across the
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
        semantic_tokens: present("semanticTokensProvider"),
        inlay_hints: present("inlayHintProvider"),
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
