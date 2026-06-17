//! Server-side LSP integration: the `treesitter.rs` analogue for language
//! servers.
//!
//! Where `nxvim-lsp` owns the client machinery (spawning/supervising servers and
//! the JSON-RPC bridge), this module owns the *editor* half. It is split by
//! concern across submodules: [`sync`] (document sync + server lifecycle),
//! [`diagnostics`], [`request`] (language-feature requests/replies),
//! [`completion`] (the insert-mode popup), and [`edit`] (buffer-mutating
//! features). This file holds the shared types, the request-kind enum, and the
//! pure conversion/formatting helpers the submodules draw on.
//!
//! All [`EditHost`] methods here run on the single editor thread and only ever
//! hand the manager fire-and-forget notifications, so a slow or hung server can
//! never stall keystroke->buffer->redraw.

use std::path::{Path, PathBuf};

use nxvim_core::unicode;
use nxvim_core::{Buffer, BufferEdit, BufferId};
use std::collections::BTreeMap;

use nxvim_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Position, Range, SemanticToken, TextDocumentContentChangeEvent,
    TextDocumentSyncKind, Url,
};
use nxvim_lsp::{PositionEncoding, ProviderCaps, SemanticLegend, ServerKey, ServerSpawn};
use nxvim_lua::{DiagnosticData, LspServerCapabilities};

mod completion;
pub(crate) use completion::{complete_doc_lines, LspComplete};
mod diagnostics;
mod edit;
mod inlay;
mod request;
mod semantic;
mod sync;

/// Per-buffer LSP document-sync bookkeeping, mirroring `SyntaxState`. One per
/// open buffer that mapped to a configured server, keyed by [`BufferId`] in
/// [`EditHost::lsp_states`].
#[derive(Default)]
pub(crate) struct LspDocState {
    /// Which server owns this buffer (`None` until a `vim.lsp.start` binds one).
    server: Option<ServerKey>,
    /// The LSP `languageId` for `didOpen` — the buffer's filetype, set when the
    /// `vim.lsp.enable` dispatcher binds the buffer (no longer re-derived in sync).
    language_id: String,
    /// The document URI, kept so `didClose` can be sent after the buffer is gone.
    uri: Option<Url>,
    /// Has `didOpen` been sent for the current server instance?
    opened: bool,
    /// LSP document version (monotonic, bumped per `didChange`).
    version: i32,
    /// `changedtick` of the last sync we sent (drives `didChange`).
    last_tick: u64,
    /// `save_tick` of the last sync, mirrored to fire `didSave` exactly when the
    /// buffer is written (`save_tick` bumps only on a successful `:w`).
    last_save_tick: u64,
    /// Latest `publishDiagnostics` for this buffer, projected into the redraw
    /// (`diagnostics_for`) and the under-cursor message line.
    diagnostics: Vec<Diagnostic>,
    /// Latest `semanticTokens/full` result for this buffer, decoded into the
    /// per-line highlight spans `highlights_for` merges over the treesitter floor
    /// (ADR 0001 bridge #2). Empty until the first reply lands.
    semantic: SemanticTokensCache,
    /// Per-buffer semantic-token override (Phase 3): `None` is the auto default
    /// (enabled when the server advertises a legend); `Some(false)` is an explicit
    /// `vim.lsp.semantic_tokens.stop` (hide the paint, skip refreshes);
    /// `Some(true)` an explicit `start`. The cache survives a stop, so a later
    /// `start` repaints from it without a round-trip.
    semantic_enabled: Option<bool>,
    /// Latest `textDocument/inlayHint` result for this buffer, decoded into the
    /// per-line inline hints `inlay_hints_for` projects over the buffer text. Empty
    /// until the first reply lands (and while inlay hints are disabled).
    inlay: InlayHintsCache,
    /// Whether inlay hints are enabled for this buffer — **off by default** (unlike
    /// semantic tokens, neovim's inlay hints are opt-in via
    /// `vim.lsp.inlay_hint.enable`). The projection and the refresh request both
    /// gate on this; toggling it off clears the cache (no surviving paint).
    inlay_enabled: bool,
}

impl LspDocState {
    /// Whether the semantic-token projection is active for this buffer: the auto
    /// default (on) unless explicitly stopped via `vim.lsp.semantic_tokens.stop`.
    /// The editor-wide gate ([`EditHost::semantic_tokens_enabled`]) is checked
    /// separately by the projection.
    pub(crate) fn semantic_on(&self) -> bool {
        self.semantic_enabled.unwrap_or(true)
    }
}

/// A buffer's decoded semantic tokens, plus the bookkeeping a `full/delta`
/// refresh (Phase 2) needs. The decoded `spans` are what the projection reads;
/// `result_id` is the delta cursor the next request quotes.
#[derive(Default)]
pub(crate) struct SemanticTokensCache {
    /// The server's `resultId` for the current token set, quoted as
    /// `previousResultId` by the next `full/delta` request. `None` until the first
    /// reply, or when the server sends none — in which case the next refresh falls
    /// back to a whole `full` request.
    pub(crate) result_id: Option<String>,
    /// The raw packed token set the current `spans` were decoded from, kept so a
    /// `full/delta` reply can splice its edits into it and re-decode (Phase 2). The
    /// LSP delta edits index this array's *flat integer* form. Empty until the first
    /// reply.
    pub(crate) tokens: Vec<SemanticToken>,
    /// Decoded tokens bucketed by 0-based buffer line — the same per-line shape
    /// the syntax engine caches, so projection mirrors `highlights_for`. Each span
    /// is line-local byte offsets plus the candidate `@lsp.*` capture names (most
    /// specific first) the projection resolves at paint time.
    pub(crate) spans: BTreeMap<usize, Vec<SemanticSpan>>,
}

/// One decoded semantic token on a single buffer line: a line-local `[start, end)`
/// byte span and the candidate highlight-capture names it could paint as, ordered
/// most-specific first (`lsp.typemod.<type>.<mod>` … `lsp.type.<type>`). The
/// projection resolves the first candidate that maps to a style and **drops the
/// token entirely if none do**, so an undefined `@lsp.*` group never blanks the
/// treesitter span beneath it.
pub(crate) struct SemanticSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) groups: Vec<String>,
    /// The legend token-type name (e.g. `"function"`), kept for the
    /// `vim.lsp.semantic_tokens.get_at_pos` mirror — the projection uses `groups`.
    pub(crate) ty: String,
    /// The active modifier names (legend order), for the same mirror.
    pub(crate) mods: Vec<String>,
}

/// A buffer's decoded inlay hints, bucketed by 0-based buffer line — the same
/// per-line shape the diagnostics/semantic caches use, so the projection mirrors
/// them. Empty until the first reply (and while inlay hints are disabled, which
/// clears it).
#[derive(Default)]
pub(crate) struct InlayHintsCache {
    pub(crate) hints: BTreeMap<usize, Vec<InlayHintSpan>>,
}

/// One decoded inlay hint on a single buffer line: the line-local `byte_col` it
/// anchors at (the LSP character already converted through the negotiated
/// encoding), the rendered `text` (label with padding folded in), and the `kind`
/// (`1`=type, `2`=parameter, `0`=unspecified) for the highlight group. The client
/// inserts `text` at this column, shifting the real glyphs right.
pub(crate) struct InlayHintSpan {
    pub(crate) byte_col: usize,
    pub(crate) text: String,
    #[allow(dead_code)] // kind drives the group split in a later phase; one group today.
    pub(crate) kind: u8,
    /// For a **lazy** hint (one that arrived with no label but carried `data`): the
    /// original hint JSON, round-tripped to `inlayHint/resolve` to fill `text` on
    /// demand. A placeholder span carries an empty `text` until its resolve lands;
    /// the projection and the `get` mirror both skip empty-`text` spans, so an
    /// unresolved placeholder paints nothing. `None` for an eager hint.
    pub(crate) resolve_data: Option<nxvim_lsp::serde_json::Value>,
}

/// An in-flight `inlayHint/resolve`, keyed by the `cb_id` its [`ReqToken`] carries
/// (so concurrent resolves don't clobber each other in the single-slot
/// `lsp_requests` kind-map — they route by `cb_id` like a generic `client:request`).
/// Records where the resolved label lands: the issuing buffer, the `tick` it was
/// issued against (the reply is dropped if the buffer changed since), and the
/// `(line, idx)` of the placeholder span in [`InlayHintsCache::hints`] to fill.
pub(crate) struct InlayResolveTarget {
    pub(crate) buffer: BufferId,
    pub(crate) tick: u64,
    pub(crate) line: usize,
    pub(crate) idx: usize,
}

/// Which language-feature request a [`ReqToken`] / [`PendingLspReq`] belongs to.
/// The numeric mapping is what rides in the token's `kind` field across the
/// manager and back; the editor owns its meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum LspReqKind {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
    References,
    Hover,
    SignatureHelp,
    Completion,
    Formatting,
    Rename,
    CodeAction,
    ResolveCodeAction,
    CompletionResolve,
    SemanticTokens,
    InlayHints,
    ResolveInlayHint,
    DocumentSymbol,
    WorkspaceSymbol,
}

impl LspReqKind {
    pub(crate) fn as_u16(self) -> u16 {
        match self {
            LspReqKind::Definition => 0,
            LspReqKind::Declaration => 1,
            LspReqKind::TypeDefinition => 2,
            LspReqKind::Implementation => 3,
            LspReqKind::References => 4,
            LspReqKind::Hover => 5,
            LspReqKind::SignatureHelp => 6,
            LspReqKind::Completion => 7,
            LspReqKind::Formatting => 8,
            LspReqKind::Rename => 9,
            LspReqKind::CodeAction => 10,
            LspReqKind::ResolveCodeAction => 11,
            LspReqKind::CompletionResolve => 12,
            LspReqKind::SemanticTokens => 13,
            LspReqKind::InlayHints => 14,
            LspReqKind::ResolveInlayHint => 15,
            LspReqKind::DocumentSymbol => 16,
            LspReqKind::WorkspaceSymbol => 17,
        }
    }

    pub(crate) fn from_u16(value: u16) -> Option<Self> {
        Some(match value {
            0 => LspReqKind::Definition,
            1 => LspReqKind::Declaration,
            2 => LspReqKind::TypeDefinition,
            3 => LspReqKind::Implementation,
            4 => LspReqKind::References,
            5 => LspReqKind::Hover,
            6 => LspReqKind::SignatureHelp,
            7 => LspReqKind::Completion,
            8 => LspReqKind::Formatting,
            9 => LspReqKind::Rename,
            10 => LspReqKind::CodeAction,
            11 => LspReqKind::ResolveCodeAction,
            12 => LspReqKind::CompletionResolve,
            13 => LspReqKind::SemanticTokens,
            14 => LspReqKind::InlayHints,
            15 => LspReqKind::ResolveInlayHint,
            16 => LspReqKind::DocumentSymbol,
            17 => LspReqKind::WorkspaceSymbol,
            _ => return None,
        })
    }

    /// Whether results always go to the picker location list (references, symbols)
    /// rather than jumping a lone result directly (the goto family).
    pub(crate) fn is_list(self) -> bool {
        matches!(
            self,
            LspReqKind::References | LspReqKind::DocumentSymbol | LspReqKind::WorkspaceSymbol
        )
    }

    /// The message shown when the server returns no result. The location-list
    /// kinds phrase it as "found"; hover/signatureHelp have their own wording but
    /// are handled off the location path, so these are their fallbacks too.
    pub(crate) fn empty_message(self) -> &'static str {
        match self {
            LspReqKind::Definition => "No definition found",
            LspReqKind::Declaration => "No declaration found",
            LspReqKind::TypeDefinition => "No type definition found",
            LspReqKind::Implementation => "No implementation found",
            LspReqKind::References => "No references found",
            LspReqKind::Hover => "No hover information",
            LspReqKind::SignatureHelp => "No signature help",
            LspReqKind::Completion => "No completions",
            LspReqKind::Formatting => "No formatting changes",
            LspReqKind::Rename => "No rename changes",
            LspReqKind::CodeAction => "No code actions available",
            LspReqKind::ResolveCodeAction => "Code action returned no edit",
            // Resolve is a silent background fetch for the selected item's docs;
            // it never surfaces an empty-result message (an unresolved item just
            // shows no preview), so this is only a formal fallback.
            LspReqKind::CompletionResolve => "No completion documentation",
            // Semantic tokens are a background highlight refresh; an empty reply
            // just leaves the treesitter floor showing, never a message.
            LspReqKind::SemanticTokens => "No semantic tokens",
            // Inlay hints are a background decoration refresh; an empty reply just
            // clears the buffer's hints, never a message.
            LspReqKind::InlayHints => "No inlay hints",
            // Resolving a lazy hint is a silent background fill; an empty reply
            // just drops the placeholder, never a message.
            LspReqKind::ResolveInlayHint => "No inlay hint",
            LspReqKind::DocumentSymbol => "No document symbols",
            LspReqKind::WorkspaceSymbol => "No workspace symbols",
        }
    }
}

/// A request in flight, kept per [`LspReqKind`] so a reply can be matched to it
/// and stale ones dropped (Decision 3): the `generation` it was issued under,
/// and the `buffer`/`cursor` it was issued at (a later reply whose generation
/// differs, or that arrives after the cursor moved, is discarded).
pub(crate) struct PendingLspReq {
    pub(crate) generation: u64,
    pub(crate) buffer: BufferId,
    pub(crate) cursor: (usize, usize),
    /// The buffer's `changedtick` when the request was issued, for the
    /// content-version stale-drop of an *apply* reply (formatting/rename/code
    /// action return edits computed against this text; applying them after any
    /// edit would corrupt the buffer). Unused by the cursor-based kinds.
    pub(crate) tick: u64,
}

/// The negotiated runtime state of one server, learned from its `initialize`
/// reply: the position encoding and document-sync kind every buffer it owns uses,
/// plus the LSP client id assigned to this server (carried to Lua on
/// `LspAttach`/`LspDetach` as `data.client_id`).
pub(crate) struct ServerRuntime {
    encoding: PositionEncoding,
    sync_kind: TextDocumentSyncKind,
    client_id: u64,
    /// The server's semantic-tokens legend (`tokenTypes`/`tokenModifiers`), needed
    /// to decode a `semanticTokens/full` reply. `None` when the server advertises
    /// no semantic-tokens provider — its buffers then carry the treesitter floor
    /// alone.
    legend: Option<SemanticLegend>,
    /// Whether the server advertised `full/delta` support. When `false`, every
    /// semantic-tokens refresh re-requests the whole `full` set rather than a diff
    /// (sending `full/delta` to a server that didn't advertise it would error).
    semantic_tokens_delta: bool,
    /// Whether the server advertised `inlayHintProvider`. A buffer requests inlay
    /// hints only from a server that offers them (and only while enabled), so this
    /// gates the refresh request the same way `legend` gates semantic tokens.
    inlay_hints: bool,
}

/// Human label for a negotiated position encoding (matches the LSP wire names).
pub(crate) fn encoding_label(encoding: PositionEncoding) -> &'static str {
    match encoding {
        PositionEncoding::Utf8 => "utf-8",
        PositionEncoding::Utf16 => "utf-16",
        PositionEncoding::Utf32 => "utf-32",
    }
}

/// Convert an LSP [`Range`] (in `encoding`) to an absolute byte range in
/// `buffer`, resolving each endpoint against its line — the buffer-addressed form
/// of [`EditHost::lsp_range_to_bytes`], for a workspace edit that touches a
/// non-current buffer.
pub(crate) fn lsp_range_to_bytes_in(
    buffer: &Buffer,
    range: &Range,
    encoding: PositionEncoding,
) -> std::ops::Range<usize> {
    lsp_pos_to_byte_in(buffer, range.start, encoding)
        ..lsp_pos_to_byte_in(buffer, range.end, encoding)
}

/// Absolute byte offset of an LSP [`Position`] (in `encoding`) within `buffer`:
/// the character offset converted against its line, the row added as a line start.
///
/// The `row` is **clamped** to the buffer's last editable line: a server's
/// `Position.line` is untrusted, and an out-of-range row would otherwise reach
/// `buffer.line_start(row)` → ropey's `assert!(line_idx <= len_lines)` and panic
/// the server thread (a one-line malformed-reply DoS). Clamping mirrors the
/// column clamp `byte_col` already applies — a past-end position lands at the end
/// of the document rather than crashing the editor.
pub(crate) fn lsp_pos_to_byte_in(
    buffer: &Buffer,
    pos: Position,
    encoding: PositionEncoding,
) -> usize {
    // `line_count` is the number of editable lines; a valid line index is
    // `0..=line_count` (the count itself addresses the end of the last line).
    let row = (pos.line as usize).min(buffer.line_count());
    let line = buffer.line(row);
    buffer.line_start(row) + byte_col(encoding, &line, pos.character as usize)
}

/// A `(row, byte-column)` point in `buffer` as an LSP [`Position`] in `encoding`
/// (Decision 4): UTF-8 is the identity (an LSP UTF-8 character *is* a byte
/// offset), UTF-16/UTF-32 need column math over the line text. The buffer-
/// addressed form of [`EditHost::lsp_position`].
pub(crate) fn lsp_position_in(
    buffer: &Buffer,
    encoding: PositionEncoding,
    row: usize,
    byte_col: usize,
) -> Position {
    let character = match encoding {
        PositionEncoding::Utf8 => byte_col,
        PositionEncoding::Utf16 => {
            let line = buffer.line(row);
            unicode::byte_to_utf16(&line, byte_col)
        }
        PositionEncoding::Utf32 => {
            let line = buffer.line(row);
            line[..byte_col.min(line.len())].chars().count()
        }
    };
    Position {
        line: row as u32,
        character: character as u32,
    }
}

/// Convert a batch of journaled byte-delta edits in `buffer` into LSP incremental
/// content changes (each replacing the edit's old `(start..old_end)` range with
/// its inserted text, in `encoding`) — the buffer-addressed form of the
/// current-buffer conversion `sync_lsp` does inline.
pub(crate) fn incremental_changes_in(
    buffer: &Buffer,
    edits: &[BufferEdit],
    encoding: PositionEncoding,
) -> Vec<TextDocumentContentChangeEvent> {
    edits
        .iter()
        .map(|e| TextDocumentContentChangeEvent {
            range: Some(Range {
                start: lsp_position_in(buffer, encoding, e.start_point.0, e.start_point.1),
                end: lsp_position_in(buffer, encoding, e.old_end_point.0, e.old_end_point.1),
            }),
            range_length: None,
            text: e.text.clone(),
        })
        .collect()
}

/// Byte offset of LSP `character` on `line`, the inverse of [`EditHost::lsp_position`]
/// (Decision 4): UTF-8 is the identity (a character *is* a byte offset, clamped
/// to the line), UTF-16/UTF-32 need column math. Clamped to the line length so a
/// past-end character (a diagnostic whose range runs to EOL) lands at the end.
pub(crate) fn byte_col(encoding: PositionEncoding, line: &str, character: usize) -> usize {
    match encoding {
        PositionEncoding::Utf8 => character.min(line.len()),
        PositionEncoding::Utf16 => unicode::utf16_to_byte(line, character),
        PositionEncoding::Utf32 => line
            .char_indices()
            .nth(character)
            .map_or(line.len(), |(i, _)| i),
    }
}

/// Whether `c` belongs to a completion word — an identifier run: ASCII
/// alphanumeric or `_` (the default `iskeyword`, locale specifics aside).
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The completion word: the run of identifier characters immediately left of the
/// byte `cursor` on `line`, as `(word_start_byte, prefix)`. An empty prefix when
/// the cursor isn't preceded by an identifier char (a just-triggered menu with
/// nothing typed). Both the menu's filter string and its default replace range.
pub(crate) fn completion_word(line: &str, cursor: usize) -> (usize, String) {
    let cursor = cursor.min(line.len());
    let mut start = cursor;
    for (i, c) in line[..cursor].char_indices().rev() {
        if is_word_char(c) {
            start = i;
        } else {
            break;
        }
    }
    (start, line[start..cursor].to_string())
}

/// Map an LSP [`DiagnosticSeverity`] to nxvim's severity code (`1`=error,
/// `2`=warning, `3`=info, `4`=hint). An absent severity is treated as an error,
/// matching how most servers and neovim render an unspecified diagnostic. The
/// constants aren't enum variants (lsp-types models them as a newtype), so this
/// compares rather than pattern-matches.
pub(crate) fn severity_code(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        Some(s) if s == DiagnosticSeverity::WARNING => 2,
        Some(s) if s == DiagnosticSeverity::INFORMATION => 3,
        Some(s) if s == DiagnosticSeverity::HINT => 4,
        _ => 1,
    }
}

/// The inverse of [`severity_code`]: a 1=ERROR…4=HINT code back to the LSP
/// [`DiagnosticSeverity`]. An out-of-range code falls back to ERROR, matching
/// `severity_code`'s own default for an unspecified severity.
pub(crate) fn severity_from_code(code: u8) -> DiagnosticSeverity {
    match code {
        2 => DiagnosticSeverity::WARNING,
        3 => DiagnosticSeverity::INFORMATION,
        4 => DiagnosticSeverity::HINT,
        _ => DiagnosticSeverity::ERROR,
    }
}

/// Build an LSP [`Diagnostic`] from the plain [`DiagnosticData`] a plugin set via
/// `vim.diagnostic.set` (the inverse of [`diagnostic_mirror_data`]). Positions are
/// nxvim's native byte columns — there is no server to negotiate an encoding with,
/// so the renderer reads them back at [`PositionEncoding::Utf8`]. Only the fields
/// nxvim's surfaces consume (range / severity / message / source) are carried; the
/// rest default.
pub(crate) fn client_diagnostic(d: &DiagnosticData) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: d.lnum.max(0) as u32,
                character: d.col.max(0) as u32,
            },
            end: Position {
                line: d.end_lnum.max(0) as u32,
                character: d.end_col.max(0) as u32,
            },
        },
        severity: Some(severity_from_code(d.severity)),
        message: d.message.clone(),
        source: d.source.clone(),
        ..Default::default()
    }
}

/// Translate the LSP crate's [`ProviderCaps`] into the Lua-runtime
/// [`LspServerCapabilities`], the boundary that keeps `nxvim-lua` free of the LSP
/// crate. The two have the same per-feature fields; this is the one place they
/// are mapped across.
pub(crate) fn provider_caps_to_lua(p: &ProviderCaps) -> LspServerCapabilities {
    LspServerCapabilities {
        definition: p.definition,
        declaration: p.declaration,
        type_definition: p.type_definition,
        implementation: p.implementation,
        references: p.references,
        hover: p.hover,
        signature_help: p.signature_help,
        completion: p.completion,
        document_formatting: p.document_formatting,
        rename: p.rename,
        code_action: p.code_action,
        semantic_tokens: p.semantic_tokens,
        inlay_hints: p.inlay_hints,
    }
}

/// Project a buffer's cached diagnostics into the plain [`DiagnosticData`] the
/// Lua mirror (`nx._diagnostics`) holds for `vim.diagnostic.get`. Positions are
/// the raw 0-based LSP coordinates; `col`/`end_col` are byte offsets under the
/// UTF-8 encoding nxvim advertises first (the negotiated default), matching
/// neovim's byte-column `get` shape for the common case.
pub(crate) fn diagnostic_mirror_data(diags: &[Diagnostic]) -> Vec<DiagnosticData> {
    diags
        .iter()
        .map(|d| DiagnosticData {
            lnum: d.range.start.line as i64,
            col: d.range.start.character as i64,
            end_lnum: d.range.end.line as i64,
            end_col: d.range.end.character as i64,
            severity: severity_code(d.severity),
            message: d.message.clone(),
            source: d.source.clone(),
        })
        .collect()
}

/// The highlight group whose `sp`/underline style paints a diagnostic of this
/// severity code, resolved through the registry like the chrome groups.
pub(crate) fn severity_group(severity: u8) -> &'static str {
    match severity {
        2 => "DiagnosticUnderlineWarn",
        3 => "DiagnosticUnderlineInfo",
        4 => "DiagnosticUnderlineHint",
        _ => "DiagnosticUnderlineError",
    }
}

/// The highlight group whose foreground paints a diagnostic's inline virtual
/// text at this severity, resolved through the registry like [`severity_group`].
pub(crate) fn severity_virt_group(severity: u8) -> &'static str {
    match severity {
        2 => "DiagnosticVirtualTextWarn",
        3 => "DiagnosticVirtualTextInfo",
        4 => "DiagnosticVirtualTextHint",
        _ => "DiagnosticVirtualTextError",
    }
}

/// The highlight group whose foreground paints a diagnostic's gutter sign at this
/// severity, resolved through the registry like [`severity_group`].
pub(crate) fn severity_sign_group(severity: u8) -> &'static str {
    match severity {
        2 => "DiagnosticSignWarn",
        3 => "DiagnosticSignInfo",
        4 => "DiagnosticSignHint",
        _ => "DiagnosticSignError",
    }
}

/// The subset of `vim.diagnostic.config` keys nxvim has a backing surface for,
/// threaded from Lua via [`LspOp::DiagnosticConfig`](nxvim_lua::LspOp). `underline`
/// gates the squiggle spans; `virtual_text` gates the inline end-of-line message
/// and `virt_prefix` is its leader glyph; `signs` gates the gutter sign column and
/// `sign_text` holds its per-severity glyphs, indexed by severity code minus one
/// (`[error, warn, info, hint]`). Defaults match neovim 0.10: underline and signs
/// on, virtual text off, prefix `■ `, sign glyphs `E`/`W`/`I`/`H`.
pub(crate) struct DiagnosticConfig {
    pub(crate) underline: bool,
    pub(crate) virtual_text: bool,
    pub(crate) virt_prefix: String,
    pub(crate) signs: bool,
    pub(crate) sign_text: [String; 4],
}

impl DiagnosticConfig {
    /// The gutter glyph for a severity code (`1`=error … `4`=hint), from
    /// `sign_text` (a config `text` map, or the built-in `E`/`W`/`I`/`H` letters).
    pub(crate) fn sign_glyph(&self, severity: u8) -> &str {
        &self.sign_text[(severity.clamp(1, 4) - 1) as usize]
    }
}

impl Default for DiagnosticConfig {
    fn default() -> Self {
        Self {
            underline: true,
            virtual_text: false,
            virt_prefix: "■ ".to_string(),
            signs: true,
            sign_text: [
                "E".to_string(),
                "W".to_string(),
                "I".to_string(),
                "H".to_string(),
            ],
        }
    }
}

/// One-letter severity tag for the location-list column (`E`/`W`/`I`/`H`).
pub(crate) fn severity_short(severity: u8) -> char {
    match severity {
        2 => 'W',
        3 => 'I',
        4 => 'H',
        _ => 'E',
    }
}

/// The first non-empty line of a (possibly multi-line, markdown) diagnostic
/// message, for the single-line message line, the location-list rows, and the
/// inline virtual text. Terminal control characters (ESC, BEL, backspace, …) are
/// stripped: the message text comes from the language server (untrusted), and
/// every caller hands the result to a client that paints it, so an escape
/// sequence smuggled into a diagnostic must not reach the terminal. Sanitizing
/// here — at the single point the server projects the text — covers all clients
/// (TUI, GUI, remote) at once.
pub(crate) fn first_line(message: &str) -> String {
    message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .collect()
}

/// Human label for a document-sync kind.
pub(crate) fn sync_label(kind: TextDocumentSyncKind) -> &'static str {
    match kind {
        TextDocumentSyncKind::FULL => "full",
        TextDocumentSyncKind::INCREMENTAL => "incremental",
        _ => "none",
    }
}

/// Resolve the argv for a `vim.lsp.start`'s `cmd` into a [`ServerSpawn`]:
/// `$NXVIM_LSP_CMD` overrides the whole command (the mock hook, the LSP analogue
/// of `NXVIM_TS_WORKER`), else the config's `cmd` is used verbatim. `None` when no
/// program can be determined (an empty `cmd` and no override).
pub(crate) fn lsp_spawn(cmd: &[String]) -> Option<ServerSpawn> {
    if let Ok(override_cmd) = std::env::var("NXVIM_LSP_CMD") {
        let mut parts = override_cmd.split_whitespace().map(str::to_string);
        let program = parts.next()?;
        return Some(ServerSpawn {
            program,
            args: parts.collect(),
            ..Default::default()
        });
    }
    let (program, args) = cmd.split_first()?;
    Some(ServerSpawn {
        program: program.clone(),
        args: args.to_vec(),
        ..Default::default()
    })
}

/// `$NXVIM_LSP_ROOT`, absolutized, if set — an explicit workspace-root override
/// that supersedes the root Lua resolved (handy for tests, and for pinning a root
/// against an unusual layout). Relative values resolve against the cwd.
pub(crate) fn lsp_root_override() -> Option<PathBuf> {
    std::env::var_os("NXVIM_LSP_ROOT").map(|root| absolutize(Path::new(&root)))
}

/// A `file://` URI for an absolute-ized path, or `None` if it can't be formed.
pub(crate) fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(absolutize(path)).ok()
}

/// The filesystem path behind a `file://` URI (the inverse of [`path_to_uri`]),
/// or `None` for a non-file URI — the target of a go-to jump or a panel location.
pub(crate) fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

/// Resolve `path` against the current directory if it is relative.
pub(crate) fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}
