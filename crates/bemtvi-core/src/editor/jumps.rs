//! The jump list — vim's per-window history of the positions you jumped *from*,
//! navigated with `<C-o>` (older) and `<C-i>` / `<Tab>` (newer) and listed with
//! `:jumps`.
//!
//! A *jump* is exactly the set of motions vim counts as one (`gg`/`G`, the mark
//! jumps, a `/`/`?` search, `:line`) — every one of them funnels through
//! [`Editor::record_jump_context`], the same choke point that stamps the
//! previous-context mark (`` `` `` / `''`). Hooking the list there means it stays
//! in lock-step with vim's own definition: ordinary `h`/`j`/word/find motions are
//! *not* jumps and never touch it.
//!
//! The list lives on the [`Window`](super::Window), not the [`Editor`], because
//! vim's jumplist is per-window: each split keeps its own history (a split
//! inherits a copy of its parent's), and it rides the window through tab
//! switches. An entry names a `(buffer, line, col)`, so a `<C-o>` can cross back
//! into another file just as vim's does.
//!
//! The navigation pointer [`Window::jump_idx`](super::Window) indexes the list;
//! `idx == len` means "at the present, not yet navigating". The first `<C-o>`
//! after a jump stashes the *current* position at the end so a later `<C-i>` can
//! return to it — vim's behavior, which is why pressing `<C-o>` then `<C-i>`
//! round-trips.

use super::windows::Window;
use super::*;

/// Vim's jumplist cap (`JUMPLISTSIZE`): at most 100 positions per window. A new
/// jump that would overflow drops the oldest entry.
const JUMPLIST_SIZE: usize = 100;

/// One jumplist entry as `vim.fn.getjumplist` reads it: `(bufnr, line, col)`
/// with 0-based `line`/`col`. Returned by [`Editor::window_jumplist`] (a flat
/// tuple rather than [`JumpEntry`], whose `BufferId` doesn't cross the crate
/// boundary).
pub type JumpView = (u64, usize, usize);

/// One remembered jump position: which buffer, and the `(line, col)` within it.
/// Stored per-window on [`Window`](super::Window). The buffer may differ from the
/// window's current one — a jumplist entry survives a buffer switch — so
/// navigating onto it crosses files (see [`Editor::jump_to`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JumpEntry {
    pub(crate) buf: BufferId,
    pub(crate) line: usize,
    pub(crate) col: usize,
}

impl Editor {
    /// Push the current cursor onto the focused window's jumplist as a position to
    /// return to, then reset the navigation pointer to the end. Called only from
    /// [`Editor::record_jump_context`] — the single pre-jump choke point — so the
    /// list captures the position *before* each jump, exactly the spots `<C-o>`
    /// should walk back through. Dedups by `(buffer, line)` like vim (jumping
    /// twice from the same line moves it to the end instead of piling up) and caps
    /// the list at [`JUMPLIST_SIZE`], dropping the oldest entry on overflow.
    pub(crate) fn push_jump(&mut self) {
        let entry = JumpEntry {
            buf: self.cur_buffer(),
            line: self.cursor.line,
            col: self.cursor.col,
        };
        self.push_jump_entry(entry);
    }

    /// Push an explicit [`JumpEntry`] onto the focused window's jumplist, applying
    /// the same dedup / cap / pointer-reset rules as [`Editor::push_jump`]. Used
    /// when the position to record is *not* the live cursor — e.g. a new tab
    /// records the buffer/position it was opened *from* onto its own (freshly
    /// copied) list, matching vim's departure jump on `:tabnew`.
    pub(crate) fn push_jump_entry(&mut self, entry: JumpEntry) {
        let win = self.windows.cur_mut();
        win.jumps
            .retain(|e| !(e.buf == entry.buf && e.line == entry.line));
        if win.jumps.len() >= JUMPLIST_SIZE {
            win.jumps.remove(0);
        }
        win.jumps.push(entry);
        win.jump_idx = win.jumps.len();
    }

    /// `<C-o>` — move to an older position in the jumplist, `count` steps back.
    pub(crate) fn jump_back(&mut self, count: usize) {
        self.materialize_pending_jumplist();
        self.jump_nav(true, count);
    }

    /// `<C-i>` / `<Tab>` — move to a newer position in the jumplist, `count` steps
    /// forward.
    pub(crate) fn jump_forward(&mut self, count: usize) {
        self.materialize_pending_jumplist();
        self.jump_nav(false, count);
    }

    /// Turn a shada-restored jumplist (paths) into the focused window's live jump
    /// entries, opening each file. Deferred to the first `<C-o>`/`<C-i>` so a
    /// restored session doesn't bulk-load every jumped-to file at launch — only
    /// when you actually start walking the list. A no-op once drained, or when the
    /// session has already built its own jumplist (that one wins; the restored list
    /// is dropped). Distinct files are opened once each (find-or-load).
    fn materialize_pending_jumplist(&mut self) {
        if self.pending_jumplist.is_empty() {
            return;
        }
        let pending = std::mem::take(&mut self.pending_jumplist);
        if !self.windows.cur().jumps.is_empty() {
            return;
        }
        let mut entries = Vec::with_capacity(pending.len());
        for (path, line, col) in pending {
            if let Some(buf) = self.open_buffer(&path) {
                entries.push(JumpEntry { buf, line, col });
            }
        }
        let win = self.windows.cur_mut();
        win.jumps = entries;
        win.jump_idx = win.jumps.len();
    }

    /// Walk the jumplist `count` entries in one direction, mirroring vim's
    /// `movemark`. A `count` that would step past either end of the list refuses
    /// the move entirely (vim beeps; bemtvi has no bell). The first navigation after
    /// a jump (pointer at the end) first stashes the current position so the move
    /// is reversible with the opposite key.
    fn jump_nav(&mut self, back: bool, count: usize) {
        let delta: isize = if back {
            -(count.max(1) as isize)
        } else {
            count.max(1) as isize
        };
        let here = JumpEntry {
            buf: self.cur_buffer(),
            line: self.cursor.line,
            col: self.cursor.col,
        };
        let win = self.windows.cur_mut();
        let len = win.jumps.len() as isize;
        if len == 0 {
            return;
        }
        let idx = win.jump_idx as isize;
        // A count that overshoots either end is a no-op, as in vim.
        if idx + delta < 0 || idx + delta >= len {
            return;
        }
        let mut idx = idx;
        // First navigation after a jump: stash where we are now (at the end of the
        // list) so a later `<C-i>` returns here, then step off that stash.
        if win.jump_idx == win.jumps.len() {
            win.jumps.push(here);
            idx = win.jumps.len() as isize - 1;
        }
        idx += delta;
        win.jump_idx = idx as usize;
        let target = win.jumps[idx as usize];
        self.jump_to_entry(target);
    }

    /// Land the cursor on a jumplist entry, switching buffers first when it points
    /// into another file. The stored line/col are clamped to the (possibly edited)
    /// destination, so a stale entry lands somewhere sane rather than off the end;
    /// an entry whose buffer has since been wiped is skipped (no move) instead of
    /// crashing.
    fn jump_to_entry(&mut self, target: JumpEntry) {
        if !self.buffer_is_valid(target.buf) {
            return;
        }
        if target.buf != self.cur_buffer() {
            self.switch_buffer(target.buf);
        }
        self.settle_cursor_at(target.line, target.col);
    }

    /// Shift every window's jumplist to follow the line edits each buffer recorded
    /// since the last call (vim's `mark_adjust` for the jumplist). Drains each
    /// buffer's jump-edit journal and, for every window in every tab, moves the
    /// `<C-o>` targets pointing into that buffer by the same `(start, old_end,
    /// new_end)` rule the buffer-local marks ride — so inserting lines above a jump
    /// target pushes it down with the text, and deleting its line drops it. Called
    /// from the server's post-mutation pass, so it covers both keystroke and API
    /// edits and buffers that aren't the focused one.
    pub fn adjust_jumplists_for_edits(&mut self) {
        for id in self.buffer_ids() {
            let Some(ob) = self.buffers.map.get_mut(&id) else {
                continue;
            };
            let edits = ob.buffer.take_jump_edits();
            if edits.is_empty() {
                continue;
            }
            for edit in &edits {
                let (s, oe, ne) = (edit.start_point, edit.old_end_point, edit.new_end_point);
                for win in self.windows.all_windows_mut() {
                    shift_window_jumps(win, id, s, oe, ne);
                }
                // Every *parked* tree — inactive tabs of any layer, plus a
                // non-focused layer's active tab (the main tree while a dock is
                // focused, or a dock's tree while main is) — stashes its own window
                // tree; their jumplists must ride the same edit so a `<C-o>` stays
                // correct after a tab/layer switch. (The live tree on `self.windows`
                // was handled above.)
                for tree in self.parked_trees_mut() {
                    for win in tree.all_windows_mut() {
                        shift_window_jumps(win, id, s, oe, ne);
                    }
                }
            }
        }
    }

    /// Window `id`'s jumplist for `vim.fn.getjumplist`, oldest-first: each entry as
    /// `(bufnr, line, col)` with 0-based `line`/`col` (the Lua bridge adds neovim's
    /// 1-based `lnum`), paired with the navigation pointer `curidx` — the index
    /// `<C-o>` walks back from, equal to `entries.len()` when sitting at the present
    /// (not navigating), matching vim's `w_jumplistidx`. `None` for an unknown
    /// window id; a window in an inactive tab resolves to its stashed jumplist.
    pub fn window_jumplist(&self, id: WindowId) -> Option<(Vec<JumpView>, usize)> {
        let (_, tree) = self.any_tab_tree_of_window(id)?;
        let win = tree.try_get(id)?;
        let entries = win.jumps.iter().map(|e| (e.buf.0, e.line, e.col)).collect();
        Some((entries, win.jump_idx))
    }

    /// `:jumps` — list the focused window's jumplist into a read-only scratch
    /// listing, mirroring vim's `jump line  col file/text` table (rendered by the
    /// shared [`Editor::open_position_listing`]). A row in the current buffer shows
    /// its line's text as the detail; one in another buffer shows the file.
    pub(crate) fn ex_jumps(&mut self, _args: &str) {
        let idx = self.windows.cur().jump_idx;
        let cur_buf = self.cur_buffer();
        let rows: Vec<(usize, usize, String)> = self
            .windows
            .cur()
            .jumps
            .clone()
            .into_iter()
            .map(|e| {
                let detail = if e.buf == cur_buf {
                    self.buffer()
                        .line(e.line.min(self.last_line()))
                        .trim_end()
                        .to_string()
                } else {
                    self.buffer_fallback_name(e.buf)
                };
                (e.line, e.col, detail)
            })
            .collect();
        self.open_position_listing("[Jumps]", " jump line  col file/text", idx, &rows);
    }
}

/// Shift one window's jumplist entries for buffer `buf` across an edit (the byte
/// range `[s, oe)` became text ending at `ne`, as tree-sitter `Point`s). Entries
/// in other buffers are untouched; an entry whose line the edit deleted is
/// dropped (and the navigation pointer slides back if it sat after the drop), so
/// the list stays valid. Reuses [`crate::buffer::shift_point`] — the exact rule
/// the buffer-local marks ride — so a `<C-o>` target and a `` `a `` mark on the
/// same line move identically.
fn shift_window_jumps(
    win: &mut Window,
    buf: BufferId,
    s: (usize, usize),
    oe: (usize, usize),
    ne: (usize, usize),
) {
    if !win.jumps.iter().any(|e| e.buf == buf) {
        return;
    }
    let mut kept = Vec::with_capacity(win.jumps.len());
    let mut idx = win.jump_idx;
    for (i, e) in win.jumps.iter().enumerate() {
        if e.buf != buf {
            kept.push(*e);
            continue;
        }
        match crate::buffer::shift_point((e.line, e.col), s, oe, ne) {
            Some((line, col)) => kept.push(JumpEntry { buf, line, col }),
            None if i < win.jump_idx => idx -= 1,
            None => {}
        }
    }
    win.jumps = kept;
    win.jump_idx = idx.min(win.jumps.len());
}
