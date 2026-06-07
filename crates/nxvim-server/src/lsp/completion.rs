//! The insert-mode completion popup: ingesting completion replies, the
//! live filter/rank, lazy `completionItem/resolve`, keyboard/mouse menu
//! navigation and accept, and projecting the `pmenu` redraw payload.

use nxvim_core::unicode;
use nxvim_core::view::View;
use nxvim_core::Mode;
use nxvim_lsp::{CompletionItemData, LspRequest, PositionEncoding};
use rmpv::Value;

use super::*;
use crate::Server;

impl Server {
    /// Handle a `textDocument/completion` reply (already past the generation /
    /// buffer staleness checks in [`Server::on_lsp_reply`]). Builds the menu on
    /// the initial trigger, or replaces its list on a live re-request, then
    /// re-ranks against the current prefix. Dropped if the user has left insert
    /// mode (the menu is unwanted) or nothing matches (nothing to show).
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
        let (word_start, prefix) = completion_word(&line, col);
        match self.completion.as_mut() {
            // A refresh for the open menu: swap in the new list, re-rank in place.
            // The list is new, so every item's resolve state resets.
            Some(menu) => {
                menu.resolved = vec![false; items.len()];
                menu.resolving = None;
                menu.raw = items;
                menu.is_incomplete = is_incomplete;
                menu.anchor = (row, word_start);
                menu.prefix = prefix;
            }
            // The initial trigger opens the menu; an empty offer opens nothing.
            None => {
                if items.is_empty() {
                    return;
                }
                let resolved = vec![false; items.len()];
                self.completion = Some(CompletionMenu {
                    buffer,
                    anchor: (row, word_start),
                    prefix,
                    is_incomplete,
                    raw: items,
                    visible: Vec::new(),
                    selected: None,
                    resolved,
                    resolving: None,
                });
            }
        }
        self.rerank_menu();
        // Nothing matches what was typed: dismiss rather than show an empty popup.
        if self
            .completion
            .as_ref()
            .is_some_and(|m| m.visible.is_empty())
        {
            self.completion = None;
        }
        self.lsp_dirty = true;
    }

    /// Recompute the menu's `visible` list: filter `raw` to the items matching the
    /// live `prefix` and order them by importance — match tier (exact ▸ prefix ▸
    /// subsequence), then the server's `sortText`, then the label as a stable
    /// tiebreak. Clears the selection, since the candidate set changed.
    pub(crate) fn rerank_menu(&mut self) {
        let Some(menu) = self.completion.as_mut() else {
            return;
        };
        let prefix = menu.prefix.as_str();
        let mut ranked: Vec<(u8, &str, &str, usize)> = menu
            .raw
            .iter()
            .enumerate()
            .filter_map(|(i, item)| {
                let filter = item.filter_text.as_deref().unwrap_or(&item.label);
                let tier = match_tier(filter, prefix)?;
                let secondary = item.sort_text.as_deref().unwrap_or(&item.label);
                Some((tier, secondary, item.label.as_str(), i))
            })
            .collect();
        ranked.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(b.1)).then(a.2.cmp(b.2)));
        let visible: Vec<usize> = ranked.into_iter().map(|(_, _, _, i)| i).collect();
        menu.visible = visible;
        menu.selected = None;
    }

    /// Whether a completion menu is currently open (the insert-mode key path
    /// checks this before routing a key to the menu).
    pub(crate) fn completion_menu_open(&self) -> bool {
        self.completion.is_some()
    }

    /// Move the menu selection by `delta`, wrapping. From no selection, `+1`
    /// highlights the first item and `-1` the last (vim's `<C-n>`/`<C-p>`).
    pub(crate) fn lsp_menu_move(&mut self, delta: isize) {
        let Some(menu) = self.completion.as_mut() else {
            return;
        };
        let n = menu.visible.len();
        if n == 0 {
            return;
        }
        menu.selected = Some(match menu.selected {
            None => {
                if delta > 0 {
                    0
                } else {
                    n - 1
                }
            }
            Some(i) => (i as isize + delta).rem_euclid(n as isize) as usize,
        });
        self.lsp_dirty = true;
        // The selection settled on an item; fetch its lazy docs/detail if needed.
        self.maybe_resolve_selected();
    }

    /// Select a visible item by absolute index (clamped to the visible range) —
    /// the mouse counterpart to [`lsp_menu_move`]'s relative `<C-n>`/`<C-p>`. A
    /// click lands on a specific row, and the wheel moves one step without
    /// wrapping, so this clamps rather than wraps. No-op when the index already
    /// holds, so an idempotent re-select fires no stray resolve.
    pub(crate) fn lsp_menu_select(&mut self, index: usize) {
        let Some(menu) = self.completion.as_mut() else {
            return;
        };
        let n = menu.visible.len();
        if n == 0 {
            return;
        }
        let idx = index.min(n - 1);
        if menu.selected == Some(idx) {
            return;
        }
        menu.selected = Some(idx);
        self.lsp_dirty = true;
        self.maybe_resolve_selected();
    }

    /// Fire a `completionItem/resolve` for the just-selected item when it still
    /// lacks `documentation`/`detail` — the only way per-item docs arrive from
    /// rust_analyzer and most servers. Debounced by selection (it fires on settle,
    /// not per keystroke) and at-most-once per item (the `resolved` flag); a resolve
    /// already in flight for this same item is not re-issued. The reply merges back
    /// in [`Server::merge_resolved_completion`], gated by the request generation so
    /// a navigation that supersedes it drops the stale reply.
    pub(crate) fn maybe_resolve_selected(&mut self) {
        // Decide and capture what's needed *before* taking the mutable borrows the
        // request bookkeeping wants.
        let (raw_idx, item) = {
            let Some(menu) = self.completion.as_ref() else {
                return;
            };
            let Some(sel) = menu.selected else {
                return;
            };
            let Some(&raw_idx) = menu.visible.get(sel) else {
                return;
            };
            let entry = &menu.raw[raw_idx];
            // Already fully populated ⇒ nothing to fetch.
            if entry.documentation.is_some() && entry.detail.is_some() {
                return;
            }
            // Already resolved once, or a resolve for this item is in flight.
            if menu.resolved.get(raw_idx).copied().unwrap_or(true) {
                return;
            }
            if menu.resolving == Some(raw_idx) {
                return;
            }
            // Without the original item there is nothing the server can resolve.
            let Some(item) = entry.resolve_data.clone() else {
                return;
            };
            (raw_idx, item)
        };
        let Some((key, _uri, _encoding)) = self.current_lsp_target() else {
            return;
        };
        let token = self.register_lsp_request(LspReqKind::CompletionResolve);
        if let Some(menu) = self.completion.as_mut() {
            menu.resolving = Some(raw_idx);
        }
        self.lsp
            .request(key, token, LspRequest::ResolveCompletion { item });
    }

    /// Merge a `completionItem/resolve` reply into the in-flight target item and
    /// mark it resolved, so the menu shows the freshly-fetched `detail` (and, in
    /// Phase 3, documentation). A resolve that returned nothing for a field leaves
    /// that field as-is — the list's value wins over a resolve that omitted it, and
    /// a docless item stays docless. No-op if the menu closed or the selection
    /// superseded this resolve (its index is no longer the one in flight).
    pub(crate) fn merge_resolved_completion(
        &mut self,
        documentation: Option<String>,
        detail: Option<String>,
    ) {
        let Some(menu) = self.completion.as_mut() else {
            return;
        };
        let Some(idx) = menu.resolving.take() else {
            return;
        };
        if let Some(flag) = menu.resolved.get_mut(idx) {
            *flag = true;
        }
        if let Some(item) = menu.raw.get_mut(idx) {
            if documentation.is_some() {
                item.documentation = documentation;
            }
            if detail.is_some() {
                item.detail = detail;
            }
        }
        self.lsp_dirty = true;
    }

    /// Close the menu without inserting, dropping any in-flight completion request
    /// so a late reply can't reopen it.
    pub(crate) fn lsp_menu_close(&mut self) {
        if self.completion.take().is_some() {
            self.lsp_requests.remove(&LspReqKind::Completion);
            self.lsp_requests.remove(&LspReqKind::CompletionResolve);
            self.lsp_dirty = true;
        }
    }

    /// Accept the selected item (or the first, when nothing is highlighted):
    /// replace the completion word — or the item's explicit `textEdit` range —
    /// with its text, apply any `additionalTextEdits`, and leave the cursor after
    /// the inserted text, all as one undo step. Stays in insert mode, as vim does.
    pub(crate) fn lsp_menu_accept(&mut self) {
        let Some(menu) = self.completion.take() else {
            return;
        };
        // No refresh or stray resolve should land after an accept.
        self.lsp_requests.remove(&LspReqKind::Completion);
        self.lsp_requests.remove(&LspReqKind::CompletionResolve);
        self.lsp_dirty = true;
        let Some(&raw_idx) = menu.visible.get(menu.selected.unwrap_or(0)) else {
            return;
        };
        let item = &menu.raw[raw_idx];
        let encoding = self
            .current_lsp_target()
            .map_or(PositionEncoding::Utf8, |(_, _, e)| e);

        // The primary edit: the item's explicit textEdit, else replace the word
        // (anchor..cursor) with insertText (falling back to the label).
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let (primary_range, primary_text) = match &item.text_edit {
            Some(edit) => (
                self.lsp_range_to_bytes(&edit.range, encoding),
                edit.new_text.clone(),
            ),
            None => {
                let start = self.editor.buffer().line_start(menu.anchor.0) + menu.anchor.1;
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
        // The cursor lands after the primary insertion, shifted by the net length
        // of any edits that fall before it (e.g. an inserted `use` import).
        let shift: isize = edits
            .iter()
            .skip(1)
            .filter(|(r, _)| r.start < primary_range.start)
            .map(|(r, t)| t.len() as isize - (r.end - r.start) as isize)
            .sum();
        let cursor_byte = (primary_range.start + primary_text.len()) as isize + shift;
        self.editor.apply_edits(edits, cursor_byte.max(0) as usize);
    }

    /// After the editor inserted a word character or backspaced while the menu was
    /// open, recompute the prefix and refresh: a complete list refilters
    /// client-side; an incomplete one re-requests at the new cursor (the current
    /// items stay shown until that reply lands). Closes the menu if the cursor
    /// left the word, or if a complete list now has nothing to offer.
    pub(crate) fn lsp_menu_after_edit(&mut self) {
        let Some(menu) = self.completion.as_ref() else {
            return;
        };
        let buffer = self.editor.current_buffer_id();
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        // Left the word (backspaced before the anchor, changed line/buffer): the
        // menu no longer applies.
        if buffer != menu.buffer || row != menu.anchor.0 || col < menu.anchor.1 {
            self.lsp_menu_close();
            return;
        }
        let line = self.editor.buffer().line(row);
        let region = &line[menu.anchor.1..col.min(line.len())];
        if !region.chars().all(is_word_char) {
            self.lsp_menu_close();
            return;
        }
        let prefix = region.to_string();
        let incomplete = menu.is_incomplete;
        self.completion.as_mut().unwrap().prefix = prefix;
        if incomplete {
            // The cached list was partial: ask the server for the narrowed set.
            // The current items stay shown until the reply re-ranks them.
            self.request_lsp(LspReqKind::Completion);
        } else {
            self.rerank_menu();
            if self
                .completion
                .as_ref()
                .is_some_and(|m| m.visible.is_empty())
            {
                self.lsp_menu_close();
                return;
            }
        }
        self.lsp_dirty = true;
    }

    /// Project the open completion menu into the `pmenu` redraw value (`Nil` when
    /// closed or nothing matches): the ranked visible items, the selected index,
    /// and the overlay's anchor/size in screen cells. The menu sits one row below
    /// the cursor, flipped above when there's no room; `col` is the word-start
    /// screen column (the client adds the number gutter), so the box lines up
    /// under the word being completed — reusing `cursor_screen_col`'s math, no
    /// core change. `text_width` is the text area's cell width (the frame minus
    /// the number gutter), used only to keep the box from overflowing it.
    pub(crate) fn pmenu_value(&self, view: &View, text_width: usize) -> Value {
        let Some(menu) = &self.completion else {
            return Value::Nil;
        };
        if menu.visible.is_empty() {
            return Value::Nil;
        }
        let (arow, acol) = menu.anchor;
        let line = self.editor.buffer().line(arow);
        // The popup anchors in the focused window's text body. Under a horizontal
        // scroll the client paints text at `screen_col - leftcol`, so the anchor
        // shifts left by `leftcol` to stay under the (scrolled) completion word.
        let focused = view.focused();
        let anchor_col =
            unicode::virtcol(&line, acol, self.editor.tabstop()).saturating_sub(focused.leftcol);
        let cursor_row = focused.cursor_row;
        let text_height = focused.lines.len();

        let items: Vec<Value> = menu
            .visible
            .iter()
            .map(|&i| {
                let item = &menu.raw[i];
                Value::Array(vec![
                    Value::from(item.label.as_str()),
                    Value::from(item.kind as u64),
                    Value::from(item.detail.as_deref().unwrap_or("")),
                ])
            })
            .collect();
        let count = items.len();

        // Width: the widest item, clamped so the bordered box fits the text area.
        let content_w = menu
            .visible
            .iter()
            .map(|&i| pmenu_item_width(&menu.raw[i]))
            .max()
            .unwrap_or(1);
        let max_w = text_width.saturating_sub(anchor_col).max(1);
        let width = content_w.clamp(1, max_w);

        // Place the box below if its border+content+border fits, else above;
        // clamp the content height to the room available.
        const MAX_H: usize = 10;
        let want = count.min(MAX_H);
        let below = text_height.saturating_sub(cursor_row + 1);
        let above = cursor_row;
        let (row, height) = if want + 2 <= below {
            (cursor_row + 1, want)
        } else if want + 2 <= above {
            (cursor_row - (want + 2), want)
        } else if below >= above {
            (cursor_row + 1, below.saturating_sub(2).clamp(1, want))
        } else {
            let h = above.saturating_sub(2).clamp(1, want);
            (cursor_row.saturating_sub(h + 2), h)
        };

        // The selected item's documentation, split into display lines, for the
        // preview box the client floats beside the popup (Phase 3). Empty ⇒ no
        // preview: nothing selected, or the selected item carries no docs (yet —
        // a `completionItem/resolve` may fill it in, repainting on the merge).
        let doc: Vec<Value> = menu
            .selected
            .and_then(|s| menu.visible.get(s))
            .and_then(|&i| menu.raw[i].documentation.as_deref())
            .map(|d| d.lines().map(Value::from).collect())
            .unwrap_or_default();

        Value::Map(vec![
            (Value::from("items"), Value::Array(items)),
            (
                Value::from("selected"),
                match menu.selected {
                    Some(i) => Value::from(i as u64),
                    None => Value::Nil,
                },
            ),
            (Value::from("row"), Value::from(row as u64)),
            (Value::from("col"), Value::from(anchor_col as u64)),
            (Value::from("width"), Value::from(width as u64)),
            (Value::from("height"), Value::from(height as u64)),
            (Value::from("doc"), Value::Array(doc)),
        ])
    }
}
