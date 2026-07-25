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
use std::collections::{BTreeMap, HashMap};

use nxvim_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Location, Position, Range, SemanticToken,
    TextDocumentContentChangeEvent, TextDocumentSyncKind, TextEdit, Url,
};
use nxvim_lsp::{
    CodeActionData, PositionEncoding, ProviderCaps, SemanticLegend, ServerKey, ServerSpawn,
    SymbolData,
};
use nxvim_lua::{DiagnosticData, LspServerCapabilities};

/// A workspace edit's edits for a file whose bytes are still being fetched **off-tick**
/// (a daemon / web session): kept in LSP form plus the originating server's position
/// encoding, because their byte ranges can only be resolved once the replica buffer's
/// real contents have landed. Stashed in [`EditHost::pending_replica_edits`] keyed by
/// the replica buffer's id and applied by `EditHost::apply_pending_replica_edit` when
/// the fetch completes. (Local sessions read synchronously and apply inline, so they
/// never populate this.)
pub(crate) struct PendingReplicaEdit {
    pub(crate) edits: Vec<TextEdit>,
    pub(crate) encoding: PositionEncoding,
}

mod completion;
pub(crate) use completion::LspComplete;
mod diagnostics;
mod edit;
mod folding;
mod inlay;
mod request;
mod semantic;
mod sync;

/// Per-**(buffer, server)** document-sync state: everything that is negotiated
/// with, or tracked against, one specific server.
///
/// None of this can be shared between two servers attached to the same buffer.
/// They negotiate independent position encodings and sync kinds, so `shadow` (the
/// text *that* server holds) and `version` advance on separate clocks, and each
/// publishes its own diagnostics / semantic tokens / inlay hints. Splitting it out
/// is what lets a buffer carry N servers; see
/// `docs/plans/2026-07-25-multi-server-lsp-attach.md`.
#[derive(Default)]
pub(crate) struct LspServerDoc {
    /// Has `didOpen` been sent for the current server instance?
    opened: bool,
    /// LSP document version (monotonic, bumped per `didChange`).
    version: i32,
    /// `changedtick` of the last sync we sent (drives `didChange`).
    last_tick: u64,
    /// `save_tick` of the last sync, mirrored to fire `didSave` exactly when the
    /// buffer is written (`save_tick` bumps only on a successful `:w`).
    last_save_tick: u64,
    /// The document text the server currently holds — what we last sent it (full at
    /// `didOpen`, then advanced by each `didChange`). Incremental syncs replay the
    /// journaled edits over this to compute correct per-encoding positions, then it
    /// matches the buffer again; a full/resync push resets it to the whole text.
    /// (neovim's `prev_lines`.) See [`incremental_changes_against`].
    shadow: String,
    /// Latest `publishDiagnostics` from this server for this buffer, projected into
    /// the redraw (`diagnostics_for`) and the under-cursor message line.
    diagnostics: Vec<Diagnostic>,
    /// Latest `semanticTokens/full` result, decoded into the per-line highlight
    /// spans `highlights_for` merges over the treesitter floor (ADR 0001 bridge #2).
    /// Empty until the first reply lands.
    semantic: SemanticTokensCache,
    /// Latest `textDocument/inlayHint` result, decoded into the per-line inline
    /// hints `inlay_hints_for` projects over the buffer text. Empty until the first
    /// reply lands (and while inlay hints are disabled).
    inlay: InlayHintsCache,
}

/// Per-buffer LSP bookkeeping, mirroring `SyntaxState`. One per open buffer that
/// mapped to a configured server, keyed by [`BufferId`] in
/// [`EditHost::lsp_states`]. Holds only what is genuinely buffer-wide — the URI,
/// the `languageId`, and the user's per-buffer feature toggles — plus the map of
/// attached servers, each with its own [`LspServerDoc`].
///
/// A buffer may carry **several** servers (`pyright` + `ruff`, `ts_ls` + `eslint`):
/// each syncs its own document and publishes its own diagnostics. Requests are
/// routed by capability ([`EditHost::lsp_target_for`]), and the kinds whose answers
/// merge fan out to every capable server ([`LspFanout`]).
#[derive(Default)]
pub(crate) struct LspDocState {
    /// The LSP `languageId` for `didOpen` — the buffer's filetype, set when the
    /// `vim.lsp.enable` dispatcher binds the buffer (no longer re-derived in sync).
    language_id: String,
    /// The document URI, kept so `didClose` can be sent after the buffer is gone.
    uri: Option<Url>,
    /// The servers attached to this buffer, ordered by [`ServerKey`] (config name,
    /// then root) so iteration — and therefore "the first capable server" — is
    /// deterministic rather than hash-ordered.
    servers: BTreeMap<ServerKey, LspServerDoc>,
    /// Per-buffer semantic-token override (Phase 3): `None` is the auto default
    /// (enabled when the server advertises a legend); `Some(false)` is an explicit
    /// `vim.lsp.semantic_tokens.stop` (hide the paint, skip refreshes);
    /// `Some(true)` an explicit `start`. The cache survives a stop, so a later
    /// `start` repaints from it without a round-trip.
    semantic_enabled: Option<bool>,
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

    /// The buffer's first attached server and its document state — the lowest
    /// [`ServerKey`], so the choice is stable across runs rather than hash-ordered.
    ///
    /// The *provisional* answer to "which server serves this buffer", left for the
    /// few surfaces that still want a single one: the `:LspInfo` header, the
    /// fallback encoding when no producing server is known, and the legacy
    /// single-encoding diagnostics accessor. Every path where the choice can be
    /// wrong now selects by capability ([`EditHost::lsp_target_for`]) or asks them
    /// all ([`EditHost::lsp_capable_servers`]) — document sync, diagnostics,
    /// requests, semantic tokens and inlay hints all iterate
    /// [`servers`](Self::servers).
    pub(crate) fn primary(&self) -> Option<(&ServerKey, &LspServerDoc)> {
        self.servers.iter().next()
    }

    /// The key half of [`primary`](Self::primary).
    pub(crate) fn primary_key(&self) -> Option<&ServerKey> {
        self.servers.keys().next()
    }

    /// Is `key` among this buffer's servers?
    pub(crate) fn bound_to(&self, key: &ServerKey) -> bool {
        self.servers.contains_key(key)
    }

    /// Has this buffer sent `didOpen` to `key` — i.e. is it *attached* to that
    /// server, as opposed to merely bound and waiting for `initialize`?
    pub(crate) fn is_opened_under(&self, key: &ServerKey) -> bool {
        self.servers.get(key).is_some_and(|d| d.opened)
    }

    /// This buffer's state for `key`, if attached.
    pub(crate) fn doc(&self, key: &ServerKey) -> Option<&LspServerDoc> {
        self.servers.get(key)
    }

    /// Mutable [`doc`](Self::doc).
    pub(crate) fn doc_mut(&mut self, key: &ServerKey) -> Option<&mut LspServerDoc> {
        self.servers.get_mut(key)
    }

    /// Every attached server, in key order.
    pub(crate) fn servers(&self) -> impl Iterator<Item = (&ServerKey, &LspServerDoc)> {
        self.servers.iter()
    }

    /// Mutable [`servers`](Self::servers).
    pub(crate) fn servers_mut(&mut self) -> impl Iterator<Item = (&ServerKey, &mut LspServerDoc)> {
        self.servers.iter_mut()
    }

    /// Attach `key` to this buffer, returning its document state — creating a fresh
    /// (unopened, version 0) one the first time, so the next sync sends its `didOpen`.
    ///
    /// **Additive**: a buffer whose filetype enables two servers ends up bound to
    /// both, each syncing on its own clock. An already-attached key is returned
    /// untouched, so this is idempotent and safe to call every sync.
    pub(crate) fn attach(&mut self, key: ServerKey) -> &mut LspServerDoc {
        self.servers.entry(key).or_default()
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
/// Records where the resolved label lands: the issuing buffer, the `server` whose
/// cache holds the placeholder, the `tick` it was issued against (the reply is
/// dropped if the buffer changed since), and the `(line, idx)` of the placeholder
/// span in [`InlayHintsCache::hints`] to fill.
pub(crate) struct InlayResolveTarget {
    pub(crate) buffer: BufferId,
    /// The server that produced the lazy hint — and therefore the only one whose
    /// cache the resolved label belongs in. Two servers can both have hints on the
    /// same line, so `(line, idx)` alone addresses the wrong span.
    pub(crate) server: ServerKey,
    pub(crate) tick: u64,
    pub(crate) line: usize,
    pub(crate) idx: usize,
}

/// An in-flight **whole-buffer decoration** request — semantic tokens, inlay hints,
/// folding ranges — kept in [`EditHost::lsp_buf_requests`] keyed by the unique
/// `generation` its [`ReqToken`] carries.
///
/// These need their own map because a buffer asks **every** capable server for its
/// decorations at once (a `pyright` + `ruff` buffer has two semantic-token requests
/// in flight for one change), and their results do not merge into a single
/// presentation the way a fan-out round's do — each lands in its own server's cache
/// and the projection concatenates. The single-slot `lsp_requests` kind-map cannot
/// express either: the second request would evict the first, and the reply would
/// then be decoded against whichever server happened to be recorded last — with a
/// wrong legend or a wrong position encoding, which paints plausible nonsense.
pub(crate) struct PendingBufReq {
    pub(crate) kind: LspReqKind,
    pub(crate) buffer: BufferId,
    /// The buffer's `changedtick` at issue time; a reply computed against
    /// superseded text is dropped.
    pub(crate) tick: u64,
    /// The server asked — the legend and encoding its reply must be decoded with.
    pub(crate) server: ServerKey,
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
    FoldingRange,
}

impl LspReqKind {
    /// Whether `caps` advertises the provider that answers this request kind — the
    /// predicate [`EditHost::lsp_target_for`] selects a server with.
    ///
    /// `None` means "not capability-gated": either the LSP spec has no provider flag
    /// for it (the `*/resolve` follow-ups, which are only ever sent back to the
    /// server that produced the item), or nxvim doesn't model one. Those fall back
    /// to the buffer's first server rather than filtering everything out — failing
    /// open, because a wrongly-empty selection would silently answer nothing.
    pub(crate) fn provider(self, caps: &ProviderCaps) -> Option<bool> {
        Some(match self {
            LspReqKind::Definition => caps.definition,
            LspReqKind::Declaration => caps.declaration,
            LspReqKind::TypeDefinition => caps.type_definition,
            LspReqKind::Implementation => caps.implementation,
            LspReqKind::References => caps.references,
            LspReqKind::Hover => caps.hover,
            LspReqKind::SignatureHelp => caps.signature_help,
            LspReqKind::Completion => caps.completion,
            LspReqKind::Formatting => caps.document_formatting,
            LspReqKind::Rename => caps.rename,
            LspReqKind::CodeAction => caps.code_action,
            LspReqKind::SemanticTokens => caps.semantic_tokens,
            LspReqKind::InlayHints => caps.inlay_hints,
            LspReqKind::FoldingRange => caps.folding_range,
            // Resolve follow-ups belong to the server that produced the item being
            // resolved, so they are routed by that item, never selected here.
            LspReqKind::ResolveCodeAction
            | LspReqKind::CompletionResolve
            | LspReqKind::ResolveInlayHint => return None,
            // `documentSymbol` / `workspace/symbol` have provider flags nxvim does
            // not distil into `ProviderCaps` yet.
            LspReqKind::DocumentSymbol | LspReqKind::WorkspaceSymbol => return None,
        })
    }

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
            LspReqKind::FoldingRange => 18,
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
            18 => LspReqKind::FoldingRange,
            _ => return None,
        })
    }

    /// Whether this kind is a **whole-buffer decoration refresh** — issued per
    /// buffer on open/change/enable rather than at the cursor, and tracked in
    /// [`EditHost::lsp_buf_requests`] rather than the single-slot kind map.
    ///
    /// Semantic tokens and inlay hints go to *every* capable server (their caches
    /// are per server and the projection concatenates); folding ranges stay
    /// single-target — a buffer has one fold structure, and merging two servers'
    /// containment trees is not defined.
    pub(crate) fn is_whole_buffer(self) -> bool {
        matches!(
            self,
            LspReqKind::SemanticTokens | LspReqKind::InlayHints | LspReqKind::FoldingRange
        )
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
            // Folding ranges are a background fold refresh; an empty reply just
            // leaves the buffer unfolded, never a message.
            LspReqKind::FoldingRange => "No folding ranges",
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
    /// The `nx._cb_fns` id that settles the issuing verb's promise, or `0` for a
    /// fire-and-forget request (an internal refresh, an auto-trigger, or a keymap
    /// verb the user didn't `:next` on). Carried so a **superseding** request of
    /// the same `kind` can settle the promise it replaces (resolve `nil`), and the
    /// reply can settle it on apply. Mirrors [`ReqToken::cb_id`].
    pub(crate) cb_id: u64,
    /// The caller's `nx.lsp.code_action{ context = { only = … }, apply = … }` options,
    /// needed at *reply* time (which actions survive the filter, and whether a single
    /// survivor is applied without the chooser). Default (no filter, no auto-apply) for
    /// every other kind.
    pub(crate) code_action: CodeActionOpts,
    /// The server this request was actually sent to.
    ///
    /// Recorded because the answering server is no longer implied: once a buffer
    /// carries several, a reply must be decoded against the encoding and legend of
    /// the server that *produced* it, not the buffer's first. Re-deriving it at
    /// reply time is exactly the bug this prevents — semantic tokens decoded with
    /// another server's legend paint plausible nonsense.
    pub(crate) server: Option<ServerKey>,
}

/// One **fan-out round**: a single logical request issued to every capable server,
/// whose replies merge into one presentation.
///
/// Only the kinds where merging is well defined fan out (references, document
/// symbols, code actions) — a hover or a goto has one answer, so merging them would
/// be noise rather than completeness. See the routing table in
/// `docs/plans/2026-07-25-multi-server-lsp-attach.md`.
///
/// The round completes when every asked server has replied **or dropped out** (its
/// process exited). A server that neither replies nor exits holds the round open —
/// the same exposure a single hung server has always had, and the next request of
/// that kind supersedes it. It is why `outstanding` is keyed by server: an exit can
/// then retire its slot instead of stranding the round.
pub(crate) struct LspFanout {
    /// Generation → the server it was sent to, for every reply still outstanding.
    pub(crate) outstanding: HashMap<u64, ServerKey>,
    /// The issuing verb's promise, settled once when the merged result presents.
    pub(crate) cb_id: u64,
    pub(crate) buffer: BufferId,
    pub(crate) cursor: (usize, usize),
    pub(crate) tick: u64,
    /// The `only` / `apply` filter, applied to the MERGED list so a `source.fixAll`
    /// request still auto-applies when exactly one server offers a match.
    pub(crate) code_action: CodeActionOpts,
    /// Accumulated locations (references).
    pub(crate) locations: Vec<Location>,
    /// Accumulated symbols (document symbols).
    pub(crate) symbols: Vec<SymbolData>,
    /// Accumulated code actions, each tagged with the server that produced it.
    ///
    /// The tag is load-bearing: a lazy action is finished with `codeAction/resolve`,
    /// and its `data` blob is meaningful only to the server that issued it.
    /// Resolving ruff's action against pyright is not a degraded result, it is a
    /// wrong request.
    pub(crate) actions: Vec<(ServerKey, CodeActionData)>,
}

impl LspFanout {
    /// Whether `kind`'s replies merge across servers rather than being answered by
    /// one — the routing table, in code.
    pub(crate) fn is_fanout(kind: LspReqKind) -> bool {
        matches!(
            kind,
            LspReqKind::References | LspReqKind::DocumentSymbol | LspReqKind::CodeAction
        )
    }
}

/// The options a `nx.lsp.code_action(opts)` call carries from Lua to the reply:
/// `only` is the kind filter (empty ⇒ every kind) and `apply` asks for a **one-shot**
/// application when exactly one action survives that filter (more than one still opens
/// the chooser — there is a choice to make). Sent with the request *and* enforced on the
/// reply, since honoring `context.only` is a protocol *should*.
#[derive(Clone, Debug, Default)]
pub(crate) struct CodeActionOpts {
    pub(crate) only: Vec<String>,
    pub(crate) apply: bool,
}

impl CodeActionOpts {
    /// Whether `kind` satisfies the filter. No filter ⇒ everything passes. Otherwise the
    /// action must declare a kind that *is* one of the requested kinds or sits under it
    /// in the LSP dot-hierarchy (`source.fixAll` accepts `source.fixAll.ruff`, but not
    /// `source.fixAllTheThings`). A kind-less action never matches a filter.
    pub(crate) fn matches(&self, kind: Option<&str>) -> bool {
        if self.only.is_empty() {
            return true;
        }
        let Some(kind) = kind else { return false };
        self.only
            .iter()
            .any(|want| kind == want || kind.starts_with(&format!("{want}.")))
    }
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
    /// Everything this server advertised at `initialize`, per feature.
    ///
    /// Kept whole (rather than distilled to the two or three bools the request
    /// paths used to need) because it is now the **routing** input: with several
    /// servers on a buffer, "which server answers a hover" is "the first one, in
    /// key order, whose `providers.hover` is true". See
    /// [`EditHost::lsp_target_for`].
    providers: ProviderCaps,
    /// The server's advertised `signatureHelpProvider.{trigger,retrigger}Characters`
    /// (usually `(` / `,`), pre-reduced to `char`s. Pushed into core as the
    /// auto-trigger set when this server attaches and the user opted in; empty when the
    /// server advertises none. See `EditHost::signature_auto`.
    signature_trigger_chars: Vec<char>,
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
    let start = lsp_pos_to_byte_in(buffer, range.start, encoding);
    let end = lsp_pos_to_byte_in(buffer, range.end, encoding);
    // A well-formed LSP range has `start <= end`, but the server is untrusted: a
    // reversed range yields `start > end`, and downstream consumers compute
    // **unsigned** deltas over it — the cursor-shift planner in
    // [`Editor::apply_edits_to`](nxvim_core::Editor) (`e_row - s_row`) and the
    // completion-accept's `r.end - r.start` — which underflow and panic the server
    // thread on a malformed reply (the same one-line-DoS class the row clamp in
    // [`lsp_pos_to_byte_in`] already guards). Clamp `end` up to `start` so a
    // reversed range degrades to an empty (insert-only) edit instead of crashing;
    // a valid forward range round-trips unchanged.
    start..end.max(start)
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

/// Convert a batch of journaled byte-delta edits into LSP incremental content
/// changes by replaying them over `shadow` — the text the server *currently*
/// holds (its last-synced view) — which `shadow` is mutated to match the buffer.
///
/// Each LSP change's range describes the document as it was *before* that change
/// applied (the changes apply sequentially, server-side). The journaled byte
/// offsets are exactly those intermediate coordinates, so converting them against
/// `shadow` — and advancing `shadow` by each edit as we go — yields positions in
/// the right text at each step. This is the only correct way under UTF-16/UTF-32:
/// the byte→code-unit conversion needs the line as it stood *before* the edit, and
/// `shadow` still has it (converting against the post-edit buffer would clamp a
/// shortened line's later columns and corrupt the range — `balance`→`aa`⇒`aae`).
/// It is neovim's `prev_lines` approach, specialized to explicit edits rather than
/// a recomputed diff, so it stays incremental in every encoding. The caller keeps
/// `shadow` across syncs (seeded at `didOpen`, reset on a full/resync push).
pub(crate) fn incremental_changes_against(
    shadow: &mut String,
    edits: &[BufferEdit],
    encoding: PositionEncoding,
) -> Vec<TextDocumentContentChangeEvent> {
    let mut changes = Vec::with_capacity(edits.len());
    for e in edits {
        let start = e.start_byte.min(shadow.len());
        let end = e.old_end_byte.min(shadow.len()).max(start);
        changes.push(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: shadow_position(shadow, start, encoding),
                end: shadow_position(shadow, end, encoding),
            }),
            range_length: None,
            text: e.text.clone(),
        });
        shadow.replace_range(start..end, &e.text);
    }
    changes
}

/// An absolute byte offset in `shadow` as an LSP [`Position`] in `encoding` — the
/// shadow-addressed sibling of [`lsp_position_in`], used by
/// [`incremental_changes_against`]. Resolves the line, then the column the chosen
/// encoding wants from the line's own text (UTF-8 byte = identity).
fn shadow_position(shadow: &str, byte: usize, encoding: PositionEncoding) -> Position {
    let byte = byte.min(shadow.len());
    let prefix = &shadow[..byte];
    let line = prefix.bytes().filter(|b| *b == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |i| i + 1);
    let line_end = shadow[line_start..]
        .find('\n')
        .map_or(shadow.len(), |i| line_start + i);
    let col = byte - line_start;
    let line_text = &shadow[line_start..line_end];
    let character = match encoding {
        PositionEncoding::Utf8 => col,
        PositionEncoding::Utf16 => unicode::byte_to_utf16(line_text, col),
        PositionEncoding::Utf32 => line_text[..col.min(line_text.len())].chars().count(),
    };
    Position {
        line: line as u32,
        character: character as u32,
    }
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

/// The vim quickfix type char for a severity code (`1`=ERROR→`E` … `4`=HINT→`N`),
/// matching `vim.diagnostic.toqflist`. Stored on each loclist entry's `typ`, where
/// it drives the row's severity color. (HINT maps to `N`, vim's "note", not `H`.)
pub(crate) fn qf_type_char(severity: u8) -> u8 {
    match severity {
        2 => b'W',
        3 => b'I',
        4 => b'N',
        _ => b'E',
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
///
/// `$NXVIM_LSP_CMD_<name>` overrides just the server named `name`, and wins over the
/// blanket `$NXVIM_LSP_CMD`. Without it a test cannot stand up **two** distinct mock
/// servers — the blanket override would point both at the same script, so nothing
/// could tell which server answered, and a multi-server assertion would prove
/// nothing. (`name` is upper-cased and non-alphanumerics become `_`, so a config
/// named `ts_ls` reads `$NXVIM_LSP_CMD_TS_LS`.)
pub(crate) fn lsp_spawn(name: &str, cmd: &[String]) -> Option<ServerSpawn> {
    let per_server: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let override_cmd = std::env::var(format!("NXVIM_LSP_CMD_{per_server}"))
        .or_else(|_| std::env::var("NXVIM_LSP_CMD"));
    if let Ok(override_cmd) = override_cmd {
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

/// A human-facing spelling of `path` for a picker / panel row: stripped to a
/// cwd-relative path when it lives under the working directory, else left
/// absolute. Cosmetic only — navigation still carries the full path, which
/// [`crate::Editor::find_buffer_by_path`] reuses cwd-aware — but it keeps the LSP
/// symbol / location list readable (and matched to how the file was opened)
/// instead of every row blaring the absolute path.
pub(crate) fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
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
