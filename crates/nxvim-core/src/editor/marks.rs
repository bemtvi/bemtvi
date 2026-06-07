//! Marks — positions the user names with `m{x}` and returns to with `` `{x} ``
//! (exact byte position) or `'{x}` (first non-blank of the mark's line).
//!
//! Phase 1: the buffer-local lowercase marks `a`–`z`, stored as a [`Cursor`] on
//! each [`OpenBuffer`] so they follow the buffer across switches. The global
//! file marks `A`–`Z`, the automatic specials (`` ` `` / `'` / `.` / `[` / …),
//! and edit-tracking (a mark shifting as text is inserted/deleted above it) land
//! in later phases — see `docs/plans/2026-06-07-marks.md`. The grammar
//! ([`crate::editor::command`]) rejects any name this module doesn't yet accept
//! as a loud dead-end, never a silent no-op, matching the register surface.

use super::*;

/// Whether `c` names a mark nxvim can set and jump to today. Phase 1 supports
/// only the buffer-local lowercase marks `a`–`z`; every other name (`A`–`Z` file
/// marks, the automatic `` ` `` / `.` / `[` / `<` … specials) is rejected at the
/// grammar level — `m{X}` / `` `{X} `` is a dead-end that runs nothing — until
/// its phase lands.
pub(crate) fn is_mark_name(c: char) -> bool {
    c.is_ascii_lowercase()
}

impl Editor {
    /// Set buffer-local mark `name` at the current cursor position (`m{a-z}`).
    //
    // INCOMPLETE: the mark is frozen at the (line, col) it was set — it does not
    // yet track edits. Faithful marks shift down/up as whole lines are
    // inserted/deleted above them, shift their column as text is inserted/deleted
    // earlier in the line, and are dropped when the marked line is deleted. Until
    // Phase 2 wires an `adjust_marks` hook into the buffer-mutation chokepoint, a
    // mark silently points at the wrong position after such an edit. (See
    // docs/plans/2026-06-07-marks.md → Phase 2.)
    pub(crate) fn set_mark(&mut self, name: char) {
        let cursor = self.cursor;
        self.cur_mut().marks.insert(name, cursor);
    }

    /// The position of mark `name` in the current buffer, or `None` if it was
    /// never set — the jump then fails loudly (vim's *E20: Mark not set*) rather
    /// than silently leaving the cursor put.
    pub(crate) fn mark_position(&self, name: char) -> Option<Cursor> {
        self.buffers
            .get(self.cur_buffer())
            .marks
            .get(&name)
            .copied()
    }
}
