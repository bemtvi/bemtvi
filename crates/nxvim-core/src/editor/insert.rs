//! Insert and Replace mode key handling, including soft-tab expansion and the
//! soft-tab-aware backspace.

use super::*;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::unicode;

impl Editor {
    pub(crate) fn enter_insert_at(&mut self, col: usize) {
        self.push_undo();
        self.snapshot_taken = true;
        self.cursor.col = col.min(self.line_len());
        self.mode = Mode::Insert;
    }

    pub(crate) fn handle_insert(&mut self, key: Key) {
        // The soft-tab marker is valid only for the keystroke immediately after a
        // `<Tab>` (or a chained soft-tab `<BS>`). Take it here so every key clears
        // it; only the Tab/Backspace arms thread it through and may re-arm it.
        let soft_tab = self.soft_tab.take();
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                // `` `^ `` — where Insert mode was last left — is the insert-stop
                // column, captured before the normal-mode backstep below.
                self.record_last_insert();
                if self.cursor.col > 0 {
                    let s = self.buffer().line(self.cursor.line);
                    self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
                }
                self.clamp_cursor();
                self.snapshot_taken = false;
            }
            KeyCode::Enter => {
                let at = self.cursor_char();
                self.buffer_mut().insert_char(at, '\n');
                self.insert_text.push('\n'); // `".` last-insert register
                self.cursor.line += 1;
                self.buffer_mut().modified = true;
                self.buffer_mut().normalize();
                // Auto-indent the new line (treesitter, else copy-previous, else 0)
                // and park the cursor past the indent — vim's `Enter` behavior.
                let width = self.indent_for(self.cursor.line);
                self.cursor.col = self.set_line_indent(self.cursor.line, width);
            }
            KeyCode::Backspace => self.insert_backspace(soft_tab),
            KeyCode::Tab => self.insert_tab(soft_tab),
            KeyCode::Left => {
                let s = self.buffer().line(self.cursor.line);
                self.cursor.col = unicode::prev_grapheme(&s, self.cursor.col);
            }
            KeyCode::Right => {
                let s = self.buffer().line(self.cursor.line);
                self.cursor.col = unicode::next_grapheme(&s, self.cursor.col).min(s.len());
            }
            KeyCode::Up => self.move_vertical(-1, true),
            KeyCode::Down => self.move_vertical(1, true),
            KeyCode::Delete => {
                let len = self.line_len();
                if self.cursor.col < len {
                    let at = self.cursor_char();
                    let s = self.buffer().line(self.cursor.line);
                    let end = self.buffer().line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer_mut().remove(at..end);
                    self.buffer_mut().modified = true;
                }
            }
            KeyCode::Char(c) => {
                let at = self.cursor_char();
                if self.mode == Mode::Replace && self.cursor.col < self.line_len() {
                    let s = self.buffer().line(self.cursor.line);
                    let end = self.buffer().line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer_mut().remove(at..end);
                }
                self.buffer_mut().insert_char(at, c);
                self.cursor.col += c.len_utf8();
                self.buffer_mut().modified = true;
                self.insert_text.push(c); // `".` last-insert register
            }
            _ => {}
        }
    }

    /// Insert a tab at the cursor. The width it advances by is the buffer's
    /// resolved [`softtabstop`](crate::options::BufferOptions::effective_softtabstop)
    /// (the `softtabstop → shiftwidth → tabstop` chain), measured from the
    /// cursor's current virtual column so a partial tab only fills the remaining
    /// cells. With `expandtab` the fill is spaces; otherwise it's real tabs (each
    /// jumping a `tabstop` boundary) plus any trailing spaces.
    ///
    /// `prev` is the soft-tab marker carried from the previous keystroke; a fill
    /// of pure spaces re-arms the marker (chaining onto a contiguous prior run) so
    /// the next `<BS>` can undo this tab as a unit.
    fn insert_tab(&mut self, prev: Option<(usize, usize)>) {
        let opts = self.buffer().options;
        let unit = opts.effective_softtabstop();
        let start = self.cursor_virtcol();
        let target = start - (start % unit) + unit; // next multiple of the unit
        let ws = fill_indent(start, target, opts.effective_tabstop(), opts.expandtab);
        let begin = self.cursor.col;
        let at = self.cursor_char();
        // `ws` is ASCII (tabs/spaces), so its byte length is its column advance.
        let n = ws.len();
        self.buffer_mut().insert(at, &ws);
        self.cursor.col += n;
        self.buffer_mut().modified = true;
        // The expanded tab is part of the `".` last-insert register.
        self.insert_text.push_str(&ws);
        // Only a pure-spaces fill is unit-deletable (a literal `\t` is one
        // grapheme the normal backspace already removes wholesale). Anchor at the
        // start of a contiguous prior soft-tab run, else where this tab began.
        self.soft_tab = if n > 0 && ws.bytes().all(|b| b == b' ') {
            let line = self.cursor.line;
            let anchor = match prev {
                Some((l, a)) if l == line && a <= begin && self.is_spaces(line, a, begin) => a,
                _ => begin,
            };
            Some((line, anchor))
        } else {
            None
        };
    }

    /// Whether `line[start..end]` (byte columns) is entirely ASCII spaces.
    fn is_spaces(&self, line: usize, start: usize, end: usize) -> bool {
        let s = self.buffer().line(line);
        s.as_bytes()
            .get(start..end)
            .is_some_and(|run| run.iter().all(|&b| b == b' '))
    }

    fn insert_backspace(&mut self, soft_tab: Option<(usize, usize)>) {
        if self.cursor.col > 0 {
            if self.softtab_backspace(soft_tab) {
                return;
            }
            let at = self.cursor_char();
            let start = self.buffer().line_start(self.cursor.line);
            let s = self.buffer().line(self.cursor.line);
            let prev_col = unicode::prev_grapheme(&s, self.cursor.col);
            let removed = s[prev_col..self.cursor.col].chars().count();
            self.buffer_mut().remove(start + prev_col..at);
            self.cursor.col = prev_col;
            self.buffer_mut().modified = true;
            self.trim_insert_text(removed); // `".` last-insert register
        } else if self.cursor.line > 0 {
            let prev_len = self.buffer().line_len(self.cursor.line - 1);
            let join_at = self.buffer().byte_at(self.cursor.line - 1, prev_len);
            self.buffer_mut().remove(join_at..join_at + 1);
            self.cursor.line -= 1;
            self.cursor.col = prev_len;
            self.buffer_mut().modified = true;
            self.trim_insert_text(1); // the joined newline
        }
    }

    /// Pop the last `n` characters off the `".` last-insert accumulator, e.g. when
    /// a `<BS>` rubs out text typed earlier in the same session. Bounded so
    /// backspacing past the session's own text (into pre-existing buffer
    /// characters) merely empties the accumulator rather than underflowing.
    fn trim_insert_text(&mut self, n: usize) {
        for _ in 0..n {
            if self.insert_text.pop().is_none() {
                break;
            }
        }
    }

    /// `<BS>` over a soft tab: delete a whole [`softtabstop`] unit of spaces back
    /// to the previous tab boundary, rather than one space at a time — but *only*
    /// for whitespace a `<Tab>` inserted, tracked by `soft_tab` (the marker from
    /// the immediately preceding keystroke). Hand-typed spaces carry no marker, so
    /// they fall through to the normal one-character delete. The delete never
    /// crosses the run's anchor, and re-arms the marker while soft-tab spaces
    /// remain so a second `<BS>` peels off the next unit. Returns `true` when it
    /// handled the delete.
    ///
    /// [`softtabstop`]: crate::options::BufferOptions::effective_softtabstop
    fn softtab_backspace(&mut self, soft_tab: Option<(usize, usize)>) -> bool {
        let Some((line, anchor)) = soft_tab else {
            return false;
        };
        if line != self.cursor.line || anchor >= self.cursor.col {
            return false;
        }
        let opts = self.buffer().options;
        let unit = opts.effective_softtabstop();
        if unit <= 1 {
            return false;
        }
        let s = self.buffer().line(self.cursor.line);
        // The marked run must still be spaces from the anchor up to the cursor.
        if !self.is_spaces(line, anchor, self.cursor.col) {
            return false;
        }
        // Delete back one unit by virtual column, but never past the anchor.
        let ts = opts.effective_tabstop();
        let vcol = unicode::virtcol(&s, self.cursor.col, ts);
        let anchor_vcol = unicode::virtcol(&s, anchor, ts);
        let target_vcol = (((vcol - 1) / unit) * unit).max(anchor_vcol);
        let mut col = self.cursor.col;
        let mut vc = vcol;
        while vc > target_vcol && col > anchor {
            col -= 1;
            vc -= 1;
        }
        if col == self.cursor.col {
            return false;
        }
        let line_start = self.buffer().line_start(self.cursor.line);
        let range = line_start + col..line_start + self.cursor.col;
        let removed = self.cursor.col - col; // pure ASCII spaces: bytes == chars
        self.buffer_mut().remove(range);
        self.cursor.col = col;
        self.buffer_mut().modified = true;
        self.trim_insert_text(removed); // `".` last-insert register
                                        // Keep peeling on the next <BS> while soft-tab spaces remain before us.
        if col > anchor {
            self.soft_tab = Some((line, anchor));
        }
        true
    }
}
