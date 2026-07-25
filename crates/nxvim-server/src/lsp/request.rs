//! Language-feature requests and their replies: issuing goto/hover/references/
//! signatureHelp/format/rename/codeAction with the stale-drop generation gate,
//! matching replies back, and presenting locations (jump or panel list).

use std::path::Path;

use nxvim_lsp::lsp_types::{Location, Range, Url};
use nxvim_lsp::serde_json;
use nxvim_lsp::{
    LspNotify, LspReply, LspRequest, PositionEncoding, ReqToken, ServerKey, SymbolData,
};
use nxvim_lua::CallbackArgs;

use super::*;
use crate::EditHost;

impl EditHost {
    /// Issue a language-feature request of `kind` at the cursor, recording its
    /// generation so a stale reply is dropped (Decision 3): a newer request of
    /// the same kind, or the cursor moving before the reply lands, invalidates
    /// it. No-op (with a brief message) if the current buffer has no server that
    /// has finished `initialize`, since the negotiated encoding isn't known yet.
    pub(crate) fn request_lsp(&mut self, kind: LspReqKind, cb_id: u64) {
        // Flush any pending document edits as a `didChange` *before* the request,
        // so the server computes against the current buffer text. Requests are
        // fired during input — ahead of `redraw`'s own `sync_lsp` — so without
        // this the server would answer a stale document (e.g. completion ranges
        // computed against text the user already changed).
        // Kinds whose answers MERGE go to every capable server at once.
        if LspFanout::is_fanout(kind) {
            self.sync_lsp();
            let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
            // The cursor in each encoding a server might have negotiated, resolved up
            // front: the closure below cannot borrow the buffer while the fan-out
            // borrows `self`, and there are only three.
            let buf = self.editor.buffer();
            let (p8, p16, p32) = (
                lsp_position_in(buf, PositionEncoding::Utf8, row, col),
                lsp_position_in(buf, PositionEncoding::Utf16, row, col),
                lsp_position_in(buf, PositionEncoding::Utf32, row, col),
            );
            let asked =
                self.open_lsp_fanout(kind, cb_id, CodeActionOpts::default(), |_, uri, enc| {
                    let position = match enc {
                        PositionEncoding::Utf8 => p8,
                        PositionEncoding::Utf16 => p16,
                        PositionEncoding::Utf32 => p32,
                    };
                    match kind {
                        LspReqKind::References => LspRequest::References {
                            uri,
                            position,
                            include_declaration: false,
                        },
                        _ => LspRequest::DocumentSymbol { uri },
                    }
                });
            if asked.is_empty() {
                self.editor.echo("No language server attached");
                self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            }
            return;
        }
        // Routed by capability: the first attached server, in key order, that
        // advertises this feature. On a `pyright` + `ruff` buffer a hover must reach
        // pyright, not whichever name happens to sort first.
        let Some((key, uri, encoding)) = self.lsp_target_for_or_echo(kind) else {
            // No capable server: the request never goes, so settle the promise now
            // (resolve `nil`) rather than leave it hanging for a reply that
            // won't come.
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let position = self.lsp_position(encoding, row, col);
        let token = self.register_lsp_request_to(kind, cb_id, &key);
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
            // Document symbols are whole-document (position-less), but ride the
            // cursor-tick request path like the goto family.
            LspReqKind::DocumentSymbol => LspRequest::DocumentSymbol { uri },
            // Workspace symbols carry a query, not the cursor — issued by
            // `request_lsp_workspace_symbol` below, never here.
            LspReqKind::WorkspaceSymbol => return,
            // Completion goes to EVERY capable server, each at its own encoding, and
            // its shares stream into the open menu — `request_lsp_completion` owns
            // that round, so it never rides this single-target path.
            LspReqKind::Completion
            // Formatting/rename/codeAction(+resolve) and completion-resolve don't
            // share the uniform {uri, position} shape and have their own issue
            // functions below (resolve is fired from the menu, not the cursor).
            | LspReqKind::Formatting
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
            // Folding ranges are whole-buffer too, issued by `request_folding_range`
            // on open/change while the buffer wants LSP folds, never at the cursor.
            | LspReqKind::FoldingRange
            | LspReqKind::ResolveInlayHint => return,
        };
        self.fx.lsp_request(key, token, req);
    }

    /// Recompute core's signature-help **auto-trigger** set from the opt-in flag and
    /// the attached servers' advertised chars: the union of every started server's
    /// `signatureHelpProvider` trigger characters while opted in, else empty (which
    /// turns the auto-trigger off and ends any session). Called when the flag toggles
    /// and when a server's `initialize` reply lands. Per-buffer correctness — firing
    /// only where a signature server actually serves — is enforced at drain time, not
    /// here, so the union is a permissive superset.
    pub(crate) fn refresh_signature_autotrigger(&mut self) {
        let chars: Vec<char> = if self.signature_auto {
            let mut set: Vec<char> = Vec::new();
            for rt in self.lsp_servers.values() {
                for &c in &rt.signature_trigger_chars {
                    if !set.contains(&c) {
                        set.push(c);
                    }
                }
            }
            set
        } else {
            Vec::new()
        };
        self.editor.set_signature_trigger_chars(chars);
    }

    /// Drain core's one-shot signature auto-request (raised by a trigger keystroke):
    /// issue `textDocument/signatureHelp` at the cursor — but only when the **current**
    /// buffer's server actually advertises signature trigger chars. In any other buffer
    /// the request is silently dropped (and any session ended) rather than echoing a
    /// "no language server" error on every `(` typed in a non-LSP buffer.
    pub(crate) fn drain_signature_auto_request(&mut self) {
        if !std::mem::take(&mut self.editor.signature_auto_request) {
            return;
        }
        if self.current_buffer_has_signature_trigger() {
            self.request_lsp(LspReqKind::SignatureHelp, 0);
        } else {
            self.editor.end_signature_session();
        }
    }

    /// Whether the current buffer's (initialized) server advertises signature-help
    /// trigger characters — the per-buffer gate for the auto-trigger drain.
    fn current_buffer_has_signature_trigger(&self) -> bool {
        self.current_lsp_target()
            .and_then(|(key, _, _)| self.lsp_servers.get(&key))
            .is_some_and(|rt| !rt.signature_trigger_chars.is_empty())
    }

    /// Bump the request generation and register the in-flight request for `kind`
    /// (buffer/cursor/`changedtick` at issue time), returning its [`ReqToken`].
    /// The single home for the staleness bookkeeping every issue function shares.
    /// `cb_id` (`0` = fire-and-forget) settles the issuing verb's promise; a new
    /// request of the same `kind` **supersedes** the one it replaces, so that
    /// pending's still-live promise is settled `nil` (a benign no-op) here rather
    /// than left to hang — its reply, if it ever lands, is dropped on the
    /// generation mismatch.
    pub(crate) fn register_lsp_request(&mut self, kind: LspReqKind, cb_id: u64) -> ReqToken {
        self.register_lsp_request_with(kind, cb_id, CodeActionOpts::default())
    }

    /// [`register_lsp_request`](Self::register_lsp_request), recording which server
    /// the request went to so the reply is decoded against *that* server.
    pub(crate) fn register_lsp_request_to(
        &mut self,
        kind: LspReqKind,
        cb_id: u64,
        server: &ServerKey,
    ) -> ReqToken {
        let token = self.register_lsp_request_with(kind, cb_id, CodeActionOpts::default());
        if let Some(pending) = self.lsp_requests.get_mut(&kind) {
            pending.server = Some(server.clone());
        }
        token
    }

    /// [`register_lsp_request`](Self::register_lsp_request) carrying the code-action
    /// options (`only` / `apply`) the reply needs; every other kind uses the default.
    pub(crate) fn register_lsp_request_with(
        &mut self,
        kind: LspReqKind,
        cb_id: u64,
        code_action: CodeActionOpts,
    ) -> ReqToken {
        self.lsp_req_gen += 1;
        let generation = self.lsp_req_gen;
        if let Some(prev) = self.lsp_requests.insert(
            kind,
            PendingLspReq {
                generation,
                buffer: self.editor.current_buffer_id(),
                cursor: (self.editor.cursor.line, self.editor.cursor.col),
                tick: self.editor.buffer().changedtick,
                cb_id,
                code_action,
                server: None,
            },
        ) {
            self.settle_lsp_promise(prev.cb_id, serde_json::Value::Null);
        }
        ReqToken {
            kind: kind.as_u16(),
            generation,
            cb_id,
        }
    }

    /// Register a **buffer-scoped** pending request — the semantic-tokens /
    /// inlay-hints / folding-range shape, unlike the cursor-scoped
    /// [`register_lsp_request`](Self::register_lsp_request) which issues for the
    /// *current* buffer: bump the generation and record the issuing `buffer`, its
    /// `changedtick` (so a reply computed against superseded text is dropped) and
    /// the `server` asked (so the reply is decoded against *its* legend and
    /// encoding). These are background refreshes, not user verbs — no promise to
    /// settle.
    ///
    /// Recorded in `lsp_multi_requests`, keyed by the unique generation, so several
    /// can be in flight at once: a buffer asks **every** capable server for its
    /// decorations, and the single-slot kind map would have each request evict the
    /// last. Only a request for the same `(kind, buffer, server)` supersedes — an
    /// older one for that exact triple is retired, since its reply is about to be
    /// replaced anyway.
    pub(crate) fn register_multi_request(
        &mut self,
        kind: LspReqKind,
        buffer: BufferId,
        server: &ServerKey,
    ) -> ReqToken {
        self.lsp_req_gen += 1;
        let generation = self.lsp_req_gen;
        let tick = self.editor.buffer_of(buffer).map_or(0, |b| b.changedtick);
        self.lsp_multi_requests
            .retain(|_, p| !(p.kind == kind && p.buffer == buffer && p.server == *server));
        self.lsp_multi_requests.insert(
            generation,
            PendingMultiReq {
                kind,
                buffer,
                tick,
                server: server.clone(),
            },
        );
        ReqToken {
            kind: kind.as_u16(),
            generation,
            cb_id: 0,
        }
    }

    /// Every attached server on `buffer` that has finished `initialize` and
    /// advertises the provider answering `kind`, in [`ServerKey`] order, with the
    /// encoding each negotiated.
    ///
    /// The plural of [`lsp_target_for`](Self::lsp_target_for): the selector for the
    /// surfaces that ask *all* capable servers — the fan-out rounds (references,
    /// symbols, code actions) and the whole-buffer decorations (semantic tokens,
    /// inlay hints), whose caches are per server.
    pub(crate) fn lsp_capable_servers(
        &self,
        buffer: BufferId,
        kind: LspReqKind,
    ) -> Vec<(ServerKey, PositionEncoding)> {
        let Some(state) = self.lsp_states.get(&buffer) else {
            return Vec::new();
        };
        state
            .servers()
            .filter_map(|(key, _)| {
                let rt = self.lsp_servers.get(key)?;
                match kind.provider(&rt.providers) {
                    Some(true) | None => Some((key.clone(), rt.encoding)),
                    Some(false) => None,
                }
            })
            .collect()
    }

    /// Route a per-server reply (a whole-buffer decoration, or one server's share of
    /// a completion round) using the [`PendingMultiReq`] its generation identifies:
    /// the issuing buffer, the tick to check staleness against, and the **server that
    /// answered** — which is what its legend and position encoding must be read from.
    /// An unknown generation is a superseded request's reply and is dropped; a reply
    /// whose payload doesn't match its kind means the server answered off-protocol,
    /// and is likewise dropped.
    fn on_multi_target_reply(&mut self, token: &ReqToken, reply: LspReply) {
        let Some(pending) = self.lsp_multi_requests.remove(&token.generation) else {
            return; // superseded by a newer request for this (kind, buffer, server)
        };
        let (buffer, tick, key) = (pending.buffer, pending.tick, pending.server);
        match reply {
            LspReply::SemanticTokens(data) => {
                self.on_semantic_tokens_reply(buffer, tick, key, data)
            }
            LspReply::InlayHints(hints) => self.on_inlay_hints_reply(buffer, tick, key, hints),
            LspReply::Folds(folds) => self.on_folding_range_reply(buffer, tick, folds),
            LspReply::Completion {
                is_incomplete,
                items,
            } => self.on_completion_reply(buffer, key, is_incomplete, items),
            _ => {}
        }
    }

    /// Settle an async `nx.lsp.*` verb's promise: run its `nx._cb_fns[cb_id]`
    /// resolver with `(nil, result)` (the resolve-only [`CallbackArgs::LspReply`]
    /// contract — the built-in verbs never reject), then drain the effects the
    /// resolver queued so a verb chained in a `:next` handler issues its request in
    /// this same convergence (mirrors [`on_client_request_reply`](Self::on_client_request_reply)).
    /// A `cb_id` of `0` (a fire-and-forget request) is a no-op.
    pub(crate) fn settle_lsp_promise(&mut self, cb_id: u64, result: serde_json::Value) {
        if cb_id == 0 {
            return;
        }
        // Refresh the buffer mirror so the promise's continuation reads the *applied*
        // effect. The LSP-reply path (format/rename/resolve) gets a fresh push at the
        // next `run_pending` entry before its microtask drains, but the code-action
        // apply runs mid-`run_pending` (after that entry push), so its edit would be
        // invisible to the continuation without this. Cheap: gated on `changedtick`.
        self.push_buf_mirror();
        if let Err(e) =
            self.lua
                .run_callback(cb_id, false, CallbackArgs::LspReply { err: None, result })
        {
            self.editor
                .echo(format!("E5108: Error settling nx.lsp promise: {e}"));
        }
        self.apply_lua_effects();
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
    /// routes to the Lua handler when it lands ([`EditHost::on_client_request_reply`]).
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
        self.fx
            .lsp_request(key, token, LspRequest::Raw { method, params });
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
        self.fx.lsp_notify(key, LspNotify::Raw { method, params });
    }

    /// `:LspFormat` — request `textDocument/formatting` for the current buffer.
    /// On reply, the `TextEdit[]` is applied iff the buffer hasn't changed since
    /// (the content-version guard in [`EditHost::on_lsp_reply`]).
    /// `name` picks the formatting server by config name; `None` takes the first that
    /// advertises `documentFormatting` — so a buffer served by a type-checker that
    /// can't format and a linter that can reaches the one that can.
    ///
    /// A `name` not attached to this buffer is reported by name rather than quietly
    /// formatting with someone else: asking for ruff and silently getting pyright's
    /// formatting is exactly the failure this option exists to prevent.
    pub(crate) fn request_lsp_format(&mut self, cb_id: u64, name: Option<String>) {
        let target = match &name {
            Some(want) => {
                self.sync_lsp();
                let buffer = self.editor.current_buffer_id();
                let found = self
                    .lsp_states
                    .get(&buffer)
                    .and_then(|s| s.servers().find(|(k, _)| &k.name == want).map(|(k, _)| k))
                    .cloned()
                    .and_then(|key| {
                        let uri = self.lsp_states.get(&buffer)?.uri.clone()?;
                        let encoding = self.lsp_servers.get(&key)?.encoding;
                        Some((key, uri, encoding))
                    });
                if found.is_none() {
                    self.editor
                        .echo(format!("No LSP client named '{want}' on this buffer"));
                }
                found
            }
            None => self.lsp_target_for_or_echo(LspReqKind::Formatting),
        };
        let Some((key, uri, _encoding)) = target else {
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        };
        let token = self.register_lsp_request_to(LspReqKind::Formatting, cb_id, &key);
        self.fx.lsp_request(
            key,
            token,
            LspRequest::Formatting {
                uri,
                tab_size: self.editor.tabstop() as u32,
                insert_spaces: self.editor.buffer().options.expandtab,
            },
        );
    }

    /// `nx.lsp.workspace_symbol(query)` — request `workspace/symbol` for `query`.
    /// Unlike the cursor-anchored requests it carries the user's fuzzy query, not a
    /// position; on reply the matching symbols open in the picker (`apply_lsp_symbols`).
    pub(crate) fn request_lsp_workspace_symbol(&mut self, query: &str, cb_id: u64) {
        let Some((key, _uri, _encoding)) = self.lsp_target_or_echo() else {
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        };
        let token = self.register_lsp_request(LspReqKind::WorkspaceSymbol, cb_id);
        self.fx.lsp_request(
            key,
            token,
            LspRequest::WorkspaceSymbol {
                query: query.to_string(),
            },
        );
    }

    /// `:LspRename {newname}` — request `textDocument/rename` at the cursor with
    /// the new name. On reply the returned `WorkspaceEdit` is applied across the
    /// open buffers it touches.
    pub(crate) fn request_lsp_rename(&mut self, new_name: &str, cb_id: u64) {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            self.editor
                .echo("E471: Argument required: :LspRename {newname}");
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        }
        let Some((key, uri, encoding)) = self.lsp_target_for_or_echo(LspReqKind::Rename) else {
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        };
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let position = self.lsp_position(encoding, row, col);
        let token = self.register_lsp_request_to(LspReqKind::Rename, cb_id, &key);
        self.fx.lsp_request(
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
    /// listed in the panel; `<CR>` applies the chosen action's eager edit. `cb_id`
    /// (`0` = fire-and-forget) is the promise the reply *stashes* onto the chooser menu
    /// and settles once the picked action's edit applies (or `nil` on cancel).
    ///
    /// `opts` carries the caller's kind filter and one-shot request
    /// (`nx.lsp.code_action{ context = { only = … }, apply = true }`): `only` rides the
    /// request as `context.only` **and** is re-applied to the reply, and `apply` skips
    /// the chooser when exactly one action survives ([`EditHost::show_code_actions`]).
    ///
    /// `range` is the caller's explicit `opts.range` (0-based rows / byte columns,
    /// end-exclusive) or the line range an ex address resolved to; with none, a **live
    /// Visual / Select selection** supplies it and is consumed (the marks are stamped
    /// and the editor drops to Normal, as vim does for a `:` command on a selection).
    /// With neither, the request is a point at the cursor. The range matters: the
    /// refactor kinds (`refactor.extract`, `refactor.inline`) are exactly the actions a
    /// server gates on a non-empty one.
    pub(crate) fn request_lsp_code_action(
        &mut self,
        cb_id: u64,
        opts: CodeActionOpts,
        range: Option<(usize, usize, usize, usize)>,
    ) {
        self.sync_lsp();
        let selection = range.or_else(|| self.editor.selection_extent());
        // A selection that fed the request is consumed here, *before* the reply's edit
        // can land on it (`leave_selection` is a no-op when the range came from `opts`).
        self.editor.leave_selection();
        let extent = selection.unwrap_or_else(|| {
            let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
            (row, col, row, col)
        });
        let diagnostics = self.diagnostics_in_range(extent);
        let only = opts.only.clone();
        // The request range in each encoding a server might use — resolved before the
        // fan-out borrows `self` (see `request_lsp`).
        let buf = self.editor.buffer();
        let ranges: Vec<(PositionEncoding, Range)> = [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ]
        .into_iter()
        .map(|enc| {
            (
                enc,
                Range {
                    start: lsp_position_in(buf, enc, extent.0, extent.1),
                    end: lsp_position_in(buf, enc, extent.2, extent.3),
                },
            )
        })
        .collect();

        // Every `codeActionProvider` is asked and their actions merge into one
        // chooser — this is the fan-out that matters most in practice: a linter's
        // quick-fixes and a type-checker's refactors are both things you want offered,
        // and asking only one server silently hides half the menu.
        let asked = self.open_lsp_fanout(LspReqKind::CodeAction, cb_id, opts, |_, uri, enc| {
            let range = ranges
                .iter()
                .find(|(e, _)| *e == enc)
                .map(|(_, r)| *r)
                .unwrap_or_default();
            LspRequest::CodeAction {
                uri,
                range,
                diagnostics: diagnostics.clone(),
                only: only.clone(),
            }
        });
        if asked.is_empty() {
            self.editor.echo("No language server attached");
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
        }
    }

    /// Flush pending document edits ([`sync_lsp`](EditHost::sync_lsp)) and resolve the
    /// current buffer's [`current_lsp_target`](Self::current_lsp_target), echoing the
    /// standard "No language server attached" message on `None`. The shared preamble
    /// every cursor/buffer LSP issue function opens with.
    pub(crate) fn lsp_target_or_echo(&mut self) -> Option<(ServerKey, Url, PositionEncoding)> {
        self.sync_lsp();
        let target = self.current_lsp_target();
        if target.is_none() {
            self.editor.echo("No language server attached");
        }
        target
    }

    /// The current buffer's `(server, uri, encoding)` once its server finished
    /// `initialize` (so the negotiated encoding is known) — the precondition for
    /// any position-based request. `None` otherwise.
    pub(crate) fn current_lsp_target(&self) -> Option<(ServerKey, Url, PositionEncoding)> {
        let state = self.lsp_states.get(&self.editor.current_buffer_id())?;
        let key = state.primary_key().cloned()?;
        let uri = state.uri.clone()?;
        let encoding = self.lsp_servers.get(&key)?.encoding;
        Some((key, uri, encoding))
    }

    /// The server on `buffer` that should answer a request of `kind`: the **first**
    /// attached one, in [`ServerKey`] order, that has finished `initialize` and
    /// advertises the matching provider.
    ///
    /// This is what makes a two-server buffer behave. With `pyright` + `ruff`, a
    /// hover must go to pyright because ruff advertises none — picking by position
    /// in the map (as every path did before) would send it to whichever name sorts
    /// first and get nothing back. Ordering by key rather than by hash keeps the
    /// choice stable, so a config that works once works every time.
    ///
    /// Kinds with no modelled provider flag ([`LspReqKind::provider`] → `None`) fall
    /// back to the first initialized server: failing open beats answering nothing.
    pub(crate) fn lsp_target_for(
        &self,
        buffer: BufferId,
        kind: LspReqKind,
    ) -> Option<(ServerKey, Url, PositionEncoding)> {
        let state = self.lsp_states.get(&buffer)?;
        let uri = state.uri.clone()?;
        let (key, encoding) = state.servers().find_map(|(key, _)| {
            let rt = self.lsp_servers.get(key)?;
            match kind.provider(&rt.providers) {
                Some(true) | None => Some((key.clone(), rt.encoding)),
                Some(false) => None,
            }
        })?;
        Some((key, uri, encoding))
    }

    /// [`lsp_target_for`](Self::lsp_target_for) on the current buffer, echoing the
    /// same "no server" message [`lsp_target_or_echo`](Self::lsp_target_or_echo)
    /// does when nothing can answer.
    pub(crate) fn lsp_target_for_or_echo(
        &mut self,
        kind: LspReqKind,
    ) -> Option<(ServerKey, Url, PositionEncoding)> {
        self.sync_lsp();
        let target = self.lsp_target_for(self.editor.current_buffer_id(), kind);
        if target.is_none() {
            self.editor.echo("No language server attached");
        }
        target
    }

    /// Open a fan-out round for `kind`: issue `make` to **every** attached server
    /// that advertises the capability, and register the round so their replies merge.
    ///
    /// Returns the servers asked (empty when none can answer, in which case the
    /// caller settles its promise — no silent nothing). Any round already open for
    /// this kind is superseded: its promise settles `nil` rather than hanging, and
    /// its still-outstanding replies are dropped on arrival (their generations are
    /// gone from the new round).
    pub(crate) fn open_lsp_fanout(
        &mut self,
        kind: LspReqKind,
        cb_id: u64,
        code_action: CodeActionOpts,
        make: impl Fn(&ServerKey, Url, PositionEncoding) -> LspRequest,
    ) -> Vec<ServerKey> {
        let buffer = self.editor.current_buffer_id();
        let Some(state) = self.lsp_states.get(&buffer) else {
            return Vec::new();
        };
        let Some(uri) = state.uri.clone() else {
            return Vec::new();
        };
        let targets = self.lsp_capable_servers(buffer, kind);
        if targets.is_empty() {
            return Vec::new();
        }
        // Supersede any open round for this kind before issuing the new one.
        if let Some(prev) = self.lsp_fanouts.remove(&kind) {
            if prev.cb_id != 0 {
                self.settle_lsp_promise(prev.cb_id, serde_json::Value::Null);
            }
        }
        let mut round = LspFanout {
            outstanding: HashMap::new(),
            cb_id,
            buffer,
            cursor: (self.editor.cursor.line, self.editor.cursor.col),
            tick: self.editor.buffer().changedtick,
            code_action,
            locations: Vec::new(),
            symbols: Vec::new(),
            actions: Vec::new(),
        };
        let mut asked = Vec::new();
        for (key, encoding) in targets {
            self.lsp_req_gen += 1;
            let generation = self.lsp_req_gen;
            round.outstanding.insert(generation, key.clone());
            let token = ReqToken {
                kind: kind.as_u16(),
                generation,
                cb_id: 0,
            };
            let req = make(&key, uri.clone(), encoding);
            self.fx.lsp_request(key.clone(), token, req);
            asked.push(key);
        }
        self.lsp_fanouts.insert(kind, round);
        asked
    }

    /// Fold one reply into its fan-out round, presenting the merged result once the
    /// last outstanding server has answered. Returns whether `token` belonged to a
    /// round (so the single-target reply path skips it).
    fn absorb_fanout_reply(
        &mut self,
        kind: LspReqKind,
        token: &ReqToken,
        reply: &LspReply,
    ) -> bool {
        let Some(round) = self.lsp_fanouts.get_mut(&kind) else {
            return false;
        };
        let Some(server) = round.outstanding.remove(&token.generation) else {
            // Not part of the open round — a straggler from a superseded one.
            return self.lsp_fanouts.contains_key(&kind) && LspFanout::is_fanout(kind);
        };
        match reply {
            LspReply::Locations(locations) => round.locations.extend(locations.iter().cloned()),
            LspReply::Symbols(symbols) => round.symbols.extend(symbols.iter().cloned()),
            LspReply::CodeActions(actions) => round
                .actions
                .extend(actions.iter().map(|a| (server.clone(), a.clone()))),
            // A kind that fans out can only reply with one of the above; anything
            // else means the server answered off-protocol. Drop its slot (already
            // removed) and let the round finish on the others.
            _ => {}
        }
        if !round.outstanding.is_empty() {
            return true; // still waiting on other servers
        }
        let round = self.lsp_fanouts.remove(&kind).expect("just borrowed");
        self.present_fanout(kind, round);
        true
    }

    /// Present a completed fan-out round: the merged payload goes through the same
    /// surface a single-server reply would, so ordering, dedup and the promise
    /// contract are unchanged.
    fn present_fanout(&mut self, kind: LspReqKind, round: LspFanout) {
        let buffer_changed = round.buffer != self.editor.current_buffer_id();
        let cursor_moved = round.cursor != (self.editor.cursor.line, self.editor.cursor.col);
        let tick_changed = round.tick != self.editor.buffer().changedtick;
        match kind {
            LspReqKind::References => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                // Two servers can report the same reference; show it once.
                let mut locations = round.locations;
                locations.dedup_by(|a, b| a.uri == b.uri && a.range == b.range);
                let result = self.apply_lsp_locations(kind, locations);
                self.lsp_dirty = true;
                self.settle_lsp_promise(round.cb_id, result);
            }
            LspReqKind::DocumentSymbol => {
                if buffer_changed {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                let result = self.apply_lsp_symbols(kind, round.symbols);
                self.lsp_dirty = true;
                self.settle_lsp_promise(round.cb_id, result);
            }
            LspReqKind::CodeAction => {
                if buffer_changed || tick_changed {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                let (servers, actions): (Vec<ServerKey>, Vec<CodeActionData>) =
                    round.actions.into_iter().unzip();
                self.show_code_actions_from(actions, servers, round.cb_id, round.code_action);
                self.lsp_dirty = true;
            }
            _ => self.settle_lsp_promise(round.cb_id, serde_json::Value::Null),
        }
    }

    /// Retire every fan-out slot belonging to `key` — its server exited, so its reply
    /// is never coming and the round must not wait on it forever. A round left with
    /// no outstanding servers presents what the others returned.
    pub(crate) fn drop_fanout_server(&mut self, key: &ServerKey) {
        let kinds: Vec<LspReqKind> = self.lsp_fanouts.keys().copied().collect();
        for kind in kinds {
            let Some(round) = self.lsp_fanouts.get_mut(&kind) else {
                continue;
            };
            round.outstanding.retain(|_, k| k != key);
            if round.outstanding.is_empty() {
                let round = self.lsp_fanouts.remove(&kind).expect("just borrowed");
                self.present_fanout(kind, round);
            }
        }
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
        // A fan-out kind's replies are folded into their round instead of matching a
        // single pending — several are in flight for one user action.
        if LspFanout::is_fanout(kind) && self.absorb_fanout_reply(kind, &token, &reply) {
            return;
        }
        // Whole-buffer decorations route by generation through their own map: one
        // request per capable server can be outstanding at a time, each landing in
        // its own server's cache.
        if kind.per_server_pending() {
            self.on_multi_target_reply(&token, reply);
            return;
        }
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
        // The async verb's promise callback (`0` = fire-and-forget). Settled on a
        // successful apply (with the result value) or on a staleness drop (`nil`) so
        // it never hangs. The generation-mismatch / missing-pending drops above
        // don't settle it — a superseded request was already settled in
        // `register_lsp_request`, and a second reply for a handled kind has no live
        // promise. `code_action` stays fire-and-forget until Phase 2 (`cb_id == 0`).
        let cb_id = pending.cb_id;
        // The code-action request's `only`/`apply` options, needed to filter this reply
        // and to decide chooser-vs-one-shot. Default (and unused) for every other kind.
        let code_action = pending.code_action.clone();
        // The encoding the ANSWERING server's positions are in — an apply reply
        // (formatting / rename / resolved code action) carries ranges that must be
        // converted with it, and on a multi-server buffer that is not the first
        // server's. `format{ name = … }` makes this reachable by design.
        let reply_encoding = self.reply_encoding(pending.server.clone().as_ref());
        self.lsp_requests.remove(&kind);

        match reply {
            // Completion is per-server now (a round asks every capable server and
            // each share streams into the open menu), so it routes by generation
            // through `on_multi_target_reply` above, never here.
            LspReply::Completion { .. } => {
                unreachable!("completion replies are routed in on_multi_target_reply")
            }
            LspReply::Locations(locations) => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                let result = self.apply_lsp_locations(kind, locations);
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb_id, result);
            }
            LspReply::Symbols(symbols) => {
                // A symbol list is browsed, not anchored to the cursor — drop it
                // only on a buffer switch (the request's buffer is gone), not on a
                // mere cursor move within it.
                if buffer_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                let result = self.apply_lsp_symbols(kind, symbols);
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb_id, result);
            }
            LspReply::Hover(lines) => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                let result = self.show_hover(lines);
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb_id, result);
            }
            LspReply::SignatureHelp {
                signature,
                active_parameter,
            } => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                let result = self.show_signature_help(signature, active_parameter);
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb_id, result);
            }
            LspReply::Edits(edits) => {
                if buffer_changed || tick_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                self.apply_formatting_edits(edits, reply_encoding);
                self.lsp_dirty = true;
                // A mutation verb resolves `nil` — the effect is the buffer change.
                self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            }
            LspReply::WorkspaceEdit(changes) => {
                if buffer_changed || tick_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                self.apply_workspace_edit(changes, reply_encoding);
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            }
            LspReply::CodeActions(actions) => {
                if buffer_changed || tick_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                // The reply only opens the chooser; `show_code_actions` takes over the
                // promise (stashes `cb_id` onto the menu, or settles `nil` on an empty
                // reply), so it is NOT settled here. It resolves on the confirm/cancel
                // path (Phase 2) — or right away when the request's `apply` option and a
                // single surviving action make it a one-shot.
                self.show_code_actions(actions, cb_id, code_action);
                self.lsp_dirty = true;
            }
            LspReply::ResolvedCodeAction(edit) => {
                if buffer_changed || tick_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                match edit {
                    Some(changes) => self.apply_workspace_edit(changes, reply_encoding),
                    None => self
                        .editor
                        .echo(LspReqKind::ResolveCodeAction.empty_message()),
                }
                self.lsp_dirty = true;
                // The lazy resolve landed and applied — settle the code-action promise
                // (`cb_id` rode the `ResolveCodeAction` request from `apply_code_action`).
                self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            }
            LspReply::ResolvedCompletion {
                documentation,
                detail,
            } => {
                // The docs sidebar's lazy-docs fetch (Phase 4-D): fill the resolved
                // docs into the selected item's cache and repaint. Not cursor/buffer
                // gated — the completion menu follows the moving cursor while open
                // (like the `Completion` reply), and the resolve is keyed to its row;
                // a replaced list is dropped via the reset `lsp_complete_resolve_key`.
                self.on_completion_resolve_reply(documentation, detail);
            }
            // The whole-buffer decorations (semantic tokens / inlay hints / folding
            // ranges) never reach here — they are routed by generation through
            // `on_multi_target_reply` above, because a buffer has one request per
            // capable server outstanding rather than one per kind.
            LspReply::SemanticTokens(_) | LspReply::InlayHints(_) | LspReply::Folds(_) => {
                unreachable!("whole-buffer replies are routed in on_multi_target_reply")
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

    /// Render a hover reply in the cursor-anchored **doc float** — a real,
    /// non-focusable float window over a scratch buffer, so the (potentially long)
    /// markup scrolls with the mouse wheel and keyboard. The reply is markdown, so it
    /// renders through [`Editor::open_markdown_float`]: the markup is *rendered*
    /// (stripped + styled) rather than shown verbatim. An empty reply shows a brief
    /// message instead of an empty float.
    ///
    /// Returns the shown markup as a JSON string an async `hover` promise resolves
    /// with; `Null` when the reply was empty.
    pub(crate) fn show_hover(&mut self, lines: Vec<String>) -> serde_json::Value {
        if lines.is_empty() {
            self.editor.echo(LspReqKind::Hover.empty_message());
            return serde_json::Value::Null;
        }
        let text = lines.join("\n");
        self.editor.open_markdown_float("[Hover]", &text);
        serde_json::Value::String(text)
    }

    /// Render a signature-help reply in the cursor-anchored **doc float** (the same
    /// scrollable float window as the hover, [`Editor::open_doc_float`]): the active
    /// signature's label, with its active parameter appended in brackets when known
    /// (the float renders plain lines, so the parameter can't be styled inline yet).
    /// Triggered manually in insert mode, so it stays out of the way until asked for.
    /// Empty ⇒ a brief message.
    ///
    /// Returns the shown signature line as a JSON string an async `signature_help`
    /// promise resolves with; `Null` when the reply was empty.
    pub(crate) fn show_signature_help(
        &mut self,
        signature: Option<String>,
        active_parameter: Option<String>,
    ) -> serde_json::Value {
        let Some(signature) = signature else {
            // An auto-trigger session reaching an empty reply means you left the call
            // (typed past the `)`, or the cursor moved out): close the sticky float
            // silently. Only the manual `<C-k>` path echoes "no signature".
            if self.editor.signature_session_active() {
                self.editor.end_signature_session();
            } else {
                self.editor.echo(LspReqKind::SignatureHelp.empty_message());
            }
            return serde_json::Value::Null;
        };
        let line = match active_parameter {
            Some(param) if !param.is_empty() => format!("{signature}    [{param}]"),
            _ => signature,
        };
        // Signature help renders a code signature in the source language, so type the
        // popup as the buffer it was invoked from (the staleness gate above guarantees
        // the current buffer is still that one). `""` when that buffer has no filetype.
        let filetype = self
            .editor
            .buffer_filetype(self.editor.current_buffer_id())
            .unwrap_or_default();
        self.editor
            .open_doc_float("[Signature]", vec![line.clone()], &filetype);
        serde_json::Value::String(line)
    }

    /// Act on a reply's target locations: a single goto result jumps the cursor;
    /// references (or multiple goto results) open a select-enabled panel location
    /// list; an empty result shows a brief message. The encoding is captured from
    /// the *source* buffer before any jump switches buffers — a server reports
    /// target positions in its own negotiated encoding.
    ///
    /// Returns the resolved locations as the `{ text, path, row, col }` item list
    /// (JSON) an async `nx.lsp.*` verb resolves its promise with — a 1-element list
    /// for a single goto jump, the full list for a picker; `Null` when empty.
    pub(crate) fn apply_lsp_locations(
        &mut self,
        kind: LspReqKind,
        locations: Vec<Location>,
    ) -> serde_json::Value {
        if locations.is_empty() {
            self.editor.echo(kind.empty_message());
            return serde_json::Value::Null;
        }
        let encoding = self
            .current_lsp_target()
            .map_or(PositionEncoding::Utf8, |(_, _, e)| e);
        // Build the `path:line:col` items once — they feed both the picker and the
        // promise's resolved value.
        let mut items: Vec<(String, String, u32, u32)> = Vec::with_capacity(locations.len());
        for loc in &locations {
            let Some(path) = uri_to_path(&loc.uri) else {
                continue;
            };
            let row = loc.range.start.line as usize;
            let character = loc.range.start.character as usize;
            let byte = self.location_byte_col(&path, row, character, encoding);
            let nav = path.to_string_lossy().into_owned();
            let shown = super::display_path(&path);
            let text = format!("{shown}:{}:{}", row + 1, byte + 1);
            items.push((text, nav, (row + 1) as u32, (byte + 1) as u32));
        }
        if !kind.is_list() && locations.len() == 1 {
            self.jump_to_lsp_location(&locations[0], encoding);
        } else {
            self.present_lsp_picker(kind, items.clone(), "location");
        }
        location_items_to_json(&items)
    }

    /// Open `nx.picker` over already-built picker `items` (`(text, nav-path, 1-based
    /// row, 1-based col)`), or echo `kind`'s empty message when none survived. The
    /// shared tail of [`apply_lsp_symbols`](Self::apply_lsp_symbols) /
    /// [`open_locations_panel`](Self::open_locations_panel); `what` ("symbol" /
    /// "location") only names the surface in the error echo. The picker open is a Lua
    /// effect, so this drains it (`apply_lua_effects`) like `fire_lsp_attach` does.
    fn present_lsp_picker(
        &mut self,
        kind: LspReqKind,
        items: Vec<(String, String, u32, u32)>,
        what: &str,
    ) {
        if items.is_empty() {
            self.editor.echo(kind.empty_message());
            return;
        }
        if let Err(e) = self.lua.show_lsp_locations(&items) {
            self.editor
                .echo(format!("E5108: Error opening LSP {what} picker: {e}"));
        }
        self.apply_lua_effects();
    }

    /// Open `nx.picker` over a document/workspace symbol reply — each row is
    /// `name  [Kind]  path:line`, jumping to the symbol's location on confirm. Like
    /// the location picker this dogfreeds the shared engine; the symbol's `name` and
    /// `kind` make the rows readable (a bare `path:line` would not).
    ///
    /// Returns the symbol `{ text, path, row, col }` item list (JSON) an async
    /// `document_symbol` / `workspace_symbol` promise resolves with; `Null` when
    /// empty.
    pub(crate) fn apply_lsp_symbols(
        &mut self,
        kind: LspReqKind,
        symbols: Vec<SymbolData>,
    ) -> serde_json::Value {
        if symbols.is_empty() {
            self.editor.echo(kind.empty_message());
            return serde_json::Value::Null;
        }
        let encoding = self
            .current_lsp_target()
            .map_or(PositionEncoding::Utf8, |(_, _, e)| e);
        let mut items: Vec<(String, String, u32, u32)> = Vec::with_capacity(symbols.len());
        for sym in &symbols {
            let Some(path) = uri_to_path(&sym.location.uri) else {
                continue;
            };
            let row = sym.location.range.start.line as usize;
            let character = sym.location.range.start.character as usize;
            let byte = self.location_byte_col(&path, row, character, encoding);
            // The row text shows a cwd-relative path; the navigation field keeps
            // the full path (reused cwd-aware on jump).
            let nav = path.to_string_lossy().into_owned();
            let shown = super::display_path(&path);
            let text = format!("{}  [{}]  {shown}:{}", sym.name, sym.kind, row + 1);
            items.push((text, nav, (row + 1) as u32, (byte + 1) as u32));
        }
        let json = location_items_to_json(&items);
        self.present_lsp_picker(kind, items, "symbol");
        json
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
        if self.editor.current_buffer_is(&path) {
            let landed = self.editor.cursor.line;
            let text = self.editor.buffer().line(landed);
            let byte = byte_col(encoding, &text, character);
            self.editor.jump_to(&path, landed, byte);
        }
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

/// Marshal a picker item list (`(text, path, 1-based row, 1-based col)`) into the
/// JSON array an async navigation/symbol verb resolves its promise with: one
/// `{ text, path, row, col }` object per item. The shape matches the `nx.picker`
/// location items so a `nx.lsp.references():next(function(items) … end)` handler
/// sees the same fields the picker rows carry.
fn location_items_to_json(items: &[(String, String, u32, u32)]) -> serde_json::Value {
    serde_json::Value::Array(
        items
            .iter()
            .map(|(text, path, row, col)| {
                serde_json::json!({ "text": text, "path": path, "row": row, "col": col })
            })
            .collect(),
    )
}
