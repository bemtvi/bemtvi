//! Branching undo/redo: the per-buffer [`UndoTree`] and the editor operations
//! that navigate it.
//!
//! The model is full-snapshot-per-node (cheap thanks to `ropey`'s persistent
//! rope — see [`Snapshot`]), organized as a tree rather than two stacks. Undoing
//! then making a new edit forks a branch instead of discarding the old future,
//! so every state stays reachable: a plain `u`/`<C-r>` walks parent/newest-child
//! as before, while `:undo {N}` jumps to any seq across branches.
//!
//! A pending edit is committed to a node *lazily*, at the moment focus leaves the
//! state (the next change-group boundary, or an undo/redo/`:undo`). That is the
//! same instant the old two-stack model snapshotted, so cursor/text/seq land
//! identically; the tree just keeps the branches the stacks threw away.

use super::*;

impl UndoTree {
    /// A fresh tree whose only node is the original buffer state at seq 0.
    pub(crate) fn new(buffer: &Buffer) -> Self {
        let root = UndoNode {
            seq: 0,
            parent: None,
            children: Vec::new(),
            time: 0,
            save: None,
            snap: Snapshot {
                text: buffer.text.clone(),
                cursor: Cursor::default(),
                extmarks: buffer.extmarks.clone(),
                marks: buffer.marks.clone(),
                changelist: (buffer.changelist.clone(), buffer.changelistidx),
                cursor_window: None,
            },
        };
        UndoTree {
            nodes: vec![root],
            cur: 0,
            next_seq: 1,
            dirty: false,
            dirty_since: 0,
            save_last: 0,
        }
    }

    /// Mark that a change group has begun (the live buffer will diverge from
    /// `cur`), remembering when, for the virtual-node timestamp. Idempotent within
    /// a group — a re-mark keeps the original start time.
    fn mark_dirty(&mut self, now: i64) {
        if !self.dirty {
            self.dirty = true;
            self.dirty_since = now;
        }
    }

    /// Overwrite the current node's snapshot cursor + multi-cursor marks with the
    /// live ones, so undoing back to this node restores them — see
    /// [`Editor::push_undo`]. Only the [`CURSOR_NS`] marks are replaced; the rest of
    /// the snapshot (text, `a`–`z` marks, other extmarks) is untouched.
    fn set_cur_snapshot_cursors(
        &mut self,
        primary: Cursor,
        positions: &[usize],
        window: Option<WindowId>,
    ) {
        let snap = &mut self.nodes[self.cur].snap;
        snap.cursor = primary;
        snap.cursor_window = window;
        snap.extmarks.clear(crate::extmark::CURSOR_NS, None);
        for &at in positions {
            snap.extmarks
                .set(crate::extmark::CURSOR_NS, None, at, None, None, 0);
        }
    }

    /// Seq of the current state (`b_u_seq_cur`).
    pub(crate) fn cur_seq(&self) -> u64 {
        self.nodes[self.cur].seq
    }

    /// Highest seq minted so far (`b_u_seq_last`).
    fn seq_last(&self) -> u64 {
        self.next_seq - 1
    }

    /// The node holding state `seq`, if any.
    fn node_of_seq(&self, seq: u64) -> Option<NodeIdx> {
        self.nodes.iter().position(|n| n.seq == seq)
    }

    /// Materialize `snap` (the live, pending state) as a new child of `cur` and
    /// move onto it, stamped with monotonic time `now`. Caller guarantees
    /// `dirty` — the live buffer really has diverged from `cur`.
    fn commit(&mut self, snap: Snapshot, now: i64) {
        let seq = self.next_seq;
        self.next_seq += 1;
        let idx = self.nodes.len();
        self.nodes.push(UndoNode {
            seq,
            parent: Some(self.cur),
            children: Vec::new(),
            time: now,
            save: None,
            snap,
        });
        self.nodes[self.cur].children.push(idx);
        self.cur = idx;
        self.dirty = false;
    }

    /// Project the tree into the shape `vim.fn.undotree()` returns. Read-only: an
    /// uncommitted live edit (`dirty`) is shown as a *virtual* current node with
    /// the seq it will receive on commit, rather than committing here — committing
    /// during a read would freeze the snapshot before later same-state mutations
    /// (extmarks, marks added outside `push_undo`) and lose them on undo.
    ///
    /// `entries` is a spine (oldest child of the root first) where each entry's
    /// `alt` carries the sibling branches forking at the same point — the
    /// recursive form neovim's `undotree()` emits, from which the visualizer's
    /// parser reconstructs parent links. Order-tolerant: the consumer re-sorts
    /// children by seq, so only the parent relationships must be faithful.
    fn view(&self) -> UndoTreeView {
        // The pending edit forks off `cur` and will take `next_seq` on commit.
        let pending = self
            .dirty
            .then_some((self.cur, self.next_seq, self.dirty_since));
        let (seq_cur, seq_last, time_cur, save_cur) = match pending {
            Some((_, seq, time)) => (seq, seq, time, 0),
            None => {
                let c = &self.nodes[self.cur];
                (c.seq, self.seq_last(), c.time, c.save.unwrap_or(0))
            }
        };
        UndoTreeView {
            seq_last,
            seq_cur,
            save_last: self.save_last,
            save_cur,
            time_cur,
            entries: self.encode_children(0, pending),
        }
    }

    /// Encode the children of `parent` as an entry chain (recursively), weaving in
    /// the virtual `pending` leaf when it forks at `parent`.
    fn encode_children(&self, parent: NodeIdx, pending: Pending) -> Vec<UndoEntry> {
        let mut sibs: Vec<SibItem> = self.nodes[parent]
            .children
            .iter()
            .map(|&c| SibItem::Real(c))
            .collect();
        if let Some((p, seq, time)) = pending {
            if p == parent {
                sibs.push(SibItem::Pending { seq, time });
            }
        }
        self.encode_sibs(&sibs, pending)
    }

    /// Encode a sibling list: the head becomes the first entry (its own descendants
    /// continue the spine after it), and the remaining siblings nest into the head
    /// entry's `alt`. A `Pending` sibling is a childless leaf.
    fn encode_sibs(&self, sibs: &[SibItem], pending: Pending) -> Vec<UndoEntry> {
        let Some((head, rest)) = sibs.split_first() else {
            return Vec::new();
        };
        let alt = self.encode_sibs(rest, pending);
        let (entry, mut tail) = match *head {
            SibItem::Real(c) => {
                let n = &self.nodes[c];
                let entry = UndoEntry {
                    seq: n.seq,
                    time: n.time,
                    save: n.save,
                    alt,
                };
                (entry, self.encode_children(c, pending))
            }
            SibItem::Pending { seq, time } => (
                UndoEntry {
                    seq,
                    time,
                    save: None,
                    alt,
                },
                Vec::new(),
            ),
        };
        let mut out = vec![entry];
        out.append(&mut tail);
        out
    }
}

/// The virtual current node `vim.fn.undotree()` shows for an uncommitted live
/// edit: `(parent, seq, time)` — the node it forks off, the seq it will take on
/// commit, and its (stable) start time. `None` when nothing is pending.
type Pending = Option<(NodeIdx, u64, i64)>;

/// One entry in a sibling list during projection: an existing node or the virtual
/// pending leaf.
enum SibItem {
    Real(NodeIdx),
    Pending { seq: u64, time: i64 },
}

/// The `vim.fn.undotree()` result for one buffer — neovim's dict, minus the
/// fields the visualizer ignores. Built by the core; the server serializes it.
pub struct UndoTreeView {
    pub seq_last: u64,
    pub seq_cur: u64,
    pub save_last: u64,
    pub save_cur: u64,
    pub time_cur: i64,
    pub entries: Vec<UndoEntry>,
}

/// One node in a [`UndoTreeView`]: a state plus the sibling branches forking
/// below the same parent (`alt`), recursively. Matches a neovim `undotree()`
/// entry (`seq`, `time`, optional `save`, optional `alt`).
pub struct UndoEntry {
    pub seq: u64,
    pub time: i64,
    pub save: Option<u64>,
    pub alt: Vec<UndoEntry>,
}

impl Editor {
    /// Snapshot buffer `id`'s live state. The cursor is the live cursor when `id`
    /// is current, else the buffer's saved cursor.
    fn snapshot_of(&self, id: BufferId) -> Snapshot {
        let ob = self.buffers.get(id);
        let cursor = if id == self.cur_buffer() {
            self.cursor
        } else {
            ob.saved_cursor
        };
        // Only the current (focused) buffer carries a live secondary-cursor set,
        // owned by the focused window; a background buffer's snapshot has no owning
        // window (and no `CURSOR_NS` marks).
        let cursor_window = (id == self.cur_buffer()).then_some(self.windows.current);
        Snapshot {
            text: ob.buffer.text.clone(),
            cursor,
            extmarks: ob.buffer.extmarks.clone(),
            marks: ob.buffer.marks.clone(),
            changelist: (ob.buffer.changelist.clone(), ob.buffer.changelistidx),
            cursor_window,
        }
    }

    /// Commit any pending edit on buffer `id` into its tree as a new node. A no-op
    /// when nothing is pending. Called at every change-group boundary and before
    /// any navigation, so the tree node always exists before we leave a state.
    pub(crate) fn commit_undo(&mut self, id: BufferId) {
        if !self.buffers.get(id).undo.dirty {
            return;
        }
        let snap = self.snapshot_of(id);
        let now = self.now_mono;
        self.buffers.get_mut(id).undo.commit(snap, now);
    }

    /// Record that buffer `id` was just written to disk: commit any pending edit
    /// (so the written state is a real node), bump the save counter, and stamp the
    /// current node as that save. Drives `vim.fn.undotree()`'s `save`/`save_last`
    /// and the saved-state clean check.
    pub(crate) fn mark_undo_saved(&mut self, id: BufferId) {
        self.commit_undo(id);
        let ob = self.buffers.get_mut(id);
        ob.undo.save_last += 1;
        let nr = ob.undo.save_last;
        let cur = ob.undo.cur;
        ob.undo.nodes[cur].save = Some(nr);
        ob.saved_seq = Some(ob.undo.cur_seq());
    }

    /// The `vim.fn.undotree()` projection for buffer `id`. Read-only — a pending
    /// uncommitted edit is shown as a virtual current node (see [`UndoTree::view`])
    /// rather than committed here, so it stays visible without freezing the
    /// snapshot before later same-state extmark/mark mutations.
    pub fn undotree_of(&self, id: BufferId) -> UndoTreeView {
        self.buffers.get(id).undo.view()
    }

    /// A cheap fingerprint of buffer `id`'s undo projection — `(next_seq, cur,
    /// save_last, dirty)`. The server caches it to skip rebuilding an unchanged
    /// tree's mirror on every buffer-mirror push (the hot input path). These four
    /// fully determine the projection: `next_seq` is the pending node's seq and
    /// bumps on commit, `cur` moves on undo/redo, `save_last` on write, `dirty` on
    /// edit-start (the pending node's timestamp is fixed once dirty, so it adds no
    /// drift).
    pub fn undo_version(&self, id: BufferId) -> (u64, usize, u64, bool) {
        let t = &self.buffers.get(id).undo;
        (t.next_seq, t.cur, t.save_last, t.dirty)
    }

    /// Begin an independently-undoable change to buffer `id` (a workspace edit:
    /// LSP rename / code action). Finalizes any prior pending edit first, then
    /// marks the upcoming mutation pending. Unlike [`Editor::push_undo`] it does
    /// not consult `snapshot_taken` — a workspace edit is a one-shot mutation.
    pub(crate) fn push_undo_for(&mut self, id: BufferId) {
        self.commit_undo(id);
        let now = self.now_mono;
        self.buffers.get_mut(id).undo.mark_dirty(now);
    }

    /// Begin a new change group on the current buffer: finalize the previous
    /// group as a node, then mark the upcoming edit pending. The `snapshot_taken`
    /// guard coalesces a multi-edit group (an insert session, a `:g` batch) into a
    /// single node — every edit between a set/clear of the flag shares one commit.
    pub(crate) fn push_undo(&mut self) {
        if self.snapshot_taken {
            return;
        }
        let id = self.cur_buffer();
        self.commit_undo(id);
        // Bake the live cursor set into the node we'll undo back to, so undoing the
        // edit we're about to make restores the cursor(s) to where they are *now* —
        // not where this edit will shift them. The node `cur` may have been
        // committed earlier (we're starting a fresh edit from a state we navigated
        // to or undid back to), so its frozen snapshot cursor would otherwise be
        // stale: undo would jump to the root's top-of-file default, or to a stale
        // multi-cursor set baked in at a branch point.
        self.refresh_undo_cursor_marks(id);
        let now = self.now_mono;
        self.cur_mut().undo.mark_dirty(now);
    }

    /// Update the current undo node's snapshot so its cursor and multi-cursor marks
    /// match the live state — see [`push_undo`](Self::push_undo). Runs for the
    /// single-cursor case too: the primary is re-synced (so undo lands at the
    /// change, not the node's stale committed cursor) and any stale [`CURSOR_NS`]
    /// marks left in the snapshot are cleared (so a single-cursor undo never
    /// resurrects a multi-cursor set the user has since collapsed).
    fn refresh_undo_cursor_marks(&mut self, id: BufferId) {
        let primary = self.cursor;
        let positions = self.secondary_cursor_bytes();
        let window = Some(self.windows.current);
        self.buffers
            .get_mut(id)
            .undo
            .set_cur_snapshot_cursors(primary, &positions, window);
    }

    pub(crate) fn undo(&mut self) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let Some(parent) = tree.nodes[tree.cur].parent else {
            self.echo("Already at oldest change");
            return;
        };
        let snap = tree.nodes[parent].snap.clone();
        let seq = tree.nodes[parent].seq;
        self.buffers.get_mut(id).undo.cur = parent;
        self.restore_snapshot(snap, seq);
    }

    pub(crate) fn redo(&mut self) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        // Plain redo follows the newest branch — the last-created child.
        let Some(child) = tree.nodes[tree.cur].children.last().copied() else {
            self.echo("Already at newest change");
            return;
        };
        let snap = tree.nodes[child].snap.clone();
        let seq = tree.nodes[child].seq;
        self.buffers.get_mut(id).undo.cur = child;
        self.restore_snapshot(snap, seq);
    }

    /// `:undo {N}` — jump the current buffer to the state with seq `N`, anywhere
    /// in the tree (including an abandoned branch a stack-based undo could never
    /// reach). `:undo 0` returns to the original loaded text.
    pub(crate) fn undo_to_seq(&mut self, target: u64) {
        let id = self.cur_buffer();
        self.commit_undo(id);
        let tree = &self.buffers.get(id).undo;
        let Some(node) = tree.node_of_seq(target) else {
            self.echo(format!("E830: Undo number {target} not found"));
            return;
        };
        if node == tree.cur {
            return;
        }
        let snap = tree.nodes[node].snap.clone();
        self.buffers.get_mut(id).undo.cur = node;
        self.restore_snapshot(snap, target);
    }

    /// Restore the current buffer to `snap` (the state numbered `seq`): swap in the
    /// text, cursor, extmarks and marks, and recompute `modified`.
    fn restore_snapshot(&mut self, mut snap: Snapshot, seq: u64) {
        // Decoration-provider marks (`nx.decor`) are ephemeral viewport state —
        // republished off-tick on every viewport/edit change — not document history, so
        // undo must not swap them out. Move the LIVE marks for each ephemeral namespace
        // into the snapshot store we're about to install (below), *before* `mark_resync`
        // clears the live store. Without this, undoing to a state captured before a
        // provider first ran — notably the root node, snapshotted at buffer load — wipes
        // the live marks for one frame until the re-dispatch republishes them: a visible
        // flash that the user only sees on the first undo back to that root state.
        let ephemeral = self.ephemeral_extmark_namespaces();
        if !ephemeral.is_empty() {
            let live = &mut self.buffer_mut().extmarks;
            for ns in ephemeral {
                live.move_namespace_into(ns, &mut snap.extmarks);
            }
        }
        let ob = self.cur_mut();
        ob.buffer.text = snap.text;
        // We're back on a previously-seen state: it's clean only if it's the one
        // last written to disk. (`mark_resync` below sets `modified = true`, so
        // decide this first and re-assert it afterwards.)
        let clean = ob.saved_seq == Some(seq);
        self.cursor = snap.cursor;
        self.buffer_mut().mark_resync();
        // `mark_resync` clears extmarks and `a`–`z` marks (correct for a destructive
        // reload); undo is not a reload, so restore all as captured with this history
        // point — extmarks (including the multi-cursor marks), `a`–`z` marks, and the
        // cursor ride back to where they were in the state we return to.
        self.buffer_mut().extmarks = snap.extmarks;
        self.buffer_mut().marks = snap.marks;
        let (changelist, changelistidx) = snap.changelist;
        self.buffer_mut().changelist = changelist;
        self.buffer_mut().changelistidx = changelistidx;
        self.buffer_mut().modified = !clean;
        // The secondary multi-cursor set is window-local, but the undo tree is
        // shared by every window onto this buffer. Keep the baked `CURSOR_NS`/
        // `ANCHOR_NS` marks only when *this* window baked them; otherwise drop them
        // so undoing another window's multi-cursor edit reverts its text without
        // resurrecting its cursors here (the primary still rides back, vim-style).
        if snap.cursor_window != Some(self.windows.current) {
            let buf = self.buffer_mut();
            buf.extmarks.clear(crate::extmark::CURSOR_NS, None);
            buf.extmarks.clear(crate::extmark::ANCHOR_NS, None);
        }
        self.clamp_cursor();
    }
}
