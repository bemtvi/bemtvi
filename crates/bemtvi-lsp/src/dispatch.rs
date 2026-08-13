//! Request/notification translation: turn an editor-issued [`LspNotify`] /
//! [`LspRequest`] into the matching `async-lsp` call on a server socket, and feed
//! the response through [`crate::convert`] into an [`LspReply`].
//!
//! The typed native features (definition, hover, completion, …) each map to a
//! `LanguageServer` method; the generic `client:request`/`client:notify` path
//! (Phase 5) reaches arbitrary methods through the [`dyn_requests!`] /
//! [`dyn_notifications!`] dispatch tables. A transport error on a typed request is
//! degraded to the empty reply (the editor sees "nothing found"); a generic
//! request surfaces the error to its Lua handler instead.

use async_lsp::{LanguageServer, ServerSocket};
use lsp_types::{
    CodeActionContext, CodeActionKind, CodeActionParams, CompletionItem, CompletionParams,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    DidSaveTextDocumentParams, DocumentFormattingParams, DocumentSymbolParams, FoldingRangeParams,
    FormattingOptions, GotoDefinitionParams, HoverParams, InlayHint, InlayHintParams,
    PartialResultParams, Position, ReferenceContext, ReferenceParams, RenameParams,
    SemanticTokensDeltaParams, SemanticTokensParams, SignatureHelpParams, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Url, VersionedTextDocumentIdentifier,
    WorkspaceSymbolParams,
};

use crate::convert::{
    code_actions_value, completion_reply, document_symbols, folding_ranges, goto_locations,
    hover_reply, inlay_hint, resolved_completion, resolved_inlay_hint, semantic_tokens_delta_data,
    semantic_tokens_full, signature_help_reply, workspace_symbols,
};
use crate::log::{LogLevel, LspLog};
use crate::protocol::{LspNotify, LspReply, LspRequest};

/// Translate an [`LspNotify`] into the corresponding `async-lsp` notification.
/// Send errors are ignored: a dead socket is detected by the main loop ending.
pub(crate) fn apply_notify(socket: &mut ServerSocket, note: LspNotify, log: &LspLog, name: &str) {
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

/// Unwrap a transport result, logging a failure and degrading it to `None` so the
/// pure [`crate::convert`] distiller (shared verbatim with the synchronous wasm
/// client) sees the uniform "nothing found" case rather than a hang. `what` names
/// the feature for the log line.
fn unwrap_logged<T>(
    result: Result<Option<T>, async_lsp::Error>,
    log: &LspLog,
    name: &str,
    what: &str,
) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(e) => {
            log.log(LogLevel::Warn, name, &format!("{what} failed: {e}"));
            None
        }
    }
}

/// Normalize a reply's `WorkspaceEdit` from its raw JSON, logging an edit that
/// doesn't parse and degrading it to an empty one — the `unwrap_logged` shape, for
/// the paths that read the wire value themselves (to keep its change annotations,
/// which the typed form drops). Without the log a malformed edit would reach the user
/// as a bare "No applicable changes", indistinguishable from a server with nothing to
/// say.
fn normalize_logged(
    value: &serde_json::Value,
    log: &LspLog,
    name: &str,
    what: &str,
) -> crate::protocol::WorkspaceEditData {
    match crate::convert::try_normalize_workspace_edit_value(value) {
        Ok(data) => data,
        Err(reason) => {
            log.log(LogLevel::Warn, name, &format!("{what}: {reason}"));
            Default::default()
        }
    }
}

/// Issue one language-feature [`LspRequest`] on the socket and await its reply,
/// normalizing every goto-family / references response to a flat [`LspReply`].
/// A transport error (a server that died mid-request, an unsupported method) is
/// logged and degraded to an empty location list, so the editor uniformly sees
/// "nothing found" rather than a hang.
pub(crate) async fn issue_request(
    sock: &mut ServerSocket,
    req: LspRequest,
    log: &LspLog,
    name: &str,
) -> LspReply {
    if log.enabled(LogLevel::Debug) {
        log.log(LogLevel::Debug, name, &describe_request(&req));
    }
    match req {
        LspRequest::Definition { uri, position } => {
            LspReply::Locations(goto_locations(unwrap_logged(
                sock.definition(goto_params(uri, position)).await,
                log,
                name,
                "definition",
            )))
        }
        LspRequest::Declaration { uri, position } => {
            LspReply::Locations(goto_locations(unwrap_logged(
                sock.declaration(goto_params(uri, position)).await,
                log,
                name,
                "declaration",
            )))
        }
        LspRequest::TypeDefinition { uri, position } => {
            LspReply::Locations(goto_locations(unwrap_logged(
                sock.type_definition(goto_params(uri, position)).await,
                log,
                name,
                "typeDefinition",
            )))
        }
        LspRequest::Implementation { uri, position } => {
            LspReply::Locations(goto_locations(unwrap_logged(
                sock.implementation(goto_params(uri, position)).await,
                log,
                name,
                "implementation",
            )))
        }
        LspRequest::DocumentSymbol { uri } => {
            let params = DocumentSymbolParams {
                text_document: TextDocumentIdentifier { uri: uri.clone() },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            LspReply::Symbols(document_symbols(
                &uri,
                unwrap_logged(
                    sock.document_symbol(params).await,
                    log,
                    name,
                    "documentSymbol",
                ),
            ))
        }
        LspRequest::FoldingRange { uri } => {
            let params = FoldingRangeParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            LspReply::Folds(folding_ranges(
                unwrap_logged(sock.folding_range(params).await, log, name, "foldingRange")
                    .unwrap_or_default(),
            ))
        }
        LspRequest::WorkspaceSymbol { query } => {
            let params = WorkspaceSymbolParams {
                query,
                work_done_progress_params: Default::default(),
                partial_result_params: PartialResultParams::default(),
            };
            LspReply::Symbols(workspace_symbols(unwrap_logged(
                sock.symbol(params).await,
                log,
                name,
                "workspace/symbol",
            )))
        }
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
            LspReply::Locations(
                unwrap_logged(sock.references(params).await, log, name, "references")
                    .unwrap_or_default(),
            )
        }
        LspRequest::Hover { uri, position } => {
            let params = HoverParams {
                text_document_position_params: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
            };
            hover_reply(unwrap_logged(sock.hover(params).await, log, name, "hover"))
        }
        LspRequest::SignatureHelp { uri, position } => {
            let params = SignatureHelpParams {
                context: None,
                text_document_position_params: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
            };
            signature_help_reply(unwrap_logged(
                sock.signature_help(params).await,
                log,
                name,
                "signatureHelp",
            ))
        }
        LspRequest::Completion { uri, position } => {
            let params = CompletionParams {
                text_document_position: text_document_position(uri, position),
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
                context: None,
            };
            completion_reply(unwrap_logged(
                sock.completion(params).await,
                log,
                name,
                "completion",
            ))
        }
        LspRequest::Formatting {
            uri,
            tab_size,
            insert_spaces,
        } => {
            let params = DocumentFormattingParams {
                text_document: TextDocumentIdentifier { uri },
                options: formatting_options(tab_size, insert_spaces),
                work_done_progress_params: Default::default(),
            };
            LspReply::Edits(
                unwrap_logged(sock.formatting(params).await, log, name, "formatting")
                    .unwrap_or_default(),
            )
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
            // Raw, not `sock.rename(…)`: a typed `WorkspaceEdit` has already lost its
            // text edits' `annotationId`s by the time we see it (see
            // `normalize_workspace_edit_value`), and a rename is exactly the refactor a
            // server annotates ("also update this in comments and strings?").
            LspReply::WorkspaceEdit(
                unwrap_logged(sock.request::<RawRename>(params).await, log, name, "rename")
                    .filter(|e| !e.is_null())
                    .as_ref()
                    .map(|e| normalize_logged(e, log, name, "rename"))
                    .unwrap_or_default(),
            )
        }
        LspRequest::CodeAction {
            uri,
            range,
            diagnostics,
            only,
        } => {
            let params = CodeActionParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                context: CodeActionContext {
                    diagnostics,
                    // The caller's kind filter, when they asked for one. Omitted (not an
                    // empty list — which would mean "no kinds at all") when they didn't.
                    only: (!only.is_empty())
                        .then(|| only.into_iter().map(CodeActionKind::from).collect()),
                    trigger_kind: None,
                },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            LspReply::CodeActions(code_actions_value(
                unwrap_logged(
                    sock.request::<RawCodeAction>(params).await,
                    log,
                    name,
                    "codeAction",
                )
                .unwrap_or_default(),
            ))
        }
        // Raw for the same reason as `rename` above: the resolved action's edit keeps
        // its change annotations only in its JSON form.
        LspRequest::ResolveCodeAction { action } => {
            match sock.request::<RawCodeActionResolve>(*action).await {
                Ok(resolved) => LspReply::ResolvedCodeAction(
                    resolved
                        .get("edit")
                        .filter(|e| !e.is_null())
                        .map(|e| normalize_logged(e, log, name, "codeAction/resolve")),
                ),
                Err(e) => {
                    log.log(
                        LogLevel::Warn,
                        name,
                        &format!("codeAction/resolve failed: {e}"),
                    );
                    LspReply::ResolvedCodeAction(None)
                }
            }
        }
        LspRequest::SemanticTokensFull { uri } => {
            let params = SemanticTokensParams {
                text_document: TextDocumentIdentifier { uri },
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let resp = unwrap_logged(
                sock.semantic_tokens_full(params).await,
                log,
                name,
                "semanticTokens/full",
            );
            LspReply::SemanticTokens(semantic_tokens_full(resp))
        }
        LspRequest::SemanticTokensDelta {
            uri,
            previous_result_id,
        } => {
            let params = SemanticTokensDeltaParams {
                text_document: TextDocumentIdentifier { uri },
                previous_result_id,
                work_done_progress_params: Default::default(),
                partial_result_params: Default::default(),
            };
            let resp = unwrap_logged(
                sock.semantic_tokens_full_delta(params).await,
                log,
                name,
                "semanticTokens/full/delta",
            );
            LspReply::SemanticTokens(semantic_tokens_delta_data(resp))
        }
        LspRequest::InlayHint { uri, range } => {
            let params = InlayHintParams {
                text_document: TextDocumentIdentifier { uri },
                range,
                work_done_progress_params: Default::default(),
            };
            LspReply::InlayHints(
                unwrap_logged(sock.inlay_hint(params).await, log, name, "inlayHint")
                    .unwrap_or_default()
                    .iter()
                    .map(inlay_hint)
                    .collect(),
            )
        }
        LspRequest::ResolveInlayHint { hint } => {
            resolve_inlay_hint_reply(sock, hint, log, name).await
        }
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

/// Issue a `completionItem/resolve` for the selected menu item and distill the
/// reply to its `documentation`/`detail` ([`LspReply::ResolvedCompletion`]). The
/// `item` is the original completion item as JSON (`CompletionItemData::resolve_data`);
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
        Ok(resolved) => resolved_completion(Some(resolved)),
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

/// Issue an `inlayHint/resolve` for a lazy hint and distill the reply to its
/// resolved label ([`LspReply::ResolvedInlayHint`]). The `hint` is the original
/// inlay hint as JSON (`InlayHintData::resolve_data`), deserialized back to an
/// [`InlayHint`] to send verbatim — a server matches the resolve against the exact
/// hint it issued. A malformed hint, an unsupported method, a server error, or a
/// resolved hint that still has no label all degrade to `label: None` (logged), so
/// the editor drops the placeholder rather than paint an empty hint — never a fake.
async fn resolve_inlay_hint_reply(
    sock: &mut ServerSocket,
    hint: serde_json::Value,
    log: &LspLog,
    name: &str,
) -> LspReply {
    let hint: InlayHint = match serde_json::from_value(hint) {
        Ok(hint) => hint,
        Err(e) => {
            log.log(
                LogLevel::Warn,
                name,
                &format!("inlayHint/resolve: malformed hint: {e}"),
            );
            return LspReply::ResolvedInlayHint { label: None };
        }
    };
    match sock.inlay_hint_resolve(hint).await {
        Ok(resolved) => resolved_inlay_hint(Some(&resolved)),
        Err(e) => {
            log.log(
                LogLevel::Warn,
                name,
                &format!("inlayHint/resolve failed: {e}"),
            );
            LspReply::ResolvedInlayHint { label: None }
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

/// Generate the request dispatch table: one `Request` impl per `(method, Type)`
/// row (raw JSON params/result) and the runtime `match` that issues it. Standard
/// LSP methods and server-specific ones (`rust-analyzer/*`, clangd's
/// `switchSourceHeader`, …) live side by side — they only differ by the method
/// string. The rows come from the single whitelist [`lsp_dyn_request_rows!`]
/// (protocol.rs), which also feeds the sync wasm client's pre-flight check — a
/// method is supported on both legs or neither.
macro_rules! dyn_requests {
    ($(($method:literal, $ty:ident)),* $(,)?) => {
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
                    "bemtvi: client:request: unsupported method '{other}' \
                     (add a row to lsp_dyn_request_rows! in bemtvi-lsp/src/protocol.rs)"
                )),
            }
        }
    };
}

/// Generate the notification dispatch table — the fire-and-forget twin of
/// [`dyn_requests!`].
macro_rules! dyn_notifications {
    ($(($method:literal, $ty:ident)),* $(,)?) => {
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

// The supported generic-request methods — ONE row list in protocol.rs feeds the
// table here and the sync wasm client's pre-flight whitelist, so the two legs
// can't drift. Add a method there, not here.
crate::lsp_dyn_request_rows!(dyn_requests);

// The supported generic-notification methods.
crate::lsp_dyn_notify_rows!(dyn_notifications);

/// The `FormattingOptions` for `textDocument/formatting`, built from the
/// requesting buffer's `tabstop` (`tab_size`) and `expandtab` (`insert_spaces`)
/// so the language server formats to the buffer's indentation.
fn formatting_options(tab_size: u32, insert_spaces: bool) -> FormattingOptions {
    FormattingOptions {
        tab_size,
        insert_spaces,
        ..Default::default()
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

/// The three requests whose replies carry a `WorkspaceEdit`, re-declared with a **raw
/// JSON result**. `lsp-types` drops a text edit's `annotationId` on the way in (its
/// `OneOf<TextEdit, AnnotatedTextEdit>` is untagged and `TextEdit` accepts unknown
/// fields), and that id is what decides whether bemtvi asks before applying — so these
/// paths take the wire shape and normalize it themselves. Same method, same params;
/// only the result type differs.
enum RawRename {}
impl lsp_types::request::Request for RawRename {
    type Params = RenameParams;
    type Result = Option<serde_json::Value>;
    const METHOD: &'static str = "textDocument/rename";
}

enum RawCodeAction {}
impl lsp_types::request::Request for RawCodeAction {
    type Params = CodeActionParams;
    type Result = Option<serde_json::Value>;
    const METHOD: &'static str = "textDocument/codeAction";
}

enum RawCodeActionResolve {}
impl lsp_types::request::Request for RawCodeActionResolve {
    type Params = lsp_types::CodeAction;
    type Result = serde_json::Value;
    const METHOD: &'static str = "codeAction/resolve";
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
        LspRequest::DocumentSymbol { .. } => return "→ documentSymbol".to_string(),
        LspRequest::FoldingRange { .. } => return "→ foldingRange".to_string(),
        LspRequest::WorkspaceSymbol { query } => return format!("→ workspace/symbol '{query}'"),
        LspRequest::SemanticTokensFull { .. } => return "→ semanticTokens/full".to_string(),
        LspRequest::SemanticTokensDelta {
            previous_result_id, ..
        } => return format!("→ semanticTokens/full/delta (prev {previous_result_id})"),
        LspRequest::InlayHint { .. } => return "→ inlayHint".to_string(),
        LspRequest::ResolveInlayHint { .. } => return "→ inlayHint/resolve".to_string(),
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
