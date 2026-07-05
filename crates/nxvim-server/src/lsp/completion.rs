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
use nxvim_lsp::{CompletionItemData, PositionEncoding};

use super::*;
use crate::EditHost;

/// The current LSP completion's raw items + the word they anchor at, kept so a
/// delegated accept can apply the chosen item's edit and so a `isIncomplete = false`
/// list can be re-served on a prefix edit without another round-trip. Indexed by the
/// `MenuItem.key` the engine carries (the position in `items`).
pub(crate) struct LspComplete {
    /// The server's items, verbatim; `items[key]` is the row the engine accepted.
    pub items: Vec<CompletionItemData>,
    /// The buffer + (row, word-start byte col) the items were computed at. Reused
    /// while the cursor stays in this word; invalidated when it moves on.
    pub buffer: BufferId,
    pub anchor: (usize, usize),
    /// The server's `isIncomplete`: when `true`, a prefix edit must re-request rather
    /// than re-serve the cached list (the list was narrowed to the old prefix).
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
        // Cache miss / incomplete / moved word: ask the server. The reply pushes into
        // whatever generation is live when it lands.
        self.request_lsp(LspReqKind::Completion);
    }

    /// Handle a `textDocument/completion` reply (already past the generation / buffer
    /// staleness checks in [`EditHost::on_lsp_reply`]). Cache the items against the
    /// current word and stream them into the open completion menu at the live
    /// generation. Dropped if the user left insert mode.
    pub(crate) fn on_completion_reply(
        &mut self,
        is_incomplete: bool,
        items: Vec<CompletionItemData>,
    ) {
        if self.editor.mode != Mode::Insert {
            return;
        }
        let buffer = self.editor.current_buffer_id();
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        let (word_start, _prefix) = completion_word(&line, col);
        self.lsp_complete = Some(LspComplete {
            items,
            buffer,
            anchor: (row, word_start),
            is_incomplete,
        });
        // A fresh list supersedes any in-flight docs resolve: its key indexed the old
        // items, so drop it (the late reply is ignored — `on_completion_resolve_reply`
        // takes a `None` key) and let the new selection re-issue against the new list.
        self.lsp_complete_resolve_key = None;
        // Push into the live menu generation — the engine bumped it on the keystroke
        // that fired the request, and a `Complete` menu is open (core seeded it, or
        // will re-seed on the next key). A `0` generation means no menu is open.
        let gen = self.editor.menu_generation();
        if gen != 0 {
            self.complete_lsp_push(gen);
        }
        self.lsp_dirty = true;
    }

    /// Build `MenuItem`s from the cached LSP items and append them to the open
    /// completion menu at generation `gen`. Each row carries the LSP merge priority,
    /// `source_accept = true` (accept is delegated back to apply its `textEdit`), and
    /// its index as the `key` so the accept can find the raw item. The engine's
    /// matcher ranks them against the prefix and merges them with the buffer rows by
    /// priority; a stale `gen` is dropped by [`Editor::menu_push`].
    pub(crate) fn complete_lsp_push(&mut self, gen: u64) {
        let Some(cache) = self.lsp_complete.as_ref() else {
            return;
        };
        let priority = self.complete_lsp_priority;
        let items: Vec<nxvim_core::MenuItem> = cache
            .items
            .iter()
            .enumerate()
            .map(|(key, item)| {
                // Display the label; insert the label as a no-op fallback (the real
                // edit is applied server-side on accept via `source_accept`).
                let label = item.label.clone();
                nxvim_core::MenuItem {
                    insert: Some(label.clone()),
                    label,
                    key,
                    preview: None,
                    priority,
                    source_accept: true,
                    // The docs sidebar reads an `lsp` row's docs from the server's
                    // item cache (`source_accept`), not an inline `doc` / `resolve`.
                    doc: None,
                    resolve: None,
                    // LSP completion edits the buffer (`textEdit`), not a cmdline span.
                    replace: None,
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
        let encoding = self
            .current_lsp_target()
            .map_or(PositionEncoding::Utf8, |(_, _, e)| e);
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
        let Some((server_key, _uri, _enc)) = self.current_lsp_target() else {
            return;
        };
        let token = self.register_lsp_request(LspReqKind::CompletionResolve);
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
