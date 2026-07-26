//! The editor↔manager contract: the distilled, protocol-facing types the
//! [`LspManager`](crate::LspManager) exchanges with the editor.
//!
//! Everything here is plain data. The editor speaks these (already in LSP
//! coordinates — the server owns byte↔position conversion, never the manager),
//! and the manager ferries them to/from the per-server `async-lsp` loops. The
//! protocol's many response shapes are reduced to these before they cross back,
//! so the JSON layer (`async-lsp`, `lsp-types`) never leaks past the manager.

use std::collections::HashMap;
use std::path::PathBuf;

use lsp_types::{
    CodeAction, Diagnostic, Location, Position, Range, SemanticToken, SemanticTokensEdit,
    TextDocumentContentChangeEvent, TextDocumentSyncKind, TextEdit, Url,
};

/// Identifies one language server instance: a `(name, workspace-root)` pair.
/// `name` is the user-chosen LSP config name (the `nx.lsp` control surface in the
/// lua crate), arbitrary rather than a fixed filetype. nxvim runs at most one child
/// per key and routes a buffer to its server by it.
///
/// `Ord` is derived so a buffer's servers can live in a `BTreeMap` and iterate in a
/// stable, human-meaningful order — by config `name`, then root. That ordering is
/// load-bearing: it is what makes "the first server that advertises this feature"
/// a deterministic choice rather than a hash-order coin flip.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Deep-merged OVER nxvim's base `client_capabilities` at `initialize`.
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
    /// The `semanticTokensProvider.legend` (`tokenTypes` / `tokenModifiers` as
    /// plain strings), needed to decode the packed token data — the integer
    /// `tokenType` / `tokenModifiers` indices a `semanticTokens/full` reply
    /// carries are positions into these arrays. `None` when the server advertises
    /// no semantic-tokens provider (the feature stays off for its buffers).
    pub legend: Option<SemanticLegend>,
    /// Whether the server advertised `semanticTokensProvider.full.delta` — it
    /// answers `semanticTokens/full/delta` with diffs. The editor only sends the
    /// delta request to a server that advertised it; otherwise every refresh re-
    /// requests the whole `full` set (Phase 2). Meaningless without a `legend`.
    pub semantic_tokens_delta: bool,
}

/// The `semanticTokensProvider.legend`: the ordered token-type and
/// token-modifier name arrays a server publishes at `initialize`. The decode maps
/// a token's `token_type` index into `token_types` and each set bit of its
/// `token_modifiers_bitset` into `token_modifiers`, yielding the names that build
/// the `@lsp.type.*` / `@lsp.typemod.*` highlight groups.
#[derive(Clone, Debug, Default)]
pub struct SemanticLegend {
    pub token_types: Vec<String>,
    pub token_modifiers: Vec<String>,
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
    /// The characters the server advertises as `signatureHelpProvider.{trigger,
    /// retrigger}Characters` (typically `(` and `,`) — what an opt-in auto-trigger
    /// fires signature help on. Empty when the server sends none (or no signature
    /// help at all), so the auto-trigger stays off without a real signal to react to.
    pub signature_trigger_chars: Vec<String>,
    pub completion: bool,
    pub document_formatting: bool,
    pub rename: bool,
    pub code_action: bool,
    pub semantic_tokens: bool,
    pub inlay_hints: bool,
    pub folding_range: bool,
    /// `textDocument/documentSymbol` and `workspace/symbol`. Both are **fan-out**
    /// kinds — every capable server on the buffer is asked and the results merge —
    /// which is exactly why they need a flag: an unmodelled provider makes the
    /// routing predicate fail *open*, so a server that never advertised the method
    /// is asked anyway, once per invocation, and whatever it answers joins the list.
    pub document_symbol: bool,
    pub workspace_symbol: bool,
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
    /// (`apply_dyn_notify`); an unknown one is logged and dropped.
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
    /// `textDocument/documentSymbol` — the symbols defined in one document. The
    /// reply ([`LspReply::Symbols`]) is flattened to a name/kind/location list the
    /// editor opens in `nx.picker`. Position-less (whole document); the `uri` is the
    /// requesting buffer.
    DocumentSymbol {
        uri: Url,
    },
    /// `workspace/symbol` — symbols across the workspace matching `query` (the
    /// fuzzy text the user typed at the prompt). The reply is the same flattened
    /// [`LspReply::Symbols`] list.
    WorkspaceSymbol {
        query: String,
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
    /// `tab_size` / `insert_spaces` carry the requesting buffer's `tabstop` /
    /// `expandtab` so the server formats to the buffer's indentation.
    Formatting {
        uri: Url,
        tab_size: u32,
        insert_spaces: bool,
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
        /// The caller's kind filter (`nx.lsp.code_action{ context = { only = … } }`),
        /// sent as the request's `context.only`. Empty ⇒ omitted (every kind).
        /// Honoring it is a *should* in the protocol, so the editor filters the reply
        /// by [`CodeActionData::kind`] as well — this is the hint, not the guarantee.
        only: Vec<String>,
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
    /// manager deserializes it back to a [`CompletionItem`](lsp_types::CompletionItem)
    /// to send, because a server matches the resolve against the exact item it
    /// issued (rust_analyzer keys on the `data` blob). Mirrors
    /// [`LspRequest::ResolveCodeAction`].
    ResolveCompletion {
        item: serde_json::Value,
    },
    /// `textDocument/semanticTokens/full` — the whole-buffer token set (ADR 0001
    /// bridge #2). Unlike the cursor-anchored features, this is requested per
    /// *buffer* (on open and after each change), and the reply
    /// ([`LspReply::SemanticTokens`]) carries the packed token array the editor
    /// decodes against the server's legend + encoding and projects over the
    /// treesitter floor.
    SemanticTokensFull {
        uri: Url,
    },
    /// `textDocument/semanticTokens/full/delta` — the *diff* from a prior token set
    /// (Phase 2). Sent in place of `SemanticTokensFull` once a buffer has cached a
    /// `result_id` (the delta cursor): the server replies with edits to splice into
    /// the cached array ([`SemanticTokensData::Delta`]) instead of the whole set,
    /// shrinking the per-edit wire payload. A server that can't honor the
    /// `previous_result_id` may instead reply with a fresh full set
    /// ([`SemanticTokensData::Full`]) — the transparent fallback the editor applies
    /// by replacing the cache.
    SemanticTokensDelta {
        uri: Url,
        previous_result_id: String,
    },
    /// `textDocument/inlayHint` — the inline type/parameter hints for a document
    /// `range` (we send the whole buffer, `0..line_count`). Requested per buffer
    /// (on enable and after each change, only while the buffer has inlay hints
    /// enabled), and the reply ([`LspReply::InlayHints`]) carries the distilled
    /// hints the editor decodes against the negotiated encoding and paints inline
    /// over the buffer text.
    InlayHint {
        uri: Url,
        range: Range,
    },
    /// `textDocument/foldingRange` — the foldable line ranges of a whole document
    /// (the LSP fold source). Position-less like `documentSymbol`; requested per
    /// buffer (on open and after each change) while the buffer's `foldmethod=expr`
    /// resolves to the LSP foldexpr marker, and the reply ([`LspReply::Folds`])
    /// carries the line spans the editor pushes into its fold engine.
    FoldingRange {
        uri: Url,
    },
    /// `inlayHint/resolve` — fill a **lazy** hint's `label`/`tooltip` on demand
    /// (Phase 2). The original [`InlayHint`](lsp_types::InlayHint) is round-tripped
    /// verbatim as JSON (kept on [`InlayHintData::resolve_data`]); the manager
    /// deserializes it back to send, and the reply ([`LspReply::ResolvedInlayHint`])
    /// carries the resolved label. Mirrors [`LspRequest::ResolveCompletion`].
    ResolveInlayHint {
        hint: serde_json::Value,
    },
    /// A **generic** request issued by Lua `client:request(method, params, …)`
    /// (Phase 5): an arbitrary `method` with raw JSON `params`, whose raw JSON
    /// result routes back to a Lua handler ([`LspReply::Raw`]). This is the seam
    /// the config `handlers` and server-specific commands (`:LspCargoReload`,
    /// `switchSourceHeader`, organize-imports) build on, distinct from the typed
    /// native features above. The `method` must be in the dispatch table
    /// (`issue_dyn_request`); an unknown one fails loud rather than no-op.
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
    /// The Lua deferred-callback id ([`nx._cb_fns`]) a generic
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
    /// Symbols from `textDocument/documentSymbol` or `workspace/symbol` — the
    /// nested `DocumentSymbol` tree and the flat `SymbolInformation`/`WorkspaceSymbol`
    /// shapes all flattened to a name/kind/location list (empty ⇒ none found).
    Symbols(Vec<SymbolData>),
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
    /// The semantic tokens from `textDocument/semanticTokens/full` or
    /// `full/delta` (ADR 0001 bridge #2). The token data stays in the protocol's
    /// *packed* form (already-deserialized 5-field [`SemanticToken`] deltas) — the
    /// editor decodes it against the server's legend + negotiated encoding into
    /// per-line byte spans, because only the editor holds the buffer text the
    /// char→byte conversion needs (like the completion edit ranges). See
    /// [`SemanticTokensData`] for the full-vs-delta shapes.
    SemanticTokens(SemanticTokensData),
    /// The inlay hints from `textDocument/inlayHint`, distilled to the inline
    /// decorations the editor paints (positions still in the negotiated encoding
    /// for the editor to convert to bytes — only it holds the buffer text). An
    /// empty list ⇒ the server produced no hints for the range.
    InlayHints(Vec<InlayHintData>),
    /// The label an `inlayHint/resolve` produced for a lazy hint (`None` ⇒ the
    /// resolved hint still carried no label, was malformed, or the request failed —
    /// the editor then drops the placeholder rather than painting an empty hint).
    ResolvedInlayHint { label: Option<String> },
    /// The folding ranges from `textDocument/foldingRange` — each reduced to a
    /// 0-based inclusive `[start, end]` line span ([`FoldRangeData`]; the optional
    /// character columns and the `kind` are dropped, nxvim folds whole lines). An
    /// empty list ⇒ the server found nothing foldable. The editor pushes these into
    /// its fold engine as the LSP fold source.
    Folds(Vec<FoldRangeData>),
    /// The reply to an [`LspRequest::Raw`] (Phase 5): the server's raw JSON result
    /// (`Ok`) or an error message (`Err`) — an unsupported method, a transport
    /// failure, or the server replying an error. Routed back to the Lua handler as
    /// `(err, result)`, bypassing the editor-feature staleness machinery the typed
    /// replies use (a config command's reply always fires its handler).
    Raw(Result<serde_json::Value, String>),
}

/// A `textDocument/semanticTokens/full` or `full/delta` reply distilled for the
/// editor, decoded editor-side ([`crate::lsp::semantic`](../../nxvim-server))
/// against the negotiated legend + encoding. The `result_id` (when present) is the
/// delta cursor a later refresh quotes as `previousResultId`.
#[derive(Clone, Debug)]
pub enum SemanticTokensData {
    /// A whole token set — from `semanticTokens/full`, or a `full/delta` the server
    /// answered with a fresh full set (it couldn't, or chose not to, diff against
    /// the quoted `previousResultId`). Replaces the editor's cached token array
    /// wholesale; an empty `tokens` ⇒ the server classified nothing (the buffer
    /// falls back to the treesitter floor).
    Full {
        result_id: Option<String>,
        tokens: Vec<SemanticToken>,
    },
    /// Splice edits from `full/delta`: each [`SemanticTokensEdit`]'s
    /// `start`/`delete_count` index the *flat integer* array of the previously
    /// cached token set, and `data` is the replacement tokens. The editor patches
    /// its cache and re-decodes. An empty `edits` ⇒ nothing changed (the cache,
    /// and so the paint, is left as-is — distinct from an empty [`Self::Full`],
    /// which clears it).
    Delta {
        result_id: Option<String>,
        edits: Vec<SemanticTokensEdit>,
    },
}

/// A [`WorkspaceEdit`](lsp_types::WorkspaceEdit) normalized to an **ordered** list
/// of [`WorkspaceChange`]s: the protocol's `changes` map and the versioned
/// `documentChanges` (with its `OneOf`/annotation edits and its interleaved
/// edit-vs-resource operations) both collapse to this one shape. Ranges stay in the
/// negotiated encoding for the editor to convert, like the goto/completion
/// normalizations.
///
/// Order is load-bearing and therefore preserved: `documentChanges` must be applied
/// in sequence, and a refactor that extracts to a new file sends `create <uri>`
/// *before* the edits that fill it (gopls's `extract_to_new_file` is the worked
/// example).
#[derive(Clone, Debug, Default)]
pub struct WorkspaceEditData {
    /// The changes, in the order the server listed them.
    pub changes: Vec<WorkspaceChange>,
    /// The edit's `changeAnnotations`, by id — the labels a change points at with
    /// its [`annotation`](WorkspaceChange::annotation). Empty for the vast majority
    /// of edits (only a server that declares annotations sends any), and the editor
    /// only *has* to read them for the ones that ask to be confirmed.
    pub annotations: HashMap<String, ChangeAnnotationData>,
}

impl WorkspaceEditData {
    /// A plain edit: changes with no annotations at all (what a `changes`-map
    /// `WorkspaceEdit`, and every internally-built edit, produces).
    pub fn plain(changes: Vec<WorkspaceChange>) -> Self {
        Self {
            changes,
            annotations: HashMap::new(),
        }
    }

    /// The annotation ids this edit's changes actually reference **and** that ask the
    /// user first (`needsConfirmation`), in first-referenced order — an annotation
    /// declared but never pointed at asks nothing. This is the whole confirm gate:
    /// empty ⇒ apply straight away.
    pub fn confirmable(&self) -> Vec<String> {
        let mut seen: Vec<String> = Vec::new();
        for change in &self.changes {
            let Some(id) = change.annotation() else {
                continue;
            };
            if seen.iter().any(|s| s == id) {
                continue;
            }
            if self
                .annotations
                .get(id)
                .is_some_and(|a| a.needs_confirmation)
            {
                seen.push(id.to_string());
            }
        }
        seen
    }
}

/// One `changeAnnotation` of a [`WorkspaceEdit`](lsp_types::WorkspaceEdit): the
/// human-readable name for a group of changes, and whether the client must ask
/// before applying that group. A server uses them to separate the safe part of a
/// refactor from the part you should look at — "Rename symbol" alongside "Update
/// string literals", the second wanting a yes/no.
#[derive(Clone, Debug)]
pub struct ChangeAnnotationData {
    /// Short human-readable name, shown as the question.
    pub label: String,
    /// Optional longer text, shown with the label.
    pub description: Option<String>,
    /// Whether the user must confirm this group before it applies.
    pub needs_confirmation: bool,
}

/// One step of a normalized [`WorkspaceEditData`]: either text edits for a document
/// or a file resource operation. The `changes`-map form of a `WorkspaceEdit` yields
/// only [`Edits`](WorkspaceChange::Edits); `documentChanges` can yield any variant.
///
/// The option flags each resource op carries are kept (rather than flattened to a
/// bare path pair) because they decide what happens when the target already exists —
/// dropping them would silently overwrite a file the server asked us to leave alone.
/// Each variant carries the `annotationId` the server tagged it with, if any — the
/// key into [`WorkspaceEditData::annotations`], and so the group a confirmation
/// accepts or declines as a whole.
#[derive(Clone, Debug)]
pub enum WorkspaceChange {
    /// Text edits for one document, in the order the server listed them. A
    /// `documentChanges` edit may annotate each `TextEdit` individually; when it
    /// annotates several differently, the document's edits split into one change per
    /// annotation so a declined group takes only its own edits with it.
    Edits {
        uri: Url,
        edits: Vec<TextEdit>,
        annotation: Option<String>,
    },
    /// Create a file. `overwrite` truncates an existing file; `ignore_if_exists`
    /// leaves it alone (overwrite wins when both are set, per the protocol). A URI
    /// ending in `/` names a **directory** to create rather than a file.
    Create {
        uri: Url,
        overwrite: bool,
        ignore_if_exists: bool,
        annotation: Option<String>,
    },
    /// Move a file. Same overwrite/ignore semantics as [`Create`](Self::Create),
    /// against the *destination*.
    Rename {
        old_uri: Url,
        new_uri: Url,
        overwrite: bool,
        ignore_if_exists: bool,
        annotation: Option<String>,
    },
    /// Delete a file (`recursive` ⇒ a directory and its contents).
    Delete {
        uri: Url,
        recursive: bool,
        ignore_if_not_exists: bool,
        annotation: Option<String>,
    },
}

impl WorkspaceChange {
    /// The `changeAnnotation` id this change belongs to, if the server tagged it.
    pub fn annotation(&self) -> Option<&str> {
        match self {
            Self::Edits { annotation, .. }
            | Self::Create { annotation, .. }
            | Self::Rename { annotation, .. }
            | Self::Delete { annotation, .. } => annotation.as_deref(),
        }
    }
}

/// The editor's answer to a server→client `workspace/applyEdit` — the
/// `ApplyWorkspaceEditResponse` body, minted editor-side and framed by whichever
/// client owns the request ([`crate::client`] natively, [`crate::sync_client`] on
/// wasm). `applied` is the *real* outcome, never an optimistic ack: a server that
/// asked for something the editor could not do (an unresolvable URI, a resource op
/// it does not implement) learns so, with `failure_reason` naming it.
#[derive(Clone, Debug, Default)]
pub struct ApplyEditOutcome {
    pub applied: bool,
    pub failure_reason: Option<String>,
    /// Which change of the edit failed, as its index in `documentChanges` — meaningful
    /// to a server precisely because nxvim declares a `failureHandling` strategy
    /// (`abort`), so "everything before this index was applied, nothing after it was"
    /// is a statement it can act on. `None` when nothing failed, or when the failure
    /// isn't attributable to one change.
    pub failed_change: Option<u32>,
}

/// One symbol distilled for the editor (`textDocument/documentSymbol` /
/// `workspace/symbol`): its `name`, a human-readable `kind` label
/// (`"Function"`, `"Struct"`, …), and the `location` to jump to. The nested
/// `DocumentSymbol` tree is flattened depth-first into a list of these; the flat
/// `SymbolInformation` / `WorkspaceSymbol` shapes map one-to-one.
#[derive(Clone, Debug)]
pub struct SymbolData {
    pub name: String,
    pub kind: String,
    pub location: Location,
}

/// One folding range from `textDocument/foldingRange`, reduced to the inclusive
/// 0-based buffer-line span nxvim folds. The protocol's `startCharacter` /
/// `endCharacter` and the semantic `kind` (comment/imports/region) are dropped —
/// nxvim's fold model is whole-line, and the line span is all its containment →
/// level builder needs.
#[derive(Clone, Copy, Debug)]
pub struct FoldRangeData {
    /// First folded line (0-based).
    pub start: u32,
    /// Last folded line (0-based, inclusive).
    pub end: u32,
}

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
    /// The action's `kind` (`"quickfix"`, `"source.fixAll.ruff"`, …), verbatim from the
    /// server. Carried so the editor can apply the caller's `only` filter itself rather
    /// than trusting the server to have honored `context.only`. `None` for a bare
    /// `Command` action, and for a `CodeAction` that declared no kind — neither can
    /// match a filter.
    pub kind: Option<String>,
    pub edit: Option<WorkspaceEditData>,
    pub resolve: Option<Box<CodeAction>>,
    pub command: Option<lsp_types::Command>,
}

/// One inlay hint distilled for the editor: its anchor `(line, character)` (the
/// character still in the server's negotiated position encoding — the editor
/// converts it to a byte column against the buffer text), the rendered `label`
/// (the string form, or label parts joined to their `value`s, with `padding_left`
/// / `padding_right` already folded into a leading/trailing space), and the
/// `kind` (`1`=type, `2`=parameter, `0`=unspecified) the editor maps to a
/// highlight group. The interactive extras (per-part `location`/`tooltip`,
/// `tooltip`, `textEdits`) are dropped in Phase 1 — recorded as an approximation.
#[derive(Clone, Debug)]
pub struct InlayHintData {
    pub line: u32,
    pub character: u32,
    pub label: String,
    pub kind: u8,
    /// The original protocol [`InlayHint`](lsp_types::InlayHint) serialized
    /// verbatim, round-tripped to `inlayHint/resolve` (Phase 2) to fill a **lazy**
    /// hint's `label`. `Some` only when the hint arrived with no usable label *and*
    /// carried `data` (a server marked it resolvable); `None` for an eager hint
    /// whose label is already present (nothing to resolve). Mirrors
    /// [`CompletionItemData::resolve_data`].
    pub resolve_data: Option<serde_json::Value>,
}

/// One completion candidate, distilled from a protocol
/// [`CompletionItem`](lsp_types::CompletionItem) to the fields the editor's menu
/// needs. `kind` is the `CompletionItemKind` as a small int (`0` = unspecified)
/// the client maps to an icon. `text_edit` and `additional_text_edits` keep their
/// ranges in the negotiated position encoding; the editor converts them to byte
/// ranges on accept (only it has the buffer text). When `text_edit` is `None`, the
/// editor replaces the completion word (the identifier run left of the cursor)
/// with `insert_text` (else `label`).
#[derive(Clone, Debug)]
pub struct CompletionItemData {
    pub label: String,
    pub kind: u8,
    pub detail: Option<String>,
    pub filter_text: Option<String>,
    pub sort_text: Option<String>,
    pub insert_text: Option<String>,
    /// Whether `insert_text` / `text_edit.new_text` is an LSP **snippet** body
    /// (`insertTextFormat == 2`) rather than plain text — `$1` / `${1:default}` /
    /// `$0` tabstops to expand through the native snippet engine, rather than inserting
    /// the markers literally. `false` for plain text (the default / format `1`).
    pub is_snippet: bool,
    pub text_edit: Option<TextEdit>,
    pub additional_text_edits: Vec<TextEdit>,
    /// The item's `documentation` (a plain string or `MarkupContent`) reduced to
    /// plain display lines joined by `\n` (markdown is not styled — same as hover),
    /// with trailing blank lines trimmed. `None` ⇒ the item carried no
    /// documentation *inline*; many servers (rust_analyzer especially) send it only
    /// on `completionItem/resolve` (Phase 2), keyed by [`Self::resolve_data`].
    pub documentation: Option<String>,
    /// The original protocol [`CompletionItem`](lsp_types::CompletionItem)
    /// serialized verbatim, round-tripped to `completionItem/resolve` in Phase 2 to
    /// fetch the lazy `documentation`/`detail`. The whole item (not just its `data`
    /// blob) is kept because a server matches the resolve against the exact item it
    /// issued — rust_analyzer rejects a resolve whose `data` it didn't send. `None`
    /// only if the item somehow failed to serialize (it always should). Mirrors
    /// [`CodeActionData::resolve`] round-tripping the original [`CodeAction`].
    pub resolve_data: Option<serde_json::Value>,
}

impl CompletionItemData {
    /// The human-readable name of this item's `CompletionItemKind` for the popup's
    /// kind column — `1`→`"Text"`, `3`→`"Function"`, `15`→`"Snippet"`, … matching the
    /// LSP enum. `None` for `0` (unspecified — the server sent no kind) so the row
    /// renders no kind label rather than a bogus one.
    pub fn kind_label(&self) -> Option<&'static str> {
        Some(match self.kind {
            1 => "Text",
            2 => "Method",
            3 => "Function",
            4 => "Constructor",
            5 => "Field",
            6 => "Variable",
            7 => "Class",
            8 => "Interface",
            9 => "Module",
            10 => "Property",
            11 => "Unit",
            12 => "Value",
            13 => "Enum",
            14 => "Keyword",
            15 => "Snippet",
            16 => "Color",
            17 => "File",
            18 => "Reference",
            19 => "Folder",
            20 => "EnumMember",
            21 => "Constant",
            22 => "Struct",
            23 => "Event",
            24 => "Operator",
            25 => "TypeParameter",
            _ => return None,
        })
    }
}

/// Server → editor, delivered to the main loop's `select!`. The distilled events
/// the editor acts on; lifecycle/framing/id-correlation are handled inside the
/// manager by `async-lsp` and never surface here.
#[derive(Debug)]
pub enum LspEvent {
    /// A server completed (or re-completed, after a respawn) its handshake. The
    /// editor records the encoding/caps and re-`didOpen`s its buffers — this
    /// doubles as the restart signal (a respawned server re-handshakes).
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
    /// A server→client refresh request (`workspace/inlayHint/refresh` or
    /// `workspace/semanticTokens/refresh`): the server recomputed and is asking the
    /// client to re-query the decorations for *all* the documents it serves. Many
    /// servers (lua_ls, gopls) compute these asynchronously and only have results
    /// to give *after* sending a refresh, so without honoring it a buffer's hints /
    /// tokens never appear. The editor re-issues the matching whole-buffer request
    /// for every buffer this server owns.
    WorkspaceRefresh { key: ServerKey, kind: RefreshKind },
    /// A server→client `workspace/applyEdit`: the server is asking the *editor* to
    /// apply a `WorkspaceEdit` it authored. This is how a refactor delivered as a
    /// `command` lands — the `workspace/executeCommand` reply carries no edit, the
    /// server pushes one back instead (gopls's `extract_to_new_file`, ts_ls's
    /// move-to-file, rust-analyzer's structural search-replace, …).
    ///
    /// Unlike every other event this one is a **request**: the server is blocked on
    /// the answer, so the editor must answer it exactly once — via
    /// `HostEffects::lsp_apply_edit_response(key, id, outcome)` — whatever the
    /// outcome. `id` is minted by the client that received the request and is
    /// meaningful only to it (it stands in for the JSON-RPC request id, whose shape
    /// differs between the async and sync clients).
    ApplyEdit {
        key: ServerKey,
        id: u64,
        /// The server's label for the edit (`"Extract to new file"`), for the
        /// user-facing message; `None` when it sent none.
        label: Option<String>,
        changes: WorkspaceEditData,
    },
}

/// Which decoration a [`LspEvent::WorkspaceRefresh`] asks the editor to re-query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshKind {
    InlayHint,
    SemanticTokens,
}
