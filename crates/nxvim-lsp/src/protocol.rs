//! The editor↔manager contract: the distilled, protocol-facing types the
//! [`LspManager`](crate::LspManager) exchanges with the editor.
//!
//! Everything here is plain data. The editor speaks these (already in LSP
//! coordinates — the server owns byte↔position conversion, never the manager),
//! and the manager ferries them to/from the per-server `async-lsp` loops. The
//! protocol's many response shapes are reduced to these before they cross back,
//! so the JSON layer (`async-lsp`, `lsp-types`) never leaks past the manager.

use std::path::PathBuf;

use lsp_types::{
    CodeAction, Diagnostic, Location, Position, Range, TextDocumentContentChangeEvent,
    TextDocumentSyncKind, TextEdit, Url,
};

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

/// A [`WorkspaceEdit`](lsp_types::WorkspaceEdit) normalized to per-document text
/// edits: the protocol's `changes` map and the versioned `documentChanges` (with
/// `OneOf`/annotation edits, and edit-vs-resource operations) both collapse to one
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
