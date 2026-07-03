//! Operators (`d`/`c`/`y`/`>`/…) and the editing primitives they drive
//! (delete/yank/paste/join/replace/case).

use super::command::{is_clipboard_register, is_readonly_register, COMMENT_OP, FOLD_OP};
use super::syntax::indent_width;
use super::*;
use crate::clipboard::Clipboard;
use crate::mode::Mode;
use crate::unicode;
use std::cmp::{max, min};

/// Echoed when a `"+` / `"*` yank/delete/paste finds no clipboard provider
/// installed (a headless box whose platform backend failed, or a bare-core
/// test). vim prints a similar `clipboard: No provider` warning.
const CLIPBOARD_UNAVAILABLE: &str = "clipboard: No provider available for register '+'/'*'";

impl Editor {
    pub(crate) fn apply_operator(&mut self, op: char, m: MotionResult) {
        let cur = self.cursor_char();
        let (lo, hi, linewise, first_line) = match m.kind {
            MotionKind::Exclusive => (min(cur, m.target), max(cur, m.target), false, 0),
            MotionKind::Inclusive => {
                // vim never lets an inclusive charwise motion swallow a line
                // break: when the motion's end sits where no character exists —
                // `$`/`g$` on an empty line, `e` stopped at the buffer's final
                // newline — the would-be included cell is the line's `\n`, and
                // vim clips the range short of it (`d$`/`y$` on an empty line
                // are no-ops; `ye` at the buffer end yanks without the newline).
                let end = max(cur, m.target);
                let hi = if self.char_at(end) == '\n' {
                    end
                } else {
                    end + 1
                };
                (min(cur, m.target), hi, false, 0)
            }
            MotionKind::Linewise => {
                let l1 = self.cursor.line;
                let l2 = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                // Expand the line range to cover any closed fold it touches: a
                // linewise operator over a collapsed fold acts on the whole fold
                // (vim's rule — `dd`/`yy`/`>>` on a fold takes all its lines).
                let a = self.fold_line_start(min(l1, l2));
                let b = self.fold_line_end(max(l1, l2));
                let lo = self.buffer().line_start(a);
                let hi = self
                    .buffer()
                    .line_start((b + 1).min(self.buffer().line_count()));
                (lo, hi, true, a)
            }
        };
        self.apply_operator_to_range(op, lo, hi, linewise, first_line);
    }

    /// The inclusive line span `[first, last]` touched by the byte range `[lo,
    /// hi)`: the line containing `lo` through the line containing the last byte
    /// before `hi`. Used by the linewise operators (`=`/`>`/`<`/`zf`/`gc`).
    fn line_span(&self, lo: usize, hi: usize) -> (usize, usize) {
        let first = self.buffer().byte_to_line(lo);
        let last = self.buffer().byte_to_line(hi.saturating_sub(1));
        (first, last)
    }

    /// Apply `op` to the absolute byte range `[lo, hi)`. `linewise`/`first_line`
    /// control linewise settling; charwise callers (motions, text objects) pass
    /// `(false, 0)`. Unlike `apply_operator`, the range is explicit and need not
    /// touch the cursor — text objects span both sides of it.
    pub(crate) fn apply_operator_to_range(
        &mut self,
        op: char,
        lo: usize,
        hi: usize,
        linewise: bool,
        first_line: usize,
    ) {
        if lo >= hi {
            // A change over an empty range (`cl` / `c$` / `cw` on an empty
            // line) still enters Insert mode in vim — the deletion is empty,
            // the mode switch isn't.
            if op == 'c' {
                if !self.modifiable() {
                    self.refuse_edit();
                    return;
                }
                self.push_undo();
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            return;
        }
        // `zf{motion}` creates a fold over the motion's lines — it touches no text
        // and writes no register, so it bypasses the modifiable / register checks
        // below (folding a read-only buffer is fine).
        if op == FOLD_OP {
            let (first, last) = self.line_span(lo, hi);
            self.create_fold(first, last);
            return;
        }
        // A live terminal buffer is read-only — refuse every operator that changes
        // text (`d`/`c`/`>`/`<`/case/…); `y` (yank) reads only, so it's still allowed.
        if op != 'y' && !self.modifiable() {
            self.refuse_edit();
            return;
        }
        // `d`/`y`/`c` write a register; a read-only target aborts the whole
        // operator before any text is touched (vim beeps and does nothing), and a
        // clipboard target with no provider aborts loudly rather than deleting.
        if matches!(op, 'd' | 'y' | 'c') {
            if self.register_write_blocked() {
                return;
            }
            if self.clipboard_write_unavailable() {
                self.echo(CLIPBOARD_UNAVAILABLE);
                return;
            }
            // The `` `[ `` / `` `] `` change-bound marks bracket the affected text.
            // Recorded before the mutation: a yank leaves them around the yanked
            // span, a delete/change lets the edit's `shift_marks` collapse them onto
            // the edit start (vim's behavior).
            self.record_change_bounds(lo, hi);
        }
        match op {
            'y' => {
                self.yank_range(lo, hi, linewise);
                if linewise {
                    self.cursor.line = first_line;
                } else {
                    self.set_cursor_char(lo);
                }
                self.clamp_cursor();
            }
            'd' => {
                self.delete_yank_range(lo, hi, linewise);
                self.delete_range(lo, hi);
                if linewise {
                    self.settle_after_linewise_delete(first_line);
                } else {
                    self.set_cursor_char(lo);
                }
                self.clamp_cursor();
            }
            'c' => {
                self.delete_yank_range(lo, hi, linewise);
                if linewise {
                    self.linewise_change(lo, hi, first_line);
                } else {
                    self.delete_range(lo, hi);
                    self.set_cursor_char_insert(lo);
                }
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            // `=` reindents whole lines (always linewise, even from a charwise
            // motion / text object), then settles on the first line's first non-blank.
            '=' => {
                let (first, last) = self.line_span(lo, hi);
                self.reindent_lines(first, last);
            }
            // `>{motion}` / `<{motion}` / `>>` / `<<`: shift whole lines one
            // `shiftwidth` right or left (always linewise, like `=`). A count
            // before the operator already became the line range, so the shift
            // amount here is exactly one `shiftwidth`.
            '>' | '<' => {
                let (first, last) = self.line_span(lo, hi);
                self.shift_lines(first, last, op == '>', 1);
            }
            // `gc{motion}` / `gcc` / `gcip`: toggle line comments over whichever
            // lines the range touches (always linewise, even from a charwise
            // motion / text object, like `=`).
            COMMENT_OP => {
                let (first, last) = self.line_span(lo, hi);
                self.toggle_comment_lines(first, last);
            }
            _ => {}
        }
    }

    /// Reindent lines `[first, last]` to their treesitter (else copy-previous,
    /// else 0) indent — the body shared by `==`, `=motion`, `gg=G`, and visual `=`.
    /// One undo step covers the whole run; the cursor settles on `first`'s first
    /// non-blank, as vim does after `=`.
    fn reindent_lines(&mut self, first: usize, last: usize) {
        self.push_undo();
        let last = last.min(self.last_line());
        for line in first..=last {
            let width = self.indent_for(line);
            self.set_line_indent(line, width);
        }
        self.buffer_mut().modified = true;
        self.cursor.line = first.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
    }

    /// Shift lines `[first, last]` by `count` shiftwidths — right when `right`
    /// (`>`/`>>`), else left (`<`/`<<`). The body shared by `>{motion}`, `>>`,
    /// and visual `>`/`<`. Each line's existing indent (measured in virtual
    /// columns, tabs honoring `tabstop`) gains / loses `count * shiftwidth`
    /// columns, clamped at 0; the whitespace is then re-laid by `set_line_indent`
    /// (spaces under `expandtab`, else tabs+spaces). Blank lines (only whitespace)
    /// are left untouched, as vim's `>>` never indents an empty line. One undo
    /// step covers the whole run; the cursor settles on `first`'s first non-blank.
    fn shift_lines(&mut self, first: usize, last: usize, right: bool, count: usize) {
        self.push_undo();
        let last = last.min(self.last_line());
        let opts = self.buffer().options;
        let amount = opts.effective_shiftwidth() * count;
        let tabstop = opts.effective_tabstop();
        for line in first..=last {
            let s = self.buffer().line(line);
            // A blank line keeps no indent (vim leaves empty lines at column 0).
            if s.trim().is_empty() {
                continue;
            }
            let cur = indent_width(&s, tabstop);
            let new = if right {
                cur + amount
            } else {
                cur.saturating_sub(amount)
            };
            self.set_line_indent(line, new);
        }
        self.buffer_mut().modified = true;
        self.cursor.line = first.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
    }

    /// Settle the cursor after a linewise delete: first non-blank of the line that
    /// now occupies the deleted lines' position. Shared by `apply_operator` and
    /// `visual_operate`.
    fn settle_after_linewise_delete(&mut self, first_line: usize) {
        self.cursor.line = first_line.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
    }

    /// Linewise change (`cc`/`S`, linewise-visual `c`): delete `lo..hi`, reopen a
    /// single empty line where the deleted block was, and park the cursor there for
    /// insert. Shared by `apply_operator` and `visual_operate`.
    fn linewise_change(&mut self, lo: usize, hi: usize, first_line: usize) {
        // Did the change consume the whole buffer? If so, `normalize` below leaves
        // exactly one empty line, which *is* the reopened line — adding another
        // would leave a stray blank.
        let whole_buffer = lo == 0 && hi >= self.buffer().len_bytes();
        self.delete_range(lo, hi);
        self.buffer_mut().normalize();
        if whole_buffer {
            self.cursor.line = 0;
            self.cursor.col = 0;
            return;
        }
        // Reopen the empty line at `first_line`. When the block was the buffer's
        // tail, `first_line` now equals the line count, so `line_start` resolves to
        // end-of-buffer and the newline *appends* the line in place — rather than
        // the old `min(last_line())` clamp, which wrongly opened it before the
        // surviving last line.
        let target = first_line.min(self.buffer().line_count());
        let at = self.buffer().line_start(target);
        self.buffer_mut().insert_char(at, '\n');
        self.buffer_mut().normalize();
        self.cursor.line = target;
        self.cursor.col = 0;
    }

    pub(crate) fn visual_operate(&mut self, op: char) {
        // With a multi-cursor set placed, the operator runs over every cursor's own
        // selection as one undo group; this single-cursor body is the primary-only
        // path.
        if self.has_secondary_cursors() {
            self.visual_operate_multi(op);
            return;
        }
        let (lo, hi, linewise, first_line) = self.visual_range();
        // `=` reindents the selected lines; unlike d/y/c it neither yanks nor needs
        // the shared snapshot (`reindent_lines` takes its own), so handle it first.
        if op == '=' {
            let (first, last) = self.line_span(lo, hi);
            self.reindent_lines(first, last);
            self.mode = Mode::Normal;
            self.reset_pending();
            return;
        }
        // Visual `>`/`<` shift the selected lines — like `=`, no yank / register.
        // A count multiplies the shift (`2>` indents by two shiftwidths), matching
        // vim's visual shift. The cursor settles on the first line, then visual
        // mode exits.
        if op == '>' || op == '<' {
            // Stash the selection's shape so `.` reselects the same extent and
            // re-shifts (vim's visual `.`), captured before the buffer mutates.
            self.capture_visual_shape(op);
            let (first, last) = self.line_span(lo, hi);
            self.shift_lines(first, last, op == '>', self.effective_count());
            self.mode = Mode::Normal;
            self.reset_pending();
            return;
        }
        // Visual `zf` folds the selected lines — like `=`, no yank / register; it
        // creates the fold and parks the cursor on its first line.
        if op == FOLD_OP {
            let (first, last) = self.line_span(lo, hi);
            self.create_fold(first, last);
            self.mode = Mode::Normal;
            self.reset_pending();
            return;
        }
        // Visual `gc` toggles comments on the selected lines — like `=`, no yank /
        // register, its own undo step inside `toggle_comment_lines`.
        if op == COMMENT_OP {
            let (first, last) = self.line_span(lo, hi);
            self.toggle_comment_lines(first, last);
            self.mode = Mode::Normal;
            self.reset_pending();
            return;
        }
        // A read-only register target aborts the operation; vim beeps and stays
        // in visual mode with the selection intact. A clipboard target with no
        // provider aborts loudly, likewise leaving the selection untouched.
        if self.register_write_blocked() {
            self.reset_pending();
            return;
        }
        if self.clipboard_write_unavailable() {
            self.echo(CLIPBOARD_UNAVAILABLE);
            self.reset_pending();
            return;
        }
        // Stash the selection's shape for dot-repeat before it is consumed: a
        // visual `d`/`c` replays as a size-faithful reselect from the new cursor.
        if matches!(op, 'd' | 'c') {
            self.capture_visual_shape(op);
        }
        // Leaving Visual mode (by operating on the selection) sets the `` `< `` /
        // `` `> `` selection marks and the `` `[ `` / `` `] `` change bounds, both
        // from the pre-edit selection.
        self.record_visual_marks();
        self.record_change_bounds(lo, hi);
        self.push_undo();
        // `y` records a yank; `d`/`c` record a delete (ring / small-delete).
        if op == 'y' {
            self.yank_range(lo, hi, linewise);
        } else {
            self.delete_yank_range(lo, hi, linewise);
        }
        match op {
            'd' => {
                self.delete_range(lo, hi);
                if linewise {
                    self.settle_after_linewise_delete(first_line);
                } else {
                    self.set_cursor_char(lo);
                }
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            'y' => {
                if linewise {
                    self.cursor.line = first_line;
                } else {
                    self.set_cursor_char(lo);
                }
                self.mode = Mode::Normal;
                self.clamp_cursor();
            }
            'c' => {
                if linewise {
                    self.linewise_change(lo, hi, first_line);
                } else {
                    self.delete_range(lo, hi);
                    self.set_cursor_char_insert(lo);
                }
                self.mode = Mode::Insert;
                self.snapshot_taken = true;
            }
            _ => {}
        }
        self.reset_pending();
    }

    /// Capture the active visual selection's shape as a size-faithful dot-repeat
    /// stream ([`VisualShape`]), stashed in `pending_visual` for the commit point
    /// in [`Editor::input`]. Called before the buffer mutates, while
    /// `visual_anchor`/`cursor` still describe the selection. The synthesized keys
    /// reselect the same *extent* (line count, and column span on the last line)
    /// from wherever the cursor sits, rather than re-running the original motions —
    /// matching vim's visual `.`.
    fn capture_visual_shape(&mut self, op: char) {
        let linewise = self.mode == Mode::VisualLine;
        let a = self.visual_anchor;
        let b = self.cursor;
        let (start, end) = if (a.line, a.col) <= (b.line, b.col) {
            (a, b)
        } else {
            (b, a)
        };
        let rows = end.line - start.line;
        let mut keys = Vec::new();
        if linewise {
            keys.push(Key::char('V'));
            for _ in 0..rows {
                keys.push(Key::char('j'));
            }
        } else {
            keys.push(Key::char('v'));
            for _ in 0..rows {
                keys.push(Key::char('j'));
            }
            // The final line's column span. Single-line: extend right by the
            // selected width − 1. Multi-line: `j` only carried the start column
            // down, so snap to column 0 and walk out to the end column.
            let cols = if rows == 0 {
                self.grapheme_steps(start.line, start.col, end.col)
            } else {
                keys.push(Key::char('0'));
                self.grapheme_steps(end.line, 0, end.col)
            };
            for _ in 0..cols {
                keys.push(Key::char('l'));
            }
        }
        keys.push(Key::char(op));
        self.pending_visual = Some(VisualShape {
            keys,
            is_change: op == 'c',
        });
    }

    /// The number of grapheme steps (i.e. `l` presses) from byte column `from` to
    /// byte column `to` on `line` — how far a reselect must walk to span them.
    fn grapheme_steps(&self, line: usize, from: usize, to: usize) -> usize {
        let s = self.buffer().line(line);
        let mut col = from;
        let mut n = 0;
        while col < to {
            col = unicode::next_grapheme(&s, col);
            n += 1;
        }
        n
    }

    fn visual_range(&self) -> (usize, usize, bool, usize) {
        let linewise = self.mode == Mode::VisualLine;
        let (lo, hi, first_line) = self.visual_range_lw(linewise);
        (lo, hi, linewise, first_line)
    }

    /// The selection's `(lo, hi, first_line)` byte range between `visual_anchor`
    /// and the cursor, with `linewise` passed explicitly rather than read from
    /// `self.mode` — so a per-cursor operator sweep keeps using the right kind
    /// even after an editing `f` (visual `c`) has flipped the mode to Insert.
    pub(crate) fn visual_range_lw(&self, linewise: bool) -> (usize, usize, usize) {
        let a = self.visual_anchor;
        let b = self.cursor;
        if linewise {
            // Expand to cover any closed fold the selection touches — a linewise
            // operator over a collapsed fold takes the whole fold (vim's rule).
            let la = self.fold_line_start(min(a.line, b.line));
            let lb = self.fold_line_end(max(a.line, b.line));
            let lo = self.buffer().line_start(la);
            let hi = self
                .buffer()
                .line_start((lb + 1).min(self.buffer().line_count()));
            (lo, hi, la)
        } else {
            let ca = self.buffer().byte_at(a.line, a.col);
            let cb = self.buffer().byte_at(b.line, b.col);
            let lo = min(ca, cb);
            let hi = max(ca, cb) + 1;
            (lo, hi.min(self.last_char_idx().max(lo + 1)), 0)
        }
    }

    /// Per-cursor visual operator: apply `op` over **every** cursor's own
    /// selection (`anchor`..`head`) as a single undo group, then leave visual —
    /// `c` drops into Insert at every cursor, `d`/`y`/`=` settle back in Normal.
    /// The cursor heads survive into the next mode (the multi-cursor set persists).
    fn visual_operate_multi(&mut self, op: char) {
        // `d`/`y`/`c` write a register; a read-only or unavailable-clipboard target
        // aborts the whole operation, leaving the selection intact (vim beeps).
        if matches!(op, 'd' | 'y' | 'c') {
            if self.register_write_blocked() {
                self.reset_pending();
                return;
            }
            if self.clipboard_write_unavailable() {
                self.echo(CLIPBOARD_UNAVAILABLE);
                self.reset_pending();
                return;
            }
        }
        let linewise = self.mode == Mode::VisualLine;
        // Stamp the primary selection's `` `< `` / `` `> `` marks before leaving.
        self.record_visual_marks();
        // One undo group; `for_each_cursor` restores each cursor's own anchor into
        // `visual_anchor` so `visual_range_lw` brackets that cursor's selection.
        self.edit_each_cursor(|ed| ed.visual_operate_once(op, linewise));
        self.mode = if op == 'c' {
            Mode::Insert
        } else {
            Mode::Normal
        };
        self.clear_anchor_marks();
        self.reset_pending();
    }

    /// One cursor's slice of a multi-cursor visual operator: apply `op` over the
    /// selection between this cursor's `visual_anchor` and head. Reads no pending
    /// state and opens no undo group of its own — [`edit_each_cursor`] wraps the
    /// whole sweep in one.
    ///
    /// [`edit_each_cursor`]: Editor::edit_each_cursor
    pub(crate) fn visual_operate_once(&mut self, op: char, linewise: bool) {
        let (lo, hi, first_line) = self.visual_range_lw(linewise);
        self.apply_operator_to_range(op, lo, hi, linewise, first_line);
    }

    /// Snap `[lo, hi)` and extract it as register-bound text + its kind, or
    /// `None` when the range is empty.
    fn slice_for_register(
        &self,
        lo: usize,
        hi: usize,
        linewise: bool,
    ) -> Option<(String, RegKind)> {
        let (lo, hi) = self.snap_range(lo, hi);
        if lo >= hi {
            return None;
        }
        let text = self.buffer().text.slice(lo..hi).to_string();
        let kind = if linewise {
            RegKind::Line
        } else {
            RegKind::Char
        };
        Some((text, kind))
    }

    /// Yank `[lo, hi)` into the active register (the `"x` selection, or the
    /// unnamed register when none is selected), routing through vim's yank rules.
    fn yank_range(&mut self, lo: usize, hi: usize, linewise: bool) {
        if let Some((text, kind)) = self.slice_for_register(lo, hi, linewise) {
            self.collect_cursor_register(lo, &text, kind);
            let reg = self.pending.register;
            if is_clipboard_register(reg) {
                self.clipboard_write(text, kind);
            } else {
                self.registers.record_yank(reg, text, kind);
            }
        }
    }

    /// Capture `[lo, hi)` for a *delete* (or change): like `yank_range` but
    /// routed through the delete rules (numbered ring / small-delete register).
    /// Call this — never `yank_range` — before any operator that removes text.
    fn delete_yank_range(&mut self, lo: usize, hi: usize, linewise: bool) {
        if let Some((text, kind)) = self.slice_for_register(lo, hi, linewise) {
            self.collect_cursor_register(lo, &text, kind);
            let reg = self.pending.register;
            if is_clipboard_register(reg) {
                self.clipboard_write(text, kind);
            } else {
                self.registers.record_delete(reg, text, kind);
            }
        }
    }

    /// During a multi-cursor editing sweep, stash this cursor's yanked/deleted
    /// slice (keyed by its range start for document-order sorting) so a later
    /// multi-cursor paste can return each cursor its own text. A no-op outside a
    /// sweep (the collector is `None`). See [`Editor::cursor_registers`].
    fn collect_cursor_register(&mut self, at: usize, text: &str, kind: RegKind) {
        if let Some(collect) = self.cursor_register_collect.as_mut() {
            collect.push((
                at,
                RegisterCell {
                    text: text.to_string(),
                    kind,
                },
            ));
        }
    }

    /// Remove `[lo, hi)` bytes, recording undo and keeping the buffer invariant.
    fn delete_range(&mut self, lo: usize, hi: usize) {
        let (lo, hi) = self.snap_range(lo, hi);
        if lo >= hi {
            return;
        }
        self.push_undo();
        self.buffer_mut().remove(lo..hi);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
    }

    /// Clamp a byte range into bounds and onto grapheme boundaries, so a
    /// motion-derived endpoint can never split a cluster (a no-op for ASCII).
    fn snap_range(&self, lo: usize, hi: usize) -> (usize, usize) {
        let hi = hi.min(self.buffer().len_bytes());
        let lo = self.grapheme_floor_abs(lo.min(hi));
        let hi = self.grapheme_ceil_abs(hi);
        (lo, hi)
    }

    pub(crate) fn delete_under_cursor(&mut self, count: usize) {
        let len = self.line_len();
        if len == 0 {
            return;
        }
        let lo = self.cursor_char();
        let line_end = self.buffer().byte_at(self.cursor.line, len);
        let (hi, _) = self.advance_graphemes(lo, count, line_end);
        self.delete_yank_range(lo, hi, false);
        self.delete_range(lo, hi);
        self.clamp_cursor();
    }

    pub(crate) fn delete_before_cursor(&mut self, count: usize) {
        if self.cursor.col == 0 {
            return;
        }
        let new_col = self.cursor.col.saturating_sub(count);
        let lo = self.buffer().byte_at(self.cursor.line, new_col);
        let hi = self.cursor_char();
        self.delete_yank_range(lo, hi, false);
        self.delete_range(lo, hi);
        self.cursor.col = new_col;
        self.clamp_cursor();
    }

    pub(crate) fn delete_to_eol(&mut self) {
        let len = self.line_len();
        let lo = self.cursor_char();
        let hi = self.buffer().byte_at(self.cursor.line, len);
        if lo < hi {
            self.delete_yank_range(lo, hi, false);
            self.delete_range(lo, hi);
        }
        self.clamp_cursor();
    }

    pub(crate) fn replace_char(&mut self, c: char, count: usize) {
        let len = self.line_len();
        let lo = self.cursor_char();
        let line_end = self.buffer().byte_at(self.cursor.line, len);
        let (hi, crossed) = self.advance_graphemes(lo, count, line_end);
        // `r` does nothing unless `count` whole characters remain on the line.
        if crossed < count {
            return;
        }
        self.push_undo();
        self.buffer_mut().remove(lo..hi);
        let repl: String = std::iter::repeat(c).take(count).collect();
        self.buffer_mut().insert(lo, &repl);
        self.buffer_mut().modified = true;
        self.cursor.col =
            (lo - self.buffer().line_start(self.cursor.line)) + (count - 1) * c.len_utf8();
        self.clamp_cursor();
    }

    pub(crate) fn toggle_case(&mut self, count: usize) {
        if self.cursor.col >= self.line_len() {
            return;
        }
        self.push_undo();
        for _ in 0..count {
            if self.cursor.col >= self.line_len() {
                break;
            }
            let idx = self.cursor_char();
            let c = self.char_at(idx);
            let swapped: String = if c.is_uppercase() {
                c.to_lowercase().collect()
            } else {
                c.to_uppercase().collect()
            };
            self.buffer_mut().remove(idx..idx + c.len_utf8());
            self.buffer_mut().insert(idx, &swapped);
            let s = self.buffer().line(self.cursor.line);
            self.cursor.col = unicode::next_grapheme(&s, self.cursor.col);
        }
        self.buffer_mut().modified = true;
        self.clamp_cursor();
    }

    pub(crate) fn join_lines(&mut self, count: usize) {
        let joins = count.saturating_sub(1).max(1);
        self.push_undo();
        for _ in 0..joins {
            if self.cursor.line + 1 >= self.buffer().line_count() {
                break;
            }
            let cur_len = self.line_len();
            let eol = self.buffer().byte_at(self.cursor.line, cur_len);
            // Remove the newline and any leading whitespace of the next line.
            let next_start = self.buffer().line_start(self.cursor.line + 1);
            let mut ws_end = next_start;
            while ws_end < self.last_char_idx() {
                let c = self.char_at(ws_end);
                if c == ' ' || c == '\t' {
                    ws_end += 1;
                } else {
                    break;
                }
            }
            self.buffer_mut().remove(eol..ws_end);
            // Insert a single separating space unless the line was empty.
            if cur_len > 0 {
                self.buffer_mut().insert_char(eol, ' ');
            }
            self.cursor.col = cur_len;
        }
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.clamp_cursor();
    }

    pub(crate) fn open_line(&mut self, below: bool) {
        self.push_undo();
        if below {
            let at = self.buffer().byte_at(self.cursor.line, self.line_len());
            self.buffer_mut().insert_char(at, '\n');
            self.cursor.line += 1;
        } else {
            let at = self.buffer().line_start(self.cursor.line);
            self.buffer_mut().insert_char(at, '\n');
        }
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        // Auto-indent the fresh line: treesitter, else copy-previous-line, else 0.
        // The `\n` is synced to the engine inside `indent_for` before it queries,
        // so the tree reflects the line being opened.
        let width = self.indent_for(self.cursor.line);
        self.cursor.col = self.set_line_indent(self.cursor.line, width);
        self.mode = Mode::Insert;
        self.snapshot_taken = true;
    }

    /// Install the host clipboard backing the `"+` / `"*` registers. The server
    /// hands over a real OS provider at startup; a bare-core test leaves it
    /// `None` and selecting `"+` / `"*` then errors loudly.
    pub fn set_clipboard(&mut self, clipboard: Box<dyn Clipboard>) {
        self.clipboard = Some(clipboard);
    }

    /// Read the host clipboard backing `"+` / `"*` — a test seam (the plugin test
    /// framework's `t`/`nx.test.clipboard.peek`) and introspection point. `None`
    /// with no provider installed or an empty clipboard.
    pub fn clipboard_contents(&self) -> Option<(String, bool)> {
        self.clipboard.as_ref().and_then(|c| c.get())
    }

    /// Write the host clipboard as if an external app set it — the test seam that
    /// seeds `"+` / `"*` before a plugin reads them. A no-op with no provider.
    pub fn clipboard_seed(&self, text: &str, linewise: bool) {
        if let Some(c) = self.clipboard.as_ref() {
            c.set(text, linewise);
        }
    }

    /// Resolve the active register's contents for a paste. Read-only specials
    /// project from live editor state — `"%` the file name, `"/` the last search
    /// pattern, `":` the last ex command — the clipboard registers `"+` / `"*`
    /// read the injected provider, and every other name reads the stored register
    /// file. `None` for an empty / absent register (paste does nothing).
    pub(crate) fn register_text(&self, reg: Option<char>) -> Option<(String, RegKind)> {
        match reg {
            Some('%') => {
                let name = self.buffer_name(self.cur_buffer())?;
                (!name.is_empty()).then_some((name, RegKind::Char))
            }
            Some('/') => self
                .last_search
                .as_ref()
                .map(|(pat, _, _)| (pat.clone(), RegKind::Char)),
            Some(':') => self
                .ex_history
                .last()
                .map(|cmd| (cmd.clone(), RegKind::Char)),
            Some('.') => {
                (!self.insert_text.is_empty()).then(|| (self.insert_text.clone(), RegKind::Char))
            }
            Some('+') | Some('*') => self.clipboard.as_ref()?.get().map(|(text, linewise)| {
                let kind = if linewise {
                    RegKind::Line
                } else {
                    RegKind::Char
                };
                (text, kind)
            }),
            _ => self.registers.get(reg).map(|c| (c.text.clone(), c.kind)),
        }
    }

    /// Write a yank/delete to the host clipboard (the `"+` / `"*` target). Mirrors
    /// the unnamed register too — vim sets `""` on any yank/delete regardless of
    /// the explicit target — so a plain `p` still works after `"+y`. With no
    /// provider installed, errors loudly rather than silently dropping the text.
    fn clipboard_write(&mut self, text: String, kind: RegKind) {
        let Some(clipboard) = self.clipboard.as_ref() else {
            self.echo(CLIPBOARD_UNAVAILABLE);
            return;
        };
        clipboard.set(&text, kind == RegKind::Line);
        self.registers.set_api('"', text, kind, false);
    }

    /// Whether the active register targets the clipboard but no provider is
    /// installed — the write must abort loudly instead of touching anything.
    fn clipboard_write_unavailable(&self) -> bool {
        is_clipboard_register(self.pending.register) && self.clipboard.is_none()
    }

    /// Whether the active register refuses writes (a read-only special). A
    /// yank/delete targeting one is aborted — vim beeps and changes nothing; with
    /// no bell, the abort is the whole signal. See [`is_readonly_register`].
    fn register_write_blocked(&self) -> bool {
        self.pending.register.is_some_and(is_readonly_register)
    }

    /// The register file projected for the Lua `getreg` / `getregtype` mirror:
    /// every stored cell plus the read-only specials, as `(name, text,
    /// linewise)`. Names are the stored keys (lowercase / digit / symbol); the
    /// server pushes this before any Lua that can read registers.
    pub fn register_mirror(&self) -> Vec<(char, String, bool)> {
        let mut out: Vec<(char, String, bool)> = self
            .registers
            .entries()
            .into_iter()
            .map(|(name, text, kind)| (name, text.to_string(), kind == RegKind::Line))
            .collect();
        for name in ['%', '/', ':', '.'] {
            if let Some((text, kind)) = self.register_text(Some(name)) {
                if !text.is_empty() {
                    out.push((name, text, kind == RegKind::Line));
                }
            }
        }
        out
    }

    /// Apply a `vim.fn.setreg` write to the register file. The Lua bridge has
    /// already rejected read-only specials and resolved uppercase/`a`-flag into
    /// `append`, so this is the mechanical store (the black hole `'_'` discards
    /// inside [`Registers::set_api`]).
    pub fn set_register_api(&mut self, name: char, text: String, linewise: bool, append: bool) {
        let kind = if linewise {
            RegKind::Line
        } else {
            RegKind::Char
        };
        self.registers.set_api(name, text, kind, append);
    }

    pub(crate) fn paste(&mut self, after: bool, count: usize) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let reg = self.pending.register;
        // A clipboard paste with no provider errors loudly rather than silently
        // pasting the unnamed register's contents instead.
        if self.clipboard_write_unavailable() {
            self.echo(CLIPBOARD_UNAVAILABLE);
            return;
        }
        let (text, linewise) = match self.register_text(reg) {
            Some((text, kind)) if !text.is_empty() => (text, kind == RegKind::Line),
            // An explicitly-named but empty/unreadable register reports loudly (vim's
            // E353, as `:put` does) instead of a silent no-op — notably `"+p` when the
            // clipboard couldn't be read yet (e.g. a browser that hasn't granted
            // clipboard access on a fresh load). A bare `p` with nothing yanked stays a
            // quiet no-op, matching vim's unnamed-register feel.
            _ => {
                if let Some(name) = reg {
                    self.echo(format!("E353: Nothing in register {name}"));
                }
                return;
            }
        };
        self.paste_text(&text, linewise, count, after);
    }

    /// Insert register-resolved `text` at the cursor `count` times — the body of
    /// [`Editor::paste`] once the source text and its line/char kind are known.
    /// Split out so a multi-cursor paste can feed each cursor its own per-cursor
    /// register text (see [`Editor::cursor_registers`]) through the same logic.
    pub(crate) fn paste_text(&mut self, text: &str, linewise: bool, count: usize, after: bool) {
        if text.is_empty() {
            return;
        }
        self.push_undo();
        if linewise {
            let at = if after {
                self.buffer()
                    .line_start((self.cursor.line + 1).min(self.buffer().line_count()))
            } else {
                self.buffer().line_start(self.cursor.line)
            };
            let chunk = text.repeat(count);
            self.buffer_mut().insert(at, &chunk);
            self.buffer_mut().normalize();
            self.cursor.line = if after {
                self.cursor.line + 1
            } else {
                self.cursor.line
            };
            self.cursor.col = self.first_non_blank(self.cursor.line);
        } else {
            let len = self.line_len();
            let cur = self.cursor_char();
            let line_end = self.buffer().byte_at(self.cursor.line, len);
            // Paste *after* lands past the whole grapheme under the cursor, never
            // between a base char and its combining mark.
            let at = if after && len > 0 {
                self.next_grapheme_idx(cur).min(line_end)
            } else {
                cur
            };
            let chunk = text.repeat(count);
            // Byte length of the chunk's final grapheme, so the cursor lands on
            // it (not on a trailing combining mark) — vim leaves it on the last
            // pasted character.
            let last_len = chunk.len() - unicode::prev_grapheme(&chunk, chunk.len());
            let end = at + chunk.len();
            self.buffer_mut().insert(at, &chunk);
            self.set_cursor_char(end.saturating_sub(last_len));
        }
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.clamp_cursor();
    }

    /// `p`/`P` with a multi-cursor set active. When the per-cursor register set
    /// from the last multi-cursor yank/delete still matches the live cursor count,
    /// each cursor pastes its **own** captured text (so `yy`+`p` over two cursors
    /// duplicates each line under itself, not one line under both). Otherwise — a
    /// single-source yank, or the set changed — every cursor pastes the active
    /// register, vim's plain `p` broadcast to all.
    pub(crate) fn paste_multi(&mut self, after: bool, count: usize) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let mut positions = self.secondary_cursor_bytes();
        positions.push(self.cursor_char());
        positions.sort_unstable();
        if positions.len() == self.cursor_registers.len() {
            // Pair each cursor (ascending) with its own captured slice. Visiting
            // highest-byte-first (inside `edit_each_cursor`/`for_each_cursor`) means
            // a paste never shifts a not-yet-visited lower cursor, so the original
            // position is still the live one when we look it up.
            let by_pos: std::collections::HashMap<usize, RegisterCell> = positions
                .into_iter()
                .zip(self.cursor_registers.iter().cloned())
                .collect();
            self.edit_each_cursor(move |ed| {
                if let Some(cell) = by_pos.get(&ed.cursor_char()) {
                    ed.paste_text(&cell.text, cell.kind == RegKind::Line, count, after);
                }
            });
        } else {
            self.edit_each_cursor(|ed| ed.paste(after, count));
        }
    }
}
