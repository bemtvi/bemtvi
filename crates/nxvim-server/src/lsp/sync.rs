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
            LspOp::Restart {
                name,
                init_options,
                settings,
                capabilities,
            } => {
                self.restart_lsp_servers(&name, init_options, settings, capabilities);
                return;
            }
            LspOp::BufRequest { kind, cb_id } => {
                match LspReqKind::from_u16(kind) {
                    Some(kind) => self.request_lsp(kind, cb_id),
                    // An unknown kind can't be issued — settle its promise (resolve
                    // `nil`) rather than leak it.
                    None => self.settle_lsp_promise(cb_id, serde_json::Value::Null),
                }
                return;
            }
            LspOp::Format { cb_id, name } => {
                self.request_lsp_format(cb_id, name);
                return;
            }
            LspOp::Rename { new_name, cb_id } => {
                self.request_lsp_rename(&new_name, cb_id);
                return;
            }
            LspOp::CodeAction {
                cb_id,
                only,
                apply,
                range,
            } => {
                self.request_lsp_code_action(cb_id, CodeActionOpts { only, apply }, range);
                return;
            }
            LspOp::SignatureAutoTrigger { enable } => {
                // Latch the opt-in and (re)derive core's trigger set from whatever
                // servers are already attached; future attaches refresh it too.
                self.signature_auto = enable;
                self.refresh_signature_autotrigger();
                return;
            }
            LspOp::WorkspaceSymbol { query, cb_id } => {
                self.request_lsp_workspace_symbol(&query, cb_id);
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
            LspOp::ApplyWorkspaceEdit { edit, encoding } => {
                self.apply_lua_workspace_edit(edit, &encoding);
                return;
            }
            LspOp::WorkspaceEditDecision { group, accepted } => {
                self.on_workspace_edit_decision(group, accepted);
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
                // Drop every server's delta cursor so the refresh re-requests whole
                // `full` sets (neovim's `force_refresh` discards the prior result) —
                // the user asked to recompute the buffer, not one server's half of it.
                if let Some(state) = self.lsp_states.get_mut(&buffer) {
                    for (_, doc) in state.servers_mut() {
                        doc.semantic.result_id = None;
                    }
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
                        // Disabling clears EVERY server's cache — no surviving paint
                        // (neovim drops the hints on disable; they re-fetch on the next
                        // enable). Clearing only one would leave the other's painted.
                        for (_, doc) in state.servers_mut() {
                            doc.inlay = Default::default();
                        }
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
        let Some(uri) = self.buffer_uri(&path) else {
            return;
        };
        // Root: `$NXVIM_LSP_ROOT` overrides (the test hook), else the root Lua
        // resolved, else the file's own directory. Rust never re-runs the marker
        // search — that is the config's job now (`vim.fs.root` in Lua).
        let root = lsp_root_override()
            .or_else(|| root.map(|r| absolutize(Path::new(&r))))
            .unwrap_or_else(|| {
                let abs = self.abs_buffer_path(&path);
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
        let Some(mut spawn) = lsp_spawn(&key.name, &cmd) else {
            return;
        };
        spawn.init_options = init_options;
        spawn.settings = settings;
        spawn.capabilities = capabilities;
        // Always remember the LATEST spawn for this key — the daemon-reconnect resync
        // and `nx.lsp.restart` both re-`ensure` from it, so it must reflect the config
        // in force now (which may have grown since the server first started), not the
        // one captured at first start.
        self.lsp_spawns.insert(key.clone(), spawn.clone());
        if !self.lsp_ensured.contains(&key) {
            self.fx.lsp_ensure(key.clone(), spawn);
            self.lsp_ensured.insert(key.clone());
        }
        let state = self.lsp_states.entry(buffer).or_default();
        // ADDITIVE: a filetype that enables two servers binds the buffer to both, and
        // each gets its own `didOpen` on the next sync. Re-binding an already-attached
        // server leaves its document state alone (idempotent), so the repeated
        // `FileType` dispatch a `nx.lsp.enable` does can't reset a live document.
        state.attach(key);
        state.language_id = filetype;
        state.uri = Some(uri);
        // Wake a sync so the bound buffer opens as soon as the server initializes.
        self.lsp_dirty = true;
    }

    /// Re-attach every LSP server after a **daemon reconnect**. A dropped link kills the
    /// remote language-server children (the synthetic `lsp_exited` detached their buffers),
    /// and the manager's own auto-respawn would have fired at the dead wire — so a fresh
    /// connection has no servers. Tear each known server down (clearing the lazy-start guard)
    /// and re-`ensure` it from the cached [`ServerSpawn`] against the new connection; the
    /// `Initialized` reply for each fresh server re-opens its bound buffers via the existing
    /// restart path, so documents re-sync with no per-buffer `didOpen` here. Shared by the
    /// native run loop and the wasm edit-host's reconnect (the daemon-reconnect plan's Phase 7) —
    /// `fx.lsp_shutdown` / `fx.lsp_ensure` have both a native (`LspManager`) and a wasm
    /// (`SyncLspClient`) implementation.
    pub(crate) fn resync_lsp_after_reconnect(&mut self) {
        // Every server a buffer is still bound to (the binding on `lsp_states` survives a
        // drop — only `opened` was cleared), plus any still in the runtime map.
        let keys: std::collections::HashSet<ServerKey> = self
            .lsp_states
            .values()
            .flat_map(|s| s.servers().map(|(k, _)| k.clone()).collect::<Vec<_>>())
            .chain(self.lsp_servers.keys().cloned())
            .collect();
        for key in keys {
            // Drop the dead/phantom server task so a re-`ensure` spawns a fresh one (the
            // manager leaves a still-open server's task alone), then re-ensure from the
            // remembered spawn against the live wire.
            self.fx.lsp_shutdown(key.clone());
            self.lsp_ensured.remove(&key);
            self.lsp_servers.remove(&key);
            if let Some(spawn) = self.lsp_spawns.get(&key).cloned() {
                self.fx.lsp_ensure(key.clone(), spawn);
                self.lsp_ensured.insert(key);
            }
        }
        // Force a fresh `didOpen` for every bound buffer once its server re-initializes.
        for state in self.lsp_states.values_mut() {
            for (_, doc) in state.servers_mut() {
                doc.opened = false;
                doc.version = 0;
            }
        }
        self.lsp_dirty = true;
    }

    /// Restart every running server whose config `name` matches (`nx.lsp.restart`).
    /// Reuses the reconnect teardown → re-`ensure` path, scoped to one config name.
    /// The caller passes the config's payloads *as they are now* (resolved in Lua):
    /// they overwrite the remembered spawn's before it respawns, so the fresh process
    /// runs the config in force NOW rather than one cached before the change (a config
    /// grown after start does not otherwise reach the cache until an async
    /// FileType/root-resolution fires a fresh `Start`, which races this op). A server
    /// that reads its whole config only at startup — efm-langserver's `languages` map
    /// is the motivating case — thus picks it up. Every bound buffer re-`didOpen`s
    /// under the fresh process. A no-op when nothing with `name` is running; each
    /// payload that is `None` keeps the cached value.
    pub(crate) fn restart_lsp_servers(
        &mut self,
        name: &str,
        init_options: Option<serde_json::Value>,
        settings: Option<serde_json::Value>,
        capabilities: Option<serde_json::Value>,
    ) {
        let keys: Vec<ServerKey> = self
            .lsp_ensured
            .iter()
            .filter(|k| k.name == name)
            .cloned()
            .collect();
        for key in keys {
            self.fx.lsp_shutdown(key.clone());
            self.lsp_ensured.remove(&key);
            self.lsp_servers.remove(&key);
            if let Some(mut spawn) = self.lsp_spawns.get(&key).cloned() {
                // Refresh the config payloads to what is in force now (the cmd stays as
                // cached — restart applies config changes, not a new command).
                if init_options.is_some() {
                    spawn.init_options = init_options.clone();
                }
                if settings.is_some() {
                    spawn.settings = settings.clone();
                }
                if capabilities.is_some() {
                    spawn.capabilities = capabilities.clone();
                }
                self.lsp_spawns.insert(key.clone(), spawn.clone());
                self.fx.lsp_ensure(key.clone(), spawn);
                self.lsp_ensured.insert(key);
            }
        }
        // Re-open every buffer bound to a restarted server under the fresh process.
        for state in self.lsp_states.values_mut() {
            for (key, doc) in state.servers_mut() {
                if key.name == name {
                    doc.opened = false;
                    doc.version = 0;
                }
            }
        }
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
        // auto-start (Phase 7a: LSP startup is 100% user Lua). Every attached server
        // syncs, each on its own clock, in key order.
        let keys: Vec<ServerKey> = match self.lsp_states.get(&buffer) {
            Some(state) => state.servers().map(|(k, _)| k.clone()).collect(),
            None => return,
        };
        if keys.is_empty() {
            return;
        }
        let Some(path) = self.editor.buffer().path.clone() else {
            return;
        };
        let Some(uri) = self.buffer_uri(&path) else {
            return;
        };

        let cur_tick = self.editor.buffer().changedtick;
        let cur_save_tick = self.editor.buffer().save_tick;

        // The edit journal is drained ONCE for the whole buffer, then replayed into
        // each server's own shadow. Draining per server would hand the first server
        // the deltas and every later one an empty batch — they would silently diverge
        // from the buffer, which incremental sync can never recover from.
        //
        // It is taken only when at least one attached server actually wants deltas
        // (opened, initialized, changed, and not sync-NONE); otherwise the journal is
        // left intact for a later sync.
        let mut batch: Option<nxvim_core::EditBatch> = None;

        let mut state = self.lsp_states.remove(&buffer).unwrap_or_default();
        state.uri = Some(uri.clone());
        let language_id = state.language_id.clone();

        // Servers that sent their first `didOpen` this sync — their attach moment, so
        // `LspAttach` fires once per server after the state is back in the map.
        let mut attached: Vec<u64> = Vec::new();
        // Set when any server saw new content, so the whole-buffer semantic-token and
        // inlay refreshes are re-issued once below.
        let mut content_synced = false;

        for key in &keys {
            // The encoding/sync kind aren't known until this server's `initialize`
            // reply lands (the `Initialized` event). Until then it just waits — the
            // URI is already recorded, so it opens as soon as it is ready.
            let Some(&ServerRuntime {
                encoding,
                sync_kind,
                client_id,
                ..
            }) = self.lsp_servers.get(key)
            else {
                continue;
            };
            let Some(doc) = state.doc_mut(key) else {
                continue;
            };

            if !doc.opened {
                // First open (or re-open after a respawn): full text supersedes any
                // journaled deltas. The journal is dropped only once, by the first
                // such server; a *second* server opening later must not discard
                // deltas the first one still needs, so the drop is guarded on the
                // batch not having been taken for a `didChange` this sync.
                let text = self.editor.buffer().text.to_string();
                // Seed the sync shadow: this is exactly the text the server now
                // holds, so later incremental `didChange`s replay their deltas over it.
                doc.shadow.clone_from(&text);
                doc.version = 1;
                self.fx.lsp_notify(
                    key.clone(),
                    LspNotify::DidOpen {
                        uri: uri.clone(),
                        language_id: language_id.clone(),
                        version: doc.version,
                        text,
                    },
                );
                doc.opened = true;
                doc.last_tick = cur_tick;
                // The freshly-opened content is the on-disk state, so don't fire a
                // spurious `didSave` for saves that predate the open.
                doc.last_save_tick = cur_save_tick;
                attached.push(client_id);
                content_synced = true;
            } else if cur_tick != doc.last_tick && sync_kind != TextDocumentSyncKind::NONE {
                let batch = batch.get_or_insert_with(|| self.editor.buffer_mut().take_lsp_edits());
                doc.version += 1;
                let changes = Self::did_change_content(
                    self.editor.buffer(),
                    &mut doc.shadow,
                    batch,
                    sync_kind,
                    encoding,
                );
                self.fx.lsp_notify(
                    key.clone(),
                    LspNotify::DidChange {
                        uri: uri.clone(),
                        version: doc.version,
                        changes,
                    },
                );
                doc.last_tick = cur_tick;
                content_synced = true;
            }

            // Save: the buffer's write counter advanced since the last sync, so a `:w`
            // landed bytes on disk (a real hook, not a `modified`-flag heuristic).
            if doc.opened && cur_save_tick != doc.last_save_tick {
                self.fx.lsp_notify(
                    key.clone(),
                    LspNotify::DidSave {
                        uri: uri.clone(),
                        text: None,
                    },
                );
                doc.last_save_tick = cur_save_tick;
            }
        }

        // Every attached server saw this tick's deltas (or opened fresh at it), so the
        // journal has served its purpose and must not replay on the next sync.
        if batch.is_none() && content_synced {
            let _ = self.editor.buffer_mut().take_lsp_edits();
        }

        self.lsp_states.insert(buffer, state);

        // The attach hooks fire after the state is back in the map (so an `on_attach`
        // that re-enters the LSP paths sees a consistent state) — once per server that
        // just sent its first `didOpen`. `sync_lsp` only ever syncs the current buffer,
        // so the snapshot each autocmd reads is this buffer.
        if !attached.is_empty() {
            let file = path.to_string_lossy().into_owned();
            for client_id in attached {
                self.fire_lsp_attach(buffer, &file, client_id);
            }
        }

        // Refresh semantic tokens and inlay hints whenever any server saw new content
        // (each request no-ops unless the server advertised the feature and, for inlay
        // hints, the buffer enabled it). After the attach hooks, so an `on_attach` that
        // toggles either feature is already in effect.
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
        let Some(uri) = state.uri.clone() else {
            return;
        };
        // Every server that has this document open and wants deltas. Collected first
        // so the single journal drain below is shared by all of them.
        let targets: Vec<(ServerKey, PositionEncoding, TextDocumentSyncKind)> = state
            .servers()
            .filter(|(_, doc)| doc.opened)
            .filter_map(|(key, _)| {
                let rt = self.lsp_servers.get(key)?;
                (rt.sync_kind != TextDocumentSyncKind::NONE)
                    .then(|| (key.clone(), rt.encoding, rt.sync_kind))
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        // Drained once and replayed into each server's own shadow — see `sync_lsp`.
        let batch = self.editor.take_lsp_edits_of(id).unwrap_or_default();
        if batch.is_empty() {
            return;
        }
        let cur_tick = self
            .editor
            .buffer_of(id)
            .map(|b| b.changedtick)
            .unwrap_or(0);
        for (key, encoding, sync_kind) in targets {
            let buffer = self.editor.buffer_of(id).unwrap();
            let Some(doc) = self.lsp_states.get_mut(&id).and_then(|s| s.doc_mut(&key)) else {
                continue;
            };
            let changes =
                Self::did_change_content(buffer, &mut doc.shadow, &batch, sync_kind, encoding);
            doc.version += 1;
            doc.last_tick = cur_tick;
            let version = doc.version;
            self.fx.lsp_notify(
                key,
                LspNotify::DidChange {
                    uri: uri.clone(),
                    version,
                    changes,
                },
            );
        }
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
        let r = self
            .lua
            .fire_autocmd_data("LspAttach", file, buf.0, file, client_id);
        self.report_autocmd_err("LspAttach", r);
        self.apply_lua_effects();
    }

    /// Fire `LspDetach` for `buf` with `client_id` as `args.data.client_id` — the
    /// detach counterpart to [`EditHost::fire_lsp_attach`]. Unlike attach it does
    /// not push a buffer snapshot: detach fires for a buffer being closed
    /// (`didClose`) or a server that exited, neither of which is necessarily the
    /// current buffer. User `LspDetach` callbacks still get `args.buf`/`data`.
    pub(crate) fn fire_lsp_detach(&mut self, buf: BufferId, file: &str, client_id: u64) {
        let r = self
            .lua
            .fire_autocmd_data("LspDetach", file, buf.0, file, client_id);
        self.report_autocmd_err("LspDetach", r);
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
                self.close_lsp_state(id, state);
            }
        }
    }

    /// A buffer's stored `path` as an absolute one, resolved against the **session's**
    /// effective directory rather than this process's cwd. Buffer paths are stored the
    /// way they were opened — `:e src/main.rs` keeps `src/main.rs`, and a workspace
    /// edit names a file it creates the same way — while a daemon session's files live
    /// on the remote, where the local process cwd means nothing. (Locally the two are
    /// the same: `fix_current_dir` keeps the process cwd on the effective dir.)
    pub(crate) fn abs_buffer_path(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            return path.to_path_buf();
        }
        let win = self.editor.current_window_id();
        let tab = self.editor.current_tab_id();
        let (_, base) = self.dirs.effective(win, tab);
        base.join(path)
    }

    /// The `file://` URI addressing a buffer's document, from its stored path (see
    /// [`abs_buffer_path`](Self::abs_buffer_path)). `None` when no URI can be formed.
    pub(crate) fn buffer_uri(&self, path: &Path) -> Option<Url> {
        Url::from_file_path(self.abs_buffer_path(path)).ok()
    }

    /// Close buffer `id`'s LSP document but **keep its servers**: `didClose` on the URI
    /// they currently hold, then reset each one's per-document state so the next sync
    /// re-`didOpen`s under the buffer's new URI.
    ///
    /// This is what a **file move** needs (a workspace edit's `rename`): the same
    /// buffer, on the same servers, is a *different document* afterwards — a server
    /// left holding the old URI would answer about a path that no longer exists, while
    /// dropping the state outright would silently detach the buffer from its servers
    /// (nothing re-attaches it: the `FileType` that bound them doesn't fire again when
    /// only the stem changed).
    pub(crate) fn reopen_lsp_document(&mut self, id: BufferId) {
        let Some(state) = self.lsp_states.get_mut(&id) else {
            return;
        };
        // Taken, so the next `sync_lsp_buffer` recomputes it from the buffer's path.
        let Some(uri) = state.uri.take() else {
            return;
        };
        let opened: Vec<ServerKey> = state
            .servers()
            .filter(|(_, doc)| doc.opened)
            .map(|(k, _)| k.clone())
            .collect();
        // A fresh document per server: not opened, version 0, empty shadow, and none
        // of the old URI's decorations (the server re-publishes for the new one).
        for (_, doc) in state.servers_mut() {
            *doc = LspServerDoc::default();
        }
        for key in opened {
            if let Some(client_id) = self.lsp_servers.get(&key).map(|r| r.client_id) {
                let file = uri_to_path(&uri)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.fire_lsp_detach(id, &file, client_id);
            }
            self.fx
                .lsp_notify(key, LspNotify::DidClose { uri: uri.clone() });
        }
    }

    /// The close itself, given the state already taken out of `lsp_states`.
    fn close_lsp_state(&mut self, id: BufferId, state: LspDocState) {
        // Every server this buffer had open gets its own `didClose` and its own
        // `LspDetach` — symmetric with the per-server attach-on-`didOpen`.
        let opened: Vec<ServerKey> = state
            .servers()
            .filter(|(_, doc)| doc.opened)
            .map(|(k, _)| k.clone())
            .collect();
        let Some(uri) = state.uri else {
            return;
        };
        for key in opened {
            // Fire `LspDetach` before the close goes out, while the runtime — and so
            // the client id — is still around.
            if let Some(client_id) = self.lsp_servers.get(&key).map(|r| r.client_id) {
                let file = uri_to_path(&uri)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.fire_lsp_detach(id, &file, client_id);
            }
            self.fx
                .lsp_notify(key, LspNotify::DidClose { uri: uri.clone() });
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
                        providers: caps.providers.clone(),
                        // Flatten the advertised trigger/retrigger strings to `char`s
                        // (each is a single character in practice); the auto-trigger
                        // matches a typed key against these.
                        signature_trigger_chars: caps
                            .providers
                            .signature_trigger_chars
                            .iter()
                            .filter_map(|s| s.chars().next())
                            .collect(),
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
                    if let Some(doc) = state.doc_mut(&key) {
                        doc.opened = false;
                        doc.version = 0;
                    }
                }
                self.lsp_dirty = true;
                // Now that this server's advertised signature trigger chars are known,
                // refresh core's auto-trigger set (a no-op unless the user opted in).
                self.refresh_signature_autotrigger();
            }
            LspEvent::Diagnostics {
                key,
                uri,
                diagnostics,
                ..
            } => {
                // Cache the latest publish under the SERVER that sent it; the redraw
                // projects whichever buffer is current (route by `uri`, dropping a
                // publish for a buffer closed while it was in flight). Mark dirty so
                // the coalesced repaint paints the new squiggles.
                //
                // Per-server is mandatory once a buffer can carry two: `publishDiagnostics`
                // is a push, so both servers publish independently, and a shared slot
                // would have each one's set erase the other's on every publish.
                let buffer = self
                    .lsp_states
                    .iter_mut()
                    .find(|(_, s)| s.uri.as_ref() == Some(&uri))
                    .and_then(|(id, state)| {
                        let doc = state.doc_mut(&key)?;
                        doc.diagnostics = diagnostics;
                        Some(*id)
                    });
                // The Lua mirror is the buffer's WHOLE set, so it merges across
                // servers — otherwise `vim.diagnostic.get` would report only
                // whichever server published last. Each server's set is projected
                // with ITS OWN `client_id`, so a reader can tell the type-checker's
                // errors from the linter's; the merged list is otherwise
                // indistinguishable from one server publishing everything.
                let mirror = buffer.and_then(|id| {
                    let state = self.lsp_states.get(&id)?;
                    let all: Vec<DiagnosticData> = state
                        .servers()
                        .flat_map(|(k, d)| {
                            let client_id = self.lsp_servers.get(k).map(|rt| rt.client_id);
                            diagnostic_mirror_data(&d.diagnostics, client_id)
                        })
                        .collect();
                    Some((id.0, all))
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
                //
                // Clear the lazy-start guard for the dead server, so the next
                // `vim.lsp.start` / FileType dispatch re-`ensure`s it. Without this
                // a server whose breaker gave up (or, on wasm, any exited server —
                // the sync client doesn't auto-respawn) could never be started
                // again: the guard would swallow every later start. While the
                // native breaker is still retrying, the extra `lsp_ensure` a
                // re-start sends is idempotent (the manager leaves a still-open
                // server's task alone). Cleared unconditionally — a server that
                // died before `Initialized` never registered in `lsp_servers`, and
                // it especially must stay re-startable.
                self.lsp_ensured.remove(&key);
                // Retire this server's slot in any open fan-out round: its reply is
                // never coming, and without this the round would wait on it forever
                // (the merged result would simply never present).
                self.drop_fanout_server(&key);
                // Same reasoning for the whole-buffer decoration requests and the
                // lazy inlay resolves it had outstanding: those replies are never
                // coming, and both maps are keyed per (buffer, server) rather than
                // being single slots, so a dead server's entries would otherwise
                // accumulate one per buffer it served.
                self.lsp_multi_requests.retain(|_, p| p.server != key);
                self.inlay_resolves.retain(|_, t| t.server != key);
                // A `workspace/applyEdit` this server was still waiting on: there is
                // nobody left to answer (its request died with the connection), so drop
                // the held-back response rather than keep a record that can only be
                // settled into the void. The edit's own file operations are left to
                // finish — they were accepted, and the buffers they touch are still here.
                self.pending_apply_edits.retain(|_, p| p.key != key);
                if let Some(client_id) = self.lsp_servers.remove(&key).map(|r| r.client_id) {
                    // Buffers attached to this server, with a display name for the
                    // event's `args.file`. Clear `opened` so a later `:bdelete`
                    // doesn't re-fire `LspDetach`, and so a respawn re-`didOpen`s.
                    let detaching: Vec<(BufferId, String)> = self
                        .lsp_states
                        .iter_mut()
                        .filter(|(_, s): &(&BufferId, &mut LspDocState)| s.is_opened_under(&key))
                        .map(|(id, s)| {
                            if let Some(doc) = s.doc_mut(&key) {
                                doc.opened = false;
                                doc.version = 0;
                            }
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
            LspEvent::ApplyEdit {
                key,
                id,
                label,
                changes,
            } => self.on_apply_edit(key, id, label, changes),
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
            .filter(|(_, s)| s.bound_to(&key))
            .map(|(id, _)| *id)
            .collect();
        for buffer in buffers {
            match kind {
                RefreshKind::InlayHint => self.request_inlay_hints(buffer),
                RefreshKind::SemanticTokens => {
                    // A refresh means "recompute"; drop the delta cursor so the
                    // re-request fetches the whole `full` set (like force_refresh).
                    // Only for the server that ASKED — the others' cursors are still
                    // valid, and invalidating them would refetch two whole token sets
                    // because one server recomputed.
                    if let Some(doc) = self
                        .lsp_states
                        .get_mut(&buffer)
                        .and_then(|s| s.doc_mut(&key))
                    {
                        doc.semantic.result_id = None;
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
        // EVERY attached server, in key order — a buffer routinely carries several
        // (`pyright` + `ruff`), each with its own negotiated encoding, sync kind,
        // document version and diagnostics. Reporting only the first described half
        // the state with no hint that the rest existed, which is worse than
        // incomplete on the surface whose whole job is to say what is attached.
        let servers: Vec<(&ServerKey, &LspServerDoc)> = self
            .lsp_states
            .get(&current)
            .map(|s| s.servers().collect())
            .unwrap_or_default();
        if servers.is_empty() {
            lines.push("  (no language server for this buffer)".to_string());
        }
        for (i, (key, doc)) in servers.iter().enumerate() {
            if i > 0 {
                lines.push(String::new());
            }
            let runtime = self.lsp_servers.get(*key);
            lines.push(format!(
                "  server:      {} ({})",
                key.name,
                key.root.display()
            ));
            lines.push(format!(
                "  status:      {}",
                if !self.lsp_ensured.contains(*key) {
                    "not started"
                } else if runtime.is_none() {
                    "starting (awaiting initialize)"
                } else if doc.opened {
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
            lines.push(format!("  version:     {}", doc.version));
            lines.push(format!("  diagnostics: {}", doc.diagnostics.len()));
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
                    .filter(|s| s.is_opened_under(key))
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
        let key = self.lsp_states.get(&id)?.primary_key()?;
        Some(self.lsp_servers.get(key)?.encoding)
    }

    /// The position encoding a reply from `key` is authored in, falling back to the
    /// current buffer's first server (then utf-8) when the producing server isn't
    /// known — the Lua-supplied `apply_workspace_edit`, which has no server at all.
    ///
    /// The fallback is deliberately last: with several servers on a buffer, reading
    /// a reply at the *first* server's encoding is right only by luck, so every
    /// native path threads the answering server through instead.
    pub(crate) fn reply_encoding(&self, key: Option<&ServerKey>) -> PositionEncoding {
        key.and_then(|k| self.lsp_servers.get(k))
            .map(|rt| rt.encoding)
            .or_else(|| self.buffer_encoding(self.editor.current_buffer_id()))
            .unwrap_or(PositionEncoding::Utf8)
    }
}
