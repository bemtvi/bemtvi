//! Marks — positions the user names with `m{x}` and returns to with `` `{x} ``
//! (exact byte position) or `'{x}` (first non-blank of the mark's line).
//!
//! Phase 1–4: the buffer-local lowercase marks `a`–`z`, stored as `(line, col)`
//! on the text [`Buffer`](crate::buffer::Buffer) so they follow the buffer across
//! switches *and* ride the single edit choke point ([`crate::buffer::Buffer`]'s
//! `record`) that keeps them correct as text is inserted/deleted — exactly the
//! arrangement `extmarks` use; the global file marks `A`–`Z`, each naming a
//! `(buffer, cursor)` on the [`Editor`] so a jump can cross buffers; and the
//! automatic *special* marks (`` `` `` / `''` previous-context, `` `. `` last
//! change, `` `^ `` last insert, `` `[ `` / `` `] `` last yank/change bounds,
//! `` `< `` / `` `> `` last visual selection). The specials are **read-only**
//! (jump-only): they ride the *same* buffer `marks` store under their punctuation
//! key, so they shift with edits and restore on undo like the named marks, but
//! `m{special}` is rejected loudly. See `docs/plans/2026-06-07-marks.md`.

use super::*;

/// Whether `m{c}` may *set* mark `c`. Only the named marks are settable: the
/// buffer-local lowercase `a`–`z` and the global file `A`–`Z`. The automatic
/// specials are read-only — `m.` / `m<` error loudly (vim's *E191*) rather than
/// silently doing nothing.
pub(crate) fn is_settable_mark(c: char) -> bool {
    c.is_ascii_alphabetic()
}

/// Whether `` `{c} `` / `'{c}` may *jump* to mark `c`: the settable named marks,
/// the read-only automatic specials, and the shada-restored numbered marks
/// `'0`–`'9`. `` ` `` and `'` both name the previous-context mark; `.` `^` `[` `]`
/// `<` `>` name the change/insert/visual automatics; `"` the last-cursor mark; a
/// digit names a numbered mark. Untracked punctuation is a grammar dead-end.
pub(crate) fn is_jumpable_mark(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '\'' | '`' | '.' | '^' | '[' | ']' | '<' | '>' | '"')
}

/// The buffer-local store key a jump name reads. `` ` `` and `'` are two spellings
/// of the one previous-context mark, so both fold to `'`; every other name keys
/// itself. (Uppercase global names never reach here — they key
/// [`Editor::global_marks`] instead.)
fn buffer_mark_key(name: char) -> char {
    match name {
        '`' => '\'',
        other => other,
    }
}

/// The automatic special marks in vim's `:marks` display order. Each lives in the
/// buffer `marks` store under this key, written at its capture point (a jump, an
/// edit, an insert-exit, a yank/change, a visual selection, or — for `"` — leaving
/// the buffer) — never by `m`. `"` is the *last-cursor* mark shada restores so a
/// reopened file lands where it was left.
const SPECIAL_MARKS: [char; 8] = ['"', '\'', '.', '^', '[', ']', '<', '>'];

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
    /// buffer. Only called for a settable name; the read-only specials are
    /// rejected at the grammar boundary.
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

    /// Record the pre-jump cursor into the previous-context mark (`` `` `` / `''`)
    /// so a jump can be undone with `` `` ``. Stored in the buffer `marks` under
    /// `'`, so it shifts with later edits and restores on undo. Called by the
    /// jump-class motions (`gg`/`G`/`` `x ``/`'x`), search, and `:line` — *before*
    /// they move — and never for an operator's range or an ordinary `h`/`j`/word
    /// motion, matching vim's definition of a jump.
    pub(crate) fn record_jump_context(&mut self) {
        let Cursor { line, col } = self.cursor;
        self.buffer_mut().marks.insert('\'', (line, col));
        // The same pre-jump position also enters the focused window's jump list,
        // which `<C-o>`/`<C-i>` walk. Sharing this one choke point keeps the list
        // in lock-step with vim's definition of a jump (see `editor/jumps.rs`).
        self.push_jump();
    }

    /// Record the cursor as the last-insert mark (`` `^ ``) — where Insert mode was
    /// last left. Called from the insert-mode `<Esc>` handler at the insert-stop
    /// column (before the normal-mode backstep), so `` `^ `` returns there.
    pub(crate) fn record_last_insert(&mut self) {
        let Cursor { line, col } = self.cursor;
        self.buffer_mut().marks.insert('^', (line, col));
    }

    /// Record the bounds of the just yanked/changed byte range `[lo, hi)` into the
    /// `` `[ `` / `` `] `` marks: `` `[ `` on the first affected character, `` `] ``
    /// on the last. Called from the operator path *before* the text is mutated, so
    /// for a delete/change the edit's `shift_marks` then collapses both onto the
    /// edit start (vim's behavior); for a yank, which mutates nothing, they bracket
    /// the yanked text.
    pub(crate) fn record_change_bounds(&mut self, lo: usize, hi: usize) {
        let buf = self.buffer();
        let len = buf.len_bytes();
        let lo = lo.min(len);
        let last = hi.clamp(lo + 1, len.max(lo + 1)).saturating_sub(1).min(len);
        let start_line = buf.byte_to_line(lo);
        let end_line = buf.byte_to_line(last);
        let start = (start_line, lo - buf.line_start(start_line));
        let end = (end_line, last - buf.line_start(end_line));
        let marks = &mut self.buffer_mut().marks;
        marks.insert('[', start);
        marks.insert(']', end);
    }

    /// Record the bounds of the selection just left in Visual mode into the
    /// `` `< `` / `` `> `` marks (vim's selection marks, also what `gv` reads):
    /// `` `< `` on the earlier of anchor/cursor, `` `> `` on the later. Called as
    /// Visual mode is left (a `<Esc>` cancel or a completed visual operator).
    pub(crate) fn record_visual_marks(&mut self) {
        let a = self.visual_anchor;
        let b = self.cursor;
        let (lo, hi) = if (a.line, a.col) <= (b.line, b.col) {
            (a, b)
        } else {
            (b, a)
        };
        // Remember the selection's *shape* too, so `gv` restores a linewise
        // selection as linewise rather than guessing from the (position-only)
        // marks. `record_visual_marks` is only ever called while leaving Visual
        // mode, so `self.mode` is the kind we want; guard anyway.
        let kind = self.mode.is_visual().then_some(self.mode);
        let buf = self.buffer_mut();
        buf.marks.insert('<', (lo.line, lo.col));
        buf.marks.insert('>', (hi.line, hi.col));
        if let Some(kind) = kind {
            buf.last_visual = Some(kind);
        }
    }

    /// `gv`: reselect the last Visual selection in the current buffer — the area
    /// bracketed by the `` `< `` / `` `> `` marks, restored in its recorded
    /// charwise/linewise *shape* (the position-only marks can't tell the two
    /// apart). The anchor lands on `` `< `` and the live cursor on `` `> ``.
    /// Fails loudly with vim's *E20* when no Visual selection has been made in
    /// this buffer yet (the marks were never set, or their lines were since
    /// deleted and the marks dropped).
    pub(crate) fn reselect_visual(&mut self) {
        let buf = self.buffer();
        let (Some(&(lo_line, lo_col)), Some(&(hi_line, hi_col))) =
            (buf.marks.get(&'<'), buf.marks.get(&'>'))
        else {
            self.echo_err("E20: Mark not set");
            return;
        };
        // The marks are only ever stamped together with the kind, so the
        // fallback to charwise is just belt-and-suspenders.
        let kind = buf.last_visual.unwrap_or(Mode::Visual);
        self.visual_anchor = Cursor {
            line: lo_line,
            col: lo_col,
        };
        self.cursor = Cursor {
            line: hi_line,
            col: hi_col,
        };
        self.mode = if kind == Mode::VisualLine {
            Mode::VisualLine
        } else {
            Mode::Visual
        };
        // Anchor each placed secondary cursor's own selection (a no-op without a
        // multi-cursor set), then pull the live end back inside the buffer.
        self.begin_visual_anchors();
        self.clamp_cursor();
        // Scroll so the reselected span is maximally visible, rather than letting
        // the cursor-only `ensure_visible` pin its far end to the top edge.
        self.reveal_selection();
    }

    /// Promote a shada-restored global mark (`A`–`Z`) from its pending
    /// `(path, cursor)` form into a live `(BufferId, cursor)` by opening or
    /// finding the buffer for its file. Called on the jump path *before*
    /// [`Editor::mark_location`], so a restored `` `A `` opens its file lazily on
    /// the first jump — vim never bulk-loads marked files at startup. A no-op when
    /// the mark is already live, isn't a pending restore, or its file can't be
    /// opened (an off-tick/daemon load, or a vanished file); the jump then misses
    /// loudly via the *E20* path, never landing in a phantom buffer.
    pub(crate) fn resolve_pending_global_mark(&mut self, name: char) {
        if !name.is_ascii_uppercase() || self.global_marks.contains_key(&name) {
            return;
        }
        let Some((path, cursor)) = self.pending_global_marks.get(&name).cloned() else {
            return;
        };
        if let Some(buf) = self.open_buffer(&path) {
            self.global_marks.insert(name, (buf, cursor));
            self.pending_global_marks.remove(&name);
        }
    }

    /// Resolve a numbered mark `'0`–`'9` to a live [`MarkLocation`], opening (or
    /// finding) the buffer for its stored file. Numbered marks are a pure
    /// persistence construct that always point into a *past* session's file, so
    /// they resolve by path exactly like a restored global mark — and like one,
    /// `None` (the digit was never restored, or its file is gone / unopenable
    /// off-tick) falls through to the loud *E20* miss. Read-only: `m0` never sets
    /// one.
    pub(crate) fn resolve_numbered_mark(&mut self, name: char) -> Option<MarkLocation> {
        if !name.is_ascii_digit() {
            return None;
        }
        let (path, cursor) = self.numbered_marks.get(&name)?.clone();
        let buf = self.open_buffer(&path)?;
        Some(MarkLocation { buf, cursor })
    }

    /// The full location of mark `name` — its buffer and cursor — or `None` when
    /// the mark was never set, was dropped (its line deleted), or, for a global
    /// mark, the buffer it pointed at is no longer open. `None` makes the jump
    /// fail loudly (vim's *E20: Mark not set*) rather than silently leaving the
    /// cursor put or diving into a phantom buffer. The read-only specials resolve
    /// here too, reading the buffer `marks` store under their canonical key.
    pub(crate) fn mark_location(&self, name: char) -> Option<MarkLocation> {
        if name.is_ascii_uppercase() {
            let &(buf, cursor) = self.global_marks.get(&name)?;
            self.buffers
                .map
                .contains_key(&buf)
                .then_some(MarkLocation { buf, cursor })
        } else {
            let &(line, col) = self.buffer().marks.get(&buffer_mark_key(name))?;
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
        // The mark's file may be **lazily (re)opened** by a deferred open (a shada-restored
        // global/numbered mark whose buffer was just minted, or any open behind a
        // `BufReadCmd` handler): its content hasn't landed yet, so positioning now would
        // snap to the top and the read landing would reset it. Record the target; the
        // landing ([`settle_loaded_cursor`]) applies it. Exact `` ` `` lands at the saved
        // column; line-anchored `'` lands at column 0 (the first-non-blank can't be found
        // before the lines exist — a negligible nuance for an as-yet-unloaded file).
        if self.has_pending_open(loc.buf) {
            let col = if line_anchor { 0 } else { loc.cursor.col };
            self.pending_open_cursor = Some(super::buffers::PendingOpenCursor {
                buffer: loc.buf,
                line: loc.cursor.line,
                col,
                top: None,
            });
            return;
        }
        let line = loc.cursor.line.min(self.last_line());
        let col = if line_anchor {
            self.first_non_blank(line)
        } else {
            loc.cursor.col
        };
        self.settle_cursor_byte(self.buffer().byte_at(line, col));
    }

    /// `:marks [names]` — list the set marks into a read-only scratch listing,
    /// mirroring vim's `mark line col file/text` table. An argument filters to the
    /// named marks (`:marks aB`). Rendered straight off [`Self::marks_mirror`] —
    /// the one copy of the membership/order/detail walk — whose `text` field is
    /// exactly this table's file/text column.
    pub(crate) fn ex_marks(&mut self, args: &str) {
        let filter: Vec<char> = args.chars().filter(|c| !c.is_whitespace()).collect();
        let mut lines = vec!["mark line  col file/text".to_string()];
        for row in self.marks_mirror() {
            if filter.is_empty() || filter.contains(&row.name) {
                lines.push(format_mark_line(row.name, row.line, row.col, &row.text));
            }
        }
        self.open_scratch_listing("[Marks]", lines, 0);
    }

    /// The structured mark listing behind both `:marks` ([`Self::ex_marks`]) and
    /// the Lua `btv._marks` mirror (`btv.mark.list` / the `marks` picker).
    /// Membership and order: the current buffer's specials then `a`–`z`, the
    /// global `A`–`Z` (their stored line/col and the file they point into — its
    /// line text when that buffer is current; a mark still *pending* from a shada
    /// restore, its file not yet reopened, lists by its stored path, exactly as
    /// vim shows a restored global mark before its file loads), then the numbered
    /// `0`–`9` (shada-restored last-exit positions, always pending). Each row
    /// carries the fields a jump needs: the target `bufnr` (`0` when the mark is
    /// pending), a `path` to open (empty for an unnamed current buffer), the
    /// 0-based `line`/`col`, and a `text` detail (the line's text, or the file for
    /// a mark pointing outside the current buffer).
    pub fn marks_mirror(&self) -> Vec<MarkMirrorEntry> {
        let cur = self.cur_buffer();
        let cur_name = self.buffer_name(cur).unwrap_or_default();
        let mut out = Vec::new();

        // Buffer-local marks (specials first, then a–z): line/col + the line text.
        for name in SPECIAL_MARKS.iter().copied().chain('a'..='z') {
            if let Some(&(line, col)) = self.buffer().marks.get(&name) {
                let text = self
                    .buffer()
                    .line(line.min(self.last_line()))
                    .trim_end()
                    .to_string();
                out.push(MarkMirrorEntry {
                    name,
                    bufnr: cur.0,
                    line,
                    col,
                    path: cur_name.clone(),
                    text,
                });
            }
        }

        // Global marks A–Z: text is the line when they point into the current
        // buffer, else the file name; a shada-pending one lists by its stored path.
        for name in 'A'..='Z' {
            if let Some(&(buf, pos)) = self.global_marks.get(&name) {
                let (path, text) = if buf == cur {
                    let t = self
                        .buffer()
                        .line(pos.line.min(self.last_line()))
                        .trim_end()
                        .to_string();
                    (cur_name.clone(), t)
                } else {
                    let n = self.buffer_fallback_name(buf);
                    (n.clone(), n)
                };
                out.push(MarkMirrorEntry {
                    name,
                    bufnr: buf.0,
                    line: pos.line,
                    col: pos.col,
                    path,
                    text,
                });
            } else if let Some((path, pos)) = self.pending_global_marks.get(&name) {
                let p = path.display().to_string();
                out.push(MarkMirrorEntry {
                    name,
                    bufnr: 0,
                    line: pos.line,
                    col: pos.col,
                    path: p.clone(),
                    text: p,
                });
            }
        }

        // Numbered marks 0–9 — shada-restored last-exit positions, always pending.
        for name in '0'..='9' {
            if let Some((path, pos)) = self.numbered_marks.get(&name) {
                let p = path.display().to_string();
                out.push(MarkMirrorEntry {
                    name,
                    bufnr: 0,
                    line: pos.line,
                    col: pos.col,
                    path: p.clone(),
                    text: p,
                });
            }
        }

        out
    }
}

/// One row of the `btv._marks` mirror (`btv.mark.list` / the `marks` picker). The
/// structured counterpart of a `:marks` display line: `line`/`col` are 0-based,
/// `bufnr` is `0` for a pending (not-yet-reopened) mark, and `path` is empty when
/// the current buffer has no file.
pub struct MarkMirrorEntry {
    pub name: char,
    pub bufnr: u64,
    pub line: usize,
    pub col: usize,
    pub path: String,
    pub text: String,
}

/// One `:marks` row in vim's layout: the mark name, its 1-based line, 0-based
/// column, then the line text or file name.
fn format_mark_line(name: char, line: usize, col: usize, detail: &str) -> String {
    format!("{name:>4} {:>4} {col:>4} {detail}", line + 1)
}
