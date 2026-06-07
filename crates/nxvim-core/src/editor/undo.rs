//! Undo/redo: buffer snapshotting and state restoration.

use super::*;

impl Editor {
    /// Push an undo snapshot for buffer `id` (the buffer-addressed [`push_undo`],
    /// minting a fresh sequence number) so a subsequent edit to it is a distinct,
    /// independently-undoable step. Unlike [`Editor::push_undo`] it does not
    /// consult `snapshot_taken` (that flag tracks the *current* buffer's insert
    /// session); a workspace edit is a one-shot, normal-mode mutation per buffer.
    /// The snapshot's cursor is the live cursor when `id` is current, else the
    /// buffer's saved cursor.
    pub(crate) fn push_undo_for(&mut self, id: BufferId) {
        let (text, cursor, seq, extmarks) = {
            let ob = self.buffers.get(id);
            let cursor = if id == self.cur_buffer() {
                self.cursor
            } else {
                ob.saved_cursor
            };
            (
                ob.buffer.text.clone(),
                cursor,
                ob.cur_seq,
                ob.buffer.extmarks.clone(),
            )
        };
        let ob = self.buffers.get_mut(id);
        ob.undo_stack.push(Snapshot {
            text,
            cursor,
            seq,
            extmarks,
        });
        ob.redo_stack.clear();
        ob.cur_seq = ob.next_seq;
        ob.next_seq += 1;
    }

    /// Capture the current text + cursor + sequence number as an undo/redo
    /// snapshot.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.buffer().text.clone(),
            cursor: self.cursor,
            seq: self.buffers.get(self.cur_buffer()).cur_seq,
            extmarks: self.buffer().extmarks.clone(),
        }
    }

    pub(crate) fn push_undo(&mut self) {
        if self.snapshot_taken {
            return;
        }
        let snap = self.snapshot();
        let ob = self.cur_mut();
        ob.undo_stack.push(snap);
        ob.redo_stack.clear();
        // The edit about to happen produces a brand-new state — mint its id so
        // undo can later recognise (and redo can return to) this exact point.
        ob.cur_seq = ob.next_seq;
        ob.next_seq += 1;
    }

    pub(crate) fn undo(&mut self) {
        self.restore(true);
    }

    pub(crate) fn redo(&mut self) {
        self.restore(false);
    }

    /// Shared body of `undo`/`redo`: pop a snapshot off one history stack, push the
    /// current state onto the other, and restore text + cursor. `from_undo` picks
    /// the direction (undo: pop undo / push redo; redo: the reverse).
    fn restore(&mut self, from_undo: bool) {
        let popped = if from_undo {
            self.cur_mut().undo_stack.pop()
        } else {
            self.cur_mut().redo_stack.pop()
        };
        let Some(snap) = popped else {
            self.echo(if from_undo {
                "Already at oldest change"
            } else {
                "Already at newest change"
            });
            return;
        };
        let current = self.snapshot();
        let ob = self.cur_mut();
        if from_undo {
            ob.redo_stack.push(current);
        } else {
            ob.undo_stack.push(current);
        }
        ob.buffer.text = snap.text;
        ob.cur_seq = snap.seq;
        // We're back on a previously-seen state: it's clean only if it's the one
        // last written to disk. (`mark_resync` below sets `modified = true`, so
        // decide this first and re-assert it afterwards.)
        let clean = ob.saved_seq == Some(ob.cur_seq);
        self.cursor = snap.cursor;
        self.buffer_mut().mark_resync();
        // `mark_resync` clears extmarks (correct for a destructive reload); undo
        // is not a reload, so restore the marks captured with this history point —
        // they ride back to their positions in the state we're returning to.
        self.buffer_mut().extmarks = snap.extmarks;
        self.buffer_mut().modified = !clean;
        self.clamp_cursor();
    }
}
