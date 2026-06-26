//! Document sync and server lifecycle: draining `nx.lsp.start`, per-buffer
//! `didOpen`/`didChange`/`didSave`/`didClose`, `LspAttach`/`LspDetach`
//! emission, ingesting [`LspEvent`]s, and the `:LspInfo` dump.

use std::path::{Path, PathBuf};

use nxvim_core::BufferId;
use nxvim_lsp::lsp_types::{TextDocumentContentChangeEvent, TextDocumentSyncKind, Url};
use nxvim_lsp::{LspEvent, LspNotify, LspReply, PositionEncoding, RefreshKind, ServerKey};
use nxvim_lua::{LspClientData, LspOp};

use super::*;
use crate::EditHost;

impl EditHost {
    /// Apply one [`LspOp`] drained from the Lua runtime — a `nx.lsp.start` queued
    /// by user Lua (directly, or through the `nx.lsp.enable` `FileType`
    /// dispatcher). Ensures the `(name, root)` server exists and binds `bufnr` to
    /// it; the next [`EditHost::sync_lsp`] sends `didOpen`. Phase 7a's replacement
    /// for the built-in auto-spawn: a server starts *only* via this path.
    pub(crate) fn apply_lsp_op(&mut self, op: LspOp) {
        let start = match op {
            LspOp::Start { .. } => op,
            // `nx.lsp.*` verbs route into the existing native request paths. No
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
            LspOp::WorkspaceSymbol { query } => {
                self.request_lsp_workspace_symbol(&query);
                return;
            }
            LspOp::DiagnosticGoto { forward, severity } => {
                self.diagnostic_goto(forward, severity);
                return;
            }
            LspOp::DiagnosticSetloclist => {
                match self.diagnostics_location_list() {
                    Some(entries) => self.editor.open_location_list(entries, "LSP diagnostics"),
                    None => self.editor.echo("No diagnostics"),
                }
                return;
            }
            LspOp::DiagnosticOpenFloat => {
                self.diagnostics_open_float();
                return;
            }
            LspOp::DiagnosticConfig {
                underline,
                virtual_text,
                virt_prefix,
                signs,
                sign_text,
            } => {
                self.diag_config.underline = underline;
                self.diag_config.virtual_text = virtual_text;
                self.diag_config.virt_prefix = virt_prefix;
                self.diag_config.signs = signs;
                self.diag_config.sign_text = sign_text;
                self.lsp_dirty = true;
                return;
            }
            LspOp::SetClientDiagnostics { bufnr, diags } => {
                let buffer = BufferId(bufnr);
                if diags.is_empty() {
                    // An empty set (a cleared namespace, or every namespace reset)
                    // drops the buffer's entry so it stops projecting entirely.
                    self.client_diagnostics.remove(&buffer);
                } else {
                    self.client_diagnostics
                        .insert(buffer, diags.iter().map(client_diagnostic).collect());
                }
                self.lsp_dirty = true;
                return;
            }
            LspOp::ClientRequest {
                client_id,
                method,
                params,
                cb_id,
            } => {
                self.client_request(client_id, method, params, cb_id);
                return;
            }
            LspOp::ClientNotify {
                client_id,
                method,
                params,
            } => {
                self.client_notify(client_id, method, params);
                return;
            }
            LspOp::ApplyWorkspaceEdit { edit } => {
                self.apply_lua_workspace_edit(edit);
                return;
            }
            LspOp::ShowDocument {
                uri,
                line,
                character,
                encoding,
            } => {
                self.show_lua_document(&uri, line, character, &encoding);
                return;
            }
            LspOp::SemanticTokensEnable { bufnr, enabled } => {
                let buffer = BufferId(bufnr);
                let state = self.lsp_states.entry(buffer).or_default();
                state.semantic_enabled = Some(enabled);
                // Starting (re-)requests so a cold cache fills; stopping just hides
                // the existing paint. Either way the projection must re-evaluate.
                if enabled {
                    self.request_semantic_tokens(buffer);
                }
                self.lsp_dirty = true;
                return;
            }
            LspOp::SemanticTokensRefresh { bufnr } => {
                let buffer = BufferId(bufnr);
                // Drop the delta cursor so the refresh re-requests the whole `full`
                // set (neovim's `force_refresh` discards the prior result).
                if let Some(state) = self.lsp_states.get_mut(&buffer) {
                    state.semantic.result_id = None;
                }
                self.request_semantic_tokens(buffer);
                return;
            }
            LspOp::InlayHintEnable { bufnr, enabled } => {
                let buffer = BufferId(bufnr);
                let state = self.lsp_states.entry(buffer).or_default();
                state.inlay_enabled = enabled;
                if enabled {
                    // Request a fresh set so the cache fills; the projection then
                    // paints it. (No-op unless the server advertises the provider.)
                    self.request_inlay_hints(buffer);
                } else {
                    if let Some(state) = self.lsp_states.get_mut(&buffer) {
                        // Disabling clears the cache — no surviving paint (neovim drops
                        // the hints on disable; they re-fetch on the next enable).
                        state.inlay = Default::default();
                    }
                    // Clear the read mirror too, so `vim.lsp.inlay_hint.get` returns
                    // nothing for a disabled buffer.
                    let _ = self.lua.set_inlay_hints(buffer.0, &[]);
                }
                self.lsp_dirty = true;
                return;
            }
            LspOp::SemanticTokensConfig { enabled } => {
                self.semantic_tokens_enabled = enabled;
                // Flipping back on re-requests every attached buffer so the paint
                // returns even if its cache was never filled (e.g. the feature was
                // off at attach time); off just hides the paint (cache survives).
                if enabled {
                    let buffers: Vec<BufferId> = self.lsp_states.keys().copied().collect();
                    for buffer in buffers {
                        self.request_semantic_tokens(buffer);
                    }
                }
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
        // A serverless browser session (no daemon) has no process host to run a
        // language server on, so fail *loud* rather than silently no-op (Phase 6e).
        // Native always has a process host; with a daemon the wasm build runs the
        // server there over the `lsp_*` wire.
        #[cfg(not(feature = "native"))]
        if !self.fx.has_remote_lsp() {
            self.editor
                .echo("E: language servers require a daemon — :connect to one to use LSP");
            return;
        }
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
            self.fx.lsp_ensure(key.clone(), spawn);
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
    /// `nx.lsp.start` already bound to a server, send `didOpen`/`didChange`/
    /// `didSave` as its state requires. Called from `redraw()` alongside
    /// `refresh_highlights`. Never spawns (that is [`EditHost::apply_lsp_op`]) and
    /// never blocks: every send is a fire-and-forget [`LspNotify`].
    pub(crate) fn sync_lsp(&mut self) {
        self.reap_closed_lsp_buffers();

        let buffer = self.editor.current_buffer_id();
        // Only buffers a `nx.lsp.start` bound to a server are synced — there is no
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
            ..
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

        // Set when this sync pushed new content to the server (open or change), so
        // a `semanticTokens/full` refresh is requested once the state is back in
        // the map (the request reads it). Gated server-side on the server actually
        // advertising semantic tokens, so this is free for servers without them.
        let mut content_synced = false;

        if !state.opened {
            // First open (or re-open after a respawn): full text supersedes any
            // journaled deltas, so drop the LSP journal (the treesitter journal is
            // drained independently when the editor queries highlights).
            let _ = self.editor.buffer_mut().take_lsp_edits();
            let text = self.editor.buffer().text.to_string();
            // Seed the sync shadow: this is exactly the text the server now holds, so
            // later incremental `didChange`s replay their deltas over it.
            state.shadow.clone_from(&text);
            state.version = 1;
            let language_id = state.language_id.clone();
            self.fx.lsp_notify(
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
            content_synced = true;
        } else if tick_changed && sync_kind != TextDocumentSyncKind::NONE {
            let batch = self.editor.buffer_mut().take_lsp_edits();
            state.version += 1;
            let changes = Self::did_change_content(
                self.editor.buffer(),
                &mut state.shadow,
                &batch,
                sync_kind,
                encoding,
            );
            self.fx.lsp_notify(
                key.clone(),
                LspNotify::DidChange {
                    uri: uri.clone(),
                    version: state.version,
                    changes,
                },
            );
            state.last_tick = cur_tick;
            content_synced = true;
        }

        // Save: the buffer's write counter advanced since the last sync, so a `:w`
        // landed bytes on disk (a real hook, not a `modified`-flag heuristic).
        if state.opened && cur_save_tick != state.last_save_tick {
            self.fx
                .lsp_notify(key, LspNotify::DidSave { uri, text: None });
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

        // Refresh semantic tokens and inlay hints whenever the server saw new
        // content (each request no-ops unless the server advertised the feature and,
        // for inlay hints, the buffer enabled it). After the attach hook, so an
        // `on_attach` that toggles either feature is already in effect.
        if content_synced {
            self.request_semantic_tokens(buffer);
            self.request_inlay_hints(buffer);
        }
    }

    /// Flush a `didChange` for buffer `id` (which need not be current) after a
    /// workspace edit touched it, so the server's document version stays
    /// consistent (the plan's `sync_lsp_buffer`). The current buffer is delegated
    /// to the normal `sync_lsp` path (so each journal entry reaches exactly one
    /// `didChange`); a non-current, attached buffer drains its own journal and
    /// sends the deltas (or full text) here. A no-op for an unopened / unattached
    /// / sync-none buffer (its journal is still drained so it can't replay later).
    pub(crate) fn sync_lsp_buffer(&mut self, id: BufferId) {
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
        let buffer = self.editor.buffer_of(id).unwrap();
        let shadow = &mut self.lsp_states.get_mut(&id).unwrap().shadow;
        let changes = Self::did_change_content(buffer, shadow, &batch, sync_kind, encoding);
        let version = {
            let state = self.lsp_states.get_mut(&id).unwrap();
            state.version += 1;
            state.last_tick = cur_tick;
            state.version
        };
        self.fx.lsp_notify(
            key,
            LspNotify::DidChange {
                uri,
                version,
                changes,
            },
        );
    }

    /// The `didChange` content for a drained `batch` against `buffer`: incremental
    /// deltas only when they are provably faithful, else a whole-document
    /// replacement.
    ///
    /// Incremental changes are journaled byte deltas, converted to LSP positions at
    /// *sync* time. Incremental deltas replay the journaled edits over `shadow` —
    /// the text the server currently holds — which converts each delta's columns
    /// against the line as it stood *before* that edit (correct in any encoding) and
    /// advances `shadow` to match the buffer. This stays incremental even under
    /// UTF-16/UTF-32, where converting against the *post-edit* buffer would clamp a
    /// shortened line's later columns and corrupt the range (`balance`→`aa`⇒`aae`).
    /// A `resync` batch (whole-rope replace) or a server that asked for `FULL` sync
    /// sends the whole text and reseeds `shadow` to it.
    fn did_change_content(
        buffer: &nxvim_core::Buffer,
        shadow: &mut String,
        batch: &nxvim_core::EditBatch,
        sync_kind: TextDocumentSyncKind,
        encoding: PositionEncoding,
    ) -> Vec<TextDocumentContentChangeEvent> {
        if batch.resync || sync_kind == TextDocumentSyncKind::FULL {
            let text = buffer.text.to_string();
            shadow.clone_from(&text);
            vec![TextDocumentContentChangeEvent {
                range: None,
                range_length: None,
                text,
            }]
        } else {
            incremental_changes_against(shadow, &batch.edits, encoding)
        }
    }

    /// Fire `LspAttach` for the just-attached current buffer with the server's
    /// `client_id` as `args.data.client_id`. Pushes the buffer snapshot first (so
    /// the callback resolves the buffer), then folds in the Lua effects the
    /// `on_attach` left — buffer-local keymaps it set bump the keymap version and
    /// are picked up on the next input. Mirrors [`EditHost::fire_lifecycle`].
    pub(crate) fn fire_lsp_attach(&mut self, buf: BufferId, file: &str, client_id: u64) {
        let ft = self
            .lsp_states
            .get(&buf)
            .map(|s| s.language_id.clone())
            .unwrap_or_default();
        let _ = self.lua.set_buf_snapshot(buf.0, file, &ft);
        // Keep the buffer mirror fresh: an `on_attach` body commonly reads buffer
        // lines / the cursor and runs before the trailing `run_pending` (Phase 6).
        self.push_buf_mirror();
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
    /// detach counterpart to [`EditHost::fire_lsp_attach`]. Unlike attach it does
    /// not push a buffer snapshot: detach fires for a buffer being closed
    /// (`didClose`) or a server that exited, neither of which is necessarily the
    /// current buffer. User `LspDetach` callbacks still get `args.buf`/`data`.
    pub(crate) fn fire_lsp_detach(&mut self, buf: BufferId, file: &str, client_id: u64) {
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
    pub(crate) fn reap_closed_lsp_buffers(&mut self) {
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
                    self.fx.lsp_notify(key, LspNotify::DidClose { uri });
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
                // `client_id` Lua sees stays stable (and `nx.lsp._clients` isn't
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
                        legend: caps.legend.clone(),
                        semantic_tokens_delta: caps.semantic_tokens_delta,
                        inlay_hints: caps.providers.inlay_hints,
                        folding_range: caps.providers.folding_range,
                    },
                );
                // Mirror the client into `nx.lsp._clients[id]` so `on_attach` can
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
                // handler.
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
                // a publish for a buffer closed while it was in flight). Mark dirty
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
                    // Mirror into `nx._diagnostics[bufnr]` so the synchronous
                    // `vim.diagnostic.get` (Slice 2) can read it from pure Lua.
                    let _ = self.lua.set_diagnostics(bufnr, &data);
                }
            }
            // A generic `client:request` reply (Phase 5) routes to its Lua handler
            // by the callback id the token carries, bypassing the editor-feature
            // staleness machinery the typed replies go through.
            LspEvent::Reply {
                token,
                reply: LspReply::Raw(res),
                ..
            } => self.on_client_request_reply(token.cb_id, res),
            // An `inlayHint/resolve` reply routes by the `cb_id` its token carries
            // (like a generic `client:request`), since many lazy hints can resolve
            // at once and the single-slot kind-map can't tell them apart.
            LspEvent::Reply {
                token,
                reply: LspReply::ResolvedInlayHint { label },
                ..
            } => self.on_inlay_hint_resolved(token.cb_id, label),
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
                self.editor.record_message(message, false);
            }
            LspEvent::WorkspaceRefresh { key, kind } => self.on_workspace_refresh(key, kind),
        }
    }

    /// Honor a server→client `workspace/{inlayHint,semanticTokens}/refresh`: the
    /// server recomputed and asked us to re-query, so re-issue the matching
    /// whole-buffer request for every buffer this server owns. This is what makes a
    /// server that produces decorations *asynchronously* (lua_ls, gopls — they have
    /// nothing to return on the first request and only signal readiness via refresh)
    /// actually paint: without it the editor would keep its initial empty cache.
    /// `request_inlay_hints` / `request_semantic_tokens` already gate on the
    /// per-buffer enable/provider, so a disabled or unsupported buffer is a no-op.
    fn on_workspace_refresh(&mut self, key: ServerKey, kind: RefreshKind) {
        let buffers: Vec<BufferId> = self
            .lsp_states
            .iter()
            .filter(|(_, s)| s.server.as_ref() == Some(&key))
            .map(|(id, _)| *id)
            .collect();
        for buffer in buffers {
            match kind {
                RefreshKind::InlayHint => self.request_inlay_hints(buffer),
                RefreshKind::SemanticTokens => {
                    // A refresh means "recompute"; drop the delta cursor so the
                    // re-request fetches the whole `full` set (like force_refresh).
                    if let Some(state) = self.lsp_states.get_mut(&buffer) {
                        state.semantic.result_id = None;
                    }
                    self.request_semantic_tokens(buffer);
                }
            }
        }
    }

    /// Build the `:LspInfo` report: the current buffer's server/encoding/sync/
    /// version/diagnostics, then every running server and every attached buffer.
    /// The textual companion to the on-screen LSP features (diagnostics, hover,
    /// completion, …) — for inspecting server/attach state.
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

    /// The **open** buffer a workspace-edit URI refers to, or `None` (we edit only
    /// open buffers). First an exact match against the URI we sent at `didOpen`
    /// (what diagnostics route by); then a canonicalized-path fallback, so a
    /// server that resolves symlinks in its returned URI — e.g. `/var` →
    /// `/private/var` on macOS — still matches the buffer we opened under the
    /// un-resolved path.
    pub(crate) fn buffer_id_for_uri(&self, uri: &Url) -> Option<BufferId> {
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

    /// The negotiated position encoding of buffer `id`'s attached server, or
    /// `None` if the buffer has no server that finished `initialize`.
    pub(crate) fn buffer_encoding(&self, id: BufferId) -> Option<PositionEncoding> {
        let key = self.lsp_states.get(&id)?.server.as_ref()?;
        Some(self.lsp_servers.get(key)?.encoding)
    }
}
