//! Multi-cursor (Helix-style) — the first slice.
//!
//! bemtvi has one primary cursor ([`Editor::cursor`]); this adds *secondary*
//! cursors that edits apply to in lockstep. The hard part of multi-cursor —
//! keeping every cursor's position correct as an edit at one of them shifts the
//! bytes under the others — is already solved for [`crate::extmark`]: each
//! secondary cursor is a point extmark in the reserved [`CURSOR_NS`], so the
//! buffer's single edit choke point ([`crate::buffer::Buffer`]) auto-shifts them
//! all. They also ride undo snapshots like any extmark, so undo/redo restores
//! the cursor set for free.
//!
//! The headline operation is [`Editor::for_each_cursor`]: it runs an edit at the
//! primary and every secondary, placing [`Editor::cursor`] at each in turn so the
//! ordinary single-cursor effect helpers (`x`, an insert keystroke) work
//! unchanged. `<A-c>` enters placement mode and drops a cursor at the current
//! position (each repeat drops another); motions move only the primary, so you
//! navigate and place. `<Esc>` then applies subsequent motions/edits to every
//! cursor; a second `<Esc>` collapses back to one.

use super::*;
use crate::extmark::{ANCHOR_NS, CURSOR_NS};
use crate::mode::Mode;

/// A point-in-time snapshot of the placed-cursor set, captured before a placement
/// command for [`Mode::MultiCursor`] undo/redo: the primary position plus every
/// secondary cursor's byte offset. Unlike the text [`UndoTree`]'s snapshot it
/// holds no rope — placing cursors never touches the document — so stepping
/// through it is pure cursor-set bookkeeping. See [`Editor::placement_undo`].
#[derive(Clone)]
pub(crate) struct PlacementSnapshot {
    primary: Cursor,
    secondaries: Vec<usize>,
}

impl Editor {
    /// Set/replace a secondary-cursor extmark at byte `at` in the current buffer,
    /// returning its id. Carries no highlight or range — it only tracks a point.
    fn set_cursor_mark(&mut self, id: Option<u64>, at: usize) -> u64 {
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .set(CURSOR_NS, id, at, None, None, 0, None)
    }

    /// The live byte anchor of secondary-cursor mark `id`, or `None` if it was
    /// swallowed away (deleted from the store).
    fn cursor_mark_pos(&self, id: u64) -> Option<usize> {
        self.buffer().extmarks.get(CURSOR_NS, id).map(|m| m.start)
    }

    /// Set/replace the [`ANCHOR_NS`] visual-anchor mark `id` at byte `at`, paired
    /// by id with the [`CURSOR_NS`] head of the same cursor.
    fn set_anchor_mark(&mut self, id: u64, at: usize) {
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .set(ANCHOR_NS, Some(id), at, None, None, 0, None);
    }

    /// The live byte position of the visual anchor paired with cursor `id`, or
    /// `None` when none is set (outside visual mode, or for the primary — whose
    /// anchor lives in [`Editor::visual_anchor`]).
    fn anchor_mark_pos(&self, id: u64) -> Option<usize> {
        self.buffer().extmarks.get(ANCHOR_NS, id).map(|m| m.start)
    }

    /// The [`Cursor`] at byte offset `idx`, clamped onto the last real character.
    /// Used to turn a stored anchor byte back into the `(line, col)` the visual
    /// range helpers expect.
    pub(crate) fn cursor_at_byte(&self, idx: usize) -> Cursor {
        let idx = idx.min(self.last_char_idx());
        let line = self.buffer().byte_to_line(idx);
        Cursor {
            line,
            col: idx - self.buffer().line_start(line),
        }
    }

    /// The byte offset of [`Cursor`] `c` in the current buffer.
    pub(crate) fn anchor_byte(&self, c: Cursor) -> usize {
        self.buffer().byte_at(c.line, c.col)
    }

    /// Seed per-cursor visual anchors when a visual mode opens over an existing
    /// multi-cursor set: each secondary [`CURSOR_NS`] head gets a paired
    /// [`ANCHOR_NS`] anchor at its current position (a 1-wide selection, like
    /// vim's `v`). The primary's anchor is [`Editor::visual_anchor`], set by the
    /// caller. A no-op without secondary cursors.
    pub(crate) fn begin_visual_anchors(&mut self) {
        if !self.has_secondary_cursors() {
            return;
        }
        self.clear_anchor_marks();
        let heads: Vec<(u64, usize)> = self.cursor_marks().map(|m| (m.id, m.start)).collect();
        for (id, at) in heads {
            self.set_anchor_mark(id, at);
        }
    }

    /// Project the live cursor state into the unified [`Selections`] view both
    /// editing grammars read: the **primary** ([`Editor::cursor`], paired with
    /// [`Editor::visual_anchor`] inside a visual mode, else a point) as range 0,
    /// then every **secondary** cursor extmark ([`CURSOR_NS`] head + its paired
    /// [`ANCHOR_NS`] anchor, falling back to the head for an unpaired mark),
    /// ordered by head byte. Always non-empty — the primary is always present.
    /// The inverse of [`Editor::set_selections`]; see [`crate::editor::selection`].
    pub(crate) fn selections(&self) -> Selections {
        // Whether the primary carries a distinct anchor (a visual selection or a
        // Helix range); otherwise it's a point at the cursor.
        let anchored = self.mode.shows_selection();
        let primary = Range {
            anchor: if anchored {
                self.visual_anchor
            } else {
                self.cursor
            },
            head: self.cursor,
        };
        // Collect the secondary (head, anchor) byte pairs first — chaining a
        // `self.anchor_mark_pos` inside the `cursor_marks` iterator is fine (both
        // are shared borrows), matching `secondary_selections`.
        let mut secs: Vec<(usize, usize)> = self
            .cursor_marks()
            .map(|m| (m.start, self.anchor_mark_pos(m.id).unwrap_or(m.start)))
            .collect();
        secs.sort_unstable_by_key(|&(head, _)| head);
        let mut ranges = Vec::with_capacity(1 + secs.len());
        ranges.push(primary);
        for (head, anchor) in secs {
            ranges.push(Range {
                anchor: self.cursor_at_byte(anchor),
                head: self.cursor_at_byte(head),
            });
        }
        Selections { ranges, primary: 0 }
    }

    /// Write a [`Selections`] set back into the live state — the inverse of
    /// [`Editor::selections`]. The primary range lands in [`Editor::cursor`] (and
    /// [`Editor::visual_anchor`] when the mode carries a selection); every **other**
    /// range is rebuilt as a secondary [`CURSOR_NS`] head (with a paired
    /// [`ANCHOR_NS`] anchor). The secondary set is rebuilt wholesale, so extmark ids
    /// are *not* stable across a write — callers must not rely on them.
    pub(crate) fn set_selections(&mut self, sel: &Selections) {
        let anchored = self.mode.shows_selection();
        let primary = sel.primary();
        self.cursor = primary.head;
        if anchored {
            self.visual_anchor = primary.anchor;
        }
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .clear(CURSOR_NS, None);
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .clear(ANCHOR_NS, None);
        for (i, r) in sel.ranges.iter().enumerate() {
            if i == sel.primary {
                continue;
            }
            let head = self.anchor_byte(r.head);
            let id = self.set_cursor_mark(None, head);
            if anchored {
                let anchor = self.anchor_byte(r.anchor);
                self.set_anchor_mark(id, anchor);
            }
        }
    }

    /// Visual `o`/`O`: move the cursor to the **other end** of the selection,
    /// swapping head and anchor so the side you started from becomes the movable
    /// one — at the primary and every secondary alike. The selection spans are
    /// unchanged; only which end moves. Expressed as a round-trip through the
    /// shared [`Selections`] seam (flip every range). A no-op outside a visual mode.
    pub(crate) fn visual_swap_ends(&mut self) {
        if !self.mode.is_visual() {
            return;
        }
        let mut sel = self.selections();
        for r in &mut sel.ranges {
            *r = r.flipped();
        }
        self.set_selections(&sel);
        self.clamp_cursor();
    }

    /// Drop every per-cursor visual anchor — when a visual mode ends (its operator
    /// or `<Esc>`), collapsing each selection back to its cursor head.
    pub(crate) fn clear_anchor_marks(&mut self) {
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .clear(ANCHOR_NS, None);
    }

    /// Each secondary cursor as its `(anchor, head)` [`Cursor`] pair, for
    /// rendering the per-cursor visual selections (the primary's lives in the
    /// editor's `visual_anchor`/`cursor`). The head is the [`CURSOR_NS`] mark, the
    /// anchor its paired [`ANCHOR_NS`] mark — falling back to the head (a 1-wide
    /// selection) if somehow unpaired. Only meaningful inside a visual mode.
    pub(crate) fn secondary_selections(&self) -> Vec<(Cursor, Cursor)> {
        let mut heads: Vec<(usize, usize)> = self
            .cursor_marks()
            .map(|m| {
                let anchor = self.anchor_mark_pos(m.id).unwrap_or(m.start);
                (anchor, m.start)
            })
            .collect();
        heads.sort_unstable();
        heads
            .into_iter()
            .map(|(a, h)| (self.cursor_at_byte(a), self.cursor_at_byte(h)))
            .collect()
    }

    /// Byte offsets of every secondary cursor in the current buffer, ascending.
    pub(crate) fn secondary_cursor_bytes(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.cursor_marks().map(|m| m.start).collect();
        v.sort_unstable();
        v
    }

    /// Every secondary cursor's [`CURSOR_NS`] extmark in the current buffer, in
    /// store order. The shared read prelude behind the multi-cursor queries (heads,
    /// byte positions, selection pairs) — each chains its own `map`/`filter`.
    fn cursor_marks(&self) -> impl Iterator<Item = &crate::extmark::Extmark> {
        self.buffer()
            .extmarks
            .iter_with_ns()
            .filter(|(ns, _)| *ns == CURSOR_NS)
            .map(|(_, m)| m)
    }

    /// Whether any secondary cursor is active on the current buffer.
    pub(crate) fn has_secondary_cursors(&self) -> bool {
        self.cursor_marks().next().is_some()
    }

    /// Whether secondary cursors should take part in the current command. True
    /// once cursors are placed *and* placement is over: while still in
    /// [`Mode::MultiCursor`] only the primary moves (you're navigating to drop
    /// more), so motions/edits stay single-cursor until `<Esc>` finishes placement.
    pub(crate) fn cursors_active(&self) -> bool {
        self.has_secondary_cursors() && self.mode != Mode::MultiCursor
    }

    /// Drop every secondary cursor, collapsing back to the single primary —
    /// Helix's `<Esc>`.
    pub(crate) fn clear_secondary_cursors(&mut self) {
        self.clear_placement_history();
        if !self.has_secondary_cursors() {
            return;
        }
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .clear(CURSOR_NS, None);
        // The per-cursor visual anchors belong to those heads — drop them too.
        self.clear_anchor_marks();
    }

    /// Restore a window's stashed secondary multi-cursor set onto the current
    /// buffer — the counterpart of stashing into `Window::saved_cursors` on
    /// focus-out. Any leftover live marks are cleared first: a focus change that
    /// did not stash the outgoing window (a window close, a tab switch landing on a
    /// shared buffer) can leave stale [`CURSOR_NS`] marks behind, and restoring on
    /// top of them would duplicate or leak cursors across the windows.
    pub(crate) fn restore_secondary_cursors(&mut self, positions: Vec<usize>) {
        self.clear_secondary_cursors();
        for at in positions {
            self.set_cursor_mark(None, at);
        }
    }

    /// Discard the placement-mode undo/redo history — at the start and end of a
    /// placement session, since it tracks only the live placing of cursors, never
    /// the document.
    fn clear_placement_history(&mut self) {
        self.placement_undo.clear();
        self.placement_redo.clear();
    }

    /// Snapshot the live placed-cursor set (primary + secondaries) for
    /// placement-mode undo/redo.
    fn placement_snapshot(&self) -> PlacementSnapshot {
        PlacementSnapshot {
            primary: self.cursor,
            secondaries: self.secondary_cursor_bytes(),
        }
    }

    /// Restore a [`PlacementSnapshot`]: replace the secondary-cursor set wholesale
    /// and move the primary back to where it sat in that state.
    fn restore_placement(&mut self, snap: &PlacementSnapshot) {
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .clear(CURSOR_NS, None);
        for &at in &snap.secondaries {
            self.set_cursor_mark(None, at);
        }
        self.cursor = snap.primary;
        self.clamp_cursor();
    }

    /// Record the placed-cursor set before a placement command mutates it, so a
    /// later `u` in [`Mode::MultiCursor`] can step back to it. Beginning a new
    /// placement invalidates any redo future. Call once per placement command — a
    /// counted `{count}c{motion}` records before the whole batch, so it undoes as
    /// one step.
    pub(crate) fn record_placement_undo(&mut self) {
        let snap = self.placement_snapshot();
        self.placement_undo.push(snap);
        self.placement_redo.clear();
    }

    /// `u` in [`Mode::MultiCursor`]: undo the last cursor *placement* — a `c`,
    /// `{count}c{motion}`, or `cc` drop steps back as one — restoring the prior
    /// cursor set rather than walking the text undo tree.
    pub(crate) fn undo_placement(&mut self) {
        let Some(prev) = self.placement_undo.pop() else {
            self.echo("Already at oldest cursor placement");
            return;
        };
        let cur = self.placement_snapshot();
        self.placement_redo.push(cur);
        self.restore_placement(&prev);
    }

    /// `<C-r>` in [`Mode::MultiCursor`]: redo a placement undone with `u`.
    pub(crate) fn redo_placement(&mut self) {
        let Some(next) = self.placement_redo.pop() else {
            self.echo("Already at newest cursor placement");
            return;
        };
        let cur = self.placement_snapshot();
        self.placement_undo.push(cur);
        self.restore_placement(&next);
    }

    /// Enter multi-cursor *placement* mode (`<A-c>`) and drop a cursor at the
    /// current position. From within the mode it just drops another. Motions then
    /// move only the primary, so you navigate and place; `<Esc>` ends placement.
    pub(crate) fn add_cursor(&mut self) {
        // Entering fresh starts a new placement session — its undo history begins
        // empty (the baseline recorded just below is "no cursors yet").
        if self.mode != Mode::MultiCursor {
            self.clear_placement_history();
            self.mode = Mode::MultiCursor;
        }
        self.record_placement_undo();
        self.place_cursor_here();
    }

    /// Toggle a secondary cursor at the primary's current position (the placement
    /// command `c` in [`Mode::MultiCursor`]): drop one if the cell is empty, or
    /// clear it if a cursor is already there. So `c` twice on one cell leaves it
    /// bare — never a duplicate.
    pub(crate) fn place_cursor_here(&mut self) {
        let at = self.cursor_char();
        let existing: Vec<u64> = self
            .cursor_marks()
            .filter(|m| m.start == at)
            .map(|m| m.id)
            .collect();
        let bid = self.cur_buffer();
        if existing.is_empty() {
            self.set_cursor_mark(None, at);
        } else {
            for id in existing {
                self.buffers.get_mut(bid).buffer.extmarks.del(CURSOR_NS, id);
            }
        }
    }

    /// Drop a secondary cursor at the primary's position **only if one isn't there
    /// already** — the non-toggling form of [`place_cursor_here`] used by the
    /// counted placements (`{count}c{motion}`, `{count}cc`). Those only ever *add*
    /// cursors, so a step that lands on an existing cursor (or on the entry cursor
    /// sitting under the primary) leaves it in place rather than toggling it off.
    pub(crate) fn ensure_cursor_here(&mut self) {
        let at = self.cursor_char();
        let occupied = self.cursor_marks().any(|m| m.start == at);
        if !occupied {
            self.set_cursor_mark(None, at);
        }
    }

    /// Leave placement mode (`<Esc>` in [`Mode::MultiCursor`]) back to Normal,
    /// keeping the placed cursors. The primary represents its own position, so a
    /// placed mark coinciding with it is dropped (else that spot would be operated
    /// on twice once motions/edits start applying to every cursor).
    ///
    /// Once back in Normal the primary becomes an ordinary edit cursor, so it must
    /// land *on* a placed cursor. If the primary merely **navigated** to a spot
    /// where no cursor was placed (motions move only the primary while placing),
    /// leaving it there would silently add a phantom edit cursor at that parked
    /// position. So when the primary sits off every placed cursor, snap it onto the
    /// nearest one (ties → topmost) before the dedup below — the placed set is then
    /// exactly the cursors the user dropped, no more.
    pub(crate) fn finish_multicursor(&mut self) {
        let marks: Vec<(u64, usize)> = self.cursor_marks().map(|m| (m.id, m.start)).collect();
        let at = self.cursor_char();
        if !marks.is_empty() && !marks.iter().any(|&(_, s)| s == at) {
            let target = marks
                .iter()
                .map(|&(_, s)| s)
                .min_by_key(|&s| (at.abs_diff(s), s))
                .expect("marks is non-empty");
            self.cursor = self.cursor_at_byte(target);
            self.clamp_cursor();
        }

        let at = self.cursor_char();
        let dups: Vec<u64> = marks
            .iter()
            .filter(|&&(_, s)| s == at)
            .map(|&(id, _)| id)
            .collect();
        let bid = self.cur_buffer();
        for id in dups {
            self.buffers.get_mut(bid).buffer.extmarks.del(CURSOR_NS, id);
        }
        // Placement is over; its undo history was only reachable while in the mode.
        self.clear_placement_history();
        self.mode = Mode::Normal;
    }

    /// Run `f` once at the primary cursor and once at each secondary cursor, with
    /// [`Editor::cursor`] placed at each in turn — so the ordinary single-cursor
    /// helpers (a motion, an operator, an insert keystroke) operate at every
    /// cursor.
    ///
    /// While `f` runs, the primary is parked in a temporary extmark too, so *all*
    /// cursors shift uniformly through the buffer's edit choke point. Cursors are
    /// visited highest-byte-first: an edit at one cursor only shifts anchors at or
    /// after it, so a not-yet-visited (lower) cursor's position stays valid until
    /// we reach it. Final positions are read back from the (now-shifted) marks, so
    /// edits below an already-visited cursor still move it correctly.
    ///
    /// This primitive is undo-neutral — it neither snapshots nor coalesces. A
    /// *movement* `f` needs no undo; an *editing* `f` must be wrapped in
    /// [`Editor::edit_each_cursor`], which opens one undo group around the sweep.
    /// With no secondary cursors — or while still *placing* them in
    /// [`Mode::MultiCursor`], where only the primary moves — this is just `f(self)`.
    pub(crate) fn for_each_cursor(&mut self, f: impl Fn(&mut Editor)) {
        if !self.cursors_active() {
            f(self);
            return;
        }

        // In a selection-carrying mode (visual, or a Helix mode — the
        // `shows_selection` seam) each cursor carries its own anchor; restore it
        // into `visual_anchor` before each `f` so an operator brackets that
        // cursor's own selection. Captured up front because an editing `f`
        // (visual/Helix `c`) flips the mode out of it mid-sweep.
        let visual = self.mode.shows_selection();

        // Park the primary as a head mark too (so it rides the same auto-shift and
        // is handled like any other cursor), plus its anchor when in visual.
        let primary = self.set_cursor_mark(None, self.cursor_char());
        if visual {
            let ab = self.anchor_byte(self.visual_anchor);
            self.set_anchor_mark(primary, ab);
        }

        // Visit highest byte first (see the method doc). Read ids now; positions
        // are re-read live each step, after prior edits have shifted them.
        let mut ids: Vec<(u64, usize)> = self.cursor_marks().map(|m| (m.id, m.start)).collect();
        ids.sort_unstable_by_key(|&(_, start)| std::cmp::Reverse(start));

        for (id, _) in ids {
            let Some(pos) = self.cursor_mark_pos(id) else {
                continue;
            };
            self.set_cursor_char_insert(pos);
            if visual {
                if let Some(ab) = self.anchor_mark_pos(id) {
                    self.visual_anchor = self.cursor_at_byte(ab);
                }
            }
            f(self);
            // Re-anchor where `f` left the cursor; later (lower) edits shift it.
            let landed = self.cursor_char();
            self.set_cursor_mark(Some(id), landed);
        }

        // Restore the primary (head, and its anchor in visual) from the now-shifted
        // marks and retire them — the per-cursor loop above left `visual_anchor`
        // pointing at the last cursor it visited.
        if let Some(pos) = self.cursor_mark_pos(primary) {
            self.set_cursor_char_insert(pos);
        }
        if visual {
            if let Some(ab) = self.anchor_mark_pos(primary) {
                self.visual_anchor = self.cursor_at_byte(ab);
            }
        }
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .del(CURSOR_NS, primary);
        if visual {
            self.buffers
                .get_mut(bid)
                .buffer
                .extmarks
                .del(ANCHOR_NS, primary);
        }

        self.clamp_cursor();
        // A motion/edit may have driven cursors onto the same cell; collapse them.
        self.merge_overlapping_cursors();
    }

    /// Collapse secondary cursors that now sit on the same byte — as each other, or
    /// as the primary — into a single cursor. Runs after every multi-cursor
    /// sweep so a motion that converges several cursors (`0`, `gg`) doesn't leave
    /// a pile of coincident marks that would each act on the same spot.
    fn merge_overlapping_cursors(&mut self) {
        let mut seen = std::collections::HashSet::new();
        // A secondary coinciding with the primary is redundant — the primary owns
        // that cell — so seed `seen` with it.
        seen.insert(self.cursor_char());
        let dups: Vec<u64> = self
            .cursor_marks()
            .filter_map(|m| (!seen.insert(m.start)).then_some(m.id))
            .collect();
        let bid = self.cur_buffer();
        for id in dups {
            self.buffers.get_mut(bid).buffer.extmarks.del(CURSOR_NS, id);
            // Drop the merged head's paired visual anchor so no orphan lingers.
            self.buffers.get_mut(bid).buffer.extmarks.del(ANCHOR_NS, id);
        }
    }

    /// [`Editor::for_each_cursor`] for an *editing* `f`, wrapped in a single undo
    /// group so a multi-cursor `dw`/`x`/`cw` undoes in one step. The pre-edit
    /// snapshot is taken once and `snapshot_taken` coalesces the per-cursor edits;
    /// an `f` that enters Insert (`cw`/`s`) keeps the flag set so the following
    /// insert session stays in the same group, exactly as single-cursor `cw` does.
    pub(crate) fn edit_each_cursor(&mut self, f: impl Fn(&mut Editor)) {
        // A live terminal buffer is read-only (its lines mirror the child's screen);
        // refuse the edit rather than corrupt the mirror. See [`Editor::modifiable`].
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        if !self.cursors_active() {
            f(self);
            return;
        }
        let resume = self.snapshot_taken;
        if !resume {
            self.push_undo();
            self.snapshot_taken = true;
        }
        // Collect each cursor's yank/delete slice so a following multi-cursor paste
        // can give every cursor back its *own* text. A non-yanking edit (`~`, `J`)
        // collects nothing and leaves the previous per-cursor set intact.
        self.cursor_register_collect = Some(Vec::new());
        self.for_each_cursor(f);
        if let Some(mut collected) = self.cursor_register_collect.take() {
            if !collected.is_empty() {
                collected.sort_by_key(|(at, _)| *at);
                self.cursor_registers = collected.into_iter().map(|(_, cell)| cell).collect();
            }
        }
        // Keep the insert session's snapshot guard if `f` opened one (`cw`/`s`).
        self.snapshot_taken = resume || self.mode.is_insert();
    }
}
