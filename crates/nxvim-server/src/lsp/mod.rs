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

/// A goto whose target file wasn't open yet, so its bytes are still being fetched
/// **off-tick** (a daemon / web session): the LSP position, kept with the answering
/// server's encoding, because the `character`→byte-column conversion needs the target
/// *line text* and there isn't any until the fetch lands. Stashed in
/// [`EditHost::pending_goto_cols`] by the replica buffer's id and settled by
/// `EditHost::settle_pending_goto` when the bytes arrive — the exact conversion the
/// local path does inline, done at the only moment the off-tick path can do it.
/// (Locally a jump reads its file synchronously, so this is never populated.)
pub(crate) struct PendingGoto {
    pub(crate) encoding: PositionEncoding,
    pub(crate) line: usize,
    pub(crate) character: usize,
}

/// What [`EditHost::apply_workspace_edit`] did: the outcome a server-initiated
/// `workspace/applyEdit` answers with, plus the ids of the **file** operations it
/// queued off-tick and which have not landed yet. A server-initiated edit adopts
/// those (its response waits for them); every other caller ignores them, since their
/// failures echo on their own.
pub(crate) struct AppliedEdit {
    pub(crate) outcome: nxvim_lsp::ApplyEditOutcome,
    /// The edit is held, unapplied, on a question the user hasn't answered
    /// (`changeAnnotations` with `needsConfirmation`). A server-initiated apply keeps
    /// its response back until the answer arrives — nothing has been applied yet, and
    /// saying otherwise would be a lie the server may act on.
    pub(crate) awaiting_confirm: bool,
    /// The apply's group id: the file operations it queued carry it, so a
    /// server-initiated edit can key the response it holds back by the same id.
    pub(crate) group: u64,
    /// How many file operations are still to run. Zero for the common edit (text and
    /// creates are synchronous), and for one that aborted before starting any.
    pub(crate) pending: usize,
}

/// One **file** operation a workspace edit asked for (a `rename` / `delete` resource
/// operation), in flight on the off-tick `FsJob` seam. Moving or removing a real file
/// can only happen off the editor tick — and must work identically local, over a
/// daemon and in the browser — so the filesystem half rides the same seam `nx.fs`
/// does, and the buffer half (rebind / wipe) waits here for the result.
/// Stashed in [`EditHost::workspace_fs_jobs`] keyed by the job id.
pub(crate) struct WorkspaceFsJob {
    pub(crate) op: WorkspaceFsOp,
    /// The apply this operation belongs to. A failure drops the rest of *this* group
    /// (the `abort` strategy), and a server-initiated apply's held-back response is
    /// keyed by the same id in [`EditHost::pending_apply_edits`].
    pub(crate) group: u64,
    /// The operation's position in the edit's `documentChanges`, reported to the server
    /// as `failedChange` when it fails.
    pub(crate) index: usize,
    /// The filesystem half, held until this operation's turn — the queue runs one at a
    /// time, in order. Taken at dispatch; `None` once it is in flight.
    pub(crate) job: Option<nxvim_lua::FsJob>,
}

/// What to do once a [`WorkspaceFsJob`]'s filesystem half lands.
pub(crate) enum WorkspaceFsOp {
    /// The directory holding a `create`d file is there (created, or already was): put
    /// the empty file in it ([`CreatePlaceholder`](Self::CreatePlaceholder), chained
    /// from here). A refactor may extract into a directory that does not exist yet (a
    /// new module / package), so the file is queued *behind* a recursive `mkdir` on this
    /// same ordered seam rather than fired straight at a path with no parent.
    CreateDir {
        buffer: BufferId,
        dir: PathBuf,
        /// The file itself, carried through so the chained write knows its target.
        path: PathBuf,
    },
    /// Put the `create`d file on disk — **empty**, which is the whole of what a `create`
    /// resource operation means: the file exists, and the content the edits after it put
    /// in `buffer` stays there, modified and unsaved, until you write it (neovim's
    /// model). Its landing re-snapshots `buffer`'s disk baseline, so the file we just
    /// made is not then reported back to the user as an external change.
    CreatePlaceholder { buffer: BufferId, path: PathBuf },
    /// A `create` whose URI named a **directory** (it ended in `/`): make it, and any
    /// missing parent. There is no buffer half — a directory isn't editable content —
    /// so this op only reports. Ordered with the rest of the edit's file operations,
    /// which is what lets a later `create` put a file inside it.
    MakeDir { dir: PathBuf },
    /// The result of an `exists` probe on a rename's *destination*, for the
    /// `ignoreIfExists` case: an existing destination means "leave it alone" (the
    /// rename is skipped), an absent one queues the [`Rename`](Self::Rename) itself.
    /// A probe rather than a guess, because the seam's rename — like `rename(2)` —
    /// would silently clobber the file the server asked us to preserve, and nothing
    /// on the editor tick can see a daemon's or the browser's filesystem.
    RenameGuard {
        from: PathBuf,
        to: PathBuf,
        /// The spelling to *store* on the buffer (cwd-relative where possible); `to`
        /// stays absolute for the filesystem side. See `EditHost::buffer_path_for`.
        to_name: PathBuf,
    },
    /// The file moved: rebind the buffer holding it — looked up by `from` when the
    /// move lands, not when it was queued, since the same edit's text-edit half may
    /// have opened it in between. Its content is unchanged (the bytes moved with the
    /// file), so only the name follows, and the per-tick lifecycle diff re-fires
    /// `FileType` (and so re-dispatches LSP) if the new extension resolves
    /// differently.
    Rename {
        from: PathBuf,
        to: PathBuf,
        /// The spelling to *store* on the buffer (cwd-relative where possible); `to`
        /// stays absolute for the filesystem side. See `EditHost::buffer_path_for`.
        to_name: PathBuf,
    },
    /// The file is gone: wipe the buffer holding it (looked up by `path` when the
    /// delete lands) so the editor doesn't hold a window onto a file that no longer
    /// exists. `ignore_missing` is the operation's `ignoreIfNotExists`: with it, a
    /// file that was already absent is the asked-for outcome rather than a failure to
    /// report.
    Delete { path: PathBuf, ignore_missing: bool },
}

/// An edit parked on the user's answer to its `changeAnnotations`
/// (`needsConfirmation`), kept in [`EditHost::pending_confirm_edits`] by group id.
/// Held whole — the accepted groups are filtered out of it when the answer lands, and
/// the whole thing is dropped if the answer is "no".
pub(crate) struct PendingConfirmEdit {
    pub(crate) edit: nxvim_lsp::WorkspaceEditData,
    /// The encoding every position in it is expressed in — the *producing* server's,
    /// which must survive the wait (the buffer's own servers are irrelevant).
    pub(crate) encoding: PositionEncoding,
}

/// A server-initiated `workspace/applyEdit` whose response is held back until the file
/// operations it asked for have landed: the `applied` flag must describe what actually
/// happened, and a `rename`/`delete` only finishes off-tick. Kept in
/// [`EditHost::pending_apply_edits`] keyed by an internal ticket, counting down as each
/// operation settles.
pub(crate) struct PendingApplyEdit {
    /// The server to answer, and the id it is blocked on.
    pub(crate) key: ServerKey,
    pub(crate) id: u64,
    /// The server's label for the edit, prefixed onto a failure reason.
    pub(crate) label: Option<String>,
    /// File operations still in flight; the response goes out when this hits zero.
    pub(crate) outstanding: usize,
    /// Everything that went wrong so far — empty ⇒ `applied: true`.
    pub(crate) trouble: Vec<String>,
    /// The index of the change that failed, reported as the response's `failedChange`
    /// (meaningful because nxvim declares the `abort` failure-handling strategy).
    pub(crate) failed_change: Option<u32>,
    /// The edit hasn't been applied at all yet: it is waiting on the user's answer to
    /// a `needsConfirmation` annotation. The response waits with it.
    pub(crate) awaiting_confirm: bool,
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
    /// The buffer's `'endofline'` as of the last push — i.e. whether `shadow` is the
    /// whole rope (`true`) or the rope minus its phantom trailing `\n` (`false`).
    /// Kept per server because each advances its own shadow, and consulted rather than
    /// re-read from the buffer so a flip *between* syncs (a `'fixendofline'` write) is
    /// noticed and bridged instead of silently desynchronizing the two.
    /// See [`incremental_changes_bridging_eol`].
    shadow_endofline: bool,
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

    /// The buffer's first attached server — the lowest [`ServerKey`], so the choice
    /// is stable across runs rather than hash-ordered.
    ///
    /// The **last-resort** answer to "which server serves this buffer", and the only
    /// remaining caller is [`EditHost::reply_encoding`]'s fallback for an edit with
    /// no producing server at all (one built in Lua). Every path where the choice
    /// can be wrong selects by capability ([`EditHost::lsp_target_for`]), asks them
    /// all ([`EditHost::lsp_capable_servers`]), or carries the answering server on
    /// the reply — document sync, diagnostics, requests, decorations, merged results
    /// and the apply/dispatch follow-ups all do one of those.
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

    /// The inverse of [`attach`](Self::attach): forget `key`'s document state for
    /// this buffer. Used when a server is stopped (`nx.lsp.stop`) — a buffer left
    /// bound to a dead server would keep looking served while nothing answered, and
    /// its next sync would push `didChange` into a shut-down process.
    pub(crate) fn detach(&mut self, key: &ServerKey) {
        self.servers.remove(key);
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

/// An in-flight request whose pending is tracked per **(kind, buffer, server)**
/// rather than one slot per kind — the whole-buffer decorations (semantic tokens,
/// inlay hints, folding ranges) and completion. Kept in
/// [`EditHost::lsp_multi_requests`], keyed by the unique `generation` its
/// [`ReqToken`] carries.
///
/// These need their own map because a buffer asks **every** capable server at once
/// (a `pyright` + `ruff` buffer has two semantic-token requests, and two completion
/// requests, in flight for one keystroke), and their results do not merge into a
/// single presentation the way a fan-out round's do — each lands in its own server's
/// cache, or streams into the open menu as it arrives. The single-slot `lsp_requests`
/// kind-map cannot express either: the second request would evict the first, and the
/// reply would then be decoded against whichever server happened to be recorded last
/// — with a wrong legend or a wrong position encoding, which paints plausible
/// nonsense rather than failing visibly.
///
/// Folding ranges ride this map for the buffer scoping alone; they stay
/// single-target ([`LspReqKind::per_server_pending`]).
pub(crate) struct PendingMultiReq {
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
            LspReqKind::DocumentSymbol => caps.document_symbol,
            LspReqKind::WorkspaceSymbol => caps.workspace_symbol,
            // Resolve follow-ups belong to the server that produced the item being
            // resolved, so they are routed by that item, never selected here.
            LspReqKind::ResolveCodeAction
            | LspReqKind::CompletionResolve
            | LspReqKind::ResolveInlayHint => return None,
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

    /// Whether this kind's in-flight requests are tracked per **(kind, buffer,
    /// server)** in [`EditHost::lsp_multi_requests`], rather than in the single-slot
    /// kind map — see [`PendingMultiReq`].
    ///
    /// Semantic tokens, inlay hints and completion go to *every* capable server: the
    /// first two cache per server and the projection concatenates, completion streams
    /// each server's candidates into the open menu as they land. Folding ranges are
    /// here only for the buffer scoping and stay **single-target** — a buffer has one
    /// fold structure, and merging two servers' containment trees is not defined.
    pub(crate) fn per_server_pending(self) -> bool {
        matches!(
            self,
            LspReqKind::SemanticTokens
                | LspReqKind::InlayHints
                | LspReqKind::FoldingRange
                | LspReqKind::Completion
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

    /// The feature's name in prose, for the message a **routed** request
    /// ([`EditHost::lsp_route`]) echoes when the client it names is attached but
    /// doesn't advertise the provider — "'ruff' does not provide hover".
    pub(crate) fn label(self) -> &'static str {
        match self {
            LspReqKind::Definition => "definition",
            LspReqKind::Declaration => "declaration",
            LspReqKind::TypeDefinition => "type definition",
            LspReqKind::Implementation => "implementation",
            LspReqKind::References => "references",
            LspReqKind::Hover => "hover",
            LspReqKind::SignatureHelp => "signature help",
            LspReqKind::Completion => "completion",
            LspReqKind::Formatting => "formatting",
            LspReqKind::Rename => "rename",
            LspReqKind::CodeAction => "code actions",
            LspReqKind::ResolveCodeAction => "code-action resolve",
            LspReqKind::CompletionResolve => "completion resolve",
            LspReqKind::SemanticTokens => "semantic tokens",
            LspReqKind::InlayHints => "inlay hints",
            LspReqKind::ResolveInlayHint => "inlay-hint resolve",
            LspReqKind::DocumentSymbol => "document symbols",
            LspReqKind::WorkspaceSymbol => "workspace symbols",
            LspReqKind::FoldingRange => "folding ranges",
        }
    }
}

/// A request in flight, kept per [`LspReqKind`] so a reply can be matched to it
/// and stale ones dropped (Decision 3): the `generation` it was issued under and
/// the `buffer` it was issued in (a later reply whose generation differs, or that
/// arrives after the buffer changed, is discarded).
///
/// No cursor here: every kind *anchored* to the cursor now merges across servers,
/// and its staleness is [`LspFanout`]'s to judge. What is left on this path acts on
/// the document or browses a list.
pub(crate) struct PendingLspReq {
    pub(crate) generation: u64,
    pub(crate) buffer: BufferId,
    /// The buffer's `changedtick` when the request was issued, for the
    /// content-version stale-drop of an *apply* reply (formatting/rename/code
    /// action return edits computed against this text; applying them after any
    /// edit would corrupt the buffer). Unused by the browsing kinds.
    pub(crate) tick: u64,
    /// The `nx._cb_fns` id that settles the issuing verb's promise, or `0` for a
    /// fire-and-forget request (an internal refresh, an auto-trigger, or a keymap
    /// verb the user didn't `:next` on). Carried so a **superseding** request of
    /// the same `kind` can settle the promise it replaces (resolve `nil`), and the
    /// reply can settle it on apply. Mirrors [`ReqToken::cb_id`].
    pub(crate) cb_id: u64,
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
/// Only the kinds where merging is well defined fan out (references, document and
/// workspace symbols, code actions) — a hover or a goto has one answer, so merging
/// them would be noise rather than completeness. See the routing table in
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
    /// Accumulated locations (references), each with the position encoding of the
    /// server that reported it.
    ///
    /// The pairing is load-bearing for the same reason the diagnostics store's is:
    /// two servers on one buffer may have negotiated different encodings, so a
    /// location's `character` is only meaningful against the encoding of the server
    /// that produced it. Converting the merged list at one encoding puts every
    /// column from the other server past the line's first multi-byte glyph in the
    /// wrong place — and, because the two spellings of one position then differ,
    /// silently defeats the duplicate check as well.
    pub(crate) locations: Vec<(Location, PositionEncoding)>,
    /// Accumulated symbols (document / workspace symbols), paired with their
    /// producing server's encoding for the same reason as
    /// [`locations`](Self::locations).
    pub(crate) symbols: Vec<(SymbolData, PositionEncoding)>,
    /// Accumulated hover documents, each tagged with the server that produced it so
    /// the merged float can head its section with the client's name. Unlike the
    /// location/symbol accumulators these need no encoding: a hover's payload is
    /// markdown, not positions.
    pub(crate) hovers: Vec<(ServerKey, Vec<String>)>,
    /// Accumulated signature help — `(server, signature label, active parameter)`,
    /// tagged for the same reason as [`hovers`](Self::hovers): with two servers
    /// answering, an unlabelled list of signatures says nothing about which language
    /// tool is claiming what. Also encoding-free (labels, not positions).
    pub(crate) signatures: Vec<(ServerKey, String, Option<String>)>,
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
    ///
    /// Hover is here for the same reason as the rest: on a `pyright` + `ruff` buffer
    /// each server knows something the other doesn't (a type, a lint rationale), and
    /// answering from one silently hides the other. It merges into a single float with
    /// a `# <client>` heading per server, which is what neovim's `vim.lsp.buf.hover`
    /// does. `nx.lsp.hover{ name = … }` still narrows the round to one server.
    ///
    /// The **goto family** merges its locations the way references always did — a
    /// definition can genuinely live in two places to two servers (a generated stub and
    /// its source, a `.d.ts` and its implementation), and the merged list still *jumps*
    /// when it holds exactly one place, so the one-server experience is unchanged.
    /// Duplicates collapse in [`EditHost::apply_lsp_locations`], which compares
    /// converted byte positions rather than raw LSP ones — necessary here, since two
    /// servers at different encodings spell one position differently.
    ///
    /// **Every** kind whose answer is a list or a document is now a fan-out; what is
    /// left single-target is the kinds that *act* (`Formatting`, `Rename`), where two
    /// servers' edits cannot be merged into one buffer, and the resolve/decoration
    /// kinds that belong to the server that produced their item.
    pub(crate) fn is_fanout(kind: LspReqKind) -> bool {
        matches!(
            kind,
            LspReqKind::References
                | LspReqKind::DocumentSymbol
                | LspReqKind::WorkspaceSymbol
                | LspReqKind::CodeAction
                | LspReqKind::Hover
                | LspReqKind::SignatureHelp
                | LspReqKind::Definition
                | LspReqKind::Declaration
                | LspReqKind::TypeDefinition
                | LspReqKind::Implementation
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

/// One live `$/progress` task of one server: the accumulated state of a
/// `begin` → `report`* sequence that has not yet seen its `end`.
///
/// Accumulated rather than last-write, because the protocol's fields are
/// **sticky**: `title` arrives only on `begin`, and a `report` that omits
/// `message`/`percentage` means "the previous value still stands" — so folding an
/// update in patches only what it actually carried. A naive overwrite would blank
/// the title on the first report, which is precisely the frame a statusline renders.
#[derive(Clone, Debug)]
pub(crate) struct ProgressEntry {
    /// The `$/progress` token, normalized to a string by the client. Identifies the
    /// task within its server; unique only per server, so the store is keyed by both.
    pub token: String,
    /// The `begin`'s title (`"Indexing"`). Empty for a server that reported without
    /// ever beginning — accepted rather than dropped, so a non-conforming server
    /// still shows activity.
    pub title: String,
    /// The latest detail line the server sent, `None` if it never sent one.
    pub message: Option<String>,
    /// The latest percentage (`0..=100`), `None` for an indeterminate task.
    pub percentage: Option<u32>,
    /// Whether the server would honor a cancel for this token.
    pub cancellable: bool,
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
/// The `row` is **clamped** to the last row of the *document*: a server's
/// `Position.line` is untrusted, and an out-of-range row would otherwise reach
/// `buffer.line_start(row)` → ropey's `assert!(line_idx <= len_lines)` and panic
/// the server thread (a one-line malformed-reply DoS). Clamping mirrors the
/// column clamp `byte_col` already applies — a past-end position lands at the end
/// of the document rather than crashing the editor.
///
/// The bound is the *document's* last row, not the rope's. The rope's phantom final
/// row — the empty one after its trailing `\n` — is a real, addressable row of the
/// document only when `'endofline'` says that newline is genuinely there; in a buffer
/// read without a final newline it is nxvim bookkeeping the server has never been told
/// about, and a position resolved onto it would land one byte past the document's end.
pub(crate) fn lsp_pos_to_byte_in(
    buffer: &Buffer,
    pos: Position,
    encoding: PositionEncoding,
) -> usize {
    // `line_count` is the number of editable lines; a valid line index is
    // `0..=line_count` (the count itself addresses the phantom row, i.e. the end of
    // the document) — one less when the document doesn't end with a newline.
    let last_row = buffer
        .line_count()
        .saturating_sub(usize::from(!buffer.options.endofline));
    // Past the last row, the position is the document's **end** — not the start of the
    // last row, which is what clamping the row and then resolving `character` against it
    // gives. On a terminated document the two coincide (the clamp lands on the empty
    // phantom row, whose start *is* the end), which is why clamping the row alone read as
    // correct; on an unterminated one they are a whole line apart. That gap corrupted the
    // single most ordinary LSP edit there is: a formatter's whole-document range ends at
    // `{ line: <line count>, character: 0 }` — past the last row of a file with no
    // trailing newline — so `:LspFormat` on such a file replaced only its *first* line
    // with the whole formatted document and left the rest appended to it.
    if pos.line as usize > last_row {
        return buffer.document_len_bytes();
    }
    let row = pos.line as usize;
    let line = buffer.line(row);
    buffer.line_start(row) + byte_col(encoding, &line, pos.character as usize)
}

/// Convert LSP `(range, new_text)` edits into the byte edits
/// [`Editor::apply_edits_to`](nxvim_core::Editor::apply_edits_to) takes, in **document**
/// coordinates, plus the `'endofline'` the buffer must carry once they land (`None` when
/// no edit reached the document's end, so the flag is unaffected).
///
/// An LSP range addresses the document; the rope carries one byte more than the document
/// whenever `'endofline'` is off. So the edit that owns the document's **tail** —
/// the one reaching its end — is widened to the rope's end, and its replacement
/// supersedes the phantom newline instead of being inserted before it. Without that, a
/// server filling an empty document with `fn f() {}\n` leaves a spurious blank last line
/// (the phantom is still sitting after the inserted text), which is exactly the bug two
/// `len_bytes() == 1` special cases in [`edit`](super::edit) used to paper over.
///
/// Re-deriving `'endofline'` from the tail edit's text is what makes the round trip
/// honest in both directions: a formatter that appends a trailing newline to an
/// unterminated file gives the buffer one, and a server appending text *after* the final
/// newline yields an unterminated document rather than a blank line. It is the only
/// information available about the tail — the server is telling us, in document terms,
/// what the document now ends with.
pub(crate) fn lsp_edits_to_byte_edits<'a>(
    buffer: &Buffer,
    edits: impl IntoIterator<Item = (&'a Range, &'a str)>,
    encoding: PositionEncoding,
) -> (Vec<(std::ops::Range<usize>, String)>, Option<bool>) {
    let mut out: Vec<(std::ops::Range<usize>, String)> = edits
        .into_iter()
        .map(|(range, text)| {
            (
                lsp_range_to_bytes_in(buffer, range, encoding),
                text.to_string(),
            )
        })
        .collect();
    let doc_len = buffer.document_len_bytes();
    // The tail edit is the one whose text ends up rightmost in the result, and it only
    // counts if that is the document's end. `apply_edits_to` orders by **start** byte
    // (descending), so among edits reaching the document's end the one that *starts*
    // latest is the tail — not the one listed last, which is what a server emitting its
    // edits bottom-up hands over. Ranking by `end` alone and breaking the tie on array
    // index picks the wrong one there, and since the tail is widened over the rope's
    // phantom newline, widening the earlier edit stretches it across its sibling and
    // swallows it (`let a = 1` + `[append ";", replace "1"→"2"]` ⇒ `let a = 2`).
    // Only a genuine tie — same start *and* same end, the several-edits-share-a-position
    // case the LSP spec allows — falls back to array order, where `apply_edits_to`
    // applies same-start edits in reverse input order so the last one lands rightmost.
    let tail = out
        .iter()
        .enumerate()
        .filter(|(_, (r, _))| r.end >= doc_len)
        .max_by_key(|(i, (r, _))| (r.end, r.start, *i))
        .map(|(i, _)| i);
    let Some(i) = tail else {
        return (out, None);
    };
    let (range, text) = &out[i];
    let ends_with_newline = match text.is_empty() {
        // A pure deletion of the tail: the document now ends wherever the range began,
        // so what precedes that byte decides. (`range.start == 0` ⇒ the document is
        // emptied, and an empty document ends with no newline.)
        true => range.start > 0 && matches!(buffer.text.get_char(range.start - 1), Ok('\n')),
        false => text.ends_with('\n'),
    };
    out[i].0.end = buffer.len_bytes();
    (out, Some(ends_with_newline))
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

/// [`incremental_changes_against`] for a buffer whose document is not the whole rope
/// — the `'endofline'`-off case, where the document is the rope minus its phantom
/// trailing `\n`.
///
/// The journal speaks **rope**; the server holds the **document**. A rope-space delta
/// is not a document-space delta when the two differ: `dd` on the last line of `a\nb`
/// deletes rope bytes `2..4`, but the document goes `a\nb` → `a`, a delete of `1..3`,
/// because the byte that preceded the phantom *becomes* the new phantom and so leaves
/// the document from outside the edit's own range. No endpoint remapping expresses
/// that.
///
/// Bracketing does, exactly and in O(1) extra changes: an LSP `didChange` carries a
/// *sequence* of edits, each addressing the document as the previous one left it. So
/// put the phantom on, replay the journal verbatim in the rope coordinates it was
/// written in, and take the phantom off again. Every intermediate state the server
/// reaches is one nxvim really passed through, which is what makes this correct
/// rather than merely plausible.
///
/// The two brackets are independent, which is what lets `'endofline'` *change* without
/// a resync: `was_short` (the shadow is a document, so it needs the phantom back) is
/// the flag as of the last sync, `is_short` (the new document drops it) the flag now.
/// A `'fixendofline'` write flipping the buffer to `endofline` therefore syncs as a
/// lone "append the newline" change.
///
/// `None` when the replayed shadow doesn't end in `\n` — the rope invariant would have
/// to have been violated for that, so rather than emit a delete of some other byte the
/// caller falls back to pushing the whole document.
pub(crate) fn incremental_changes_bridging_eol(
    shadow: &mut String,
    edits: &[BufferEdit],
    encoding: PositionEncoding,
    was_short: bool,
    is_short: bool,
) -> Option<Vec<TextDocumentContentChangeEvent>> {
    // Nothing to say: no edits, and the document's shape didn't change either.
    if edits.is_empty() && was_short == is_short {
        return Some(Vec::new());
    }
    let mut changes = Vec::with_capacity(edits.len() + 2);
    if was_short {
        let at = shadow_position(shadow, shadow.len(), encoding);
        changes.push(TextDocumentContentChangeEvent {
            range: Some(Range { start: at, end: at }),
            range_length: None,
            text: "\n".to_string(),
        });
        shadow.push('\n');
    }
    changes.extend(incremental_changes_against(shadow, edits, encoding));
    if is_short {
        let cut = shadow.strip_suffix('\n')?.len();
        changes.push(TextDocumentContentChangeEvent {
            range: Some(Range {
                start: shadow_position(shadow, cut, encoding),
                end: shadow_position(shadow, shadow.len(), encoding),
            }),
            range_length: None,
            text: String::new(),
        });
        shadow.truncate(cut);
    }
    Some(changes)
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
        document_symbol: p.document_symbol,
        workspace_symbol: p.workspace_symbol,
    }
}

/// Project a buffer's cached diagnostics into the plain [`DiagnosticData`] the
/// Lua mirror (`nx._diagnostics`) holds for `vim.diagnostic.get`. Positions are
/// the raw 0-based LSP coordinates; `col`/`end_col` are byte offsets under the
/// UTF-8 encoding nxvim advertises first (the negotiated default), matching
/// neovim's byte-column `get` shape for the common case.
///
/// `client_id` tags every entry with the server that published it — the mirror is
/// one flat list per buffer merged across servers, so the caller projects each
/// server's set separately and concatenates.
pub(crate) fn diagnostic_mirror_data(
    diags: &[Diagnostic],
    client_id: Option<u64>,
) -> Vec<DiagnosticData> {
    diags
        .iter()
        .map(|d| DiagnosticData {
            client_id,
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

/// The filesystem path behind a `file://` URI (the inverse of
/// [`EditHost::buffer_uri`](EditHost::buffer_uri)), or `None` for a non-file URI — the target of a go-to jump or a panel location.
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
