//! Marks — positions the user names with `m{x}` and returns to with `` `{x} ``
//! (exact byte position) or `'{x}` (first non-blank of the mark's line).
//!
//! Phase 1–3: the buffer-local lowercase marks `a`–`z`, stored as `(line, col)`
//! on the text [`Buffer`](crate::buffer::Buffer) so they follow the buffer across
//! switches *and* ride the single edit choke point ([`crate::buffer::Buffer`]'s
//! `record`) that keeps them correct as text is inserted/deleted — exactly the
//! arrangement `extmarks` use; plus the global file marks `A`–`Z`, each naming a
//! `(buffer, cursor)` on the [`Editor`] so a jump can cross buffers. The automatic
//! specials (`` ` `` / `'` / `.` / `[` / …) land in later phases — see
//! `docs/plans/2026-06-07-marks.md`. The grammar ([`crate::editor::command`])
//! rejects any name this module doesn't yet accept as a loud dead-end, never a
//! silent no-op, matching the register surface.

use super::*;

/// Whether `c` names a mark nxvim can set and jump to today. Phases 1–3 support
/// the buffer-local lowercase marks `a`–`z` and the global file marks `A`–`Z`;
/// every other name (the automatic `` ` `` / `.` / `[` / `<` … specials) is
/// rejected at the grammar level — `m{X}` / `` `{X} `` is a dead-end that runs
/// nothing — until its phase lands.
pub(crate) fn is_mark_name(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// Where a mark points: which buffer and the cursor within it. A buffer-local
/// lowercase mark resolves into the *current* buffer; a global `A`–`Z` mark
/// resolves into the buffer it was set in, which may differ from the current one
/// (the cross-buffer jump in [`Editor::execute`] keys off exactly that).
pub(crate) struct MarkLocation {
    pub(crate) buf: BufferId,
    pub(crate) cursor: Cursor,
}

impl Editor {
    /// Set mark `name` at the current cursor position (`m{a-zA-Z}`). A lowercase
    /// mark is stored on the buffer as `(line, col)` and tracks edits from there —
    /// shifting as text is inserted/deleted above or earlier in its line, dropped
    /// when its line is deleted (see [`crate::buffer`]'s `shift_marks`). An
    /// uppercase mark is a *global* file mark: it records `(current buffer,
    /// cursor)` on the editor, so jumping to it later can cross back to that
    /// buffer.
    pub(crate) fn set_mark(&mut self, name: char) {
        let cursor = self.cursor;
        if name.is_ascii_uppercase() {
            self.global_marks.insert(name, (self.cur_buffer(), cursor));
        } else {
            self.buffer_mut()
                .marks
                .insert(name, (cursor.line, cursor.col));
        }
    }

    /// The full location of mark `name` — its buffer and cursor — or `None` when
    /// the mark was never set, was dropped (its line deleted), or, for a global
    /// mark, the buffer it pointed at is no longer open. `None` makes the jump
    /// fail loudly (vim's *E20: Mark not set*) rather than silently leaving the
    /// cursor put or diving into a phantom buffer.
    pub(crate) fn mark_location(&self, name: char) -> Option<MarkLocation> {
        if name.is_ascii_uppercase() {
            let &(buf, cursor) = self.global_marks.get(&name)?;
            self.buffers
                .map
                .contains_key(&buf)
                .then_some(MarkLocation { buf, cursor })
        } else {
            let &(line, col) = self.buffer().marks.get(&name)?;
            Some(MarkLocation {
                buf: self.cur_buffer(),
                cursor: Cursor { line, col },
            })
        }
    }

    /// The position of mark `name` **within the current buffer**, for the motion
    /// path. `Some` only when the mark resolves into the current buffer (every
    /// lowercase mark, and a global mark whose buffer is current); a global mark
    /// pointing at *another* buffer returns `None` here, because that jump can't be
    /// a within-buffer motion offset — it is intercepted ahead of motion
    /// resolution in [`Editor::execute`] and routed through
    /// [`Editor::jump_to_mark_buffer`] instead.
    pub(crate) fn mark_position(&self, name: char) -> Option<Cursor> {
        let loc = self.mark_location(name)?;
        (loc.buf == self.cur_buffer()).then_some(loc.cursor)
    }

    /// Jump to a global mark that lives in another buffer: switch to its buffer
    /// (reusing the buffer-switch that saves/restores each buffer's window
    /// position), then land the cursor — at the mark's exact `(line, col)` for
    /// `` ` ``, or on the first non-blank of its line for `'`. The mark's line is
    /// clamped to the destination buffer in case it shrank since the mark was set.
    pub(crate) fn jump_to_mark_buffer(&mut self, loc: MarkLocation, line_anchor: bool) {
        self.switch_buffer(loc.buf);
        let line = loc.cursor.line.min(self.last_line());
        let col = if line_anchor {
            self.first_non_blank(line)
        } else {
            loc.cursor.col
        };
        self.set_cursor_char(self.buffer().byte_at(line, col));
        self.desired_col = self.cursor_virtcol();
        self.desired_eol = false;
        self.ensure_visible();
    }
}
