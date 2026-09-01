//! Language-feature requests and their replies: issuing goto/hover/references/
//! signatureHelp/format/rename/codeAction with the stale-drop generation gate,
//! matching replies back, and presenting locations (jump or panel list).

use std::path::Path;

use bemtvi_core::markdown::DocFormat;
use bemtvi_core::DocsSection;
use bemtvi_lsp::lsp_types::{Location, Range, Url};
use bemtvi_lsp::serde_json;
use bemtvi_lsp::{
    LspNotify, LspReply, LspRequest, PositionEncoding, ReqToken, ServerKey, SymbolData,
};
use bemtvi_lua::{CallbackArgs, LspPickerItem};

use super::*;
use crate::EditHost;

impl EditHost {
    /// Issue a language-feature request of `kind` at the cursor as a **fan-out round**:
    /// every capable server is asked and their answers merge on arrival, with the
    /// round's generation dropping a stale reply (Decision 3) — a newer round of the
    /// same kind, or the cursor moving before the replies land, invalidates it.
    /// No-op (with a brief message) if no attached server that finished `initialize`
    /// advertises the feature.
    ///
    /// Every cursor-anchored kind rides this path: the lists (references, symbols),
    /// the documents (hover, signature help) and the goto family alike. A round of one
    /// — the ordinary single-server buffer, or `name` naming a client — merges nothing
    /// and behaves exactly as a single-target request did.
    ///
    /// `name` routes the round to one attached client by config name
    /// (`:LspHover pyright`, `btv.lsp.hover{ name = "pyright" }`); `None` asks every
    /// capable server, in [routing order](Self::lsp_capable_servers). See
    /// [`lsp_route`](Self::lsp_route).
    pub(crate) fn request_lsp(&mut self, kind: LspReqKind, cb_id: u64, name: Option<&str>) {
        // Kinds that don't have the uniform `{uri, position}` shape have their own
        // issue functions — because they carry an argument the cursor can't supply
        // (a new name, a query), or they are whole-buffer background refreshes. A
        // raw `btv._lsp_buf(<kind>)` can name one anyway, so reject it by name rather
        // than falling through to a request of a *different* kind, and settle the
        // promise so the caller isn't left waiting on a reply that never went.
        match kind {
            // Code actions do fan out, but with a range, a context and options the
            // generic path has no way to build — hand them to their own issuer.
            LspReqKind::CodeAction => {
                self.request_lsp_code_action(cb_id, CodeActionOpts::default(), None, name);
                return;
            }
            LspReqKind::Formatting => {
                self.request_lsp_format(cb_id, name);
                return;
            }
            LspReqKind::Rename
            | LspReqKind::WorkspaceSymbol
            | LspReqKind::Completion
            | LspReqKind::CompletionResolve
            | LspReqKind::ResolveCodeAction
            | LspReqKind::SemanticTokens
            | LspReqKind::InlayHints
            | LspReqKind::ResolveInlayHint
            | LspReqKind::FoldingRange => {
                self.editor
                    .echo(format!("btv.lsp: {kind:?} is not a cursor request"));
                self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                return;
            }
            _ => {}
        }
        // Flush any pending document edits as a `didChange` *before* the request,
        // so the server computes against the current buffer text. Requests are
        // fired during input — ahead of `redraw`'s own `sync_lsp` — so without
        // this the server would answer a stale document (e.g. completion ranges
        // computed against text the user already changed).
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
        let asked = self.open_lsp_fanout(
            kind,
            cb_id,
            CodeActionOpts::default(),
            name,
            |_, uri, enc| {
                let position = match enc {
                    PositionEncoding::Utf8 => p8,
                    PositionEncoding::Utf16 => p16,
                    PositionEncoding::Utf32 => p32,
                };
                match kind {
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
                    LspReqKind::DocumentSymbol => LspRequest::DocumentSymbol { uri },
                    // The remaining fan-out kinds are routed away above: code actions
                    // to their own issuer (they carry a range and a context), workspace
                    // symbols rejected (they need a query). Falling back to *some*
                    // request here is how a mis-routed kind used to become a silently
                    // wrong one — a code-action ask issuing `documentSymbol`.
                    other => unreachable!("{other:?} does not ride the cursor fan-out"),
                }
            },
        );
        if asked.is_empty() {
            // Nobody could answer (`lsp_route` has echoed why), so the request never
            // went: settle the promise now (resolve `nil`) rather than leave it
            // hanging for a reply that won't come.
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
        }
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
            self.request_lsp(LspReqKind::SignatureHelp, 0, None);
        } else {
            self.editor.end_signature_session();
        }
    }

    /// Whether the current buffer has **any** (initialized) server advertising
    /// signature-help trigger characters — the per-buffer gate for the auto-trigger
    /// drain.
    ///
    /// Selected by **capability**, not by position: core's trigger set is the union
    /// across every started server, so it raises the request whenever any of them
    /// wants the typed character. Resolving the gate to the buffer's *first* server
    /// then dropped it on every buffer whose first server has no signature help —
    /// `eslint` ahead of `ts_ls` — swallowing every `(` the second server would have
    /// answered. Now that the request itself fans out, "any capable server wants this
    /// character" is also exactly the condition under which the round has a recipient.
    fn current_buffer_has_signature_trigger(&self) -> bool {
        self.lsp_capable_servers(self.editor.current_buffer_id(), LspReqKind::SignatureHelp)
            .into_iter()
            .filter_map(|(key, _)| self.lsp_servers.get(&key))
            .any(|rt| !rt.signature_trigger_chars.is_empty())
    }

    /// Bump the request generation and register the in-flight request for `kind`
    /// (buffer / `changedtick` at issue time), returning its [`ReqToken`].
    /// The single home for the staleness bookkeeping every issue function shares.
    /// `cb_id` (`0` = fire-and-forget) settles the issuing verb's promise; a new
    /// request of the same `kind` **supersedes** the one it replaces, so that
    /// pending's still-live promise is settled `nil` (a benign no-op) here rather
    /// than left to hang — its reply, if it ever lands, is dropped on the
    /// generation mismatch. `server` is the one the request is going to, so the
    /// reply is decoded against *that* server's encoding rather than re-derived.
    pub(crate) fn register_lsp_request_to(
        &mut self,
        kind: LspReqKind,
        cb_id: u64,
        server: &ServerKey,
    ) -> ReqToken {
        self.lsp_req_gen += 1;
        let generation = self.lsp_req_gen;
        if let Some(prev) = self.lsp_requests.insert(
            kind,
            PendingLspReq {
                generation,
                buffer: self.editor.current_buffer_id(),
                tick: self.editor.buffer().changedtick,
                cb_id,
                server: Some(server.clone()),
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

    /// The `btv.lsp.config{ priority = … }` routing rank of the config `name`, or `0`
    /// when it set none. The one place the default lives.
    pub(crate) fn lsp_priority_of(&self, name: &str) -> i64 {
        self.lsp_priorities.get(name).copied().unwrap_or(0)
    }

    /// Compare two servers in **routing order**: `priority` descending, then
    /// [`ServerKey`] ascending. The single comparator behind every ordered view of a
    /// buffer's servers — who a single-target verb asks, what order a merged surface
    /// presents, and the order `:LspInfo` lists — so the listing can't disagree with
    /// the routing it is meant to explain.
    pub(crate) fn lsp_routing_order(&self, a: &ServerKey, b: &ServerKey) -> std::cmp::Ordering {
        self.lsp_priority_of(&b.name)
            .cmp(&self.lsp_priority_of(&a.name))
            .then_with(|| a.cmp(b))
    }

    /// Every attached server on `buffer` that has finished `initialize` and
    /// advertises the provider answering `kind`, **in routing order**, with the
    /// encoding each negotiated.
    ///
    /// The plural of [`lsp_target_for`](Self::lsp_target_for), and the one place the
    /// order is decided: the fan-out rounds (references, symbols, code actions, hover)
    /// present in it, the whole-buffer decorations (semantic tokens, inlay hints)
    /// concatenate in it, and every single-target verb takes its **first** element.
    ///
    /// Routing order is `priority` descending, then [`ServerKey`] ascending. The key
    /// order alone — config name, then root — is deterministic but arbitrary as a
    /// *preference*: it makes `pyright` beat `ruff` for hover because of how the two
    /// are spelled. `priority` is how a config states the preference outright, and the
    /// key stays as the tiebreak so servers that don't set one keep the old stable
    /// order.
    pub(crate) fn lsp_capable_servers(
        &self,
        buffer: BufferId,
        kind: LspReqKind,
    ) -> Vec<(ServerKey, PositionEncoding)> {
        let Some(state) = self.lsp_states.get(&buffer) else {
            return Vec::new();
        };
        let mut capable: Vec<(ServerKey, PositionEncoding)> = state
            .servers()
            .filter_map(|(key, _)| {
                let rt = self.lsp_servers.get(key)?;
                match kind.provider(&rt.providers) {
                    Some(true) | None => Some((key.clone(), rt.encoding)),
                    Some(false) => None,
                }
            })
            .collect();
        capable.sort_by(|(a, _), (b, _)| self.lsp_routing_order(a, b));
        capable
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
            } => self.on_completion_reply(buffer, tick, key, is_incomplete, items),
            _ => {}
        }
    }

    /// Settle an async `btv.lsp.*` verb's promise: run its `btv._cb_fns[cb_id]`
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
                .echo(format!("E5108: Error settling btv.lsp promise: {e}"));
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
                "bemtvi: client:notify: LSP client {client_id} is not running"
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
    pub(crate) fn request_lsp_format(&mut self, cb_id: u64, name: Option<&str>) {
        let Some((key, uri, _encoding)) = self.lsp_target_for_or_echo(LspReqKind::Formatting, name)
        else {
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

    /// `btv.lsp.workspace_symbol(query)` — request `workspace/symbol` for `query`.
    /// Unlike the cursor-anchored requests it carries the user's fuzzy query, not a
    /// position; on reply the matching symbols open in the picker (`apply_lsp_symbols`).
    ///
    /// Fans out to every server advertising `workspaceSymbolProvider` and merges,
    /// like `documentSymbol`: two servers indexing one project each know symbols the
    /// other does not (a type-checker's definitions, a linter's rule ids), so asking
    /// only the first silently halves the picker. Merging is as well defined here as
    /// for document symbols — the result is a *list*, and duplicates collapse on
    /// their resolved position.
    ///
    /// `name` narrows the round to one client, for the same reason the merge exists:
    /// when two servers index the project, "search only ts_ls's symbols" is a real
    /// ask, and the merged picker can't express it.
    pub(crate) fn request_lsp_workspace_symbol(
        &mut self,
        query: &str,
        cb_id: u64,
        name: Option<&str>,
    ) {
        self.sync_lsp();
        let query = query.to_string();
        let asked = self.open_lsp_fanout(
            LspReqKind::WorkspaceSymbol,
            cb_id,
            CodeActionOpts::default(),
            name,
            |_, _uri, _enc| LspRequest::WorkspaceSymbol {
                query: query.clone(),
            },
        );
        if asked.is_empty() {
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
        }
    }

    /// `:LspRename {newname} [server]` — request `textDocument/rename` at the cursor
    /// with the new name. On reply the returned `WorkspaceEdit` is applied across the
    /// open buffers it touches. `name` routes the request to one attached client.
    pub(crate) fn request_lsp_rename(&mut self, new_name: &str, cb_id: u64, name: Option<&str>) {
        let new_name = new_name.trim();
        if new_name.is_empty() {
            self.editor
                .echo("E471: Argument required: :LspRename {newname}");
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
            return;
        }
        let Some((key, uri, encoding)) = self.lsp_target_for_or_echo(LspReqKind::Rename, name)
        else {
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
    /// (`btv.lsp.code_action{ context = { only = … }, apply = true }`): `only` rides the
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
    ///
    /// `name` routes the round to one attached client (`:LspCodeAction eslint`,
    /// `btv.lsp.code_action{ name = "eslint" }`) instead of merging every capable
    /// server's actions — the way to run *that* linter's fixes when two servers both
    /// offer some.
    pub(crate) fn request_lsp_code_action(
        &mut self,
        cb_id: u64,
        opts: CodeActionOpts,
        range: Option<(usize, usize, usize, usize)>,
        name: Option<&str>,
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
        // `context.diagnostics` per server: each is sent the diagnostics **it**
        // published over this range, in its own encoding.
        //
        // This is what a linter gates its quick-fixes on — ruff offers "remove the
        // unused import" only when the request carries ruff's own diagnostic — so
        // handing every server one list harvested from the buffer's first server
        // means the second one is asked about diagnostics it never published, and
        // its fixes are silently never offered. Which is precisely the hole the
        // code-action fan-out exists to close. (A server's diagnostics also carry
        // its own `data`/`code` and its own columns, so they are not another
        // server's to forward even when the two agree on the problem.)
        let buffer = self.editor.current_buffer_id();
        let diagnostics: HashMap<ServerKey, Vec<bemtvi_lsp::lsp_types::Diagnostic>> = self
            .lsp_capable_servers(buffer, LspReqKind::CodeAction)
            .into_iter()
            .map(|(key, _)| {
                let diags = self.diagnostics_in_range_from(&key, extent);
                (key, diags)
            })
            .collect();
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
        let asked = self.open_lsp_fanout(
            LspReqKind::CodeAction,
            cb_id,
            opts,
            name,
            |key, uri, enc| {
                let range = ranges
                    .iter()
                    .find(|(e, _)| *e == enc)
                    .map(|(_, r)| *r)
                    .unwrap_or_default();
                LspRequest::CodeAction {
                    uri,
                    range,
                    diagnostics: diagnostics.get(key).cloned().unwrap_or_default(),
                    only: only.clone(),
                }
            },
        );
        if asked.is_empty() {
            self.settle_lsp_promise(cb_id, serde_json::Value::Null);
        }
    }

    /// The server on `buffer` that should answer a request of `kind`: the **first** in
    /// [routing order](Self::lsp_capable_servers) — highest `priority`, then
    /// [`ServerKey`] — that has finished `initialize` and advertises the matching
    /// provider.
    ///
    /// This is what makes a two-server buffer behave. With `pyright` + `ruff`, a
    /// hover must go to pyright because ruff advertises none — picking by position
    /// in the map (as every path did before) would send it to whichever name sorts
    /// first and get nothing back. Capability decides *who can*; `priority` decides
    /// *who first* when several can.
    ///
    /// Kinds with no modelled provider flag ([`LspReqKind::provider`] → `None`) fall
    /// back to the first initialized server: failing open beats answering nothing.
    pub(crate) fn lsp_target_for(
        &self,
        buffer: BufferId,
        kind: LspReqKind,
    ) -> Option<(ServerKey, Url, PositionEncoding)> {
        let uri = self.lsp_states.get(&buffer)?.uri.clone()?;
        let (key, encoding) = self.lsp_capable_servers(buffer, kind).into_iter().next()?;
        Some((key, uri, encoding))
    }

    /// The servers on `buffer` that should answer a request of `kind`, honoring an
    /// explicit **route by name** — `:LspHover pyright`, `btv.lsp.hover{ name = … }`.
    ///
    /// `None` is the default routing: every attached server advertising the provider,
    /// in [`ServerKey`] order (a single-target caller takes the first, a fan-out takes
    /// them all). `Some(want)` narrows that to the client configured under `want`,
    /// which is what makes a multi-server buffer *addressable*: with `ts_ls` + `eslint`
    /// both advertising code actions, or `pyright` + `ruff` both advertising hover,
    /// "ask this one" is otherwise unsayable — the default pick is by key order, and
    /// the one you want is not always first.
    ///
    /// Every way the route can come up empty is echoed **here**, the one place that
    /// knows *why*, so callers only settle their promise. A named client that isn't
    /// attached never falls back to a different server: silently answering from
    /// pyright when ruff was named is exactly the failure naming one prevents.
    pub(crate) fn lsp_route(
        &mut self,
        buffer: BufferId,
        kind: LspReqKind,
        name: Option<&str>,
    ) -> Vec<(ServerKey, PositionEncoding)> {
        let capable = self.lsp_capable_servers(buffer, kind);
        let Some(want) = name else {
            if capable.is_empty() {
                self.editor.echo("No language server attached");
            }
            return capable;
        };
        let routed: Vec<_> = capable
            .into_iter()
            .filter(|(key, _)| key.name == want)
            .collect();
        if !routed.is_empty() {
            return routed;
        }
        // Empty for one of three distinct reasons, and they call for different fixes:
        // a typo'd/unstarted name, a server still initializing (retry in a moment),
        // or a server that simply doesn't do this (use another one).
        let attached = self
            .lsp_states
            .get(&buffer)
            .is_some_and(|s| s.servers().any(|(key, _)| key.name == want));
        let initialized = attached && self.lsp_servers.keys().any(|key| key.name == want);
        let msg = match (attached, initialized) {
            (false, _) => format!("No LSP client named '{want}' on this buffer"),
            (true, false) => format!("LSP client '{want}' has not finished initializing"),
            (true, true) => format!("LSP client '{want}' does not provide {}", kind.label()),
        };
        self.editor.echo(msg);
        Vec::new()
    }

    /// The single server a cursor request of `kind` goes to on the **current** buffer:
    /// the first of [`lsp_route`](Self::lsp_route), with the buffer's document `uri`.
    /// `name` routes it to one client by config name (`None` = the capability-ordered
    /// default). Syncs pending edits first, and every empty case is already echoed by
    /// `lsp_route`, so the caller only settles its promise.
    pub(crate) fn lsp_target_for_or_echo(
        &mut self,
        kind: LspReqKind,
        name: Option<&str>,
    ) -> Option<(ServerKey, Url, PositionEncoding)> {
        self.sync_lsp();
        let buffer = self.editor.current_buffer_id();
        let (key, encoding) = self.lsp_route(buffer, kind, name).into_iter().next()?;
        // A server attached to a buffer whose document uri never resolved can't be
        // asked about a document — say so rather than dropping the request silently.
        let Some(uri) = self.lsp_states.get(&buffer).and_then(|s| s.uri.clone()) else {
            self.editor
                .echo("btv.lsp: the buffer has no file path to ask about");
            return None;
        };
        Some((key, uri, encoding))
    }

    /// Open a fan-out round for `kind`: issue `make` to **every** attached server
    /// that advertises the capability, and register the round so their replies merge.
    /// `name` routes the round to a single client by config name — a fan-out of one,
    /// so "just eslint's code actions" merges nothing and lists only its own.
    ///
    /// Returns the servers asked (empty when none can answer — [`lsp_route`] has
    /// already echoed why, and the caller settles its promise: no silent nothing).
    /// Any round already open for this kind is superseded: its promise settles `nil`
    /// rather than hanging, and its still-outstanding replies are dropped on arrival
    /// (their generations are gone from the new round).
    pub(crate) fn open_lsp_fanout(
        &mut self,
        kind: LspReqKind,
        cb_id: u64,
        code_action: CodeActionOpts,
        name: Option<&str>,
        make: impl Fn(&ServerKey, Url, PositionEncoding) -> LspRequest,
    ) -> Vec<ServerKey> {
        let buffer = self.editor.current_buffer_id();
        let targets = self.lsp_route(buffer, kind, name);
        if targets.is_empty() {
            return Vec::new();
        }
        let Some(uri) = self.lsp_states.get(&buffer).and_then(|s| s.uri.clone()) else {
            self.editor
                .echo("btv.lsp: the buffer has no file path to ask about");
            return Vec::new();
        };
        // Supersede any open round for this kind before issuing the new one. The
        // round must be in place BEFORE the superseded promise is settled: settling
        // runs the Lua continuation synchronously, and a continuation that chains
        // another request of this kind would otherwise insert its own round here and
        // have it overwritten by this one, orphaning its outstanding generations (its
        // replies would find no matching entry, and its promise would hang until the
        // next supersede).
        let prev = self.lsp_fanouts.remove(&kind);
        let mut round = LspFanout {
            outstanding: HashMap::new(),
            cb_id,
            buffer,
            cursor: (self.editor.cursor.line, self.editor.cursor.col),
            tick: self.editor.buffer().changedtick,
            code_action,
            locations: Vec::new(),
            symbols: Vec::new(),
            hovers: Vec::new(),
            signatures: Vec::new(),
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
        if let Some(prev) = prev {
            if prev.cb_id != 0 {
                self.settle_lsp_promise(prev.cb_id, serde_json::Value::Null);
            }
        }
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
            // Not part of the open round — a straggler from a superseded one. It is
            // still this kind's reply, so swallow it rather than letting the
            // single-target path try to match it.
            return true;
        };
        // Every position in this reply is authored in the encoding the ANSWERING
        // server negotiated, so it is captured here — at the one point the server is
        // still known — rather than re-derived when the merged list is presented.
        let encoding = self.reply_encoding(Some(&server));
        let Some(round) = self.lsp_fanouts.get_mut(&kind) else {
            return true; // `reply_encoding` released the borrow; re-take it
        };
        match reply {
            LspReply::Locations(locations) => round
                .locations
                .extend(locations.iter().map(|l| (l.clone(), encoding))),
            LspReply::Symbols(symbols) => round
                .symbols
                .extend(symbols.iter().map(|s| (s.clone(), encoding))),
            LspReply::CodeActions(actions) => round
                .actions
                .extend(actions.iter().map(|a| (server.clone(), a.clone()))),
            // Hover payloads are markdown, so an empty one is a server saying "I know
            // nothing here" — dropped now rather than at present time, so the "was
            // anything found at all" test and the "does this need a heading" count
            // are the same number.
            LspReply::Hover { lines, format } if !lines.is_empty() => {
                round.hovers.push((server.clone(), lines.clone(), *format))
            }
            // As with hover, a server with nothing to say is dropped on arrival rather
            // than carried as an empty slot to the presentation.
            LspReply::SignatureHelp(Some(info)) => {
                round.signatures.push((server.clone(), info.clone()))
            }
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
            // The goto family merges with references: the answers are all locations,
            // and `apply_lsp_locations` still jumps when the MERGED list holds exactly
            // one place — so `gd` on a one-server buffer, or on two servers that agree,
            // jumps exactly as before and only opens the picker when the servers
            // genuinely disagree about where the definition is.
            LspReqKind::References
            | LspReqKind::Definition
            | LspReqKind::Declaration
            | LspReqKind::TypeDefinition
            | LspReqKind::Implementation => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                // Two servers reporting the same location show it once —
                // `apply_lsp_locations` drops the duplicate *after* converting each
                // to a byte column, which is the only place the two spellings of one
                // position (utf-8 vs utf-16 `character`) are comparable.
                let result = self.apply_lsp_locations(kind, round.locations);
                self.lsp_dirty = true;
                self.settle_lsp_promise(round.cb_id, result);
            }
            // A symbol list is browsed rather than anchored to the cursor, so only a
            // buffer switch retires it. `workspace/symbol` merges the same way: its
            // results are the union of what each indexer knows.
            LspReqKind::DocumentSymbol | LspReqKind::WorkspaceSymbol => {
                if buffer_changed {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                let result = self.apply_lsp_symbols(kind, round.symbols);
                self.lsp_dirty = true;
                self.settle_lsp_promise(round.cb_id, result);
            }
            // One float, every server that had something to say, in routing order.
            // The round already arrives in that order (`open_lsp_fanout` issues over
            // `lsp_capable_servers`), but replies land in whatever order the servers
            // answer, so the accumulated list is re-sorted here.
            LspReqKind::Hover => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                let mut hovers = round.hovers;
                hovers.sort_by(|(a, ..), (b, ..)| self.lsp_routing_order(a, b));
                let result = self.show_merged_hover(hovers);
                self.lsp_dirty = true;
                self.settle_lsp_promise(round.cb_id, result);
            }
            // Signature help follows the cursor while you type the call, so it retires
            // on the same cursor/buffer gate the single-target path used.
            LspReqKind::SignatureHelp => {
                if buffer_changed || cursor_moved {
                    self.settle_lsp_promise(round.cb_id, serde_json::Value::Null);
                    return;
                }
                let mut signatures = round.signatures;
                signatures.sort_by(|(a, ..), (b, ..)| self.lsp_routing_order(a, b));
                let result = self.show_merged_signature_help(signatures);
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
        // No cursor gate here: every kind that is *anchored* to the cursor — the goto
        // family, hover, signature help, references — merges across servers now, so it
        // retires on `PendingLspReq.cursor`'s twin inside `present_fanout`. What is
        // left on this path acts on the document (edits, a workspace edit) or browses a
        // list (symbols), and neither cares where the cursor drifted to.
        //
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
            // Every kind that answers with locations — the goto family and references
            // alike — merges across servers, so those replies are folded into their
            // round by `absorb_fanout_reply` and never reach this single-slot path.
            LspReply::Locations(_) => {
                unreachable!("location replies are routed in absorb_fanout_reply")
            }
            LspReply::Symbols(symbols) => {
                // A symbol list is browsed, not anchored to the cursor — drop it
                // only on a buffer switch (the request's buffer is gone), not on a
                // mere cursor move within it.
                if buffer_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                let found = symbols.into_iter().map(|s| (s, reply_encoding)).collect();
                let result = self.apply_lsp_symbols(kind, found);
                self.lsp_dirty = true;
                self.settle_lsp_promise(cb_id, result);
            }
            // Hover merges across servers, so its replies are folded into their round
            // by `absorb_fanout_reply` and never reach the single-slot kind path (see
            // the `CodeActions` arm below, and `LspFanout::is_fanout`).
            LspReply::Hover { .. } => {
                unreachable!("hover replies are routed in absorb_fanout_reply")
            }
            LspReply::SignatureHelp { .. } => {
                unreachable!("signature-help replies are routed in absorb_fanout_reply")
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
            // Code actions always fan out (every `codeActionProvider` is asked and
            // the chooser merges them), so their replies fold into a round in
            // `absorb_fanout_reply` and never reach the single-slot kind path — a
            // reply that finds no open round is a straggler the round already
            // swallowed.
            LspReply::CodeActions(_) => {
                unreachable!("code-action replies are routed in absorb_fanout_reply")
            }
            LspReply::ResolvedCodeAction(edit) => {
                if buffer_changed || tick_changed {
                    self.settle_lsp_promise(cb_id, serde_json::Value::Null);
                    return;
                }
                match edit {
                    // The outcome is only meaningful when a *server* asked us to
                    // apply (`workspace/applyEdit`); here the user did, and
                    // `apply_workspace_edit` has already echoed anything that failed.
                    Some(changes) => {
                        self.apply_workspace_edit(changes, reply_encoding);
                    }
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
                documentation_format,
                detail,
            } => {
                // The docs sidebar's lazy-docs fetch (Phase 4-D): fill the resolved
                // docs into the selected item's cache and repaint. Not cursor/buffer
                // gated — the completion menu follows the moving cursor while open
                // (like the `Completion` reply), and the resolve is keyed to its row;
                // a replaced list is dropped via the reset `lsp_complete_resolve_key`.
                self.on_completion_resolve_reply(documentation, documentation_format, detail);
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
    ///
    /// Every server that answered contributes, in the order the caller sorted them
    /// (routing order — `priority`, then key). With more than one contributor each
    /// section is headed by a **labelled rule** — `─ pyright ────────`, the shape the
    /// float's own border title has — and its markup renders straight under it: the
    /// reader has to know which server said what, since a type-checker's signature and
    /// a linter's rule explanation are different kinds of claim. A rule rather than a
    /// `# <client>` heading because a heading is markup the *server's own* markdown
    /// also uses (so the two ranks compete), and it separates nothing — two adjacent
    /// sections still ran together. A lone contributor renders bare: a title naming the
    /// only server there is would be noise on every hover in a one-server buffer, the
    /// common case. (This is `vim.lsp.buf.hover`'s composition; bemtvi's order is
    /// `priority` rather than neovim's unordered `pairs()` walk over its client table.)
    ///
    /// The returned string — what an async `hover` promise resolves with — is the
    /// *markup*, so a section is announced there as the `# <client>` heading a caller
    /// can parse; the rule is a rendering of it, not text a caller should have to strip.
    fn show_merged_hover(
        &mut self,
        hovers: Vec<(ServerKey, Vec<String>, DocFormat)>,
    ) -> serde_json::Value {
        if hovers.is_empty() {
            self.editor.echo(LspReqKind::Hover.empty_message());
            return serde_json::Value::Null;
        }
        let multi = hovers.len() > 1;
        let sections: Vec<DocsSection> = hovers
            .into_iter()
            .map(|(key, doc, format)| DocsSection {
                label: if multi {
                    key.name.clone()
                } else {
                    String::new()
                },
                // Hover has no `detail`: every section is the server's own markup.
                detail: None,
                body: doc.join("\n"),
                format,
            })
            .collect();
        let text = sections
            .iter()
            .map(|s| {
                if s.label.is_empty() {
                    s.body.clone()
                } else {
                    format!("# {}\n\n{}", s.label, s.body)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");
        self.editor.open_markdown_sections("[Hover]", &sections);
        serde_json::Value::String(text)
    }

    /// Render a signature-help round in the cursor-anchored **doc float** (the same
    /// scrollable float window as the hover, [`Editor::open_signature_float`]): each
    /// answering server's active signature, laid out one parameter per line with the
    /// parameter the cursor is in marked (see [`super::signature`]). Triggered
    /// manually in insert mode, or by the opt-in auto-trigger, so it stays out of the
    /// way until asked for.
    ///
    /// With several servers answering, each signature is headed by a labelled rule
    /// naming its client — `─ pyright ─────`, the hover float's section header — or two
    /// signatures for the same call sit anonymously on top of each other. A lone
    /// contributor renders bare, so the ordinary one-server float is unchanged.
    ///
    /// This is where bemtvi **departs from neovim**: `vim.lsp.buf.signature_help` shows
    /// one client's signature at a time, titled `(1/3)`, and binds `<C-s>` to cycle.
    /// Cycling needs a focusable, key-grabbing float with session state; bemtvi's is a
    /// passive doc float that the next keystroke dismisses, so showing them together
    /// says the same thing without a mode to leave. (Both editors ask every capable
    /// server — only the presentation differs.)
    ///
    /// Returns the shown text as a JSON string an async `signature_help` promise
    /// resolves with; `Null` when no server had a signature.
    fn show_merged_signature_help(
        &mut self,
        signatures: Vec<(ServerKey, SignatureInfo)>,
    ) -> serde_json::Value {
        if signatures.is_empty() {
            // An auto-trigger session reaching an empty round means you left the call
            // (typed past the `)`, or the cursor moved out): close the sticky float
            // silently. Only the manual `<C-k>` path echoes "no signature".
            if self.editor.signature_session_active() {
                self.editor.end_signature_session();
            } else {
                self.editor.echo(LspReqKind::SignatureHelp.empty_message());
            }
            return serde_json::Value::Null;
        }
        let multi = signatures.len() > 1;
        // Each server's signature becomes a block of lines; `marker_rows` collects
        // where each block's active parameter landed *in the concatenation*, since
        // that is the coordinate core paints the marker at.
        let mut lines: Vec<String> = Vec::new();
        let mut marker_rows: Vec<usize> = Vec::new();
        let mut header_rows: Vec<usize> = Vec::new();
        for (key, info) in signatures {
            let layout = super::signature::layout_signature(&info);
            let layout = if multi {
                super::signature::with_server_name(layout, &key.name)
            } else {
                layout
            };
            // One blank row parts a headed block from the one above (never above the
            // first), as the hover float parts its sections.
            if layout.header_row.is_some() && !lines.is_empty() {
                lines.push(String::new());
            }
            if let Some(row) = layout.active_row {
                marker_rows.push(lines.len() + row);
            }
            if let Some(row) = layout.header_row {
                header_rows.push(lines.len() + row);
            }
            lines.extend(layout.lines);
        }
        // Signature help renders a code signature in the source language, so type the
        // popup as the buffer it was invoked from (the staleness gate above guarantees
        // the current buffer is still that one). `""` when that buffer has no filetype.
        let filetype = self
            .editor
            .buffer_filetype(self.editor.current_buffer_id())
            .unwrap_or_default();
        self.editor
            .open_signature_float(lines.clone(), &filetype, &marker_rows, &header_rows);
        serde_json::Value::String(lines.join("\n"))
    }

    /// Act on a reply's target locations: a single goto result jumps the cursor;
    /// references (or multiple goto results) open a select-enabled panel location
    /// list; an empty result shows a brief message.
    ///
    /// Each location carries the position encoding of the server that **reported**
    /// it, rather than one derived from the buffer here. On a buffer with two
    /// servers the reporting server is not necessarily the first one listed — a
    /// goto routes by capability, and a references round merges every capable
    /// server — so a single derived encoding is right only by luck, and reading a
    /// utf-16 server's columns as utf-8 bytes shifts every result past a line's
    /// first multi-byte glyph.
    ///
    /// Locations that resolve to the **same** place are shown once. The comparison
    /// is on the converted `(path, row, byte)`, not on the raw LSP position: two
    /// servers at different encodings spell one position differently, so raw ranges
    /// would compare unequal for the very case the merge creates.
    ///
    /// Returns the resolved locations as the `{ text, path, row, col }` item list
    /// (JSON) an async `btv.lsp.*` verb resolves its promise with — a 1-element list
    /// for a single goto jump, the full list for a picker; `Null` when empty.
    pub(crate) fn apply_lsp_locations(
        &mut self,
        kind: LspReqKind,
        locations: Vec<(Location, PositionEncoding)>,
    ) -> serde_json::Value {
        if locations.is_empty() {
            self.editor.echo(kind.empty_message());
            return serde_json::Value::Null;
        }
        // Build the `path:line:col` items once — they feed both the picker and the
        // promise's resolved value. The first surviving location is kept whole so a
        // lone goto result can be jumped to.
        let mut items: Vec<LspPickerItem> = Vec::with_capacity(locations.len());
        let mut seen: std::collections::HashSet<(PathBuf, usize, usize)> =
            std::collections::HashSet::new();
        let mut first: Option<(Location, PositionEncoding)> = None;
        for (loc, encoding) in &locations {
            let Some(path) = uri_to_path(&loc.uri) else {
                continue;
            };
            let row = loc.range.start.line as usize;
            let character = loc.range.start.character as usize;
            let byte = self.location_byte_col(&path, row, character, *encoding);
            if !seen.insert((path.clone(), row, byte)) {
                continue; // the same place, reported by another server too
            }
            if first.is_none() {
                first = Some((loc.clone(), *encoding));
            }
            let nav = path.to_string_lossy().into_owned();
            let shown = super::display_path(&path);
            // A pure-location row: one column, which elides keeping its tail (the file
            // name and line) — there is no second column to split it into.
            items.push(LspPickerItem {
                text: format!("{shown}:{}:{}", row + 1, byte + 1),
                path: nav,
                row: (row + 1) as u32,
                col: (byte + 1) as u32,
                ..LspPickerItem::default()
            });
        }
        match first {
            // A goto whose result is one place — after dedup, so two servers
            // agreeing on a definition still jumps rather than opening a
            // two-row picker of the same line.
            Some((loc, encoding)) if !kind.is_list() && items.len() == 1 => {
                self.jump_to_lsp_location(&loc, encoding)
            }
            _ => self.present_lsp_picker(kind, items.clone(), "location"),
        }
        location_items_to_json(&items)
    }

    /// Open `btv.picker` over already-built picker `items` (`(text, nav-path, 1-based
    /// row, 1-based col)`), or echo `kind`'s empty message when none survived. The
    /// shared tail of [`apply_lsp_symbols`](Self::apply_lsp_symbols) /
    /// [`open_locations_panel`](Self::open_locations_panel); `what` ("symbol" /
    /// "location") only names the surface in the error echo. The picker open is a Lua
    /// effect, so this drains it (`apply_lua_effects`) like `fire_lsp_attach` does.
    fn present_lsp_picker(&mut self, kind: LspReqKind, items: Vec<LspPickerItem>, what: &str) {
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

    /// Open `btv.picker` over a document/workspace symbol reply — each row is
    /// `name  [Kind]  path:line`, jumping to the symbol's location on confirm. Like
    /// the location picker this dogfoods the shared engine; the symbol's `name` and
    /// `kind` make the rows readable (a bare `path:line` would not).
    ///
    /// Each symbol carries its reporting server's position encoding, and an
    /// identical symbol reported by two servers is listed once — see
    /// [`apply_lsp_locations`](Self::apply_lsp_locations) for why both are keyed off
    /// the *converted* position.
    ///
    /// Returns the symbol `{ text, path, row, col }` item list (JSON) an async
    /// `document_symbol` / `workspace_symbol` promise resolves with; `Null` when
    /// empty.
    pub(crate) fn apply_lsp_symbols(
        &mut self,
        kind: LspReqKind,
        symbols: Vec<(SymbolData, PositionEncoding)>,
    ) -> serde_json::Value {
        if symbols.is_empty() {
            self.editor.echo(kind.empty_message());
            return serde_json::Value::Null;
        }
        let mut items: Vec<LspPickerItem> = Vec::with_capacity(symbols.len());
        let mut seen: std::collections::HashSet<(String, PathBuf, usize, usize)> =
            std::collections::HashSet::new();
        for (sym, encoding) in &symbols {
            let Some(path) = uri_to_path(&sym.location.uri) else {
                continue;
            };
            let row = sym.location.range.start.line as usize;
            let character = sym.location.range.start.character as usize;
            let byte = self.location_byte_col(&path, row, character, *encoding);
            if !seen.insert((sym.name.clone(), path.clone(), row, byte)) {
                continue; // the same symbol, reported by another server too
            }
            // The row text shows a cwd-relative path; the navigation field keeps
            // the full path (reused cwd-aware on jump).
            let nav = path.to_string_lossy().into_owned();
            let shown = super::display_path(&path);
            // A symbol row has real columns, so it declares them instead of padding one
            // string: the KIND is the pinned tag (what the row IS — never elided), the
            // NAME is the head the widget aligns down the list, and the location is the
            // body. The promise's `{ text, … }` JSON is composed from the three by
            // [`location_items_to_json`], so it still reads as the whole row.
            items.push(LspPickerItem {
                text: format!("{shown}:{}", row + 1),
                path: nav,
                row: (row + 1) as u32,
                col: (byte + 1) as u32,
                tag: Some(format!("[{}]", sym.kind)),
                head: Some(format!("{} ", sym.name)),
            });
        }
        let json = location_items_to_json(&items);
        self.present_lsp_picker(kind, items, "symbol");
        json
    }

    /// Jump the cursor to one LSP [`Location`]. Opens/switches to the target on its
    /// line first, then refines the column once the line text is loaded (the
    /// char→byte conversion needs the target line, which may live in a file this jump
    /// just opened). The second `jump_to` finds the buffer already current and on the
    /// same line, so it only moves the cursor — the alternate `#` and the jumplist
    /// entry are recorded exactly once.
    ///
    /// The **first** jump already carries the raw `character` as its column rather
    /// than `0`, which matters when the target file isn't open yet: in a daemon / web
    /// session its read is deferred, so the cursor set here is clamped to a still-empty
    /// buffer and the real landing happens when the bytes arrive, from the target
    /// `land_cursor` recorded. Refining against that clamped cursor would overwrite the
    /// record with a column read off an empty line — which is exactly where a goto into
    /// an unopened file used to land.
    ///
    /// So a deferred open is not refined here at all: the position is stashed
    /// ([`PendingGoto`]) and converted by
    /// [`settle_pending_goto`](Self::settle_pending_goto) at the landing, where the
    /// target line's text finally exists. That is what makes the off-tick jump land
    /// exactly where the local one does — the remote session is tier-1 — including a
    /// **line-0** target, whose clamped cursor agrees with the target line and so used
    /// to take the refinement's column-`0` answer whatever the server asked for.
    pub(crate) fn jump_to_lsp_location(&mut self, loc: &Location, encoding: PositionEncoding) {
        let Some(path) = uri_to_path(&loc.uri) else {
            return;
        };
        let line = loc.range.start.line as usize;
        let character = loc.range.start.character as usize;
        self.editor.jump_to(&path, line, character);
        if !self.editor.current_buffer_is(&path) {
            return;
        }
        // Bytes still crossing the wire: there is no line text to convert against, and
        // jumping again would clobber the landing target. Hand it to the landing.
        let buffer = self.editor.current_buffer_id();
        if self.editor.has_pending_open(buffer) {
            self.pending_goto_cols.insert(
                buffer,
                PendingGoto {
                    encoding,
                    line,
                    character,
                },
            );
            return;
        }
        // Loaded: the exact conversion, against the line we actually landed on.
        if self.editor.cursor.line == line {
            let text = self.editor.buffer().line(line);
            let byte = byte_col(encoding, &text, character);
            self.editor.jump_to(&path, line, byte);
        }
    }

    /// Refine a deferred goto's column once its file's bytes have landed — the off-tick
    /// tail of [`jump_to_lsp_location`](Self::jump_to_lsp_location), called from the
    /// fetch-landing site (`load_replica_bytes`, shared native/wasm).
    ///
    /// The core's own landing (`settle_loaded_cursor`) has already put the cursor on the
    /// recorded `(line, raw character)`; only now is the line's text here to turn that
    /// protocol `character` into the byte column it names. A no-op when nothing is
    /// stashed for `buffer` (the common case on every open), when the landing wasn't the
    /// current buffer (a background fetch keeps its own saved position), or when the
    /// target line turned out not to exist — the same guard the synchronous path uses.
    pub(crate) fn settle_pending_goto(&mut self, buffer: BufferId) {
        let Some(goto) = self.pending_goto_cols.remove(&buffer) else {
            return;
        };
        if self.editor.current_buffer_id() != buffer || self.editor.cursor.line != goto.line {
            return;
        }
        let Some(path) = self.editor.buffer_of(buffer).and_then(|b| b.path.clone()) else {
            return;
        };
        let text = self.editor.buffer().line(goto.line);
        let byte = byte_col(goto.encoding, &text, goto.character);
        self.editor.jump_to(&path, goto.line, byte);
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

    /// The Lua `client_id` assigned to `key`'s server (the reverse of the id handed
    /// out at `Initialized`), or `None` once it has exited.
    pub(crate) fn lsp_client_id_of(&self, key: &ServerKey) -> Option<u64> {
        self.lsp_servers.get(key).map(|r| r.client_id)
    }

    /// The Lua `client_id` of the server that would answer a code action on the
    /// current buffer — the fallback for dispatching a command whose originating
    /// server is no longer known.
    pub(crate) fn current_lsp_client_id(&self) -> Option<u64> {
        let (key, _, _) =
            self.lsp_target_for(self.editor.current_buffer_id(), LspReqKind::CodeAction)?;
        self.lsp_client_id_of(&key)
    }
}

/// Marshal a picker item list into the JSON array an async navigation/symbol verb
/// resolves its promise with: one `{ text, path, row, col }` object per item. The
/// shape matches the `btv.picker` location items so a
/// `btv.lsp.references():next(function(items) … end)` handler sees the same fields the
/// picker rows carry.
///
/// A column-shaped row (a symbol's `[Kind]` tag + name head) is *composed* back into
/// one `text` here — the same string the picker renders — so a caller reading the
/// promise gets the whole row rather than only its trailing column.
fn location_items_to_json(items: &[LspPickerItem]) -> serde_json::Value {
    serde_json::Value::Array(
        items
            .iter()
            .map(|i| {
                let tag = i.tag.as_ref().map(|t| format!("{t} ")).unwrap_or_default();
                let head = i.head.clone().unwrap_or_default();
                let text = format!("{tag}{head}{}", i.text);
                serde_json::json!({ "text": text, "path": i.path, "row": i.row, "col": i.col })
            })
            .collect(),
    )
}
