//! The built-in **`lsp` completion source** for the unified `btv.complete` engine
//! (Phase 4-C). LSP completion is server-native — the async `textDocument/completion`
//! request, the encoding-aware `textEdit` / `additionalTextEdits` accept, and the
//! `isIncomplete` re-request all live here, not in Lua/core (the Lua mutation API is
//! nil and `bemtvi-core` is LSP-agnostic). The engine (core) owns the menu, the
//! prefix, the fuzzy matcher, navigation, and the generation token; this module only
//! feeds it candidates and applies the chosen item's edit when accept is delegated
//! back (`MenuItem.source_accept`).
//!
//! This replaces the retired bespoke completion pmenu. The docs-beside-popup the old
//! `pmenu_value` projected is back as a **doc-float window** (Phase 4-D): the selected
//! `lsp` row's `detail` + `documentation`, lazily fetched via `completionItem/resolve`
//! (`complete_lsp_maybe_resolve` / `on_completion_resolve_reply`), built into markdown by
//! `EditHost::lsp_complete_docs_parts` and rendered by `Editor::open_completion_docs_float`.
//! A candidate several servers all offer is **one** row carrying an [`Offer`] each, so
//! the float can show every server's docs under its own labelled rule
//! (`EditHost::lsp_complete_docs_sections`) instead of the first-to-answer's alone.

use bemtvi_core::markdown::DocFormat;
use bemtvi_core::{BufferId, DocsSection, Mode};
use bemtvi_lsp::CompletionItemData;

use super::*;
use crate::EditHost;

/// Whether two candidates are the **same offer**: same label, same kind, and the same
/// text an accept would insert. This is what folds two servers' takes on one symbol
/// into a single [`CompletionRow`] — the rows would be indistinguishable in the popup
/// and accept identically, so a second row would be noise rather than a second choice
/// (their *docs* are kept apart, one section each).
///
/// Compared on the *effective* insert text (the `textEdit`'s replacement, else
/// `insertText`, else the label) so an item that merely spells its insertion
/// differently — a different range, a snippet body — gets its own row: accepting it
/// does something different.
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

/// `server`'s place in the buffer's routing order — its index in `rank`, or the tail
/// for a server no longer attached (one that detached mid-round, whose share is still
/// in flight). The tail rather than the head: an unranked contributor is the one we
/// know least about, so it must not silently outrank the servers we do.
fn rank_position(rank: &[ServerKey], server: &ServerKey) -> usize {
    rank.iter().position(|k| k == server).unwrap_or(rank.len())
}

/// One server's **offer** of a candidate: the item it sent, and which server sent it.
///
/// The server is load-bearing three times over: the accept converts that item's
/// `textEdit` ranges at **its** server's negotiated encoding, its lazy
/// `completionItem/resolve` must go back to the same server (the `data` blob it
/// round-trips is only meaningful there), and the docs float labels the offer's
/// section with its name.
pub(crate) struct Offer {
    pub server: ServerKey,
    pub item: CompletionItemData,
}

/// One **row** of the merged popup: every server that made the same offer
/// ([`same_offer`]), ordered by the buffer's server rank (`priority`, then key —
/// the order every other multi-server surface merges in).
///
/// Two servers on one buffer routinely name the same symbol, and their rows would be
/// indistinguishable in the popup and accept to the same text — so they share a row
/// rather than doubling it. What they do *not* share is what they have to say about
/// it: a type-checker's signature and a linter's note are different claims, and the
/// docs float shows each under its own labelled rule, the way a merged hover does.
/// Before this, the duplicate was dropped outright and the surviving row's docs came
/// from whichever server answered first — i.e. at random.
pub(crate) struct CompletionRow {
    pub offers: Vec<Offer>,
}

impl CompletionRow {
    /// The **primary** offer — the best-ranked contributor's. Its item is what the
    /// popup displays and what an accept applies (every offer in the row inserts the
    /// same text by construction, but they may differ in `textEdit` range or in the
    /// `additionalTextEdits` they carry, and one server has to be the one that wins).
    pub fn primary(&self) -> &Offer {
        &self.offers[0]
    }
}

/// One completion **round**'s merged candidates: every capable server is asked at
/// once and their items accumulate here as the replies land, kept so a delegated
/// accept can apply the chosen item's edit and so an `isIncomplete = false` list can
/// be re-served on a prefix edit without another round-trip. Indexed by the
/// `MenuItem.key` the engine carries (the position in `rows`).
///
/// The round is opened — and this cache reset — when the requests go out, so a reply
/// only ever *appends* rows (a duplicate offer joins an existing one in place). That
/// is what keeps `key` stable while a round is still filling: a lazy
/// `completionItem/resolve` issued against row 3 of the first server's share still
/// addresses row 3 after the second server's share arrives.
pub(crate) struct LspComplete {
    /// The merged rows; `rows[key]` is the row the engine displayed and accepted.
    pub rows: Vec<CompletionRow>,
    /// The buffer + (row, word-start byte col) the round was requested at. Reused
    /// while the cursor stays in this word; invalidated when it moves on.
    pub buffer: BufferId,
    pub anchor: (usize, usize),
    /// The buffer's `changedtick` when the round was requested. The items' `textEdit`
    /// ranges are authored against the text at that moment; re-serving or accepting
    /// them after a text change would splice the replacement into the middle of the
    /// grown word. Cursor movement within the word leaves the tick untouched, so
    /// reuse survives it; a typed character bumps it and forces a fresh round.
    pub tick: u64,
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

        // Cache hit: same buffer + same word start + the text is unchanged since the
        // round was requested, and the cached list is complete (covers every candidate
        // for this word). Re-serve it; core filters by prefix. The `tick` arm is what
        // makes typing more word characters re-request: the cached items' `textEdit`
        // ranges cover the word as it was at request time, so re-serving them over the
        // grown word would let an accept splice the replacement into its middle.
        let reuse = self.lsp_complete.as_ref().is_some_and(|c| {
            !c.is_incomplete
                && c.buffer == buffer
                && c.anchor == (row, word_start)
                && c.tick == self.editor.buffer().changedtick
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
            rows: Vec::new(),
            buffer,
            anchor: (row, word_start),
            tick: self.editor.buffer().changedtick,
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
                bemtvi_lsp::LspRequest::Completion {
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
    /// inserted text) does not get a second row — the two would be indistinguishable in
    /// the popup and accept to the same text. It **joins** that row instead, as a
    /// further [`Offer`], so both servers' docs reach the docs float under their own
    /// labelled section and the row's identity is the best-ranked contributor's rather
    /// than the quickest one's. Anything that differs — a different `textEdit`, kind, or
    /// insert text — still gets its own row, because accepting it does something else.
    pub(crate) fn on_completion_reply(
        &mut self,
        buffer: BufferId,
        tick: u64,
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
        // The items' `textEdit` ranges were computed against the text at request time;
        // the user typed while the request was in flight, so they no longer describe
        // the buffer. Dropping the reply forces the next keystroke's dispatch to issue
        // a fresh round — caching ranges that point into superseded text would let an
        // accept corrupt the word it replaced.
        if tick != self.editor.buffer().changedtick {
            return;
        }
        // Where this server sits in the buffer's routing order, and the priority a row
        // it produced carries — both resolved before the cache is borrowed mutably.
        let rank = self.lsp_server_rank(buffer);
        let rank_of = |k: &ServerKey| rank_position(&rank, k);
        let priority = self.lsp_row_priority(&rank, &server);
        let gen = self.editor.menu_generation();
        let Some(cache) = self.lsp_complete.as_mut() else {
            return; // the round was superseded before this share landed
        };
        if cache.buffer != buffer {
            return;
        }
        let start = cache.rows.len();
        // Rows whose primary contributor changed to this (better-ranked) server, so
        // their already-pushed menu priority can be raised once the borrow ends.
        let mut promoted: Vec<usize> = Vec::new();
        for item in items {
            let offer = Offer {
                server: server.clone(),
                item,
            };
            match cache
                .rows
                .iter_mut()
                .position(|r| same_offer(&r.primary().item, &offer.item))
            {
                Some(key) => {
                    let offers = &mut cache.rows[key].offers;
                    // One section per SERVER: a server that lists the same offer twice
                    // (two `sortText`s over one symbol) contributes once — a second
                    // section under the same name says nothing the first didn't.
                    if offers.iter().any(|o| o.server == offer.server) {
                        continue;
                    }
                    // Insert in rank order, so the row's primary — the item an accept
                    // applies and the first section the float shows — is the best-ranked
                    // server's however the replies happened to interleave.
                    let at = offers.partition_point(|o| rank_of(&o.server) <= rank_of(&server));
                    offers.insert(at, offer);
                    if at == 0 {
                        promoted.push(key);
                    }
                }
                None => cache.rows.push(CompletionRow {
                    offers: vec![offer],
                }),
            }
        }
        cache.is_incomplete |= is_incomplete;
        // Push into the live menu generation — the engine bumped it on the keystroke
        // that fired the request, and a `Complete` menu is open (core seeded it, or
        // will re-seed on the next key). A `0` generation means no menu is open.
        if gen != 0 {
            // A row this reply took over as primary already sits in the menu at the
            // previous primary's (worse) rank — raise it to where its new best
            // contributor belongs, so the order states the routing priority rather than
            // recording which server was quicker.
            for key in promoted {
                self.editor.menu_reprioritize(gen, key, priority);
            }
            self.complete_lsp_push_from(gen, start);
        }
        self.lsp_dirty = true;
    }

    /// The buffer's servers in **routing order** (`priority` descending, then key) — the
    /// rank every multi-server surface merges by, and the order a merged row's offers and
    /// docs sections are held in. Empty when the buffer has no attached servers.
    ///
    /// Sorted through the shared [`lsp_routing_order`](Self::lsp_routing_order)
    /// comparator, not taken from the state map's own iteration: that map is keyed by
    /// [`ServerKey`], so walking it yields **key** order — config name, then root — and a
    /// stated `btv.lsp.config{ priority = … }` had no effect on the popup at all. The
    /// comparator is the one every other ordered view of a buffer's servers uses, so the
    /// completion merge can't disagree with the routing it is meant to state.
    fn lsp_server_rank(&self, buffer: BufferId) -> Vec<ServerKey> {
        let mut rank: Vec<ServerKey> = self
            .lsp_states
            .get(&buffer)
            .map(|s| s.servers().map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
        rank.sort_by(|a, b| self.lsp_routing_order(a, b));
        rank
    }

    /// The menu priority a row whose primary contributor is `server` carries: the `lsp`
    /// source's merge bias stepped down one point per place in `rank`, so **equally-good**
    /// matches from two servers order by the stated routing priority instead of by which
    /// reply landed first (the engine's blended sort is stable, so streamed order would
    /// otherwise decide ties). One point per server keeps every LSP row far above the
    /// buffer-word tier — the `lsp` bias is 8 against 0.
    ///
    /// Shared by the initial push and the promotion a merged row takes when a
    /// better-ranked server joins it, so the two can't disagree about where a row goes.
    fn lsp_row_priority(&self, rank: &[ServerKey], server: &ServerKey) -> i32 {
        self.complete_lsp_priority - rank_position(rank, server) as i32
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
    /// Each row ranks by its **primary** contributor
    /// ([`lsp_row_priority`](Self::lsp_row_priority)).
    fn complete_lsp_push_from(&mut self, gen: u64, start: usize) {
        let Some(cache) = self.lsp_complete.as_ref() else {
            return;
        };
        let rank = self.lsp_server_rank(cache.buffer);
        let items: Vec<bemtvi_core::MenuItem> = cache
            .rows
            .iter()
            .enumerate()
            .skip(start)
            .map(|(key, row)| {
                let item = &row.primary().item;
                let priority = self.lsp_row_priority(&rank, &row.primary().server);
                // Display the label; insert the label as a no-op fallback (the real
                // edit is applied server-side on accept via `source_accept`).
                let label = item.label.clone();
                bemtvi_core::MenuItem {
                    insert: Some(label.clone()),
                    // The LSP `CompletionItemKind` name (`"Function"`, `"Variable"`, …);
                    // `None` when the server sent no kind (code `0`).
                    kind: item.kind_label().map(str::to_string),
                    priority,
                    // The server's own order for equally-good matches — how it says
                    // "these parameters belong above those globals" for the call the
                    // cursor is in. Falls back to the label, as the spec defines a
                    // missing `sortText` to, so a server that sends it for some items
                    // only still orders the rest against them coherently.
                    sort_key: Some(item.sort_text.clone().unwrap_or_else(|| label.clone())),
                    // The docs sidebar reads an `lsp` row's docs from the server's
                    // item cache (`source_accept`), not an inline `doc` / `resolve`.
                    source_accept: true,
                    ..bemtvi_core::MenuItem::new(label, key)
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
        // The row's **primary** offer — the best-ranked server that made it. Several
        // servers may have offered the same completion; they insert the same text by
        // construction, but the range they replace and the imports they bring along are
        // their own, so one has to win, and it is the one the routing order names.
        let Some((item, origin)) = self
            .lsp_complete
            .as_ref()
            .and_then(|c| c.rows.get(key))
            .map(|r| (r.primary().item.clone(), r.primary().server.clone()))
        else {
            return;
        };
        // At the encoding of the server that OFFERED this item: a `textEdit` range is
        // authored in its own server's coordinates, and a two-server buffer can hold
        // a utf-8 and a utf-16 one at once — reading one as the other shifts every
        // column after the line's first multi-byte glyph.
        let encoding = self.reply_encoding(Some(&origin));
        // Anchor the word fallback at the current word start (the engine kept the
        // cursor in the word; recompute rather than thread core's anchor through).
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        let (word_start, _) = completion_word(&line, col);

        // A cached item's explicit `textEdit` is authored against the buffer text at
        // request time. The dispatch guard normally re-requests on the keystroke that
        // changed the text (so the cache is reset before an accept can see it), but a
        // text change that lands without a dispatch — a settle-order edit from an
        // autocmd, a paste that didn't re-arm the source — leaves the stale range
        // behind; applying it would splice the replacement into the middle of the
        // grown word. When the tick moved, ignore the item's range and fall back to
        // the word replacement, which is recomputed against the live text.
        let tick_stale = self
            .lsp_complete
            .as_ref()
            .is_some_and(|c| c.tick != self.editor.buffer().changedtick);
        // The primary edit: the item's explicit textEdit, else replace the word
        // (word_start..cursor) with insertText (falling back to the label).
        let (mut primary_range, primary_text) = match (&item.text_edit, tick_stale) {
            (Some(edit), false) => (
                self.lsp_range_to_bytes(&edit.range, encoding),
                edit.new_text.clone(),
            ),
            _ => {
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
            match bemtvi_core::parse_snippet(&primary_text) {
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

    /// The docs float's lazy-docs fetch (Phase 4-D): when the **highlighted** row is an
    /// `lsp` row one of whose contributors carries no inline `documentation` yet but has
    /// `resolve_data`, issue a `completionItem/resolve` to pull its docs. Called once
    /// per key from [`EditHost::run_pending`] (the selection is final by then). A no-op
    /// for a native `buffer` row, a fully-resolved row, contributors the server gave no
    /// `data` to resolve against, or while a resolve for this row is already in flight.
    /// The reply ([`EditHost::on_completion_resolve_reply`]) fills that contributor's
    /// item and repaints.
    ///
    /// **One at a time, in rank order.** A merged row has a contributor per server and
    /// each holds its docs behind its own resolve, but `lsp_requests` keeps a single
    /// slot per request kind — a second concurrent resolve would supersede the first and
    /// settle it empty. So the earliest unresolved contributor goes out now and the
    /// reply re-settles, which brings us straight back here for the next one: the float
    /// fills section by section rather than all at once, and never loses a section to a
    /// race.
    pub(crate) fn complete_lsp_maybe_resolve(&mut self) {
        if !self.complete_lsp_active {
            return;
        }
        // Only an actively-selected `lsp` row has docs to resolve (a noselect popup or
        // a `buffer` row yields `None` / `source_accept = false`).
        let Some((key, true)) = self.editor.complete_selected() else {
            return;
        };
        // A resolve for this row is already pending — let it land (it will bring us
        // back for the next unresolved contributor). A pending resolve for a *different*
        // row is stale: the selection moved on, so supersede it.
        if matches!(self.lsp_complete_resolve_key, Some((k, _)) if k == key) {
            return;
        }
        // The first contributor still missing its docs. `documentation` is stamped
        // `Some("")` by a docless reply, so a resolved-but-empty section is never
        // re-requested and the walk always terminates.
        let Some((idx, resolve_data, server_key)) = self.lsp_complete.as_ref().and_then(|c| {
            let row = c.rows.get(key)?;
            row.offers.iter().enumerate().find_map(|(idx, o)| {
                if o.item.documentation.is_some() {
                    return None;
                }
                // Back to the server that OFFERED this section, not the buffer's
                // first: the `data` blob being round-tripped is that server's own
                // handle on the item. Resolving ruff's candidate against pyright is
                // a wrong request, not a degraded one.
                Some((idx, o.item.resolve_data.clone()?, o.server.clone()))
            })
        }) else {
            return;
        };
        let token = self.register_lsp_request_to(LspReqKind::CompletionResolve, 0, &server_key);
        self.lsp_complete_resolve_key = Some((key, idx));
        self.fx.lsp_request(
            server_key,
            token,
            bemtvi_lsp::LspRequest::ResolveCompletion { item: resolve_data },
        );
    }

    /// The docs sidebar's lazy-docs fetch for a **plugin** completion row (Phase 4-E,
    /// the analogue of [`EditHost::complete_lsp_maybe_resolve`] for `btv.complete.source`
    /// sources): when the highlighted row carries a `resolve` handle and its docs are
    /// not cached yet, ask Lua to run the source's `resolve` callback
    /// ([`LuaRuntime::run_complete_resolve`]). Called once per key from
    /// [`EditHost::run_pending`]. A no-op for an inline-doc / `buffer` / `lsp` row (no
    /// handle), an already-resolved handle, or one already in flight. The reply lands
    /// via `btv._complete_resolve_done` → the resolve cache, drained in
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
                .echo(format!("E5108: Error in btv.complete resolve: {e}"));
            self.complete_resolve_inflight = None;
            return;
        }
        // A synchronous `respond` already queued the docs; drain + apply so the
        // sidebar paints this same key. An async source's reply lands on a later tick.
        self.apply_lua_effects();
    }

    /// Apply a delegated **plugin `on_accept`** accept (P4): a `btv.complete.source`
    /// item carried an `on_accept` callback, so its accept was delegated here instead
    /// of core splicing `insert`. Run the callback (via [`LuaRuntime::run_complete_accept`]),
    /// handing it the trigger RANGE it should replace — the word under the cursor,
    /// computed exactly as the snippet accept does (`word_start..cursor`, extended over
    /// the rest of the word under a `Replace` accept). The callback owns the edit (it
    /// typically `btv.buf.set_text`s an expansion, or `btv.snippet.expand`s a body); any
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
                .echo(format!("E5108: Error in btv.complete on_accept: {e}"));
            return;
        }
        self.apply_lua_effects();
    }

    /// Apply a `completionItem/resolve` reply (Phase 4-D): fill the resolved
    /// `documentation` / `detail` into the cached item the docs float reads, keyed by
    /// the **(row, contributor)** the resolve was issued for
    /// ([`EditHost::lsp_complete_resolve_key`]) — a merged row resolves one section per
    /// server, so the reply has to land in the section that asked for it rather than in
    /// the row's first. `documentation` is stamped `Some` even when the server returned
    /// nothing (an empty string ⇒ resolved-but-docless), so it is never re-requested. A
    /// reply whose list was replaced meanwhile finds a `None` key and is ignored.
    /// `lsp_dirty` repaints the float with the freshly resolved docs.
    pub(crate) fn on_completion_resolve_reply(
        &mut self,
        documentation: Option<String>,
        documentation_format: DocFormat,
        detail: Option<String>,
    ) {
        let Some((key, idx)) = self.lsp_complete_resolve_key.take() else {
            return;
        };
        if let Some(offer) = self
            .lsp_complete
            .as_mut()
            .and_then(|c| c.rows.get_mut(key))
            .and_then(|r| r.offers.get_mut(idx))
        {
            offer.item.documentation = Some(documentation.unwrap_or_default());
            offer.item.documentation_format = documentation_format;
            if detail.is_some() {
                offer.item.detail = detail;
            }
        }
        self.lsp_dirty = true;
    }
}

impl EditHost {
    /// How to render a block `server` sent: what it **declared**, unless it declared
    /// `plaintext` and this server has a configured `docs_format` (`btv.lsp.config`'s
    /// `docs_format = "rst"`).
    ///
    /// The override reaches only `plaintext`, deliberately. That is the value a server
    /// with no way to name its format is forced into — LSP's `MarkupKind` has no rst
    /// kind — so it is the only one a person can know better than the server does. A
    /// block declared `markdown` is markdown whatever the configuration says: there
    /// the server *did* tell us, and it outranks us.
    pub(crate) fn docs_format_for(&self, server: &ServerKey, declared: DocFormat) -> DocFormat {
        match declared {
            DocFormat::PlainText => self
                .lsp_docs_format
                .get(&server.name)
                .copied()
                .unwrap_or(DocFormat::PlainText),
            declared => declared,
        }
    }

    /// The **labelled sections** the completion docs float renders for the `lsp` row
    /// `key`: one per server that offered it, in routing order, each holding that
    /// server's own `detail` + `documentation`
    /// ([`lsp_complete_docs_parts`](Self::lsp_complete_docs_parts)).
    ///
    /// This is the completion twin of the merged hover: with two servers on a buffer,
    /// the same symbol is routinely offered by both, and what they say about it differs
    /// — a type-checker's signature and a linter's note are different claims, so the
    /// reader has to see which one made which. A **lone** contributor (the one-server
    /// buffer, and any row only one server offered) takes an empty label and renders
    /// bare, so the ordinary float is unchanged; the label is dropped, too, when only
    /// one of several contributors actually has docs — a rule naming the only section
    /// present separates nothing.
    ///
    /// Empty when no contributor has docs yet, which closes the float rather than
    /// showing an empty box.
    pub(crate) fn lsp_complete_docs_sections(&self, key: usize) -> Vec<DocsSection> {
        let Some(row) = self.lsp_complete.as_ref().and_then(|c| c.rows.get(key)) else {
            return Vec::new();
        };
        let mut sections: Vec<DocsSection> = row
            .offers
            .iter()
            .filter_map(|o| {
                let (detail, body) = self.lsp_complete_docs_parts(&o.item)?;
                Some(DocsSection {
                    label: o.server.name.clone(),
                    detail,
                    body,
                    format: self.docs_format_for(&o.server, o.item.documentation_format),
                })
            })
            .collect();
        if sections.len() < 2 {
            for s in &mut sections {
                s.label = String::new();
            }
        }
        sections
    }

    /// The two parts the completion docs float renders for an `lsp` row (Phase 4-D,
    /// now the doc-float-window model): the item's `detail` — a one-line code
    /// signature — and its `documentation` body (already markdown). `None` when the
    /// item carries neither, which closes the float rather than showing an empty box.
    ///
    /// The detail rides *beside* the body rather than pre-fenced into it. Core fences
    /// it in the buffer's language, which is what buys a signature syntax highlighting
    /// in the float for free (the win over the old text-only sidebar) — but as an
    /// [asserted](bemtvi_core::markdown::MdCode::asserted) block, because the fence is
    /// bemtvi's claim and not the server's: LSP defines `detail` as "additional
    /// information about this item", so `rust-analyzer` puts a signature there and
    /// `pyright` puts the label `Auto-import`.
    pub(crate) fn lsp_complete_docs_parts(
        &self,
        item: &CompletionItemData,
    ) -> Option<(Option<String>, String)> {
        let text = |s: &Option<String>| {
            s.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let detail = text(&item.detail);
        let body = text(&item.documentation).unwrap_or_default();
        (detail.is_some() || !body.is_empty()).then_some((detail, body))
    }
}
