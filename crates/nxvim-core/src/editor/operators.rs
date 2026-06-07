//! Operators (`d`/`c`/`y`/`>`/…) and the editing primitives they drive
//! (delete/yank/paste/join/replace/case).

use super::command::{is_clipboard_register, is_readonly_register};
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
            MotionKind::Inclusive => (min(cur, m.target), max(cur, m.target) + 1, false, 0),
            MotionKind::Linewise => {
                let l1 = self.cursor.line;
                let l2 = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                let (a, b) = (min(l1, l2), max(l1, l2));
                let lo = self.buffer().line_start(a);
                let hi = self
                    .buffer()
                    .line_start((b + 1).min(self.buffer().line_count()));
                (lo, hi, true, a)
            }
        };
        self.apply_operator_to_range(op, lo, hi, linewise, first_line);
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
                let first = self.buffer().byte_to_line(lo);
                let last = self.buffer().byte_to_line(hi.saturating_sub(1));
                self.reindent_lines(first, last);
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

    /// Settle the cursor after a linewise delete: first non-blank of the line that
    /// now occupies the deleted lines' position. Shared by `apply_operator` and
    /// `visual_operate`.
    fn settle_after_linewise_delete(&mut self, first_line: usize) {
        self.cursor.line = first_line.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
    }

    /// Linewise change (`cc`/`S`, linewise-visual `c`): delete `lo..hi`, reopen a
    /// single empty line at `first_line`, and park the cursor there for insert.
    /// Shared by `apply_operator` and `visual_operate`.
    fn linewise_change(&mut self, lo: usize, hi: usize, first_line: usize) {
        self.delete_range(lo, hi);
        let at = self.buffer().line_start(first_line.min(self.last_line()));
        self.buffer_mut().insert_char(at, '\n');
        self.buffer_mut().normalize();
        self.cursor.line = first_line;
        self.cursor.col = 0;
    }

    pub(crate) fn visual_operate(&mut self, op: char) {
        let (lo, hi, linewise, first_line) = self.visual_range();
        // `=` reindents the selected lines; unlike d/y/c it neither yanks nor needs
        // the shared snapshot (`reindent_lines` takes its own), so handle it first.
        if op == '=' {
            let first = self.buffer().byte_to_line(lo);
            let last = self.buffer().byte_to_line(hi.saturating_sub(1));
            self.reindent_lines(first, last);
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

    fn visual_range(&self) -> (usize, usize, bool, usize) {
        let a = self.visual_anchor;
        let b = self.cursor;
        if self.mode == Mode::VisualLine {
            let (la, lb) = (min(a.line, b.line), max(a.line, b.line));
            let lo = self.buffer().line_start(la);
            let hi = self
                .buffer()
                .line_start((lb + 1).min(self.buffer().line_count()));
            (lo, hi, true, la)
        } else {
            let ca = self.buffer().byte_at(a.line, a.col);
            let cb = self.buffer().byte_at(b.line, b.col);
            let lo = min(ca, cb);
            let hi = max(ca, cb) + 1;
            (lo, hi.min(self.last_char_idx().max(lo + 1)), false, 0)
        }
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
            let reg = self.pending.register;
            if is_clipboard_register(reg) {
                self.clipboard_write(text, kind);
            } else {
                self.registers.record_delete(reg, text, kind);
            }
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
        for name in ['%', '/', ':'] {
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
        let reg = self.pending.register;
        // A clipboard paste with no provider errors loudly rather than silently
        // pasting the unnamed register's contents instead.
        if self.clipboard_write_unavailable() {
            self.echo(CLIPBOARD_UNAVAILABLE);
            return;
        }
        let (text, linewise) = match self.register_text(reg) {
            Some((text, kind)) if !text.is_empty() => (text, kind == RegKind::Line),
            _ => return,
        };
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
}
