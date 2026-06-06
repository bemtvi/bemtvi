//! Reply normalization: distill a server's raw protocol response into the
//! editor-facing [`LspReply`] / data types in [`crate::protocol`].
//!
//! These are pure transforms — given an already-awaited result (or a value),
//! they reduce the protocol's many response shapes to the one the editor renders.
//! A transport error degrades to the empty/`None` case (logged), so the editor
//! uniformly sees "nothing found" rather than a hang. The functions that *issue*
//! the request live in [`crate::dispatch`]; this module only shapes what comes
//! back.

use lsp_types::{
    AnnotatedTextEdit, CodeActionOrCommand, CodeActionResponse, CompletionItem, CompletionItemKind,
    CompletionResponse, CompletionTextEdit, DocumentChangeOperation, DocumentChanges,
    Documentation, GotoDefinitionResponse, Hover, HoverContents, Location, MarkedString, OneOf,
    ParameterLabel, SignatureHelp, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
};

use crate::log::{LogLevel, LspLog};
use crate::protocol::{CodeActionData, CompletionItemData, LspReply, WorkspaceEditData};

/// Distill a `textDocument/codeAction` response (a mixed `(Command | CodeAction)[]`)
/// into the editor-facing list: a `CodeAction`'s `title` + normalized eager
/// `edit` + optional `command` (run via `workspace/executeCommand` after the
/// edit); a bare `Command` lands as a `command`-only entry (Phase 8).
pub(crate) fn code_actions(resp: CodeActionResponse) -> Vec<CodeActionData> {
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
pub(crate) fn completion_reply(
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

/// Reduce a completion item's `documentation` (a plain string, or a
/// `MarkupContent` whose markdown is rendered as plain lines — same as hover) to
/// its display text, trailing blank lines trimmed. `None` when the result is
/// empty, so a blank documentation block reads as "no docs" rather than an empty
/// preview (it is never *faked* into one — an absent field is simply `None`).
pub(crate) fn documentation_lines(doc: Documentation) -> Option<String> {
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
pub(crate) fn hover_reply(
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
pub(crate) fn signature_help_reply(
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
pub(crate) fn goto_locations(
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
