//! Buffer-mutating features: applying formatting/rename/workspace edits and
//! code actions (including the command-dispatch and resolve round-trips), plus
//! the byte<->LSP position conversion the apply path uses.

use nxvim_lsp::lsp_types::{Location, Position, Range, TextEdit, Url, WorkspaceEdit};
use nxvim_lsp::serde_json;
use nxvim_lsp::{
    normalize_workspace_edit, CodeActionData, LspRequest, PositionEncoding, WorkspaceEditData,
};

use super::*;
use crate::Server;

impl Server {
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
    /// names, else the file is loaded into a buffer on the spot
    /// ([`Editor::ensure_buffer_loaded`]) so a project-wide rename reaches files you
    /// haven't visited — the edit lands in memory (the buffer left modified, saved
    /// with `:wa`), exactly as neovim's `apply_text_edits` does rather than writing
    /// straight to disk. Each URI's edits convert to bytes against *its* buffer (a
    /// freshly-loaded buffer has no negotiated encoding, so it falls back to the
    /// originating — current — server's), apply as one undo step, and re-sync.
    ///
    /// A URI whose file can't be brought into a buffer (a load failure, or a
    /// daemon/off-tick session where the load would be async) is collected and
    /// reported loud rather than silently dropped (the no-silent-stubs rule). An
    /// edit that touches nothing applicable reports a brief message.
    pub(crate) fn apply_workspace_edit(&mut self, changes: WorkspaceEditData) {
        // The originating server's encoding (the current buffer's, where the rename /
        // code action was requested): the WorkspaceEdit's positions are all in that
        // one encoding, so a target buffer with no server of its own uses it.
        let origin_encoding = self
            .buffer_encoding(self.editor.current_buffer_id())
            .unwrap_or(PositionEncoding::Utf8);
        let mut touched = 0usize;
        let mut unresolved: Vec<String> = Vec::new();
        for (uri, edits) in changes {
            if edits.is_empty() {
                continue;
            }
            // The open buffer for the URI, else load its file into one. A URI we
            // can't resolve to a buffer (load failure / off-tick async fetch) is
            // recorded so it can be reported, never silently skipped.
            let id = match self.buffer_id_for_uri(&uri) {
                Some(id) => id,
                None => {
                    match uri_to_path(&uri).and_then(|p| self.editor.ensure_buffer_loaded(&p)) {
                        Some(id) => id,
                        None => {
                            unresolved.push(uri.to_string());
                            continue;
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
        } else if touched == 0 {
            self.editor.echo("No applicable changes");
        }
    }

    /// List a code-action reply's titles in a select-enabled panel and stash the
    /// actions so a `<CR>` select applies the chosen one (the `panel_selects`
    /// path, keyed by select index — see the design's code-action note). An empty
    /// reply shows a brief message instead of an empty panel.
    pub(crate) fn show_code_actions(&mut self, actions: Vec<CodeActionData>) {
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
        } else if !has_edit {
            if let Some(raw) = action.resolve {
                // A lazy action: ask the server to fill in its edit, then apply
                // when the reply lands (reply-as-event, like format/rename).
                self.resolve_code_action(raw);
            } else {
                self.editor.echo("Code action has no edit");
                self.lsp_dirty = true;
            }
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
    /// edit is applied in [`Server::on_lsp_reply`].
    pub(crate) fn resolve_code_action(&mut self, action: Box<nxvim_lsp::lsp_types::CodeAction>) {
        self.sync_lsp();
        let Some((key, _uri, _encoding)) = self.current_lsp_target() else {
            self.editor.echo("No language server attached");
            return;
        };
        let token = self.register_lsp_request(LspReqKind::ResolveCodeAction);
        self.fx
            .lsp_request(key, token, LspRequest::ResolveCodeAction { action });
    }
}
