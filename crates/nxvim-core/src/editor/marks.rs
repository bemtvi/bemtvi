//! Marks — positions the user names with `m{x}` and returns to with `` `{x} ``
//! (exact byte position) or `'{x}` (first non-blank of the mark's line).
//!
//! Phase 1–2: the buffer-local lowercase marks `a`–`z`, stored as `(line, col)`
//! on the text [`Buffer`](crate::buffer::Buffer) so they follow the buffer across
//! switches *and* ride the single edit choke point ([`crate::buffer::Buffer`]'s
//! `record`) that keeps them correct as text is inserted/deleted — exactly the
//! arrangement `extmarks` use. The global file marks `A`–`Z` and the automatic
//! specials (`` ` `` / `'` / `.` / `[` / …) land in later phases — see
//! `docs/plans/2026-06-07-marks.md`. The grammar ([`crate::editor::command`])
//! rejects any name this module doesn't yet accept as a loud dead-end, never a
//! silent no-op, matching the register surface.

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
    /// Set buffer-local mark `name` at the current cursor position (`m{a-z}`). The
    /// mark is stored on the buffer as `(line, col)` and tracks edits from there —
    /// shifting down/up as whole lines are inserted/deleted above it, shifting its
    /// column as text is inserted/deleted earlier in its line, and being dropped
    /// when its line is deleted (see [`crate::buffer`]'s `shift_marks`).
    pub(crate) fn set_mark(&mut self, name: char) {
        let Cursor { line, col } = self.cursor;
        self.buffer_mut().marks.insert(name, (line, col));
    }

    /// The position of mark `name` in the current buffer, or `None` if it was
    /// never set (or was dropped when its line was deleted) — the jump then fails
    /// loudly (vim's *E20: Mark not set*) rather than silently leaving the cursor
    /// put.
    pub(crate) fn mark_position(&self, name: char) -> Option<Cursor> {
        self.buffer()
            .marks
            .get(&name)
            .map(|&(line, col)| Cursor { line, col })
    }
}
