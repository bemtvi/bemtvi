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
    DocumentSymbol, DocumentSymbolResponse, Documentation, FoldingRange, GotoDefinitionResponse,
    Hover, HoverContents, InlayHint, InlayHintKind, InlayHintLabel, Location, MarkedString, OneOf,
    ParameterLabel, SemanticTokensFullDeltaResult, SemanticTokensResult, SignatureHelp,
    SymbolInformation, SymbolKind, TextDocumentEdit, TextEdit, Url, WorkspaceEdit,
    WorkspaceSymbolResponse,
};

use crate::protocol::{
    CodeActionData, CompletionItemData, FoldRangeData, InlayHintData, LspReply, SemanticTokensData,
    SymbolData, WorkspaceEditData,
};

/// Reduce a `textDocument/foldingRange` reply to nxvim's whole-line spans: each
/// range's `[startLine, endLine]` (0-based, inclusive), dropping the optional
/// character columns and the `kind`. A range whose `endLine` precedes its
/// `startLine` (malformed) is skipped — the editor's fold model needs `end ≥ start`.
pub(crate) fn folding_ranges(ranges: Vec<FoldingRange>) -> Vec<FoldRangeData> {
    ranges
        .into_iter()
        .filter(|r| r.end_line >= r.start_line)
        .map(|r| FoldRangeData {
            start: r.start_line,
            end: r.end_line,
        })
        .collect()
}

/// Distill a `textDocument/codeAction` response (a mixed `(Command | CodeAction)[]`)
/// into the editor-facing list: a `CodeAction`'s `title` + normalized eager
/// `edit` + optional `command` (run via `workspace/executeCommand` after the
/// edit); a bare `Command` lands as a `command`-only entry (Phase 8).
pub(crate) fn code_actions(resp: CodeActionResponse) -> Vec<CodeActionData> {
    resp.into_iter()
        .map(|item| match item {
            CodeActionOrCommand::CodeAction(mut ca) => {
                let title = ca.title.clone();
                // Move the edit/command out rather than cloning (a `WorkspaceEdit`
                // is a deep tree); the `resolve` branch below only fires when both
                // are `None`, so the boxed original is unchanged by taking them.
                let command = ca.command.take();
                let edit = ca.edit.take().map(normalize_workspace_edit);
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
/// one. `None` degrades to an empty, complete list, so the editor uniformly sees
/// "no candidates" rather than a hang. The caller unwraps the transport result
/// (logging a failure as `None`) — see the module note — so this is a pure
/// transform shared by the async (native) and sync (wasm) dispatch paths.
pub(crate) fn completion_reply(resp: Option<CompletionResponse>) -> LspReply {
    let (is_incomplete, items) = match resp {
        Some(CompletionResponse::Array(items)) => (false, items),
        Some(CompletionResponse::List(list)) => (list.is_incomplete, list.items),
        None => (false, Vec::new()),
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
    let is_snippet = item.insert_text_format == Some(lsp_types::InsertTextFormat::SNIPPET);
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
        is_snippet,
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
/// panel isn't padded. `None` degrades to an empty list ("no information"), so the
/// editor never hangs waiting on a feature a server lacks. The caller unwraps the
/// transport result (logging a failure as `None`).
pub(crate) fn hover_reply(hover: Option<Hover>) -> LspReply {
    let hover = match hover {
        Some(hover) => hover,
        None => return LspReply::Hover(Vec::new()),
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
/// lines, dropping trailing blank lines so a panel isn't padded. The shared
/// distiller for every markup-to-lines reduction.
///
/// The lines are still **raw markdown** — HTML character references and all: the
/// downstream doc-float / docs-sidebar renders them through `nxvim_core::markdown`
/// (pulldown-cmark), which owns entity decoding (`&lt;`/`&amp;` → their chars,
/// `&nbsp;` → a non-breaking space that keeps a docstring's indentation without
/// turning it into a code block). Decoding here as well would double-process it and
/// re-interpret the escaped text as markup, so it is left to the renderer.
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
/// present, else the top-level one. `None`/no signatures degrades to a "no
/// signature help" (both fields `None`). The caller unwraps the transport result
/// (logging a failure as `None`).
pub(crate) fn signature_help_reply(help: Option<SignatureHelp>) -> LspReply {
    let none = LspReply::SignatureHelp {
        signature: None,
        active_parameter: None,
    };
    let help = match help {
        Some(help) => help,
        None => return none,
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
/// locations, collapsing the `LocationLink` shape to its selection target. The
/// caller unwraps the transport result (logging a failure as `None`).
pub(crate) fn goto_locations(resp: Option<GotoDefinitionResponse>) -> Vec<Location> {
    match resp {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(loc)) => vec![loc],
        Some(GotoDefinitionResponse::Array(locs)) => locs,
        Some(GotoDefinitionResponse::Link(links)) => links
            .into_iter()
            .map(|l| Location {
                uri: l.target_uri,
                range: l.target_selection_range,
            })
            .collect(),
    }
}

/// A human-readable label for a `SymbolKind` (the picker row's `[kind]` tag).
fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::FILE => "File",
        SymbolKind::MODULE => "Module",
        SymbolKind::NAMESPACE => "Namespace",
        SymbolKind::PACKAGE => "Package",
        SymbolKind::CLASS => "Class",
        SymbolKind::METHOD => "Method",
        SymbolKind::PROPERTY => "Property",
        SymbolKind::FIELD => "Field",
        SymbolKind::CONSTRUCTOR => "Constructor",
        SymbolKind::ENUM => "Enum",
        SymbolKind::INTERFACE => "Interface",
        SymbolKind::FUNCTION => "Function",
        SymbolKind::VARIABLE => "Variable",
        SymbolKind::CONSTANT => "Constant",
        SymbolKind::STRING => "String",
        SymbolKind::NUMBER => "Number",
        SymbolKind::BOOLEAN => "Boolean",
        SymbolKind::ARRAY => "Array",
        SymbolKind::OBJECT => "Object",
        SymbolKind::KEY => "Key",
        SymbolKind::NULL => "Null",
        SymbolKind::ENUM_MEMBER => "EnumMember",
        SymbolKind::STRUCT => "Struct",
        SymbolKind::EVENT => "Event",
        SymbolKind::OPERATOR => "Operator",
        SymbolKind::TYPE_PARAMETER => "TypeParameter",
        _ => "Symbol",
    }
}

/// Flatten a `textDocument/documentSymbol` reply into a name/kind/location list.
/// The flat `SymbolInformation` form carries its own `location`; the nested
/// `DocumentSymbol` tree has none (it is implicitly this document), so `doc_uri`
/// supplies it and the tree is walked depth-first (children included).
pub(crate) fn document_symbols(
    doc_uri: &Url,
    resp: Option<DocumentSymbolResponse>,
) -> Vec<SymbolData> {
    match resp {
        None => Vec::new(),
        Some(DocumentSymbolResponse::Flat(infos)) => {
            infos.into_iter().map(symbol_information_data).collect()
        }
        Some(DocumentSymbolResponse::Nested(syms)) => {
            let mut out = Vec::new();
            for sym in &syms {
                push_nested_symbol(doc_uri, sym, &mut out);
            }
            out
        }
    }
}

/// Depth-first walk of a nested `DocumentSymbol`, appending it and its children.
fn push_nested_symbol(doc_uri: &Url, sym: &DocumentSymbol, out: &mut Vec<SymbolData>) {
    out.push(SymbolData {
        name: sym.name.clone(),
        kind: symbol_kind_name(sym.kind).to_string(),
        location: Location {
            uri: doc_uri.clone(),
            range: sym.selection_range,
        },
    });
    if let Some(children) = &sym.children {
        for child in children {
            push_nested_symbol(doc_uri, child, out);
        }
    }
}

/// Flatten a `workspace/symbol` reply into the same name/kind/location list. The
/// flat `SymbolInformation` form maps directly; the newer `WorkspaceSymbol` form
/// may carry a resolve-only location (`uri` without a range), which collapses to
/// the file start.
pub(crate) fn workspace_symbols(resp: Option<WorkspaceSymbolResponse>) -> Vec<SymbolData> {
    match resp {
        None => Vec::new(),
        Some(WorkspaceSymbolResponse::Flat(infos)) => {
            infos.into_iter().map(symbol_information_data).collect()
        }
        Some(WorkspaceSymbolResponse::Nested(syms)) => syms
            .into_iter()
            .map(|sym| {
                let location = match sym.location {
                    OneOf::Left(loc) => loc,
                    OneOf::Right(workspace_location) => Location {
                        uri: workspace_location.uri,
                        range: lsp_types::Range::default(),
                    },
                };
                SymbolData {
                    name: sym.name,
                    kind: symbol_kind_name(sym.kind).to_string(),
                    location,
                }
            })
            .collect(),
    }
}

/// Map a flat `SymbolInformation` to [`SymbolData`] (its `location` is explicit).
fn symbol_information_data(info: SymbolInformation) -> SymbolData {
    SymbolData {
        name: info.name,
        kind: symbol_kind_name(info.kind).to_string(),
        location: info.location,
    }
}

/// Distill one protocol [`InlayHint`] to the editor's [`InlayHintData`]: its
/// anchor, the rendered label (the string form, or label parts joined to their
/// `value`s — the interactive per-part `location`/`tooltip` are dropped for
/// Phase 1), with `padding_left`/`padding_right` folded into a leading/trailing
/// space, and the kind as a small int (`1`=type, `2`=parameter, `0`=unset). Shared
/// by the async (native) and sync (wasm) dispatch paths.
pub(crate) fn inlay_hint(hint: &InlayHint) -> InlayHintData {
    let core = inlay_label_core(hint);
    let kind = match hint.kind {
        Some(InlayHintKind::TYPE) => 1,
        Some(InlayHintKind::PARAMETER) => 2,
        _ => 0,
    };
    // A hint with no usable label that the server marked resolvable (`data`) is
    // *lazy*: round-trip it verbatim so the editor can fill the label on demand
    // via `inlayHint/resolve` (Phase 2). An eager hint (label already present)
    // carries no resolve data — nothing to fetch.
    let resolve_data = if core.is_empty() && hint.data.is_some() {
        serde_json::to_value(hint).ok()
    } else {
        None
    };
    InlayHintData {
        line: hint.position.line,
        character: hint.position.character,
        label: pad_label(&core, hint),
        kind,
        resolve_data,
    }
}

/// The unpadded label string of an inlay hint: a `String` label verbatim, or the
/// label parts joined to their `value`s (the interactive per-part
/// `location`/`tooltip` are dropped — recorded as an approximation). Empty ⇒ a
/// lazy hint whose label arrives only via `inlayHint/resolve`.
pub(crate) fn inlay_label_core(hint: &InlayHint) -> String {
    match &hint.label {
        InlayHintLabel::String(s) => s.clone(),
        InlayHintLabel::LabelParts(parts) => parts.iter().map(|p| p.value.as_str()).collect(),
    }
}

/// Fold the hint's `padding_left`/`padding_right` into a leading/trailing space
/// around its `core` label — the inline form the editor paints between glyphs.
pub(crate) fn pad_label(core: &str, hint: &InlayHint) -> String {
    let pad_l = hint.padding_left.unwrap_or(false);
    let pad_r = hint.padding_right.unwrap_or(false);
    let mut out = String::with_capacity(core.len() + pad_l as usize + pad_r as usize);
    if pad_l {
        out.push(' ');
    }
    out.push_str(core);
    if pad_r {
        out.push(' ');
    }
    out
}

/// Distill an `inlayHint/resolve` reply (the resolved hint, or `None` on a
/// malformed/error/absent result) into [`LspReply::ResolvedInlayHint`]: its padded
/// label, or `None` when the resolved hint still carries no usable label (the
/// editor drops the placeholder rather than paint an empty hint — never a fake).
/// Shared by the async (native) and sync (wasm) dispatch paths.
pub(crate) fn resolved_inlay_hint(hint: Option<&InlayHint>) -> LspReply {
    let label = hint.and_then(|hint| {
        let core = inlay_label_core(hint);
        (!core.is_empty()).then(|| pad_label(&core, hint))
    });
    LspReply::ResolvedInlayHint { label }
}

/// Distill a `completionItem/resolve` reply (the resolved item, or `None` on a
/// malformed/error/absent result) into [`LspReply::ResolvedCompletion`]: its
/// `documentation` (markup → plain lines) and `detail`, both `None` when absent so
/// the editor leaves a docless item docless rather than fake a doc. Shared by the
/// async (native) and sync (wasm) dispatch paths.
pub(crate) fn resolved_completion(item: Option<CompletionItem>) -> LspReply {
    match item {
        Some(item) => LspReply::ResolvedCompletion {
            documentation: item.documentation.and_then(documentation_lines),
            detail: item.detail,
        },
        None => LspReply::ResolvedCompletion {
            documentation: None,
            detail: None,
        },
    }
}

/// The "server classified nothing" semantic-tokens reply: a full set with no
/// tokens and no `result_id`, so the editor clears its cache (and drops the
/// `result_id`, falling back to `full` on the next refresh) rather than guessing.
/// Used for `null`/error replies to both `full` and `full/delta`.
pub(crate) fn empty_semantic_tokens() -> SemanticTokensData {
    SemanticTokensData::Full {
        result_id: None,
        tokens: Vec::new(),
    }
}

/// Distill a `textDocument/semanticTokens/full` reply into [`SemanticTokensData`].
/// Both the full (`Tokens`) and streamed (`Partial`) shapes carry the same packed
/// `data`; only the full one has a `result_id` (the delta cursor — Phase 2). A
/// `None` result ⇒ no tokens. Shared by the async (native) and sync (wasm) paths.
pub(crate) fn semantic_tokens_full(resp: Option<SemanticTokensResult>) -> SemanticTokensData {
    match resp {
        Some(SemanticTokensResult::Tokens(t)) => SemanticTokensData::Full {
            result_id: t.result_id,
            tokens: t.data,
        },
        Some(SemanticTokensResult::Partial(p)) => SemanticTokensData::Full {
            result_id: None,
            tokens: p.data,
        },
        None => empty_semantic_tokens(),
    }
}

/// Distill a `textDocument/semanticTokens/full/delta` reply into
/// [`SemanticTokensData`]: a `TokensDelta` becomes an edit splice against our
/// `previousResultId`; a fresh full `Tokens` set is the transparent fallback when
/// the server couldn't honor the previous id (the editor replaces its cache rather
/// than patching it). A `None` result ⇒ no tokens. Shared by both dispatch paths.
pub(crate) fn semantic_tokens_delta_data(
    resp: Option<SemanticTokensFullDeltaResult>,
) -> SemanticTokensData {
    match resp {
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
    }
}
