//! Buffer-mutating features: applying formatting/rename/workspace edits and
//! code actions (including the command-dispatch and resolve round-trips), plus
//! the byte<->LSP position conversion the apply path uses.

use nxvim_lsp::lsp_types::{Location, Position, Range, TextEdit, Url, WorkspaceEdit};
use nxvim_lsp::serde_json;
use nxvim_lsp::{
    normalize_workspace_edit, CodeActionData, LspRequest, PositionEncoding, WorkspaceEditData,
};

use super::*;
use crate::EditHost;

impl EditHost {
    /// Convert an LSP [`Range`] (in the negotiated `encoding`) to an absolute
    /// **current-buffer** byte range, resolving each endpoint against its line.
    pub(crate) fn lsp_range_to_bytes(
        &self,
        range: &Range,
        encoding: PositionEncoding,
    ) -> std::ops::Range<usize> {
        lsp_range_to_bytes_in(self.editor.buffer(), range, encoding)
    }

    /// A current-buffer `(row, byte-column)` point as an LSP [`Position`] in the
    /// server's negotiated encoding (Decision 4).
    pub(crate) fn lsp_position(
        &self,
        encoding: PositionEncoding,
        row: usize,
        byte_col: usize,
    ) -> Position {
        lsp_position_in(self.editor.buffer(), encoding, row, byte_col)
    }

    /// Apply whole-document formatting edits to the current buffer (one undo
    /// step) and re-sync so the server's version stays consistent. Empty ⇒ a
    /// brief message (already formatted), so a no-op re-run is visible.
    pub(crate) fn apply_formatting_edits(&mut self, edits: Vec<TextEdit>) {
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

    /// Apply a `WorkspaceEdit` handed up from Lua (`vim.lsp.util.apply_workspace_edit`,
    /// Phase 7). Deserializes the LSP-shape JSON and normalizes it through the same
    /// path the native rename / code-action replies use, then applies the
    /// per-document edits across the open buffers it names. A malformed edit is
    /// echoed (loud, per the no-silent-stubs rule), never silently dropped.
    pub(crate) fn apply_lua_workspace_edit(&mut self, edit: serde_json::Value) {
        match serde_json::from_value::<WorkspaceEdit>(edit) {
            Ok(edit) => {
                self.apply_workspace_edit(normalize_workspace_edit(edit));
                self.lsp_dirty = true;
            }
            Err(e) => self
                .editor
                .echo(format!("apply_workspace_edit: malformed edit: {e}")),
        }
    }

    /// Jump to an LSP location handed up from Lua (`vim.lsp.util.show_document`,
    /// Phase 7). Builds a [`Location`] from the URI / position and reuses the native
    /// single-location goto (open the file if needed, then refine the byte column on
    /// the landed line). An invalid URI is echoed rather than silently ignored.
    pub(crate) fn show_lua_document(
        &mut self,
        uri: &str,
        line: u32,
        character: u32,
        encoding: &str,
    ) {
        let Ok(url) = Url::parse(uri) else {
            self.editor
                .echo(format!("show_document: invalid uri: {uri}"));
            return;
        };
        let encoding = match encoding {
            "utf-8" => PositionEncoding::Utf8,
            "utf-32" => PositionEncoding::Utf32,
            _ => PositionEncoding::Utf16,
        };
        let position = Position { line, character };
        let location = Location {
            uri: url,
            range: Range {
                start: position,
                end: position,
            },
        };
        self.jump_to_lsp_location(&location, encoding);
        self.lsp_dirty = true;
    }

    /// Apply a normalized workspace edit (from rename or a code action) across the
    /// files it touches. Each URI is resolved to a buffer: the **open** buffer it
    /// names, else the file is loaded into a buffer on the spot so a project-wide
    /// rename reaches files you haven't visited — the edit lands in memory (the buffer
    /// left modified, saved with `:wa`), exactly as neovim's `apply_text_edits` does
    /// rather than writing straight to disk. Each URI's edits convert to bytes against
    /// *its* buffer (a freshly-loaded buffer has no negotiated encoding, so it falls
    /// back to the originating — current — server's), apply as one undo step, and
    /// re-sync.
    ///
    /// Loading an unopened file differs by session:
    /// - **Local** ([`Editor::ensure_buffer_loaded`]): the file is read synchronously
    ///   and edited inline, here and now.
    /// - **Off-tick** (daemon / web — [`Editor::host_fs_offtick`]): the file's bytes
    ///   cross the wire, so the load is async. The replica buffer is created and its
    ///   fetch enqueued ([`Editor::enqueue_replica_open`]), and these edits are stashed
    ///   in [`pending_replica_edits`](EditHost::pending_replica_edits); they apply when
    ///   the bytes land ([`apply_pending_replica_edit`](EditHost::apply_pending_replica_edit)).
    ///   Applying now would hit an empty buffer the fetch would then clobber.
    ///
    /// A URI whose file can't be brought into a buffer at all (a load failure, or a
    /// URI that doesn't map to a path) is collected and reported loud rather than
    /// silently dropped (the no-silent-stubs rule). An edit that touches — and defers —
    /// nothing applicable reports a brief message.
    pub(crate) fn apply_workspace_edit(&mut self, changes: WorkspaceEditData) {
        // The originating server's encoding (the current buffer's, where the rename /
        // code action was requested): the WorkspaceEdit's positions are all in that
        // one encoding, so a target buffer with no server of its own uses it.
        let origin_encoding = self
            .buffer_encoding(self.editor.current_buffer_id())
            .unwrap_or(PositionEncoding::Utf8);
        let mut touched = 0usize;
        let mut deferred = 0usize;
        let mut unresolved: Vec<String> = Vec::new();
        for (uri, edits) in changes {
            if edits.is_empty() {
                continue;
            }
            // The open buffer for the URI, else bring its file into one. A URI we
            // can't resolve to a buffer is recorded so it can be reported, never
            // silently skipped.
            let id = match self.buffer_id_for_uri(&uri) {
                Some(id) => id,
                None => {
                    let Some(path) = uri_to_path(&uri) else {
                        unresolved.push(uri.to_string());
                        continue;
                    };
                    // `buffer_id_for_uri` resolves symlinks via `fs::canonicalize`, which
                    // fails for an **off-tick / virtual** path the local disk can't see —
                    // so an already-open replica buffer slips past it. Match by normalized
                    // path here and apply inline; only a genuinely unopened file defers.
                    if let Some(id) = self.editor.find_buffer_by_path(&path) {
                        id
                    } else if self.editor.host_fs_offtick() {
                        // Off-tick: create the replica buffer + enqueue its fetch now,
                        // stash these edits to apply when the bytes land.
                        let id = self.editor.enqueue_replica_open(&path);
                        self.pending_replica_edits
                            .entry(id)
                            .or_insert_with(|| PendingReplicaEdit {
                                edits: Vec::new(),
                                encoding: origin_encoding,
                            })
                            .edits
                            .extend(edits);
                        deferred += 1;
                        continue;
                    } else {
                        match self.editor.ensure_buffer_loaded(&path) {
                            Some(id) => id,
                            None => {
                                unresolved.push(uri.to_string());
                                continue;
                            }
                        }
                    }
                }
            };
            let encoding = self.buffer_encoding(id).unwrap_or(origin_encoding);
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
        if !unresolved.is_empty() {
            self.editor.echo(format!(
                "apply_workspace_edit: could not open {}",
                unresolved.join(", ")
            ));
        } else if touched == 0 && deferred == 0 {
            self.editor.echo("No applicable changes");
        }
    }

    /// Apply the workspace edits stashed for an **off-tick** replica buffer once its
    /// bytes have landed — the deferred tail of [`apply_workspace_edit`], called from
    /// both fetch-landing sites (`load_replica_bytes` native, `load_replica_wasm`
    /// wasm). Converts each stashed edit's LSP range to bytes against the now-filled
    /// buffer, in the originating server's encoding (a freshly-fetched replica has no
    /// server of its own yet), applies as one undo step, and re-syncs. A no-op when
    /// nothing is stashed for `buffer` — the common case on every other open.
    pub(crate) fn apply_pending_replica_edit(&mut self, buffer: BufferId) {
        let Some(pending) = self.pending_replica_edits.remove(&buffer) else {
            return;
        };
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return;
        };
        let byte_edits = pending
            .edits
            .iter()
            .map(|e| {
                (
                    lsp_range_to_bytes_in(buf, &e.range, pending.encoding),
                    e.new_text.clone(),
                )
            })
            .collect();
        self.editor.apply_edits_to(buffer, byte_edits);
        self.sync_lsp_buffer(buffer);
    }

    /// Offer a code-action reply's titles in the **select menu** (neovim's
    /// `vim.ui.select` model) and stash the actions so confirming applies the chosen
    /// one (`pending_code_action`, keyed by the chosen index). An empty reply shows a
    /// brief message instead of an empty menu.
    /// `cb_id` (`0` = fire-and-forget) is the async `code_action` promise: it is
    /// *stashed* onto the chooser (settled later on the confirm/cancel path), or
    /// settled `nil` now on an empty reply.
    pub(crate) fn show_code_actions(&mut self, actions: Vec<CodeActionData>, cb_id: u64) {
        if actions.is_empty() {
            self.editor.echo(LspReqKind::CodeAction.empty_message());
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        }
        let lines: Vec<String> = actions.iter().map(|a| a.title.clone()).collect();
        self.lsp_code_actions = actions;
        self.editor
            .open_menu(lines, nxvim_core::MenuPlacement::Cursor, 0);
        // The select-menu → `apply_code_action` routing is native-only (the field and its
        // consumer in `effects.rs` are `#[cfg(feature = "native")]`), so the flag it sets
        // is too — keeps the wasm edit-host build (`--no-default-features`) compiling.
        #[cfg(feature = "native")]
        {
            self.pending_code_action = true;
            // Take over the promise stash. A prior chooser still awaiting confirm is
            // superseded (a second `code_action` before picking) — settle its promise
            // `nil` so it can't hang.
            let prev = std::mem::replace(&mut self.code_action_cb, cb_id);
            if prev != 0 {
                self.settle_lsp_promise(prev, serde_json::Value::Null);
            }
        }
        // On the wasm edit-host there is no confirm→apply path, so the promise would
        // never settle — resolve it `nil` now rather than leave it hanging.
        #[cfg(not(feature = "native"))]
        self.settle_lsp_promise(cb_id, serde_json::Value::Null);
    }

    /// Apply the code action selected (by index) in the code-action panel: apply
    /// its eager `edit` now, else resolve a lazy action's edit
    /// (`codeAction/resolve`) and apply when the reply lands, else (a bare
    /// command) a brief message. Clears the stashed actions either way; the select
    /// menu has already closed itself on confirm.
    pub(crate) fn apply_code_action(&mut self, index: usize) {
        // The stashed async `code_action` promise (`0` = fire-and-forget). Taken here
        // so every terminal branch settles it exactly once — except the lazy-resolve
        // branch, which hands it to `resolve_code_action` to settle when its reply lands.
        let cb = std::mem::take(&mut self.code_action_cb);
        let action = self.lsp_code_actions.get(index).cloned();
        self.lsp_code_actions.clear();
        let Some(action) = action else {
            self.settle_lsp_promise(cb, serde_json::Value::Null);
            return;
        };
        let has_edit = action.edit.is_some();
        if let Some(changes) = action.edit {
            self.apply_workspace_edit(changes);
            self.lsp_dirty = true;
        }
        // An action may carry a `command` alongside (or instead of) its edit:
        // neovim applies the edit first, then runs the command. Dispatch it through
        // Lua so a client-side `vim.lsp.commands` handler wins over the server's
        // `workspace/executeCommand` (Phase 8).
        if let Some(command) = action.command {
            self.dispatch_lsp_command(command);
            // Edit applied + command dispatched — the action's effect is done.
            self.settle_lsp_promise(cb, serde_json::Value::Null);
        } else if !has_edit {
            if let Some(raw) = action.resolve {
                // A lazy action: ask the server to fill in its edit, then apply
                // when the reply lands (reply-as-event, like format/rename). The
                // promise rides the resolve request and settles on that reply.
                self.resolve_code_action(raw, cb);
            } else {
                self.editor.echo("Code action has no edit");
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb, serde_json::Value::Null);
            }
        } else {
            // An eager edit with no command / resolve — applied above; done.
            self.settle_lsp_promise(cb, serde_json::Value::Null);
        }
    }

    /// Dispatch an LSP code-action `command` (Phase 8): route it through Lua's
    /// `vim.lsp._dispatch_command`, which runs a registered client-side
    /// `vim.lsp.commands[name]` handler, else issues a `workspace/executeCommand`
    /// to the current buffer's server (via the Phase-5 `client:request` path). The
    /// queued request drains immediately so it leaves on this tick.
    pub(crate) fn dispatch_lsp_command(&mut self, command: nxvim_lsp::lsp_types::Command) {
        let Some(client_id) = self.current_lsp_client_id() else {
            self.editor.echo("No language server attached");
            return;
        };
        let cmd_json = match serde_json::to_value(&command) {
            Ok(v) => v,
            Err(e) => {
                self.editor
                    .echo(format!("Code action command malformed: {e}"));
                return;
            }
        };
        if let Err(e) = self.lua.run_lsp_command(client_id, &cmd_json) {
            self.editor
                .echo(format!("E5108: Error dispatching command: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Fire a `codeAction/resolve` for a lazy action, recording it as a pending
    /// apply request (content-version guarded, like format/rename); its resolved
    /// edit is applied in [`EditHost::on_lsp_reply`]. `cb_id` (`0` = fire-and-forget)
    /// is the async `code_action` promise, carried on the request so the resolve reply
    /// settles it once the edit applies (no server ⇒ settle `nil` now).
    pub(crate) fn resolve_code_action(
        &mut self,
        action: Box<nxvim_lsp::lsp_types::CodeAction>,
        cb_id: u64,
    ) {
        let Some((key, _uri, _encoding)) = self.lsp_target_or_echo() else {
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        };
        let token = self.register_lsp_request(LspReqKind::ResolveCodeAction, cb_id);
        self.fx
            .lsp_request(key, token, LspRequest::ResolveCodeAction { action });
    }
}
