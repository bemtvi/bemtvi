//! Server-side LSP integration: the `syntax.rs` analogue for language servers.
//!
//! Where `nxvim-lsp` owns the client machinery (spawning/supervising servers and
//! the JSON-RPC bridge — the `SyntaxClient` analogue), this module owns the
//! *editor* half: draining the `vim.lsp.start` queue into the manager
//! ([`Server::apply_lsp_op`]), per-buffer document-sync bookkeeping
//! ([`LspDocState`], keyed by [`BufferId`] like
//! `SyntaxState`), byte↔LSP position conversion via `nxvim-core`'s pure unicode
//! helpers, and the handling of [`LspEvent`]s. Document sync reuses the buffer
//! edit journal (`take_edits`/`changedtick`) the syntax sync already drives
//! (Decision 5); the only added core signal is `Buffer::save_tick`, a monotonic
//! write counter so `didSave` fires off a real save hook rather than a heuristic.
//!
//! All [`Server`] methods here run on the single editor thread and only ever
//! hand the manager fire-and-forget [`LspNotify`]s, so a slow or hung server can
//! never stall keystroke→buffer→redraw.

use std::path::{Path, PathBuf};

use nxvim_core::unicode;
use nxvim_core::view::View;
use nxvim_core::{Buffer, BufferEdit, BufferId, Mode};
use nxvim_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, Location, Position, Range, TextDocumentContentChangeEvent,
    TextDocumentSyncKind, TextEdit, Url,
};
use nxvim_lsp::{
    CodeActionData, CompletionItemData, LspEvent, LspNotify, LspReply, LspRequest,
    PositionEncoding, ProviderCaps, ReqToken, ServerKey, ServerSpawn, WorkspaceEditData,
};
use nxvim_lua::{DiagnosticData, LspClientData, LspOp, LspServerCapabilities};
use rmpv::Value;

use crate::{Server, StyleTable};

/// One per-line jump target for a navigable LSP panel (diagnostics, references,
/// …), handed to [`nxvim_core::Editor::set_panel_targets`]: `Some((path, 0-based
/// line, 0-based **byte** column))` — the LSP character already converted through
/// the negotiated encoding — or `None` for a non-navigable line. Indexed in
/// lockstep with the panel's lines, and retained in the `:panelopen` snapshot.
pub(crate) type PanelTarget = Option<(PathBuf, usize, usize)>;

/// Per-buffer LSP document-sync bookkeeping, mirroring `SyntaxState`. One per
/// open buffer that mapped to a configured server, keyed by [`BufferId`] in
/// [`Server::lsp_states`].
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
}

impl LspReqKind {
    fn as_u16(self) -> u16 {
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
        }
    }

    fn from_u16(value: u16) -> Option<Self> {
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
            _ => return None,
        })
    }

    /// Whether results always go to a panel location list (references) rather
    /// than jumping a lone result directly (the goto family).
    fn is_list(self) -> bool {
        matches!(self, LspReqKind::References)
    }

    /// The message shown when the server returns no result. The location-list
    /// kinds phrase it as "found"; hover/signatureHelp have their own wording but
    /// are handled off the location path, so these are their fallbacks too.
    fn empty_message(self) -> &'static str {
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
        }
    }

    /// The panel title for a multi-result location list.
    fn panel_title(self) -> &'static str {
        match self {
            LspReqKind::References => "LSP references",
            _ => "LSP locations",
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
}

/// The live insert-mode completion popup (Phase 5), server-owned like the
/// diagnostics cache. Held in [`Server::completion`]; `None` when no menu is
/// open. It keeps the server's last candidate list verbatim plus the live
/// filtered/ranked view, and the anchor the menu is pinned to, so each keystroke
/// re-ranks (or re-requests) in place rather than closing and reopening.
pub(crate) struct CompletionMenu {
    /// The buffer the menu belongs to; a reply for any other buffer is dropped.
    buffer: BufferId,
    /// `(row, word-start byte column)` the completion word begins at — the menu's
    /// screen anchor and the default replace range's start. Fixed while the menu
    /// is open (typing only extends the word to the right).
    anchor: (usize, usize),
    /// The identifier run typed since `anchor` (`line[anchor.col..cursor.col]`):
    /// both the filter string and, for an item without an explicit `textEdit`, the
    /// text the accept replaces.
    prefix: String,
    /// The server's `isIncomplete` for the current list: when set, a narrowing
    /// keystroke re-requests instead of filtering the cache client-side.
    is_incomplete: bool,
    /// The server's last candidate list, verbatim (ranking is recomputed from it).
    raw: Vec<CompletionItemData>,
    /// Indices into `raw`, filtered to those matching `prefix` and ordered by
    /// importance (match tier, then `sortText`, then label). The projected menu.
    visible: Vec<usize>,
    /// The selected entry as an index into `visible`, or `None` until the user
    /// navigates (accept then falls back to the first visible item).
    selected: Option<usize>,
}

impl Server {
    /// Apply one [`LspOp`] drained from the Lua runtime — a `vim.lsp.start` queued
    /// by user Lua (directly, or through the `vim.lsp.enable` `FileType`
    /// dispatcher). Ensures the `(name, root)` server exists and binds `bufnr` to
    /// it; the next [`Server::sync_lsp`] sends `didOpen`. Phase 7a's replacement
    /// for the built-in auto-spawn: a server starts *only* via this path.
    pub(crate) fn apply_lsp_op(&mut self, op: LspOp) {
        let start = match op {
            LspOp::Start { .. } => op,
            // `vim.lsp.buf.*` routes into the existing native request paths. No
            // cursor threading: `request_lsp` reads `self.editor.cursor` here, on
            // the same input tick the Lua keymap RHS fired.
            LspOp::BufRequest { kind } => {
                if let Some(kind) = LspReqKind::from_u16(kind) {
                    self.request_lsp(kind);
                }
                return;
            }
            LspOp::Format => {
                self.request_lsp_format();
                return;
            }
            LspOp::Rename { new_name } => {
                self.request_lsp_rename(&new_name);
                return;
            }
            LspOp::CodeAction => {
                self.request_lsp_code_action();
                return;
            }
            LspOp::DiagnosticGoto { forward, severity } => {
                self.diagnostic_goto(forward, severity);
                return;
            }
            LspOp::DiagnosticSetloclist => {
                match self.diagnostics_location_list() {
                    Some((lines, targets)) => {
                        self.editor.open_panel("LSP diagnostics", lines, false, 0);
                        self.editor.set_panel_targets(targets);
                    }
                    None => self.editor.echo("No diagnostics"),
                }
                return;
            }
            LspOp::DiagnosticConfig { underline } => {
                self.diagnostics_underline = underline;
                self.lsp_dirty = true;
                return;
            }
        };
        let LspOp::Start {
            name,
            cmd,
            root,
            filetype,
            bufnr,
            init_options,
            settings,
            capabilities,
        } = start
        else {
            unreachable!("non-Start ops returned above");
        };
        let buffer = BufferId(bufnr);
        // The buffer must be open and file-backed to host an LSP document.
        let Some(name_str) = self.editor.buffer_name(buffer).filter(|n| !n.is_empty()) else {
            return;
        };
        let path = PathBuf::from(&name_str);
        let Some(uri) = path_to_uri(&path) else {
            return;
        };
        // Root: `$NXVIM_LSP_ROOT` overrides (the test hook), else the root Lua
        // resolved, else the file's own directory. Rust never re-runs the marker
        // search — that is the config's job now (`vim.fs.root` in Lua).
        let root = lsp_root_override()
            .or_else(|| root.map(|r| absolutize(Path::new(&r))))
            .unwrap_or_else(|| {
                let abs = absolutize(&path);
                abs.parent().map(Path::to_path_buf).unwrap_or(abs)
            });
        let key = ServerKey { name, root };
        // Spawn command: `$NXVIM_LSP_CMD` overrides the whole argv (the mock hook,
        // the LSP analogue of `NXVIM_TS_WORKER`), else the config's `cmd`. An
        // empty command can't start a server. The resolved config's
        // settings/init_options/capabilities ride along so the manager forwards
        // them at `initialize` (Phase 2).
        let Some(mut spawn) = lsp_spawn(&cmd) else {
            return;
        };
        spawn.init_options = init_options;
        spawn.settings = settings;
        spawn.capabilities = capabilities;
        if !self.lsp_ensured.contains(&key) {
            self.lsp.ensure_server(key.clone(), spawn);
            self.lsp_ensured.insert(key.clone());
        }
        let state = self.lsp_states.entry(buffer).or_default();
        // Rebinding to a different server re-opens the document under it.
        if state.server.as_ref() != Some(&key) {
            state.opened = false;
            state.version = 0;
        }
        state.server = Some(key);
        state.language_id = filetype;
        state.uri = Some(uri);
        // Wake a sync so the bound buffer opens as soon as the server initializes.
        self.lsp_dirty = true;
    }

    /// Drive LSP document sync for the *current* buffer this frame: for a buffer a
    /// `vim.lsp.start` already bound to a server, send `didOpen`/`didChange`/
    /// `didSave` as its state requires. Called from `redraw()` alongside
    /// `sync_syntax`. Never spawns (that is [`Server::apply_lsp_op`]) and never
    /// blocks: every send is a fire-and-forget [`LspNotify`].
    pub(crate) fn sync_lsp(&mut self) {
        self.reap_closed_lsp_buffers();

        let buffer = self.editor.current_buffer_id();
        // Only buffers a `vim.lsp.start` bound to a server are synced — there is no
        // auto-start (Phase 7a: LSP startup is 100% user Lua).
        let Some(key) = self.lsp_states.get(&buffer).and_then(|s| s.server.clone()) else {
            return;
        };
        let Some(path) = self.editor.buffer().path.clone() else {
            return;
        };
        let Some(uri) = path_to_uri(&path) else {
            return;
        };

        // The encoding/sync kind aren't known until the server's `initialize`
        // reply lands (the `Initialized` event). Until then, just remember the
        // intended URI so the buffer opens as soon as it's ready.
        let Some(&ServerRuntime {
            encoding,
            sync_kind,
            client_id,
        }) = self.lsp_servers.get(&key)
        else {
            let state = self.lsp_states.entry(buffer).or_default();
            state.uri = Some(uri);
            return;
        };

        let cur_tick = self.editor.buffer().changedtick;
        let cur_save_tick = self.editor.buffer().save_tick;

        let mut state = self.lsp_states.remove(&buffer).unwrap_or_default();
        state.server = Some(key.clone());
        state.uri = Some(uri.clone());

        // A text change since the last sync (only meaningful once opened).
        let tick_changed = state.opened && cur_tick != state.last_tick;

        // Set when this sync is the buffer's first `didOpen` under the server: the
        // attach moment, so `LspAttach` fires once the state is re-inserted below.
        let mut just_attached = false;

        if !state.opened {
            // First open (or re-open after a respawn): full text supersedes any
            // journaled deltas, so drop the LSP journal (the syntax journal is
            // drained independently by `sync_syntax`).
            let _ = self.editor.buffer_mut().take_lsp_edits();
            let text = self.editor.buffer().text.to_string();
            state.version = 1;
            let language_id = state.language_id.clone();
            self.lsp.notify(
                key.clone(),
                LspNotify::DidOpen {
                    uri: uri.clone(),
                    language_id,
                    version: state.version,
                    text,
                },
            );
            state.opened = true;
            state.last_tick = cur_tick;
            // The freshly-opened content is the on-disk state, so don't fire a
            // spurious `didSave` for saves that predate the open.
            state.last_save_tick = cur_save_tick;
            just_attached = true;
        } else if tick_changed && sync_kind != TextDocumentSyncKind::NONE {
            let batch = self.editor.buffer_mut().take_lsp_edits();
            state.version += 1;
            // Full sync (server's choice, or a whole-rope replacement where deltas
            // are meaningless) sends the entire text; otherwise incremental deltas.
            let changes = if batch.resync || sync_kind == TextDocumentSyncKind::FULL {
                vec![TextDocumentContentChangeEvent {
                    range: None,
                    range_length: None,
                    text: self.editor.buffer().text.to_string(),
                }]
            } else {
                incremental_changes_in(self.editor.buffer(), &batch.edits, encoding)
            };
            self.lsp.notify(
                key.clone(),
                LspNotify::DidChange {
                    uri: uri.clone(),
                    version: state.version,
                    changes,
                },
            );
            state.last_tick = cur_tick;
        }

        // Save: the buffer's write counter advanced since the last sync, so a `:w`
        // landed bytes on disk (a real hook, not a `modified`-flag heuristic).
        if state.opened && cur_save_tick != state.last_save_tick {
            self.lsp.notify(key, LspNotify::DidSave { uri, text: None });
            state.last_save_tick = cur_save_tick;
        }

        self.lsp_states.insert(buffer, state);

        // The attach hook fires after the state is back in the map (so an
        // `on_attach` that re-enters the LSP paths sees a consistent state): the
        // buffer just sent its first `didOpen` under an initialized server — the
        // attach moment. `sync_lsp` only ever syncs the current buffer, so the
        // snapshot the autocmd reads is this buffer.
        if just_attached {
            let file = path.to_string_lossy().into_owned();
            self.fire_lsp_attach(buffer, &file, client_id);
        }
    }

    /// Fire `LspAttach` for the just-attached current buffer with the server's
    /// `client_id` as `args.data.client_id`. Pushes the buffer snapshot first (so
    /// the callback resolves the buffer), then folds in the Lua effects the
    /// `on_attach` left — buffer-local keymaps it set bump the keymap version and
    /// are picked up on the next input. Mirrors [`Server::fire_lifecycle`].
    fn fire_lsp_attach(&mut self, buf: BufferId, file: &str, client_id: u64) {
        let ft = self
            .lsp_states
            .get(&buf)
            .map(|s| s.language_id.clone())
            .unwrap_or_default();
        let _ = self.lua.set_buf_snapshot(buf.0, file, &ft);
        if let Err(e) = self
            .lua
            .fire_autocmd_data("LspAttach", file, buf.0, file, client_id)
        {
            self.editor
                .echo(format!("E5108: Error in LspAttach autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Fire `LspDetach` for `buf` with `client_id` as `args.data.client_id` — the
    /// detach counterpart to [`Server::fire_lsp_attach`]. Unlike attach it does
    /// not push a buffer snapshot: detach fires for a buffer being closed
    /// (`didClose`) or a server that exited, neither of which is necessarily the
    /// current buffer. User `LspDetach` callbacks still get `args.buf`/`data`.
    fn fire_lsp_detach(&mut self, buf: BufferId, file: &str, client_id: u64) {
        if let Err(e) = self
            .lua
            .fire_autocmd_data("LspDetach", file, buf.0, file, client_id)
        {
            self.editor
                .echo(format!("E5108: Error in LspDetach autocmd: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Send `didClose` for, and forget the state of, every buffer the editor has
    /// since deleted (`:bdelete`) — the LSP analogue of `reap_closed_buffers`.
    fn reap_closed_lsp_buffers(&mut self) {
        let live = self.editor.buffer_ids();
        let dead: Vec<BufferId> = self
            .lsp_states
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in dead {
            if let Some(state) = self.lsp_states.remove(&id) {
                if let (true, Some(key), Some(uri)) = (state.opened, state.server, state.uri) {
                    // Fire `LspDetach` (symmetric with attach-on-`didOpen`) before
                    // the close goes out, while the runtime — and so the client id —
                    // is still around.
                    if let Some(client_id) = self.lsp_servers.get(&key).map(|r| r.client_id) {
                        let file = uri_to_path(&uri)
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        self.fire_lsp_detach(id, &file, client_id);
                    }
                    self.lsp.notify(key, LspNotify::DidClose { uri });
                }
            }
        }
    }

    /// Handle one [`LspEvent`] drained from the manager. Phase 1 acts on
    /// `Initialized` (record encoding/caps, schedule re-open), caches
    /// `Diagnostics`, logs server messages, and tolerates exits (the manager
    /// respawns or gives up; buffers stay editable).
    pub(crate) fn on_lsp_event(&mut self, event: LspEvent) {
        match event {
            LspEvent::Initialized {
                key,
                caps,
                encoding,
                init_result,
            } => {
                // Assign a client id once per server, reused across respawns so the
                // `client_id` Lua sees stays stable (and `vim.lsp._clients` isn't
                // leaked one entry per restart).
                let client_id = self
                    .lsp_servers
                    .get(&key)
                    .map(|r| r.client_id)
                    .unwrap_or_else(|| {
                        let id = self.next_lsp_client_id;
                        self.next_lsp_client_id += 1;
                        id
                    });
                self.lsp_servers.insert(
                    key.clone(),
                    ServerRuntime {
                        encoding,
                        sync_kind: caps.sync_kind,
                        client_id,
                    },
                );
                // Mirror the client into `vim.lsp._clients[id]` so `on_attach` can
                // read `client.server_capabilities` once `LspAttach` resolves it.
                let client = LspClientData {
                    id: client_id,
                    name: key.name.clone(),
                    capabilities: provider_caps_to_lua(&caps.providers),
                };
                let _ = self.lua.set_lsp_client(&client);
                // Run the config's on_init(client, result) hook (Phase 3) now that
                // the client is mirrored — it can read what the server advertised.
                if let Err(e) = self.lua.run_lsp_on_init(client_id, &init_result) {
                    self.editor
                        .echo(format!("E5108: Error in LSP on_init: {e}"));
                }
                // A fresh (or respawned) server holds no documents: re-open every
                // buffer bound to it on the next sync. This doubles as the restart
                // handler, like `SyntaxEvent::Restarted`.
                for state in self.lsp_states.values_mut() {
                    if state.server.as_ref() == Some(&key) {
                        state.opened = false;
                        state.version = 0;
                    }
                }
                self.lsp_dirty = true;
            }
            LspEvent::Diagnostics {
                uri, diagnostics, ..
            } => {
                // Cache the latest publish for the matching buffer; the redraw
                // projects whichever buffer is current (route by `uri`, dropping
                // a publish for a buffer closed while it was in flight, as
                // `store_spans` drops unknown-buffer syntax replies). Mark dirty
                // so the coalesced repaint paints the new squiggles.
                let mirror = self
                    .lsp_states
                    .iter_mut()
                    .find(|(_, s)| s.uri.as_ref() == Some(&uri))
                    .map(|(id, state)| {
                        state.diagnostics = diagnostics;
                        (id.0, diagnostic_mirror_data(&state.diagnostics))
                    });
                if let Some((bufnr, data)) = mirror {
                    self.lsp_dirty = true;
                    // Mirror into `vim._diagnostics[bufnr]` so the synchronous
                    // `vim.diagnostic.get` (Slice 2) can read it from pure Lua.
                    let _ = self.lua.set_diagnostics(bufnr, &data);
                }
            }
            LspEvent::Reply { token, reply, .. } => self.on_lsp_reply(token, reply),
            LspEvent::ServerExited {
                key, code, signal, ..
            } => {
                // The manager respawns per its breaker (or gives up cleanly); the
                // editor stays fully responsive throughout. Detach every buffer the
                // dead server held — symmetric with attach-on-`didOpen` — and drop
                // its runtime + Lua client. A respawn re-`initialize`s into a fresh
                // client id and re-attaches (neovim treats a restart as a new
                // client), so this is also the restart's detach half.
                if let Some(client_id) = self.lsp_servers.remove(&key).map(|r| r.client_id) {
                    // Buffers attached to this server, with a display name for the
                    // event's `args.file`. Clear `opened` so a later `:bdelete`
                    // doesn't re-fire `LspDetach`, and so a respawn re-`didOpen`s.
                    let detaching: Vec<(BufferId, String)> = self
                        .lsp_states
                        .iter_mut()
                        .filter(|(_, s)| s.opened && s.server.as_ref() == Some(&key))
                        .map(|(id, s)| {
                            s.opened = false;
                            s.version = 0;
                            let file = s
                                .uri
                                .as_ref()
                                .and_then(uri_to_path)
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            (*id, file)
                        })
                        .collect();
                    for (id, file) in detaching {
                        self.fire_lsp_detach(id, &file, client_id);
                    }
                    // Run the config's on_exit(code, signal, client) hook (Phase 3)
                    // while the client is still registered, then forget it.
                    if let Err(e) = self.lua.run_lsp_on_exit(client_id, code, signal) {
                        self.editor
                            .echo(format!("E5108: Error in LSP on_exit: {e}"));
                    }
                    let _ = self.lua.remove_lsp_client(client_id);
                }
            }
            LspEvent::Log { message, .. } => {
                // Record to `:messages` without disturbing the message line.
                self.editor.messages.push(message);
            }
        }
    }

    /// Build the `:LspInfo` report: the current buffer's server/encoding/sync/
    /// version/diagnostics, then every running server and every attached buffer.
    /// Phase-1 observability while there is no on-screen LSP feature yet.
    pub(crate) fn lsp_info_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let current = self.editor.current_buffer_id();

        lines.push("Current buffer".to_string());
        match self.lsp_states.get(&current).filter(|s| s.server.is_some()) {
            Some(state) => {
                let key = state.server.as_ref().unwrap();
                let runtime = self.lsp_servers.get(key);
                lines.push(format!(
                    "  server:      {} ({})",
                    key.name,
                    key.root.display()
                ));
                lines.push(format!(
                    "  status:      {}",
                    if !self.lsp_ensured.contains(key) {
                        "not started"
                    } else if runtime.is_none() {
                        "starting (awaiting initialize)"
                    } else if state.opened {
                        "attached"
                    } else {
                        "initialized (didOpen pending)"
                    }
                ));
                if let Some(runtime) = runtime {
                    lines.push(format!(
                        "  encoding:    {}    sync: {}",
                        encoding_label(runtime.encoding),
                        sync_label(runtime.sync_kind),
                    ));
                }
                lines.push(format!("  version:     {}", state.version));
                lines.push(format!("  diagnostics: {}", state.diagnostics.len()));
            }
            None => lines.push("  (no language server for this buffer)".to_string()),
        }

        lines.push(String::new());
        lines.push("Running servers".to_string());
        if self.lsp_servers.is_empty() {
            lines.push("  (none)".to_string());
        } else {
            let mut servers: Vec<_> = self.lsp_servers.iter().collect();
            servers.sort_by_key(|(k, _)| (k.name.clone(), k.root.clone()));
            for (key, runtime) in servers {
                let attached = self
                    .lsp_states
                    .values()
                    .filter(|s| s.opened && s.server.as_ref() == Some(key))
                    .count();
                lines.push(format!(
                    "  {} @ {} — {}, {}, {attached} buffer(s)",
                    key.name,
                    key.root.display(),
                    encoding_label(runtime.encoding),
                    sync_label(runtime.sync_kind),
                ));
            }
        }

        lines.push(String::new());
        lines.push(format!(
            "Log: {}",
            std::env::var("NXVIM_LSP_LOG_FILE").unwrap_or_else(|_| {
                "$XDG_STATE_HOME/nxvim/lsp.log (or ~/.local/state/nxvim/lsp.log)".to_string()
            })
        ));
        lines
    }

    /// The current buffer's cached diagnostics together with its server's
    /// negotiated position encoding, or `None` when the buffer has no attached
    /// server (so callers project nothing). Both borrows are released before any
    /// `&mut self` use.
    fn current_diagnostics(&self) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
        let state = self.lsp_states.get(&self.editor.current_buffer_id())?;
        let key = state.server.as_ref()?;
        let encoding = self.lsp_servers.get(key)?.encoding;
        Some((&state.diagnostics, encoding))
    }

    /// Build the per-row `diagnostics` redraw payload from a row→buffer-line
    /// mapping (`numbers`, 1-based, `None` for filler): each visible row's
    /// diagnostic underline spans as `[start_col, end_col, severity, style_id]`
    /// in **screen columns**. Mirrors [`Server::highlights_for`] — the LSP
    /// character offsets are converted to bytes through the negotiated encoding,
    /// then bytes to screen columns with the same tab/wide-char `virtcol` the
    /// highlights and selection use, so squiggles line up with the glyphs.
    /// `severity` is `1`=error … `4`=hint; `style_id` indexes the per-frame
    /// `styles` palette when the matching `DiagnosticUnderline*` group resolves
    /// through the registry (`Nil` otherwise, so the client falls back to a
    /// built-in severity color).
    pub(crate) fn diagnostics_for(
        &self,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        // `vim.diagnostic.config({ underline = false })` hides the squiggles; the
        // message line and the location list (other surfaces) are unaffected.
        let diags_encoding = if self.diagnostics_underline {
            self.current_diagnostics()
        } else {
            None
        };
        let Some((diags, encoding)) = diags_encoding else {
            // One empty entry per row so the client's `diagnostics[row]` index
            // stays aligned with `highlights`/`numbers`.
            return Value::Array(numbers.iter().map(|_| Value::Array(Vec::new())).collect());
        };
        let rows = numbers
            .iter()
            .map(|num| {
                let Some(n) = num else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let text = self.editor.buffer().line(line_idx);
                let spans = diags
                    .iter()
                    .filter_map(|d| {
                        let (start_byte, end_byte) =
                            self.diag_row_span(d, encoding, line_idx, &text)?;
                        let start_col = unicode::virtcol(&text, start_byte, unicode::TABSTOP);
                        let mut end_col = unicode::virtcol(&text, end_byte, unicode::TABSTOP);
                        // A zero-width range (e.g. an empty span at end-of-line)
                        // still needs one underlined cell to be visible.
                        if end_col <= start_col {
                            end_col = start_col + 1;
                        }
                        let severity = severity_code(d.severity);
                        let style_id =
                            match self.editor.highlights.resolve(severity_group(severity)) {
                                Some(style) => Value::from(styles.intern(style) as u64),
                                None => Value::Nil,
                            };
                        Some(Value::Array(vec![
                            Value::from(start_col as u64),
                            Value::from(end_col as u64),
                            Value::from(severity as u64),
                            style_id,
                        ]))
                    })
                    .collect();
                Value::Array(spans)
            })
            .collect();
        Value::Array(rows)
    }

    /// The message of the highest-severity diagnostic whose range covers the
    /// cursor, for the message line (shown only when no other message is set, so
    /// `:messages` history stays clean). `None` when the cursor is on no
    /// diagnostic. Newlines are flattened so it fits one line.
    pub(crate) fn diagnostic_under_cursor(&self) -> Option<String> {
        let (diags, encoding) = self.current_diagnostics()?;
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        diags
            .iter()
            .filter(|d| {
                self.diag_row_span(d, encoding, row, &line)
                    // Cover the resting cell of a zero-width range too.
                    .is_some_and(|(s, e)| col >= s && col < e.max(s + 1))
            })
            .min_by_key(|d| severity_code(d.severity))
            .map(|d| first_line(&d.message))
    }

    /// The `[start, end)` **byte** span a diagnostic occupies on buffer row
    /// `line_idx` (whose text is `line`), or `None` if it does not reach that
    /// row. Multi-line ends are clipped to the row: `0` before the range's first
    /// line, the line length after its last. The LSP character offsets are
    /// converted to bytes through the negotiated `encoding` (Decision 4).
    fn diag_row_span(
        &self,
        d: &Diagnostic,
        encoding: PositionEncoding,
        line_idx: usize,
        line: &str,
    ) -> Option<(usize, usize)> {
        let (s, e) = (d.range.start, d.range.end);
        let row = line_idx as u32;
        if row < s.line || row > e.line {
            return None;
        }
        let start = if s.line == row {
            byte_col(encoding, line, s.character as usize)
        } else {
            0
        };
        let end = if e.line == row {
            byte_col(encoding, line, e.character as usize)
        } else {
            line.len()
        };
        Some((start, end))
    }

    /// Build the `:LspDiagnostics` location list for the current buffer: one
    /// `severity  line:col  message` row per diagnostic (sorted by position) and
    /// a parallel [`PanelTarget`] list to attach as the panel's jump targets.
    /// `None` when the buffer has no diagnostics.
    pub(crate) fn diagnostics_location_list(&self) -> Option<(Vec<String>, Vec<PanelTarget>)> {
        let (diags, encoding) = self.current_diagnostics()?;
        if diags.is_empty() {
            return None;
        }
        let path = self.editor.buffer().path.clone();
        let mut items: Vec<&Diagnostic> = diags.iter().collect();
        items.sort_by_key(|d| (d.range.start.line, d.range.start.character));
        let mut lines = Vec::with_capacity(items.len());
        let mut targets = Vec::with_capacity(items.len());
        for d in items {
            let row = d.range.start.line as usize;
            let character = d.range.start.character as usize;
            lines.push(format!(
                "{}  {}:{}  {}",
                severity_short(severity_code(d.severity)),
                row + 1,
                character + 1,
                first_line(&d.message),
            ));
            let line = self.editor.buffer().line(row);
            let byte = byte_col(encoding, &line, character);
            targets.push(path.clone().map(|p| (p, row, byte)));
        }
        Some((lines, targets))
    }

    /// `vim.diagnostic.goto_next`/`goto_prev`: move the cursor to the next
    /// (`forward`) or previous diagnostic in the current buffer, wrapping around
    /// the ends. `severity` (1=ERROR…4=HINT) restricts the set when set. A no-op
    /// when the buffer has no (matching) diagnostics. Reuses the same byte-column
    /// conversion the underline path uses, then `jump_to`s the *current* file so
    /// the move snaps to a valid resting cell (no file open — same buffer).
    pub(crate) fn diagnostic_goto(&mut self, forward: bool, severity: Option<u8>) {
        let Some((diags, encoding)) = self.current_diagnostics() else {
            return;
        };
        // Resolve every (matching) diagnostic to a 0-based (line, byte col) and
        // sort by position, so "next/previous from the cursor" is a list walk.
        let mut positions: Vec<(usize, usize)> = diags
            .iter()
            .filter(|d| severity.map_or(true, |s| severity_code(d.severity) == s))
            .map(|d| {
                let row = d.range.start.line as usize;
                let line = self.editor.buffer().line(row);
                (
                    row,
                    byte_col(encoding, &line, d.range.start.character as usize),
                )
            })
            .collect();
        if positions.is_empty() {
            return;
        }
        positions.sort_unstable();
        positions.dedup();

        let cur = (self.editor.cursor.line, self.editor.cursor.col);
        // The next strictly-after (forward) or strictly-before (backward) target,
        // wrapping to the first/last when the cursor is past the last/before the
        // first — neovim's `goto_next`/`goto_prev` wrap behavior.
        let target = if forward {
            positions
                .iter()
                .find(|&&p| p > cur)
                .copied()
                .unwrap_or(positions[0])
        } else {
            positions
                .iter()
                .rev()
                .find(|&&p| p < cur)
                .copied()
                .unwrap_or(positions[positions.len() - 1])
        };

        let (line, byte) = target;
        if let Some(path) = self.editor.buffer().path.clone() {
            self.editor.jump_to(&path, line, byte);
        }
    }

    // ----- Phase 3: go-to definition / references --------------------------

    /// Issue a language-feature request of `kind` at the cursor, recording its
    /// generation so a stale reply is dropped (Decision 3): a newer request of
    /// the same kind, or the cursor moving before the reply lands, invalidates
    /// it. No-op (with a brief message) if the current buffer has no server that
    /// has finished `initialize`, since the negotiated encoding isn't known yet.
    pub(crate) fn request_lsp(&mut self, kind: LspReqKind) {
        // Flush any pending document edits as a `didChange` *before* the request,
        // so the server computes against the current buffer text. Requests are
        // fired during input — ahead of `redraw`'s own `sync_lsp` — so without
        // this the server would answer a stale document (e.g. completion ranges
        // computed against text the user already changed).
        self.sync_lsp();
        let Some((key, uri, encoding)) = self.current_lsp_target() else {
            self.editor.echo("No language server attached");
            return;
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let position = self.lsp_position(encoding, row, col);
        let token = self.register_lsp_request(kind);
        let req = match kind {
            LspReqKind::Definition => LspRequest::Definition { uri, position },
            LspReqKind::Declaration => LspRequest::Declaration { uri, position },
            LspReqKind::TypeDefinition => LspRequest::TypeDefinition { uri, position },
            LspReqKind::Implementation => LspRequest::Implementation { uri, position },
            LspReqKind::References => LspRequest::References {
                uri,
                position,
                include_declaration: false,
            },
            LspReqKind::Hover => LspRequest::Hover { uri, position },
            LspReqKind::SignatureHelp => LspRequest::SignatureHelp { uri, position },
            LspReqKind::Completion => LspRequest::Completion { uri, position },
            // Formatting/rename/codeAction(+resolve) don't share the uniform
            // {uri, position} shape and have their own issue functions below.
            LspReqKind::Formatting
            | LspReqKind::Rename
            | LspReqKind::CodeAction
            | LspReqKind::ResolveCodeAction => return,
        };
        self.lsp.request(key, token, req);
    }

    /// Bump the request generation and register the in-flight request for `kind`
    /// (buffer/cursor/`changedtick` at issue time), returning its [`ReqToken`].
    /// The single home for the staleness bookkeeping every issue function shares.
    fn register_lsp_request(&mut self, kind: LspReqKind) -> ReqToken {
        self.lsp_req_gen += 1;
        let generation = self.lsp_req_gen;
        self.lsp_requests.insert(
            kind,
            PendingLspReq {
                generation,
                buffer: self.editor.current_buffer_id(),
                cursor: (self.editor.cursor.line, self.editor.cursor.col),
                tick: self.editor.buffer().changedtick,
            },
        );
        ReqToken {
            kind: kind.as_u16(),
            generation,
        }
    }

    // ----- Phase 6: formatting / rename / code action ----------------------

    /// `:LspFormat` — request `textDocument/formatting` for the current buffer.
    /// On reply, the `TextEdit[]` is applied iff the buffer hasn't changed since
    /// (the content-version guard in [`Server::on_lsp_reply`]).
    pub(crate) fn request_lsp_format(&mut self) {
        self.sync_lsp();
        let Some((key, uri, _encoding)) = self.current_lsp_target() else {
            self.editor.echo("No language server attached");
            return;
        };
        let token = self.register_lsp_request(LspReqKind::Formatting);
        self.lsp.request(key, token, LspRequest::Formatting { uri });
    }

    /// `:LspRename {newname}` — request `textDocument/rename` at the cursor with
    /// the new name. On reply the returned `WorkspaceEdit` is applied across the
    /// open buffers it touches.
    pub(crate) fn request_lsp_rename(&mut self, new_name: &str) {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            self.editor
                .echo("E471: Argument required: :LspRename {newname}");
            return;
        }
        self.sync_lsp();
        let Some((key, uri, encoding)) = self.current_lsp_target() else {
            self.editor.echo("No language server attached");
            return;
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let position = self.lsp_position(encoding, row, col);
        let token = self.register_lsp_request(LspReqKind::Rename);
        self.lsp.request(
            key,
            token,
            LspRequest::Rename {
                uri,
                position,
                new_name: new_name.to_string(),
            },
        );
    }

    /// `:LspCodeAction` — request `textDocument/codeAction` at the cursor, passing
    /// the diagnostics under the cursor as context. On reply the action titles are
    /// listed in the panel; `<CR>` applies the chosen action's eager edit.
    pub(crate) fn request_lsp_code_action(&mut self) {
        self.sync_lsp();
        let Some((key, uri, encoding)) = self.current_lsp_target() else {
            self.editor.echo("No language server attached");
            return;
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let position = self.lsp_position(encoding, row, col);
        let diagnostics = self.diagnostics_at_cursor();
        let token = self.register_lsp_request(LspReqKind::CodeAction);
        self.lsp.request(
            key,
            token,
            LspRequest::CodeAction {
                uri,
                // A point range at the cursor; a visual-selection range is a
                // follow-up (needs the selection extent threaded through).
                range: Range {
                    start: position,
                    end: position,
                },
                diagnostics,
            },
        );
    }

    /// The cached diagnostics whose range covers the cursor, cloned as the
    /// `context.diagnostics` for a code-action request (empty when none / no
    /// server). They are already in the server's negotiated encoding, as the
    /// server sent them.
    fn diagnostics_at_cursor(&self) -> Vec<Diagnostic> {
        let Some((diags, encoding)) = self.current_diagnostics() else {
            return Vec::new();
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        diags
            .iter()
            .filter(|d| {
                self.diag_row_span(d, encoding, row, &line)
                    .is_some_and(|(s, e)| col >= s && col < e.max(s + 1))
            })
            .cloned()
            .collect()
    }

    /// The current buffer's `(server, uri, encoding)` once its server finished
    /// `initialize` (so the negotiated encoding is known) — the precondition for
    /// any position-based request. `None` otherwise.
    fn current_lsp_target(&self) -> Option<(ServerKey, Url, PositionEncoding)> {
        let state = self.lsp_states.get(&self.editor.current_buffer_id())?;
        let key = state.server.clone()?;
        let uri = state.uri.clone()?;
        let encoding = self.lsp_servers.get(&key)?.encoding;
        Some((key, uri, encoding))
    }

    /// Handle one [`LspEvent::Reply`]: match it to its pending request by token
    /// and act, dropping it when a newer request of the same kind superseded it
    /// (generation mismatch). The goto/hover/signature kinds also drop on a cursor
    /// move (Decision 3) — the user moved on, so the jump/popup is now irrelevant.
    /// Completion is the exception: its menu *follows* the moving cursor while
    /// open (each typed character may re-request), so it is dropped only when the
    /// buffer changed — staleness there is the generation token's job alone.
    pub(crate) fn on_lsp_reply(&mut self, token: ReqToken, reply: LspReply) {
        let Some(kind) = LspReqKind::from_u16(token.kind) else {
            return;
        };
        let Some(pending) = self.lsp_requests.get(&kind) else {
            return;
        };
        // A newer request of this kind is now in flight: this reply is stale.
        if pending.generation != token.generation {
            return;
        }
        let buffer_changed = pending.buffer != self.editor.current_buffer_id();
        let cursor_moved = pending.cursor != (self.editor.cursor.line, self.editor.cursor.col);
        // An apply reply (formatting/rename/codeAction) carries whole-document
        // edits computed against the request-time text, so a content change since
        // then must drop it — applying stale edits would corrupt the buffer. A
        // mere cursor move is fine to apply over.
        let tick_changed = pending.tick != self.editor.buffer().changedtick;
        self.lsp_requests.remove(&kind);

        match reply {
            LspReply::Completion {
                is_incomplete,
                items,
            } => {
                if buffer_changed {
                    return;
                }
                self.on_completion_reply(is_incomplete, items);
            }
            LspReply::Locations(locations) => {
                if buffer_changed || cursor_moved {
                    return;
                }
                self.apply_lsp_locations(kind, locations);
                self.lsp_dirty = true;
            }
            LspReply::Hover(lines) => {
                if buffer_changed || cursor_moved {
                    return;
                }
                self.show_hover(lines);
                self.lsp_dirty = true;
            }
            LspReply::SignatureHelp {
                signature,
                active_parameter,
            } => {
                if buffer_changed || cursor_moved {
                    return;
                }
                self.show_signature_help(signature, active_parameter);
                self.lsp_dirty = true;
            }
            LspReply::Edits(edits) => {
                if buffer_changed || tick_changed {
                    return;
                }
                self.apply_formatting_edits(edits);
                self.lsp_dirty = true;
            }
            LspReply::WorkspaceEdit(changes) => {
                if buffer_changed || tick_changed {
                    return;
                }
                self.apply_workspace_edit(changes);
                self.lsp_dirty = true;
            }
            LspReply::CodeActions(actions) => {
                if buffer_changed || tick_changed {
                    return;
                }
                self.show_code_actions(actions);
                self.lsp_dirty = true;
            }
            LspReply::ResolvedCodeAction(edit) => {
                if buffer_changed || tick_changed {
                    return;
                }
                match edit {
                    Some(changes) => self.apply_workspace_edit(changes),
                    None => self
                        .editor
                        .echo(LspReqKind::ResolveCodeAction.empty_message()),
                }
                self.lsp_dirty = true;
            }
        }
    }

    /// Render a hover reply: open the bottom panel with the markup's plain lines
    /// (the panel is the hover surface until floats exist — Decision 7). An empty
    /// reply shows a brief message instead of an empty panel.
    fn show_hover(&mut self, lines: Vec<String>) {
        if lines.is_empty() {
            self.editor.echo(LspReqKind::Hover.empty_message());
            return;
        }
        self.editor.open_panel("LSP hover", lines, false, 0);
    }

    /// Render a signature-help reply on the message line: the active signature's
    /// label, with its active parameter appended in brackets when known (a plain
    /// message line can't style the parameter inline). Triggered manually in
    /// insert mode, so it stays out of the way until asked for. Empty ⇒ a brief
    /// message.
    fn show_signature_help(&mut self, signature: Option<String>, active_parameter: Option<String>) {
        let Some(signature) = signature else {
            self.editor.echo(LspReqKind::SignatureHelp.empty_message());
            return;
        };
        let message = match active_parameter {
            Some(param) if !param.is_empty() => format!("{signature}    [{param}]"),
            _ => signature,
        };
        self.editor.echo(message);
    }

    /// Act on a reply's target locations: a single goto result jumps the cursor;
    /// references (or multiple goto results) open a select-enabled panel location
    /// list; an empty result shows a brief message. The encoding is captured from
    /// the *source* buffer before any jump switches buffers — a server reports
    /// target positions in its own negotiated encoding.
    fn apply_lsp_locations(&mut self, kind: LspReqKind, locations: Vec<Location>) {
        if locations.is_empty() {
            self.editor.echo(kind.empty_message());
            return;
        }
        let encoding = self
            .current_lsp_target()
            .map_or(PositionEncoding::Utf8, |(_, _, e)| e);
        if !kind.is_list() && locations.len() == 1 {
            self.jump_to_lsp_location(&locations[0], encoding);
        } else {
            self.open_locations_panel(kind, &locations, encoding);
        }
    }

    /// Jump the cursor to one LSP [`Location`]. Opens/switches to the target on
    /// its line first, then refines the column once the line text is loaded (the
    /// char→byte conversion needs the target line, which may live in a file just
    /// opened by the jump). The second `jump_to` finds the buffer already current
    /// and only moves the cursor, so the alternate `#` is recorded exactly once.
    fn jump_to_lsp_location(&mut self, loc: &Location, encoding: PositionEncoding) {
        let Some(path) = uri_to_path(&loc.uri) else {
            return;
        };
        let line = loc.range.start.line as usize;
        let character = loc.range.start.character as usize;
        self.editor.jump_to(&path, line, 0);
        if self.editor.buffer().path.as_deref() == Some(path.as_path()) {
            let landed = self.editor.cursor.line;
            let text = self.editor.buffer().line(landed);
            let byte = byte_col(encoding, &text, character);
            self.editor.jump_to(&path, landed, byte);
        }
    }

    /// Open a navigable panel listing `locations` (`path:line:col` per row), with
    /// a per-row jump target attached so `<CR>` navigates — the same panel
    /// machinery the `:LspDiagnostics` list uses.
    fn open_locations_panel(
        &mut self,
        kind: LspReqKind,
        locations: &[Location],
        encoding: PositionEncoding,
    ) {
        let mut lines = Vec::with_capacity(locations.len());
        let mut targets: Vec<PanelTarget> = Vec::with_capacity(locations.len());
        for loc in locations {
            let Some(path) = uri_to_path(&loc.uri) else {
                continue;
            };
            let row = loc.range.start.line as usize;
            let character = loc.range.start.character as usize;
            let byte = self.location_byte_col(&path, row, character, encoding);
            lines.push(format!("{}:{}:{}", path.display(), row + 1, byte + 1));
            targets.push(Some((path, row, byte)));
        }
        if targets.is_empty() {
            self.editor.echo(kind.empty_message());
            return;
        }
        self.editor.open_panel(kind.panel_title(), lines, false, 0);
        self.editor.set_panel_targets(targets);
    }

    /// Best-effort LSP char→byte column for a target location: exact when the
    /// target is the current buffer (its line text is in hand); otherwise the raw
    /// character, which is already the byte offset under the UTF-8 encoding the
    /// goto path negotiates. `jump_to` clamps to the line, so an over-long value
    /// lands at end-of-line rather than panicking.
    fn location_byte_col(
        &self,
        path: &Path,
        row: usize,
        character: usize,
        encoding: PositionEncoding,
    ) -> usize {
        if self.editor.buffer().path.as_deref() == Some(path) {
            byte_col(encoding, &self.editor.buffer().line(row), character)
        } else {
            character
        }
    }

    // ----- Phase 5: completion (the popup menu) ----------------------------

    /// Handle a `textDocument/completion` reply (already past the generation /
    /// buffer staleness checks in [`Server::on_lsp_reply`]). Builds the menu on
    /// the initial trigger, or replaces its list on a live re-request, then
    /// re-ranks against the current prefix. Dropped if the user has left insert
    /// mode (the menu is unwanted) or nothing matches (nothing to show).
    fn on_completion_reply(&mut self, is_incomplete: bool, items: Vec<CompletionItemData>) {
        if self.editor.mode != Mode::Insert {
            return;
        }
        let buffer = self.editor.current_buffer_id();
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        let (word_start, prefix) = completion_word(&line, col);
        match self.completion.as_mut() {
            // A refresh for the open menu: swap in the new list, re-rank in place.
            Some(menu) => {
                menu.raw = items;
                menu.is_incomplete = is_incomplete;
                menu.anchor = (row, word_start);
                menu.prefix = prefix;
            }
            // The initial trigger opens the menu; an empty offer opens nothing.
            None => {
                if items.is_empty() {
                    return;
                }
                self.completion = Some(CompletionMenu {
                    buffer,
                    anchor: (row, word_start),
                    prefix,
                    is_incomplete,
                    raw: items,
                    visible: Vec::new(),
                    selected: None,
                });
            }
        }
        self.rerank_menu();
        // Nothing matches what was typed: dismiss rather than show an empty popup.
        if self
            .completion
            .as_ref()
            .is_some_and(|m| m.visible.is_empty())
        {
            self.completion = None;
        }
        self.lsp_dirty = true;
    }

    /// Recompute the menu's `visible` list: filter `raw` to the items matching the
    /// live `prefix` and order them by importance — match tier (exact ▸ prefix ▸
    /// subsequence), then the server's `sortText`, then the label as a stable
    /// tiebreak. Clears the selection, since the candidate set changed.
    fn rerank_menu(&mut self) {
        let Some(menu) = self.completion.as_mut() else {
            return;
        };
        let prefix = menu.prefix.as_str();
        let mut ranked: Vec<(u8, &str, &str, usize)> = menu
            .raw
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let filter = item.filter_text.as_deref().unwrap_or(&item.label);
                let tier = match_tier(filter, prefix)?;
                let secondary = item.sort_text.as_deref().unwrap_or(&item.label);
                Some((tier, secondary, item.label.as_str(), i))
            })
            .collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)).then(a.2.cmp(b.2)));
        let visible: Vec<usize> = ranked.into_iter().map(|(_, _, _, i)| i).collect();
        menu.visible = visible;
        menu.selected = None;
    }

    /// Whether a completion menu is currently open (the insert-mode key path
    /// checks this before routing a key to the menu).
    pub(crate) fn completion_menu_open(&self) -> bool {
        self.completion.is_some()
    }

    /// Move the menu selection by `delta`, wrapping. From no selection, `+1`
    /// highlights the first item and `-1` the last (vim's `<C-n>`/`<C-p>`).
    pub(crate) fn lsp_menu_move(&mut self, delta: isize) {
        let Some(menu) = self.completion.as_mut() else {
            return;
        };
        let n = menu.visible.len();
        if n == 0 {
            return;
        }
        menu.selected = Some(match menu.selected {
            None => {
                if delta > 0 {
                    0
                } else {
                    n - 1
                }
            }
            Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
        });
        self.lsp_dirty = true;
    }

    /// Close the menu without inserting, dropping any in-flight completion request
    /// so a late reply can't reopen it.
    pub(crate) fn lsp_menu_close(&mut self) {
        if self.completion.take().is_some() {
            self.lsp_requests.remove(&LspReqKind::Completion);
            self.lsp_dirty = true;
        }
    }

    /// Accept the selected item (or the first, when nothing is highlighted):
    /// replace the completion word — or the item's explicit `textEdit` range —
    /// with its text, apply any `additionalTextEdits`, and leave the cursor after
    /// the inserted text, all as one undo step. Stays in insert mode, as vim does.
    pub(crate) fn lsp_menu_accept(&mut self) {
        let Some(menu) = self.completion.take() else {
            return;
        };
        // No refresh should land after an accept.
        self.lsp_requests.remove(&LspReqKind::Completion);
        self.lsp_dirty = true;
        let Some(&raw_idx) = menu.visible.get(menu.selected.unwrap_or(0)) else {
            return;
        };
        let item = &menu.raw[raw_idx];
        let encoding = self
            .current_lsp_target()
            .map_or(PositionEncoding::Utf8, |(_, _, e)| e);

        // The primary edit: the item's explicit textEdit, else replace the word
        // (anchor..cursor) with insertText (falling back to the label).
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let (primary_range, primary_text) = match &item.text_edit {
            Some(edit) => (
                self.lsp_range_to_bytes(&edit.range, encoding),
                edit.new_text.clone(),
            ),
            None => {
                let start = self.editor.buffer().line_start(menu.anchor.0) + menu.anchor.1;
                let end = self.editor.buffer().line_start(row) + col;
                let text = item
                    .insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone());
                (start..end, text)
            }
        };

        let mut edits = vec![(primary_range.clone(), primary_text.clone())];
        for ate in &item.additional_text_edits {
            edits.push((
                self.lsp_range_to_bytes(&ate.range, encoding),
                ate.new_text.clone(),
            ));
        }
        // The cursor lands after the primary insertion, shifted by the net length
        // of any edits that fall before it (e.g. an inserted `use` import).
        let shift: isize = edits
            .iter()
            .skip(1)
            .filter(|(r, _)| r.start < primary_range.start)
            .map(|(r, t)| t.len() as isize - (r.end - r.start) as isize)
            .sum();
        let cursor_byte = (primary_range.start + primary_text.len()) as isize + shift;
        self.editor.apply_edits(edits, cursor_byte.max(0) as usize);
    }

    /// After the editor inserted a word character or backspaced while the menu was
    /// open, recompute the prefix and refresh: a complete list refilters
    /// client-side; an incomplete one re-requests at the new cursor (the current
    /// items stay shown until that reply lands). Closes the menu if the cursor
    /// left the word, or if a complete list now has nothing to offer.
    pub(crate) fn lsp_menu_after_edit(&mut self) {
        let Some(menu) = self.completion.as_ref() else {
            return;
        };
        let buffer = self.editor.current_buffer_id();
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        // Left the word (backspaced before the anchor, changed line/buffer): the
        // menu no longer applies.
        if buffer != menu.buffer || row != menu.anchor.0 || col < menu.anchor.1 {
            self.lsp_menu_close();
            return;
        }
        let line = self.editor.buffer().line(row);
        let region = &line[menu.anchor.1..col.min(line.len())];
        if !region.chars().all(is_word_char) {
            self.lsp_menu_close();
            return;
        }
        let prefix = region.to_string();
        let incomplete = menu.is_incomplete;
        self.completion.as_mut().unwrap().prefix = prefix;
        if incomplete {
            // The cached list was partial: ask the server for the narrowed set.
            // The current items stay shown until the reply re-ranks them.
            self.request_lsp(LspReqKind::Completion);
        } else {
            self.rerank_menu();
            if self
                .completion
                .as_ref()
                .is_some_and(|m| m.visible.is_empty())
            {
                self.lsp_menu_close();
                return;
            }
        }
        self.lsp_dirty = true;
    }

    /// Project the open completion menu into the `pmenu` redraw value (`Nil` when
    /// closed or nothing matches): the ranked visible items, the selected index,
    /// and the overlay's anchor/size in screen cells. The menu sits one row below
    /// the cursor, flipped above when there's no room; `col` is the word-start
    /// screen column (the client adds the number gutter), so the box lines up
    /// under the word being completed — reusing `cursor_screen_col`'s math, no
    /// core change. `text_width` is the text area's cell width (the frame minus
    /// the number gutter), used only to keep the box from overflowing it.
    pub(crate) fn pmenu_value(&self, view: &View, text_width: usize) -> Value {
        let Some(menu) = &self.completion else {
            return Value::Nil;
        };
        if menu.visible.is_empty() {
            return Value::Nil;
        }
        let (arow, acol) = menu.anchor;
        let line = self.editor.buffer().line(arow);
        let anchor_col = unicode::virtcol(&line, acol, unicode::TABSTOP);
        let cursor_row = view.cursor_row;
        let text_height = view.lines.len();

        let items: Vec<Value> = menu
            .visible
            .iter()
            .map(|&i| {
                let item = &menu.raw[i];
                Value::Array(vec![
                    Value::from(item.label.as_str()),
                    Value::from(item.kind as u64),
                    Value::from(item.detail.as_deref().unwrap_or("")),
                ])
            })
            .collect();
        let count = items.len();

        // Width: the widest item, clamped so the bordered box fits the text area.
        let content_w = menu
            .visible
            .iter()
            .map(|&i| pmenu_item_width(&menu.raw[i]))
            .max()
            .unwrap_or(1);
        let max_w = text_width.saturating_sub(anchor_col).max(1);
        let width = content_w.clamp(1, max_w);

        // Place the box below if its border+content+border fits, else above;
        // clamp the content height to the room available.
        const MAX_H: usize = 10;
        let want = count.min(MAX_H);
        let below = text_height.saturating_sub(cursor_row + 1);
        let above = cursor_row;
        let (row, height) = if want + 2 <= below {
            (cursor_row + 1, want)
        } else if want + 2 <= above {
            (cursor_row - (want + 2), want)
        } else if below >= above {
            (cursor_row + 1, below.saturating_sub(2).clamp(1, want))
        } else {
            let h = above.saturating_sub(2).clamp(1, want);
            (cursor_row.saturating_sub(h + 2), h)
        };

        Value::Map(vec![
            (Value::from("items"), Value::Array(items)),
            (
                Value::from("selected"),
                match menu.selected {
                    Some(i) => Value::from(i as u64),
                    None => Value::Nil,
                },
            ),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(anchor_col as u64)),
            (Value::from("width"), Value::from(width as u64)),
            (Value::from("height"), Value::from(height as u64)),
        ])
    }

    /// Convert an LSP [`Range`] (in the negotiated `encoding`) to an absolute
    /// **current-buffer** byte range, resolving each endpoint against its line.
    fn lsp_range_to_bytes(
        &self,
        range: &Range,
        encoding: PositionEncoding,
    ) -> std::ops::Range<usize> {
        lsp_range_to_bytes_in(self.editor.buffer(), range, encoding)
    }

    /// A current-buffer `(row, byte-column)` point as an LSP [`Position`] in the
    /// server's negotiated encoding (Decision 4).
    fn lsp_position(&self, encoding: PositionEncoding, row: usize, byte_col: usize) -> Position {
        lsp_position_in(self.editor.buffer(), encoding, row, byte_col)
    }

    // ----- Phase 6 appliers ------------------------------------------------

    /// Apply whole-document formatting edits to the current buffer (one undo
    /// step) and re-sync so the server's version stays consistent. Empty ⇒ a
    /// brief message (already formatted), so a no-op re-run is visible.
    fn apply_formatting_edits(&mut self, edits: Vec<TextEdit>) {
        if edits.is_empty() {
            self.editor.echo(LspReqKind::Formatting.empty_message());
            return;
        }
        let id = self.editor.current_buffer_id();
        let encoding = self.buffer_encoding(id).unwrap_or(PositionEncoding::Utf8);
        let buffer = self.editor.buffer();
        let byte_edits = edits
            .iter()
            .map(|e| {
                (
                    lsp_range_to_bytes_in(buffer, &e.range, encoding),
                    e.new_text.clone(),
                )
            })
            .collect();
        self.editor.apply_edits_to(id, byte_edits);
        self.sync_lsp_buffer(id);
    }

    /// Apply a normalized workspace edit (from rename or a code action) across the
    /// open buffers it touches. Each URI that maps to an **open** buffer has its
    /// edits converted to bytes against *that* buffer (its negotiated encoding),
    /// applied as one undo step, and the buffer re-synced; URIs with no open
    /// buffer are skipped (the unopened-file case is scoped out). An edit that
    /// touches no open buffer reports a brief message.
    fn apply_workspace_edit(&mut self, changes: WorkspaceEditData) {
        let mut touched = 0usize;
        for (uri, edits) in changes {
            if edits.is_empty() {
                continue;
            }
            let Some(id) = self.buffer_id_for_uri(&uri) else {
                // Not an open buffer: editing an unopened file is a follow-up.
                continue;
            };
            let encoding = self.buffer_encoding(id).unwrap_or(PositionEncoding::Utf8);
            let Some(buffer) = self.editor.buffer_of(id) else {
                continue;
            };
            let byte_edits = edits
                .iter()
                .map(|e| {
                    (
                        lsp_range_to_bytes_in(buffer, &e.range, encoding),
                        e.new_text.clone(),
                    )
                })
                .collect();
            self.editor.apply_edits_to(id, byte_edits);
            self.sync_lsp_buffer(id);
            touched += 1;
        }
        if touched == 0 {
            self.editor.echo("No applicable changes");
        }
    }

    /// The **open** buffer a workspace-edit URI refers to, or `None` (we edit only
    /// open buffers). First an exact match against the URI we sent at `didOpen`
    /// (what diagnostics route by); then a canonicalized-path fallback, so a
    /// server that resolves symlinks in its returned URI — e.g. `/var` →
    /// `/private/var` on macOS — still matches the buffer we opened under the
    /// un-resolved path.
    fn buffer_id_for_uri(&self, uri: &Url) -> Option<BufferId> {
        if let Some(id) = self
            .lsp_states
            .iter()
            .find_map(|(id, s)| (s.uri.as_ref() == Some(uri)).then_some(*id))
        {
            return Some(id);
        }
        let target = uri_to_path(uri).and_then(|p| std::fs::canonicalize(p).ok())?;
        self.editor.buffer_ids().into_iter().find(|id| {
            self.editor
                .buffer_of(*id)
                .and_then(|b| b.path.as_ref())
                .and_then(|p| std::fs::canonicalize(p).ok())
                .is_some_and(|c| c == target)
        })
    }

    /// Flush a `didChange` for buffer `id` (which need not be current) after a
    /// workspace edit touched it, so the server's document version stays
    /// consistent (the plan's `sync_lsp_buffer`). The current buffer is delegated
    /// to the normal `sync_lsp` path (so each journal entry reaches exactly one
    /// `didChange`); a non-current, attached buffer drains its own journal and
    /// sends the deltas (or full text) here. A no-op for an unopened / unattached
    /// / sync-none buffer (its journal is still drained so it can't replay later).
    fn sync_lsp_buffer(&mut self, id: BufferId) {
        if id == self.editor.current_buffer_id() {
            self.sync_lsp();
            return;
        }
        let Some(state) = self.lsp_states.get(&id) else {
            return;
        };
        if !state.opened {
            return;
        }
        let (Some(key), Some(uri)) = (state.server.clone(), state.uri.clone()) else {
            return;
        };
        let Some(&ServerRuntime {
            encoding,
            sync_kind,
            ..
        }) = self.lsp_servers.get(&key)
        else {
            return;
        };
        let batch = self.editor.take_lsp_edits_of(id).unwrap_or_default();
        if sync_kind == TextDocumentSyncKind::NONE || batch.is_empty() {
            return;
        }
        let cur_tick = self
            .editor
            .buffer_of(id)
            .map(|b| b.changedtick)
            .unwrap_or(0);
        let changes = if batch.resync || sync_kind == TextDocumentSyncKind::FULL {
            let text = self
                .editor
                .buffer_of(id)
                .map(|b| b.text.to_string())
                .unwrap_or_default();
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            }]
        } else {
            let buffer = self.editor.buffer_of(id).unwrap();
            incremental_changes_in(buffer, &batch.edits, encoding)
        };
        let version = {
            let state = self.lsp_states.get_mut(&id).unwrap();
            state.version += 1;
            state.last_tick = cur_tick;
            state.version
        };
        self.lsp.notify(
            key,
            LspNotify::DidChange {
                uri,
                version,
                changes,
            },
        );
    }

    /// The negotiated position encoding of buffer `id`'s attached server, or
    /// `None` if the buffer has no server that finished `initialize`.
    fn buffer_encoding(&self, id: BufferId) -> Option<PositionEncoding> {
        let key = self.lsp_states.get(&id)?.server.as_ref()?;
        Some(self.lsp_servers.get(key)?.encoding)
    }

    /// List a code-action reply's titles in a select-enabled panel and stash the
    /// actions so a `<CR>` select applies the chosen one (the `panel_selects`
    /// path, keyed by select index — see the design's code-action note). An empty
    /// reply shows a brief message instead of an empty panel.
    fn show_code_actions(&mut self, actions: Vec<CodeActionData>) {
        if actions.is_empty() {
            self.editor.echo(LspReqKind::CodeAction.empty_message());
            return;
        }
        let lines: Vec<String> = actions.iter().map(|a| a.title.clone()).collect();
        self.lsp_code_actions = actions;
        self.editor
            .open_panel(CODE_ACTION_PANEL_TITLE, lines, true, 0);
    }

    /// Apply the code action selected (by index) in the code-action panel: apply
    /// its eager `edit` now, else resolve a lazy action's edit
    /// (`codeAction/resolve`) and apply when the reply lands, else (a bare
    /// command) a brief message. Clears the stashed actions and closes the panel
    /// either way.
    pub(crate) fn apply_code_action(&mut self, index: usize) {
        let action = self.lsp_code_actions.get(index).cloned();
        self.lsp_code_actions.clear();
        self.editor.close_panel();
        let Some(action) = action else {
            return;
        };
        if let Some(changes) = action.edit {
            self.apply_workspace_edit(changes);
            self.lsp_dirty = true;
        } else if let Some(raw) = action.resolve {
            // A lazy action: ask the server to fill in its edit, then apply when
            // the reply lands (reply-as-event, like format/rename).
            self.resolve_code_action(raw);
        } else {
            // A bare `Command` — running it needs `workspace/executeCommand`,
            // which is a scoped-out follow-up.
            self.editor
                .echo("Code action has no edit (command unsupported)");
            self.lsp_dirty = true;
        }
    }

    /// Fire a `codeAction/resolve` for a lazy action, recording it as a pending
    /// apply request (content-version guarded, like format/rename); its resolved
    /// edit is applied in [`Server::on_lsp_reply`].
    fn resolve_code_action(&mut self, action: Box<nxvim_lsp::lsp_types::CodeAction>) {
        self.sync_lsp();
        let Some((key, _uri, _encoding)) = self.current_lsp_target() else {
            self.editor.echo("No language server attached");
            return;
        };
        let token = self.register_lsp_request(LspReqKind::ResolveCodeAction);
        self.lsp
            .request(key, token, LspRequest::ResolveCodeAction { action });
    }
}

/// Human label for a negotiated position encoding (matches the LSP wire names).
fn encoding_label(encoding: PositionEncoding) -> &'static str {
    match encoding {
        PositionEncoding::Utf8 => "utf-8",
        PositionEncoding::Utf16 => "utf-16",
        PositionEncoding::Utf32 => "utf-32",
    }
}

/// The panel title for the `:LspCodeAction` list. The server recognizes a panel
/// select by this title to route it to [`Server::apply_code_action`] instead of
/// the generic scripting `on_select` path.
pub(crate) const CODE_ACTION_PANEL_TITLE: &str = "LSP code actions";

/// Convert an LSP [`Range`] (in `encoding`) to an absolute byte range in
/// `buffer`, resolving each endpoint against its line — the buffer-addressed form
/// of [`Server::lsp_range_to_bytes`], for a workspace edit that touches a
/// non-current buffer.
fn lsp_range_to_bytes_in(
    buffer: &Buffer,
    range: &Range,
    encoding: PositionEncoding,
) -> std::ops::Range<usize> {
    lsp_pos_to_byte_in(buffer, range.start, encoding)
        ..lsp_pos_to_byte_in(buffer, range.end, encoding)
}

/// Absolute byte offset of an LSP [`Position`] (in `encoding`) within `buffer`:
/// the character offset converted against its line, the row added as a line start.
fn lsp_pos_to_byte_in(buffer: &Buffer, pos: Position, encoding: PositionEncoding) -> usize {
    let row = pos.line as usize;
    let line = buffer.line(row);
    buffer.line_start(row) + byte_col(encoding, &line, pos.character as usize)
}

/// A `(row, byte-column)` point in `buffer` as an LSP [`Position`] in `encoding`
/// (Decision 4): UTF-8 is the identity (an LSP UTF-8 character *is* a byte
/// offset), UTF-16/UTF-32 need column math over the line text. The buffer-
/// addressed form of [`Server::lsp_position`].
fn lsp_position_in(
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
fn incremental_changes_in(
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

/// Byte offset of LSP `character` on `line`, the inverse of [`Server::lsp_position`]
/// (Decision 4): UTF-8 is the identity (a character *is* a byte offset, clamped
/// to the line), UTF-16/UTF-32 need column math. Clamped to the line length so a
/// past-end character (a diagnostic whose range runs to EOL) lands at the end.
fn byte_col(encoding: PositionEncoding, line: &str, character: usize) -> usize {
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
fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The completion word: the run of identifier characters immediately left of the
/// byte `cursor` on `line`, as `(word_start_byte, prefix)`. An empty prefix when
/// the cursor isn't preceded by an identifier char (a just-triggered menu with
/// nothing typed). Both the menu's filter string and its default replace range.
fn completion_word(line: &str, cursor: usize) -> (usize, String) {
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

/// The match tier of an item's `filter` string against the typed `prefix`, lower
/// = better: `0` exact, `1` case-sensitive prefix, `2` case-insensitive prefix,
/// `3` case-insensitive subsequence; `None` ⇒ no match (the item is dropped). An
/// empty prefix matches everything at tier ≤ 1, so a just-triggered menu shows
/// the whole list in the server's `sortText` order.
fn match_tier(filter: &str, prefix: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(if filter.is_empty() { 0 } else { 1 });
    }
    if filter == prefix {
        return Some(0);
    }
    if filter.starts_with(prefix) {
        return Some(1);
    }
    let (filter, prefix) = (filter.to_lowercase(), prefix.to_lowercase());
    if filter.starts_with(&prefix) {
        return Some(2);
    }
    if is_subsequence(&prefix, &filter) {
        return Some(3);
    }
    None
}

/// Whether every character of `needle` appears in `haystack` in order (a
/// subsequence). Callers lowercase both for a case-insensitive match.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|nc| hay.by_ref().any(|hc| hc == nc))
}

/// Display width (cells) of a completion item's menu row: the label plus, when
/// present, a gap and the right-aligned detail. Sizes the popup box.
fn pmenu_item_width(item: &CompletionItemData) -> usize {
    let label = item.label.chars().count();
    let detail = match item.detail.as_deref() {
        Some(d) if !d.is_empty() => 1 + d.chars().count(),
        _ => 0,
    };
    label + detail
}

/// Map an LSP [`DiagnosticSeverity`] to nxvim's severity code (`1`=error,
/// `2`=warning, `3`=info, `4`=hint). An absent severity is treated as an error,
/// matching how most servers and neovim render an unspecified diagnostic. The
/// constants aren't enum variants (lsp-types models them as a newtype), so this
/// compares rather than pattern-matches.
fn severity_code(severity: Option<DiagnosticSeverity>) -> u8 {
    match severity {
        Some(s) if s == DiagnosticSeverity::WARNING => 2,
        Some(s) if s == DiagnosticSeverity::INFORMATION => 3,
        Some(s) if s == DiagnosticSeverity::HINT => 4,
        _ => 1,
    }
}

/// Translate the LSP crate's [`ProviderCaps`] into the Lua-runtime
/// [`LspServerCapabilities`], the boundary that keeps `nxvim-lua` free of the LSP
/// crate. The two have the same per-feature fields; this is the one place they
/// are mapped across.
fn provider_caps_to_lua(p: &ProviderCaps) -> LspServerCapabilities {
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
    }
}

/// Project a buffer's cached diagnostics into the plain [`DiagnosticData`] the
/// Lua mirror (`vim._diagnostics`) holds for `vim.diagnostic.get`. Positions are
/// the raw 0-based LSP coordinates; `col`/`end_col` are byte offsets under the
/// UTF-8 encoding nxvim advertises first (the negotiated default), matching
/// neovim's byte-column `get` shape for the common case.
fn diagnostic_mirror_data(diags: &[Diagnostic]) -> Vec<DiagnosticData> {
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
fn severity_group(severity: u8) -> &'static str {
    match severity {
        2 => "DiagnosticUnderlineWarn",
        3 => "DiagnosticUnderlineInfo",
        4 => "DiagnosticUnderlineHint",
        _ => "DiagnosticUnderlineError",
    }
}

/// One-letter severity tag for the location-list column (`E`/`W`/`I`/`H`).
fn severity_short(severity: u8) -> char {
    match severity {
        2 => 'W',
        3 => 'I',
        4 => 'H',
        _ => 'E',
    }
}

/// The first non-empty line of a (possibly multi-line, markdown) diagnostic
/// message, for the single-line message line and location-list rows.
fn first_line(message: &str) -> String {
    message
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Human label for a document-sync kind.
fn sync_label(kind: TextDocumentSyncKind) -> &'static str {
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
fn lsp_spawn(cmd: &[String]) -> Option<ServerSpawn> {
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
fn lsp_root_override() -> Option<PathBuf> {
    std::env::var_os("NXVIM_LSP_ROOT").map(|root| absolutize(Path::new(&root)))
}

/// A `file://` URI for an absolute-ized path, or `None` if it can't be formed.
pub(crate) fn path_to_uri(path: &Path) -> Option<Url> {
    Url::from_file_path(absolutize(path)).ok()
}

/// The filesystem path behind a `file://` URI (the inverse of [`path_to_uri`]),
/// or `None` for a non-file URI — the target of a go-to jump or a panel location.
fn uri_to_path(uri: &Url) -> Option<PathBuf> {
    uri.to_file_path().ok()
}

/// Resolve `path` against the current directory if it is relative.
fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|d| d.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}
