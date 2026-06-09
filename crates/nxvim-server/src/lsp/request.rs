//! Language-feature requests and their replies: issuing goto/hover/references/
//! signatureHelp/format/rename/codeAction with the stale-drop generation gate,
//! matching replies back, and presenting locations (jump or panel list).

use std::path::Path;

use nxvim_lsp::lsp_types::{Location, Range, Url};
use nxvim_lsp::serde_json;
use nxvim_lsp::{LspNotify, LspReply, LspRequest, PositionEncoding, ReqToken, ServerKey};
use nxvim_lua::CallbackArgs;

use super::*;
use crate::Server;

impl Server {
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
            // Formatting/rename/codeAction(+resolve) and completion-resolve don't
            // share the uniform {uri, position} shape and have their own issue
            // functions below (resolve is fired from the menu, not the cursor).
            LspReqKind::Formatting
            | LspReqKind::Rename
            | LspReqKind::CodeAction
            | LspReqKind::ResolveCodeAction
            | LspReqKind::CompletionResolve
            // Semantic tokens and inlay hints are whole-buffer, issued by
            // `request_semantic_tokens` / `request_inlay_hints` on open/change/enable
            // rather than at the cursor; inlay-hint resolve is fired per lazy hint
            // from `issue_inlay_resolves`, never at the cursor.
            | LspReqKind::SemanticTokens
            | LspReqKind::InlayHints
            | LspReqKind::ResolveInlayHint => return,
        };
        self.lsp.request(key, token, req);
    }

    /// Bump the request generation and register the in-flight request for `kind`
    /// (buffer/cursor/`changedtick` at issue time), returning its [`ReqToken`].
    /// The single home for the staleness bookkeeping every issue function shares.
    pub(crate) fn register_lsp_request(&mut self, kind: LspReqKind) -> ReqToken {
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
            // Native typed requests dispatch by kind/generation, never by callback.
            cb_id: 0,
        }
    }

    /// Resolve a Lua `client_id` to its server [`ServerKey`] — the reverse of the
    /// id assigned at `Initialized`. `None` once that server has exited.
    pub(crate) fn client_id_to_key(&self, client_id: u64) -> Option<ServerKey> {
        self.lsp_servers
            .iter()
            .find(|(_, rt)| rt.client_id == client_id)
            .map(|(key, _)| key.clone())
    }

    /// `client:request(method, params, handler)` (Phase 5): forward a generic LSP
    /// request to `client_id`'s server, tagging the reply with `cb_id` so it
    /// routes to the Lua handler when it lands ([`Server::on_client_request_reply`]).
    /// If the client has no running server, fail loud — run the handler now with an
    /// error rather than dropping it, so the caller learns the request didn't go.
    pub(crate) fn client_request(
        &mut self,
        client_id: u64,
        method: String,
        params: serde_json::Value,
        cb_id: u64,
    ) {
        let Some(key) = self.client_id_to_key(client_id) else {
            self.on_client_request_reply(
                cb_id,
                Err(format!("LSP client {client_id} is not running")),
            );
            return;
        };
        let token = ReqToken {
            // No native kind/generation: a raw reply routes purely by `cb_id`.
            kind: u16::MAX,
            generation: 0,
            cb_id,
        };
        self.lsp
            .request(key, token, LspRequest::Raw { method, params });
    }

    /// `client:notify(method, params)` (Phase 5): fire-and-forget a generic LSP
    /// notification to `client_id`'s server. A notification carries no reply, so a
    /// missing server is echoed loudly rather than routed to a handler.
    pub(crate) fn client_notify(
        &mut self,
        client_id: u64,
        method: String,
        params: serde_json::Value,
    ) {
        let Some(key) = self.client_id_to_key(client_id) else {
            self.editor.echo(format!(
                "nxvim: client:notify: LSP client {client_id} is not running"
            ));
            return;
        };
        self.lsp.notify(key, LspNotify::Raw { method, params });
    }

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
        self.lsp.request(
            key,
            token,
            LspRequest::Formatting {
                uri,
                tab_size: self.editor.tabstop() as u32,
                insert_spaces: self.editor.buffer().options.expandtab,
            },
        );
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

    /// The current buffer's `(server, uri, encoding)` once its server finished
    /// `initialize` (so the negotiated encoding is known) — the precondition for
    /// any position-based request. `None` otherwise.
    pub(crate) fn current_lsp_target(&self) -> Option<(ServerKey, Url, PositionEncoding)> {
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
        // The buffer/tick the request was issued for — semantic tokens are
        // whole-buffer (cache to the issuing buffer regardless of focus, drop on
        // *its* content change), so they use these rather than the current-buffer
        // staleness above.
        let req_buffer = pending.buffer;
        let req_tick = pending.tick;
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
            LspReply::ResolvedCompletion {
                documentation,
                detail,
            } => {
                // The menu follows the cursor while open (like a completion reply),
                // so only a buffer change drops it; the merge itself is a no-op if
                // the menu closed or the selection moved on.
                if buffer_changed {
                    return;
                }
                self.merge_resolved_completion(documentation, detail);
            }
            LspReply::SemanticTokens(data) => {
                // Whole-buffer, focus-independent: cache to the issuing buffer
                // (which may not be current) and let `on_semantic_tokens_reply`
                // drop it on that buffer's own content change.
                self.on_semantic_tokens_reply(req_buffer, req_tick, data);
            }
            LspReply::InlayHints(hints) => {
                // Whole-buffer, focus-independent like semantic tokens: cache to the
                // issuing buffer and drop on *its* content change.
                self.on_inlay_hints_reply(req_buffer, req_tick, hints);
            }
            // Generic `client:request` replies are routed to their Lua handler in
            // `on_lsp_event` before reaching here, never through the typed path.
            LspReply::Raw(_) => unreachable!("raw replies are routed in on_lsp_event"),
            // Inlay-hint resolve replies route by `cb_id` in `on_lsp_event` (many
            // can be in flight at once), never through the single-slot kind path.
            LspReply::ResolvedInlayHint { .. } => {
                unreachable!("inlay-hint resolve replies are routed in on_lsp_event")
            }
        }
    }

    /// Run the Lua handler a generic `client:request` (Phase 5) registered under
    /// `cb_id`, handing it `(err, result)` — `Err(message)` becomes the error arg
    /// (result nil), `Ok(value)` the result arg (err nil). The reply always fires
    /// the handler: unlike the typed editor features, a config command's reply is
    /// not subject to cursor/buffer staleness dropping. Called from the
    /// `lsp_events` arm, whose `settle_events` tail drives any deferred work (a
    /// handler that `vim.cmd`s / `vim.schedule`s) to convergence and repaints.
    pub(crate) fn on_client_request_reply(
        &mut self,
        cb_id: u64,
        res: Result<serde_json::Value, String>,
    ) {
        let args = match res {
            Ok(result) => CallbackArgs::LspReply { err: None, result },
            Err(message) => CallbackArgs::LspReply {
                err: Some(message),
                result: serde_json::Value::Null,
            },
        };
        if let Err(e) = self.lua.run_callback(cb_id, false, args) {
            self.editor
                .echo(format!("E5108: Error in client:request handler: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Render a hover reply: open the bottom panel with the markup's plain lines
    /// (the panel is the hover surface until floats exist — Decision 7). An empty
    /// reply shows a brief message instead of an empty panel.
    pub(crate) fn show_hover(&mut self, lines: Vec<String>) {
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
    pub(crate) fn show_signature_help(
        &mut self,
        signature: Option<String>,
        active_parameter: Option<String>,
    ) {
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
    pub(crate) fn apply_lsp_locations(&mut self, kind: LspReqKind, locations: Vec<Location>) {
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
    pub(crate) fn jump_to_lsp_location(&mut self, loc: &Location, encoding: PositionEncoding) {
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
    pub(crate) fn open_locations_panel(
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
    pub(crate) fn location_byte_col(
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

    /// The Lua `client_id` of the current buffer's language server (the reverse of
    /// the [`ServerKey`] resolved by [`Self::current_lsp_target`]), or `None` when
    /// no server is attached / initialized. Used to route a code-action command to
    /// the right client.
    pub(crate) fn current_lsp_client_id(&self) -> Option<u64> {
        let (key, _, _) = self.current_lsp_target()?;
        self.lsp_servers.get(&key).map(|r| r.client_id)
    }
}
