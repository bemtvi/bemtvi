//! The built-in **`lsp` completion source** for the unified `nx.complete` engine
//! (Phase 4-C). LSP completion is server-native — the async `textDocument/completion`
//! request, the encoding-aware `textEdit` / `additionalTextEdits` accept, and the
//! `isIncomplete` re-request all live here, not in Lua/core (the Lua mutation API is
//! nil and `nxvim-core` is LSP-agnostic). The engine (core) owns the menu, the
//! prefix, the fuzzy matcher, navigation, and the generation token; this module only
//! feeds it candidates and applies the chosen item's edit when accept is delegated
//! back (`MenuItem.source_accept`).
//!
//! This replaces the retired bespoke completion pmenu. The docs-beside-popup the old
//! `pmenu_value` projected is back as a **doc-float window** (Phase 4-D): the selected
//! `lsp` row's `detail` + `documentation`, lazily fetched via `completionItem/resolve`
//! (`complete_lsp_maybe_resolve` / `on_completion_resolve_reply`), built into markdown by
//! `EditHost::lsp_complete_docs_md` and rendered by `Editor::open_completion_docs_float`.

use nxvim_core::{BufferId, Mode};
use nxvim_lsp::CompletionItemData;

use super::*;
use crate::EditHost;

/// Whether two candidates are the **same offer**: same label, same kind, and the same
/// text an accept would insert. Used to drop the duplicate when two servers on one
/// buffer both offer a symbol — the rows would be indistinguishable in the popup and
/// accept identically, so the second is noise rather than a second choice.
///
/// Compared on the *effective* insert text (the `textEdit`'s replacement, else
/// `insertText`, else the label) so an item that merely spells its insertion
/// differently — a different range, a snippet body — is kept: accepting it does
/// something different.
fn same_offer(a: &CompletionItemData, b: &CompletionItemData) -> bool {
    fn inserted(item: &CompletionItemData) -> &str {
        item.text_edit
            .as_ref()
            .map(|e| e.new_text.as_str())
            .or(item.insert_text.as_deref())
            .unwrap_or(&item.label)
    }
    a.label == b.label
        && a.kind_label() == b.kind_label()
        && a.is_snippet == b.is_snippet
        && inserted(a) == inserted(b)
}

/// One completion **round**'s merged candidates: every capable server is asked at
/// once and their items accumulate here as the replies land, kept so a delegated
/// accept can apply the chosen item's edit and so an `isIncomplete = false` list can
/// be re-served on a prefix edit without another round-trip. Indexed by the
/// `MenuItem.key` the engine carries (the position in `items`).
///
/// The round is opened — and this cache reset — when the requests go out, so a reply
/// only ever *appends*. That is what keeps `key` stable while a round is still
/// filling: a lazy `completionItem/resolve` issued against row 3 of the first
/// server's share still addresses row 3 after the second server's share arrives.
pub(crate) struct LspComplete {
    /// The servers' items, verbatim; `items[key]` is the row the engine accepted.
    pub items: Vec<CompletionItemData>,
    /// The server that produced each entry, parallel to [`items`](Self::items).
    ///
    /// Load-bearing twice over: the accept converts that item's `textEdit` ranges at
    /// **its** server's negotiated encoding, and its lazy `completionItem/resolve`
    /// must go back to the same server — the `data` blob it round-trips is only
    /// meaningful there.
    pub origins: Vec<ServerKey>,
    /// The buffer + (row, word-start byte col) the round was requested at. Reused
    /// while the cursor stays in this word; invalidated when it moves on.
    pub buffer: BufferId,
    pub anchor: (usize, usize),
    /// `isIncomplete` **OR-ed** across the servers that have replied: if any narrowed
    /// its list to the old prefix, a prefix edit must re-request rather than re-serve
    /// the cached list.
    pub is_incomplete: bool,
}

impl EditHost {
    /// Dispatch the `lsp` source for an engine completion trigger at generation
    /// `gen` (called from the settle loop when the `lsp` source is configured). Reuse
    /// the cached items when the cursor is still in the same word and the last reply
    /// was complete — re-push them at `gen` so core re-ranks against the new prefix,
    /// no round-trip. Otherwise issue a fresh `textDocument/completion`; its reply
    /// streams in via [`EditHost::on_completion_reply`].
    pub(crate) fn complete_lsp_dispatch(&mut self, gen: u64) {
        if !self.complete_lsp_active || self.editor.mode != Mode::Insert {
            return;
        }
        // A trigger context (`:emoji`) belongs to its trigger-char source, not the
        // language server — don't fire a `textDocument/completion` for a prefix that
        // leads with a plugin trigger char (Phase 4-E).
        if self.editor.completion_prefix_triggered() {
            return;
        }
        let buffer = self.editor.current_buffer_id();
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        let (word_start, _prefix) = completion_word(&line, col);

        // Cache hit: same buffer + same word start, and the cached list is complete
        // (covers every candidate for this word). Re-serve it; core filters by prefix.
        let reuse = self.lsp_complete.as_ref().is_some_and(|c| {
            !c.is_incomplete && c.buffer == buffer && c.anchor == (row, word_start)
        });
        if reuse {
            self.complete_lsp_push(gen);
            return;
        }
        // Cache miss / incomplete / moved word: open a fresh round against every
        // capable server. Each reply pushes into whatever generation is live when it
        // lands.
        self.request_lsp_completion();
    }

    /// Open a completion **round**: ask every attached server that advertises
    /// `completionProvider`, each at its own negotiated encoding, and reset the merged
    /// cache so their replies accumulate into this round rather than the last one's
    /// candidates.
    ///
    /// Every capable server, not the first: on a `pyright` + `ruff` buffer, asking
    /// only the first shows half the candidates — and completion is the kind where
    /// "the whole point of a second server" most often *is* its candidates.
    ///
    /// The replies **stream**: each server's share appends to the open menu the moment
    /// it lands, so a slow server delays only its own candidates instead of holding
    /// the fast one's behind a barrier. Every in-flight request for this buffer is
    /// retired first, so a straggler from the previous round can't land in this one.
    ///
    /// Silent when nothing can answer: this fires on a keystroke, so echoing "no
    /// language server" here would shout once per typed character (the same reason
    /// [`drain_signature_auto_request`](EditHost::drain_signature_auto_request) drops
    /// quietly).
    fn request_lsp_completion(&mut self) {
        self.sync_lsp();
        let buffer = self.editor.current_buffer_id();
        let Some(uri) = self.lsp_states.get(&buffer).and_then(|s| s.uri.clone()) else {
            return;
        };
        let targets = self.lsp_capable_servers(buffer, LspReqKind::Completion);
        if targets.is_empty() {
            return;
        }
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        let (word_start, _prefix) = completion_word(&line, col);
        // Retire the previous round's outstanding requests wholesale — not just the
        // per-server supersede `register_multi_request` does — so a server this round
        // does NOT ask (it stopped advertising completion, or detached) cannot land
        // its stale share in this round's cache.
        self.lsp_multi_requests
            .retain(|_, p| !(p.kind == LspReqKind::Completion && p.buffer == buffer));
        // Reset the cache to this round's (empty) merged list, anchored where the
        // request was actually computed. A round that resets on ISSUE rather than on
        // first reply is what makes every later reply a plain append.
        self.lsp_complete = Some(LspComplete {
            items: Vec::new(),
            origins: Vec::new(),
            buffer,
            anchor: (row, word_start),
            is_incomplete: false,
        });
        // A new round supersedes any in-flight docs resolve: its key indexed the old
        // items, so drop it (the late reply is ignored — `on_completion_resolve_reply`
        // takes a `None` key) and let the new selection re-issue against the new list.
        self.lsp_complete_resolve_key = None;
        for (key, encoding) in targets {
            let position = self.lsp_position(encoding, row, col);
            let token = self.register_multi_request(LspReqKind::Completion, buffer, &key);
            self.fx.lsp_request(
                key,
                token,
                nxvim_lsp::LspRequest::Completion {
                    uri: uri.clone(),
                    position,
                },
            );
        }
    }

    /// Fold one server's share of a completion round into the merged cache and stream
    /// it into the open menu at the live generation.
    ///
    /// Appends rather than replaces — the round was reset when the requests went out —
    /// so the candidates of a server that answers second join the first's instead of
    /// wiping them. Dropped if the user left insert mode, the buffer changed since the
    /// request, or the round was already superseded (no cache).
    ///
    /// A candidate another server already offered **identically** (same label, kind and
    /// inserted text) is skipped: the two rows would be indistinguishable in the popup
    /// and accept to the same text, so the duplicate is noise. Anything that differs —
    /// a different `textEdit`, kind, or insert text — is kept, because accepting it
    /// does something different.
    pub(crate) fn on_completion_reply(
        &mut self,
        buffer: BufferId,
        server: ServerKey,
        is_incomplete: bool,
        items: Vec<CompletionItemData>,
    ) {
        if self.editor.mode != Mode::Insert {
            return;
        }
        if buffer != self.editor.current_buffer_id() {
            return;
        }
        let Some(cache) = self.lsp_complete.as_mut() else {
            return; // the round was superseded before this share landed
        };
        if cache.buffer != buffer {
            return;
        }
        let start = cache.items.len();
        for item in items {
            if cache.items.iter().any(|had| same_offer(had, &item)) {
                continue;
            }
            cache.items.push(item);
            cache.origins.push(server.clone());
        }
        cache.is_incomplete |= is_incomplete;
        // Push into the live menu generation — the engine bumped it on the keystroke
        // that fired the request, and a `Complete` menu is open (core seeded it, or
        // will re-seed on the next key). A `0` generation means no menu is open.
        let gen = self.editor.menu_generation();
        if gen != 0 {
            self.complete_lsp_push_from(gen, start);
        }
        self.lsp_dirty = true;
    }

    /// Build `MenuItem`s from the whole merged cache and append them to the open
    /// completion menu at generation `gen` — the re-serve path, for a fresh generation
    /// whose menu holds none of them yet.
    pub(crate) fn complete_lsp_push(&mut self, gen: u64) {
        self.complete_lsp_push_from(gen, 0);
    }

    /// [`complete_lsp_push`](Self::complete_lsp_push) for the merged cache's tail from
    /// `start` — one server's freshly-arrived share, so a streaming append doesn't
    /// re-push (and duplicate) the shares already in the menu.
    ///
    /// Each row carries the LSP merge priority, `source_accept = true` (accept is
    /// delegated back to apply its `textEdit`), and its index as the `key` so the
    /// accept can find the raw item. The engine's matcher ranks them against the prefix
    /// and merges them with the buffer rows by priority; a stale `gen` is dropped by
    /// [`Editor::menu_push`].
    ///
    /// The priority is stepped down by the producing server's position in key order,
    /// so **equally-good** matches from two servers rank in a stable order instead of
    /// in whichever order the replies happened to arrive (the engine's blended sort is
    /// stable, so streamed order decides ties). One point per server keeps every LSP
    /// row far above the buffer-word tier — the `lsp` bias is 8 against 0.
    fn complete_lsp_push_from(&mut self, gen: u64, start: usize) {
        let Some(cache) = self.lsp_complete.as_ref() else {
            return;
        };
        let base_priority = self.complete_lsp_priority;
        let rank: Vec<ServerKey> = self
            .lsp_states
            .get(&cache.buffer)
            .map(|s| s.servers().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
        let items: Vec<nxvim_core::MenuItem> = cache
            .items
            .iter()
            .enumerate()
            .skip(start)
            .map(|(key, item)| {
                let step = cache
                    .origins
                    .get(key)
                    .and_then(|o| rank.iter().position(|k| k == o))
                    .unwrap_or(0) as i32;
                let priority = base_priority - step;
                // Display the label; insert the label as a no-op fallback (the real
                // edit is applied server-side on accept via `source_accept`).
                let label = item.label.clone();
                nxvim_core::MenuItem {
                    insert: Some(label.clone()),
                    // The LSP `CompletionItemKind` name (`"Function"`, `"Variable"`, …);
                    // `None` when the server sent no kind (code `0`).
                    kind: item.kind_label().map(str::to_string),
                    priority,
                    // The docs sidebar reads an `lsp` row's docs from the server's
                    // item cache (`source_accept`), not an inline `doc` / `resolve`.
                    source_accept: true,
                    ..nxvim_core::MenuItem::new(label, key)
                }
            })
            .collect();
        if !items.is_empty() {
            self.editor.menu_push(items, gen);
        }
    }

    /// Apply a **delegated** LSP completion accept: the engine recorded the chosen
    /// row's `key` (the index into the cached items) on `complete_accept_request`;
    /// replace the completion word — or the item's explicit `textEdit` range — with
    /// its text, apply any `additionalTextEdits` (imports), and leave the cursor after
    /// the inserted text, all as one undo step. Stays in insert mode, as vim does.
    pub(crate) fn complete_lsp_accept(&mut self, key: usize) {
        // A `Replace`-behavior accept (caret mid-word) hands us the word end to extend
        // the replaced range to; taken up front so an early return can't leak it into
        // the next accept. `None` ⇒ an `Insert` accept, or the caret was at the word end.
        let extend_to = self.editor.complete_accept_extend_to.take();
        let Some(item) = self
            .lsp_complete
            .as_ref()
            .and_then(|c| c.items.get(key))
            .cloned()
        else {
            return;
        };
        // At the encoding of the server that OFFERED this item: a `textEdit` range is
        // authored in its own server's coordinates, and a two-server buffer can hold
        // a utf-8 and a utf-16 one at once — reading one as the other shifts every
        // column after the line's first multi-byte glyph.
        let origin = self
            .lsp_complete
            .as_ref()
            .and_then(|c| c.origins.get(key))
            .cloned();
        let encoding = self.reply_encoding(origin.as_ref());
        // Anchor the word fallback at the current word start (the engine kept the
        // cursor in the word; recompute rather than thread core's anchor through).
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        let (word_start, _) = completion_word(&line, col);

        // The primary edit: the item's explicit textEdit, else replace the word
        // (word_start..cursor) with insertText (falling back to the label).
        let (mut primary_range, primary_text) = match &item.text_edit {
            Some(edit) => (
                self.lsp_range_to_bytes(&edit.range, encoding),
                edit.new_text.clone(),
            ),
            None => {
                let start = self.editor.buffer().line_start(row) + word_start;
                let end = self.editor.buffer().line_start(row) + col;
                let text = item
                    .insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone());
                (start..end, text)
            }
        };
        // Extend the primary range rightward over the rest of the word (the whole
        // token is swapped, not just the typed prefix) under a `Replace` accept.
        if let Some(end) = extend_to {
            primary_range.end = primary_range.end.max(end);
        }

        // A snippet item (`insertTextFormat = Snippet`): expand `primary_text`'s
        // `$1`/`${1:default}`/`$0` through the native engine rather than inserting it
        // literally. `additionalTextEdits` (imports) apply first, shifting the primary
        // range; then the snippet expands over the (typed-prefix) primary range.
        if item.is_snippet {
            match nxvim_core::parse_snippet(&primary_text) {
                Ok(parsed) => {
                    let mut prim = primary_range.clone();
                    let mut adds: Vec<(std::ops::Range<usize>, String)> = item
                        .additional_text_edits
                        .iter()
                        .map(|ate| {
                            (
                                self.lsp_range_to_bytes(&ate.range, encoding),
                                ate.new_text.clone(),
                            )
                        })
                        .collect();
                    if !adds.is_empty() {
                        let shift: isize = adds
                            .iter()
                            .filter(|(r, _)| r.start <= prim.start)
                            .map(|(r, t)| t.len() as isize - (r.end - r.start) as isize)
                            .sum();
                        adds.sort_by_key(|(r, _)| std::cmp::Reverse(r.start));
                        self.editor.apply_edits(adds, prim.start);
                        prim.start = (prim.start as isize + shift).max(0) as usize;
                        prim.end = (prim.end as isize + shift).max(0) as usize;
                    }
                    self.editor.expand_snippet(prim.start, prim.end, parsed);
                    self.lsp_complete = None;
                    self.lsp_complete_resolve_key = None;
                    self.lsp_dirty = true;
                    return;
                }
                Err(e) => {
                    // Unsupported / malformed server snippet: report loud and insert the
                    // plain label rather than dumping raw `$1` markers into the buffer.
                    self.editor
                        .echo(format!("E5901: LSP snippet '{}': {e}", item.label));
                    let edits = vec![(primary_range.clone(), item.label.clone())];
                    self.editor
                        .apply_edits(edits, primary_range.start + item.label.len());
                    self.lsp_complete = None;
                    self.lsp_complete_resolve_key = None;
                    self.lsp_dirty = true;
                    return;
                }
            }
        }

        let mut edits = vec![(primary_range.clone(), primary_text.clone())];
        for ate in &item.additional_text_edits {
            edits.push((
                self.lsp_range_to_bytes(&ate.range, encoding),
                ate.new_text.clone(),
            ));
        }
        // The cursor lands after the primary insertion, shifted by the net length of
        // any edits that fall before it (e.g. an inserted `use` import).
        let shift: isize = edits
            .iter()
            .skip(1)
            .filter(|(r, _)| r.start < primary_range.start)
            .map(|(r, t)| t.len() as isize - (r.end - r.start) as isize)
            .sum();
        let cursor_byte = (primary_range.start + primary_text.len()) as isize + shift;
        self.editor.apply_edits(edits, cursor_byte.max(0) as usize);
        self.lsp_complete = None;
        self.lsp_complete_resolve_key = None;
        self.lsp_dirty = true;
    }

    /// The docs sidebar's lazy-docs fetch (Phase 4-D): when the **highlighted** row is
    /// an `lsp` row whose cached item carries no inline `documentation` yet but has
    /// `resolve_data`, issue a `completionItem/resolve` to pull its docs. Called once
    /// per key from [`EditHost::run_pending`] (the selection is final by then). A no-op
    /// for a native `buffer` row, an already-resolved item, a row the server gave no
    /// `data` to resolve against, or while a resolve is already in flight for this row.
    /// The reply ([`EditHost::on_completion_resolve_reply`]) fills the item and repaints.
    pub(crate) fn complete_lsp_maybe_resolve(&mut self) {
        if !self.complete_lsp_active {
            return;
        }
        // Only an actively-selected `lsp` row has docs to resolve (a noselect popup or
        // a `buffer` row yields `None` / `source_accept = false`).
        let Some((key, true)) = self.editor.complete_selected() else {
            return;
        };
        // A resolve for this exact row is already pending — let it land.
        if self.lsp_complete_resolve_key == Some(key) {
            return;
        }
        let Some(item) = self.lsp_complete.as_ref().and_then(|c| c.items.get(key)) else {
            return;
        };
        // Inline docs already present, or a prior resolve filled them (it stamps
        // `Some("")` even when docless) — nothing to fetch.
        if item.documentation.is_some() {
            return;
        }
        let Some(resolve_data) = item.resolve_data.clone() else {
            return; // the server gave no `data` to resolve against — stays docless
        };
        // Back to the server that OFFERED this row, not the buffer's first: the
        // `data` blob being round-tripped is that server's own handle on the item.
        // Resolving ruff's candidate against pyright is a wrong request, not a
        // degraded one — and with a merged popup the two are routinely different.
        let Some(server_key) = self
            .lsp_complete
            .as_ref()
            .and_then(|c| c.origins.get(key))
            .cloned()
        else {
            return;
        };
        let token = self.register_lsp_request_to(LspReqKind::CompletionResolve, 0, &server_key);
        self.lsp_complete_resolve_key = Some(key);
        self.fx.lsp_request(
            server_key,
            token,
            nxvim_lsp::LspRequest::ResolveCompletion { item: resolve_data },
        );
    }

    /// The docs sidebar's lazy-docs fetch for a **plugin** completion row (Phase 4-E,
    /// the analogue of [`EditHost::complete_lsp_maybe_resolve`] for `nx.complete.source`
    /// sources): when the highlighted row carries a `resolve` handle and its docs are
    /// not cached yet, ask Lua to run the source's `resolve` callback
    /// ([`LuaRuntime::run_complete_resolve`]). Called once per key from
    /// [`EditHost::run_pending`]. A no-op for an inline-doc / `buffer` / `lsp` row (no
    /// handle), an already-resolved handle, or one already in flight. The reply lands
    /// via `nx._complete_resolve_done` → the resolve cache, drained in
    /// [`EditHost::apply_lua_effects`].
    pub(crate) fn complete_plugin_maybe_resolve(&mut self) {
        let Some(id) = self.editor.complete_selected_resolve() else {
            return;
        };
        if self.complete_resolve_docs.contains_key(&id)
            || self.complete_resolve_inflight == Some(id)
        {
            return;
        }
        self.complete_resolve_inflight = Some(id);
        if let Err(e) = self.lua.run_complete_resolve(id) {
            self.editor
                .echo(format!("E5108: Error in nx.complete resolve: {e}"));
            self.complete_resolve_inflight = None;
            return;
        }
        // A synchronous `respond` already queued the docs; drain + apply so the
        // sidebar paints this same key. An async source's reply lands on a later tick.
        self.apply_lua_effects();
    }

    /// Apply a delegated **plugin `on_accept`** accept (P4): a `nx.complete.source`
    /// item carried an `on_accept` callback, so its accept was delegated here instead
    /// of core splicing `insert`. Run the callback (via [`LuaRuntime::run_complete_accept`]),
    /// handing it the trigger RANGE it should replace — the word under the cursor,
    /// computed exactly as the snippet accept does (`word_start..cursor`, extended over
    /// the rest of the word under a `Replace` accept). The callback owns the edit (it
    /// typically `nx.buf.set_text`s an expansion, or `nx.snippet.expand`s a body); any
    /// buffer ops it queues drain in the enclosing `run_pending` fixpoint, kicked here
    /// by `apply_lua_effects` so a synchronous callback lands on this same key.
    pub(crate) fn complete_plugin_accept(&mut self, id: usize) {
        // A `Replace`-behavior accept (caret mid-word) hands us the word end; taken up
        // front so an early return can't leak it into the next accept.
        let extend_to = self.editor.complete_accept_extend_to.take();
        let row = self.editor.cursor.line;
        let col = self.editor.cursor.col;
        let line = self.editor.buffer().line(row);
        let word_start = crate::snippet::trigger_word_start(&line, col);
        let line_start = self.editor.buffer().line_start(row);
        let end_byte = extend_to.map_or(line_start + col, |e| (line_start + col).max(e));
        let end_row = self.editor.buffer().byte_to_line(end_byte);
        let end_col = end_byte - self.editor.buffer().line_start(end_row);
        let buf = self.editor.current_buffer_id().0;
        if let Err(e) = self.lua.run_complete_accept(
            id as u64,
            buf,
            row as u64,
            word_start as u64,
            end_row as u64,
            end_col as u64,
        ) {
            self.editor
                .echo(format!("E5108: Error in nx.complete on_accept: {e}"));
            return;
        }
        self.apply_lua_effects();
    }

    /// Apply a `completionItem/resolve` reply (Phase 4-D): fill the resolved
    /// `documentation` / `detail` into the cached item the docs sidebar reads, keyed by
    /// the row the resolve was issued for ([`EditHost::lsp_complete_resolve_key`]).
    /// `documentation` is stamped `Some` even when the server returned nothing (an
    /// empty string ⇒ resolved-but-docless), so the row is never re-requested. A reply
    /// whose list was replaced meanwhile finds a `None` key and is ignored. `lsp_dirty`
    /// repaints the sidebar with the freshly resolved docs.
    pub(crate) fn on_completion_resolve_reply(
        &mut self,
        documentation: Option<String>,
        detail: Option<String>,
    ) {
        let Some(key) = self.lsp_complete_resolve_key.take() else {
            return;
        };
        if let Some(item) = self
            .lsp_complete
            .as_mut()
            .and_then(|c| c.items.get_mut(key))
        {
            item.documentation = Some(documentation.unwrap_or_default());
            if detail.is_some() {
                item.detail = detail;
            }
        }
        self.lsp_dirty = true;
    }
}

impl EditHost {
    /// Build the **markdown** the completion docs float renders for an `lsp` row
    /// (Phase 4-D, now the doc-float-window model): the item's `detail` — a one-line
    /// code signature — as a fenced code block in the *current buffer's* language, then
    /// a blank line and the `documentation` body (already markdown). Fencing the
    /// signature is what buys it syntax highlighting in the float for free (the win over
    /// the old text-only sidebar); the float's markdown renderer highlights each fenced
    /// block in its own language (fail-soft when the grammar is absent). `None` when the
    /// item carries neither, which closes the float rather than showing an empty box.
    pub(crate) fn lsp_complete_docs_md(&self, item: &CompletionItemData) -> Option<String> {
        let mut md = String::new();
        if let Some(detail) = item
            .detail
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let ft = self
                .editor
                .buffer_filetype(self.editor.current_buffer_id())
                .unwrap_or_default();
            md.push_str(&format!("```{ft}\n{detail}\n```\n\n"));
        }
        if let Some(doc) = item
            .documentation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            md.push_str(doc);
        }
        let md = md.trim_end();
        (!md.is_empty()).then(|| md.to_string())
    }
}
