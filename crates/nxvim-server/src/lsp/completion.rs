//! The built-in **`lsp` completion source** for the unified `nx.complete` engine
//! (Phase 4-C). LSP completion is server-native — the async `textDocument/completion`
//! request, the encoding-aware `textEdit` / `additionalTextEdits` accept, and the
//! `isIncomplete` re-request all live here, not in Lua/core (the Lua mutation API is
//! nil and `nxvim-core` is LSP-agnostic). The engine (core) owns the menu, the
//! prefix, the fuzzy matcher, navigation, and the generation token; this module only
//! feeds it candidates and applies the chosen item's edit when accept is delegated
//! back (`MenuItem.source_accept`).
//!
//! This replaces the retired bespoke completion pmenu. The docs-beside-popup that the
//! old `pmenu_value` projected is deferred to Phase 4-D (the unified markdown preview
//! sidebar); `completionItem/resolve` docs are not fetched here yet.

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
        let (primary_range, primary_text) = match &item.text_edit {
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
        self.lsp_dirty = true;
    }
}
