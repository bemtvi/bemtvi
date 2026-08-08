//! Insert and Replace mode key handling, including soft-tab expansion and the
//! soft-tab-aware backspace.

use super::*;
use crate::editor::syntax::indent_width;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::unicode;

/// The closing delimiter an opener auto-pairs to (`(`→`)`, `[`→`]`, `{`→`}`), or
/// `None` for anything that isn't an auto-paired opener.
fn close_of(open: char) -> Option<char> {
    match open {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        _ => None,
    }
}

/// Whether `c` is an auto-paired closing bracket.
fn is_close_bracket(c: char) -> bool {
    matches!(c, ')' | ']' | '}')
}

/// Whether `c` is an auto-paired quote delimiter.
fn is_quote(c: char) -> bool {
    matches!(c, '\'' | '"')
}

/// Whether `c` is a word (identifier) character — a letter, digit, or `_`. Used
/// by the auto-pairs guards that suppress a pair next to a word.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

impl Editor {
    /// Enter Insert mode at a per-cursor target column. `target` is evaluated at
    /// each cursor (the primary and every secondary) so `a`/`A`/`I` reposition
    /// *every* cursor to its own line's append/line-end/first-non-blank column, not
    /// just the primary's; the typed text then lands at each. With no secondary
    /// cursors this is just a single move to `target(self)` before entering insert.
    pub(crate) fn enter_insert_each(&mut self, target: impl Fn(&Editor) -> usize) {
        // A live terminal buffer is read-only; you type into it via terminal-job mode
        // (`i`/`a` are intercepted upstream), so any other insert-entry path is refused.
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        self.push_undo();
        self.snapshot_taken = true;
        // Switch to Insert *before* the sweep: `for_each_cursor` clamps each cursor
        // at the end, and only in Insert mode may a cursor sit at `col == line_len`
        // (the append column `A`/`a`-at-EOL needs). Clamped in Normal mode it would
        // be pulled one cell left, dropping appended text a column short.
        self.mode = Mode::Insert;
        self.for_each_cursor(|ed| {
            ed.cursor.col = target(ed).min(ed.line_len());
        });
    }

    /// Split the line at the cursor (insert `\n`) and auto-indent the new line —
    /// the per-cursor primitive behind insert-mode `Enter`, run at every cursor via
    /// [`Editor::for_each_cursor`].
    fn insert_newline(&mut self) {
        // Auto-pairs block expansion: a newline pressed *between* an open/close
        // bracket pair lays the closer on its own dedented line and parks the
        // cursor on a blank line one level deeper — `{|}` + <CR> becomes
        // `{` / `····|` / `}`.
        let expand = self.buffer().options.autopairs
            && self.mode == Mode::Insert
            && matches!(
                (self.char_before_cursor(), self.char_after_cursor()),
                (Some(o), Some(n)) if close_of(o) == Some(n)
            );
        let at = self.cursor_char();
        self.buffer_mut().insert_char(at, '\n');
        self.cursor.line += 1;
        // The split leaves the cursor at the start of the new line; the
        // non-expand path overwrites this via `set_line_indent`, but the
        // expansion below reads the cursor position, so set it explicitly.
        self.cursor.col = 0;
        self.buffer_mut().modified = true;
        self.buffer_mut().normalize();
        if expand {
            self.expand_pair_newline();
            return;
        }
        // Auto-indent the new line (treesitter, else smart/auto-indent, else 0)
        // and park the cursor past the indent — vim's `Enter` behavior.
        let width = self.indent_for(self.cursor.line);
        self.cursor.col = self.set_line_indent(self.cursor.line, width);
        // The line just left behind is blank when you press `<CR>` without
        // typing on it (two `<CR>`s in a row, or `<CR>` on a fresh auto-indent):
        // vim removes its auto-indent so the empty line is *truly* empty, not a
        // run of trailing whitespace — the block stays indented, the hole
        // doesn't. Gated by the same `indentemptylines` opt-in as `=`.
        if !self.buffer().options.indentemptylines {
            let left = self.cursor.line - 1;
            if self.buffer().line(left).trim().is_empty() {
                self.set_line_indent(left, 0);
            }
        }
        // Arm the did_ai scrub for the new line too, but only when it is now
        // whitespace-only — a `<CR>` in the *middle* of a line carries real text
        // onto the new line, which must keep its indent on `<Esc>`.
        self.ai_open_line = (width > 0 && self.buffer().line(self.cursor.line).trim().is_empty())
            .then_some(self.cursor.line);
    }

    /// Finish an auto-pairs `<CR>` block expansion. On entry the opener line is
    /// `cursor.line - 1` and the cursor sits at column 0 of the line now holding
    /// the closer. Split off a blank middle line, indent the closer to the
    /// opener's level and the middle line one shiftwidth deeper, and leave the
    /// cursor on the middle line past its indent.
    fn expand_pair_newline(&mut self) {
        let opts = self.buffer().options;
        let base = indent_width(
            &self.buffer().line(self.cursor.line - 1),
            opts.effective_tabstop(),
        );
        // A second newline before the closer leaves the cursor on a blank line
        // above it (the closer slides down to `cursor.line + 1`).
        let at = self.cursor_char();
        self.buffer_mut().insert_char(at, '\n');
        self.buffer_mut().normalize();
        self.set_line_indent(self.cursor.line + 1, base);
        self.cursor.col =
            self.set_line_indent(self.cursor.line, base + opts.effective_shiftwidth());
    }

    pub(crate) fn handle_insert(&mut self, key: Key) {
        // While a snippet expansion is being filled, the jump keys (`<Tab>` /
        // `<S-Tab>` by default) move between tabstops. They take precedence over both
        // the completion popup (which shares `<Tab>`/`<S-Tab>` for navigation) and
        // soft-tab insertion: a snippet session is the more specific context, and the
        // popup stays navigable via `<C-n>`/`<C-p>` (accept with `<C-y>`/`<CR>`). Any
        // open popup is dismissed first so it doesn't linger over the jumped-to stop.
        if let Some(dir) = self.snippet_jump_for(&key) {
            self.close_completion();
            self.snippet_jump(dir);
            return;
        }

        // Whether an *open* popup is a manual session, sampled before the block below
        // closes it: the tail of this fn re-derives that popup instead of leaving the
        // manual trigger dead after one keystroke. Sampling here (rather than reading
        // the flag at the tail) is what bounds the session to a popup that was on
        // screen when this key arrived — an aborted or accepted one stays gone.
        let manual_session = self.complete_manual_session();

        // Native completion popup: while it is open, its control keys navigate /
        // accept / abort and are consumed here; every other key edits the document
        // normally and then re-triggers the engine at the end of this fn. (The
        // popup does not grab input — the buffer is the query.)
        if self.completion_active() {
            use super::complete::CompleteAction;
            match self.complete_action(&key) {
                Some(CompleteAction::Next) => {
                    self.complete_select_next();
                    return;
                }
                Some(CompleteAction::Prev) => {
                    self.complete_select_prev();
                    return;
                }
                Some(CompleteAction::Abort) => {
                    self.close_completion();
                    return;
                }
                Some(CompleteAction::Confirm) => {
                    // Accept only when a row is actively selected. With nothing
                    // selected (a just-opened popup, noselect), dismiss and let the
                    // key act normally — `<CR>` then inserts a newline rather than
                    // the popup eating it; other confirm keys (`<C-y>`) just dismiss
                    // without self-inserting.
                    if self.complete_accept() {
                        // Accepting inside a snippet tabstop (a choice pick, or any
                        // completion) replaces the value — mirror it into the stop's
                        // other occurrences (the tail sync hook is past this `return`).
                        if self.snippet_active() {
                            self.snippet_sync();
                        }
                        return;
                    }
                    self.close_completion();
                    if key.code != KeyCode::Enter {
                        return;
                    }
                    // `<CR>` falls through to the newline handling below.
                }
                None => {
                    // Any non-control key (typing, `<BS>`, motion, `<Esc>`) closes
                    // the popup; the edit/motion then proceeds below, and a word
                    // keystroke re-opens it via `complete_trigger`. `<Esc>` thus
                    // closes the popup and still leaves Insert mode in one press.
                    self.close_completion();
                }
            }
        }

        // `<C-r>{register}`: the keystroke after `<C-r>` names the register whose
        // text is inserted at every cursor. Consume it before the normal handling
        // (and before the soft-tab take below) — a non-register key cancels and
        // inserts nothing, matching vim.
        if self.awaiting_register {
            self.awaiting_register = false;
            if key.ctrl && key.code == KeyCode::Char('w') {
                // `<C-r><C-w>`: the word under the cursor, not a named register.
                if let Some(word) = self.word_under_cursor() {
                    self.insert_text_session(&word);
                }
            } else if let Some(name) = key.as_char() {
                self.insert_register(name);
            }
            return;
        }
        // The did_ai arm (an untouched auto-indent) is consumed by *this* key: any
        // key clears it, only `<Esc>` acts on it. `<CR>` clears it here and then
        // re-arms it for the line it opens (`insert_newline`).
        let ai_open = self.ai_open_line.take();
        match key.code {
            KeyCode::Esc => {
                // Leaving Insert ends any snippet session (its tabstops are dropped).
                self.end_snippet();
                // In a Helix session (an Insert opened by `c`/`i`/…) return to
                // HelixNormal, not vim Normal.
                self.mode = self.base_normal_mode();
                // Scrub an auto-indent the cursor never typed into (vim's did_ai):
                // `o`/`O`/`<CR>` then `<Esc>` leaves a *truly* empty line, not a run
                // of trailing whitespace. Opt out with `indentemptylines`.
                if ai_open == Some(self.cursor.line)
                    && !self.buffer().options.indentemptylines
                    && self.buffer().line(self.cursor.line).trim().is_empty()
                {
                    self.set_line_indent(self.cursor.line, 0);
                    self.cursor.col = 0;
                }
                // `` `^ `` — where Insert mode was last left — is the insert-stop
                // column, captured before the normal-mode backstep below.
                self.record_last_insert();
                // Back every cursor (the primary and any secondary multi-cursors)
                // off its insert-stop column onto the last inserted cell — the
                // typed text landed at all of them via `for_each_cursor`, so the
                // leaving-insert backstep must too, not just the primary.
                self.for_each_cursor(|ed| {
                    if ed.cursor.col > 0 {
                        let s = ed.buffer().line(ed.cursor.line);
                        ed.cursor.col = unicode::prev_grapheme(&s, ed.cursor.col);
                    }
                });
                self.clamp_cursor();
                // Helix leaves a caret at each cursor after an insert (`c`/`i`/`o`/…),
                // never a span stretched over the inserted text. Collapse *every*
                // selection onto its head — anchor == head, marks kept: a mark-less
                // secondary would make the next operator span from its head back to the
                // primary's anchor (`for_each_cursor` only restores `visual_anchor` from
                // a present mark). This covers `o`/`O`, whose fresh line moves each head
                // off the line its anchor sat on.
                if self.mode.is_helix() {
                    self.helix_collapse_to_cursor();
                }
                self.snapshot_taken = false;
            }
            // `Enter` and `Backspace`, like a typed `Char`, run at every cursor via
            // `for_each_cursor` (the insert session already holds the undo snapshot,
            // so no `edit_each_cursor` wrap). The `".` last-insert register records
            // the keystroke once, not once per cursor.
            KeyCode::Enter => {
                self.for_each_cursor(|ed| ed.insert_newline());
                self.insert_text.push('\n'); // `".` last-insert register
            }
            KeyCode::Backspace => self.for_each_cursor(|ed| ed.insert_backspace()),
            KeyCode::Tab => self.insert_tab(),
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
            // `i_<Home>` / `i_<End>`: jump to the first / last column of the
            // cursor's own line without leaving Insert. `<Home>` is column zero
            // (vim's `0`, not `^` — it does not stop at the first non-blank), and
            // `<End>` is the past-the-last-character append column Insert allows.
            KeyCode::Home => self.cursor.col = 0,
            KeyCode::End => self.cursor.col = self.line_len(),
            KeyCode::Delete => {
                let len = self.line_len();
                if self.cursor.col < len {
                    let at = self.cursor_char();
                    let s = self.buffer().line(self.cursor.line);
                    let end = self.buffer().line_start(self.cursor.line)
                        + unicode::next_grapheme(&s, self.cursor.col);
                    self.buffer_mut().remove(at..end);
                    self.buffer_mut().modified = true;
                } else if self.cursor.line + 1 < self.buffer().line_count() {
                    // At the end of the line `<Del>` has no character ahead of it,
                    // so it deletes the line break and pulls the next line up — the
                    // forward mirror of `<BS>` at column 0. The final line has only
                    // the phantom trailing newline after it, so this is a no-op
                    // there (guarded by the `line + 1 < line_count` check).
                    let join_at = self.cursor_char();
                    self.buffer_mut().remove(join_at..join_at + 1);
                    self.buffer_mut().modified = true;
                }
            }
            // `<C-r>` arms register insertion: the next keystroke (handled at the
            // top of this fn) names the register to pull in.
            KeyCode::Char('r') if key.ctrl => self.awaiting_register = true,
            // `i_CTRL-O`: drop to Normal for exactly one command, then resume Insert
            // here. Remember the insert flavour (Insert / Replace) to return to; the
            // resume happens in [`Editor::input`] once the one-shot command settles.
            // Unlike `<Esc>` there is no cursor backstep — the command acts from the
            // current insert position, and an EOL-append column survives via the
            // `insert_normal` allowance in `clamp_cursor`.
            KeyCode::Char('o') if key.ctrl => {
                self.end_snippet();
                self.close_completion();
                self.insert_normal = Some(self.mode);
                self.mode = Mode::Normal;
            }
            // A typed character lands at every cursor (the primary and any
            // secondary multi-cursors); with none, `for_each_cursor` is just the
            // single-cursor insert. The `".` last-insert register records the
            // keystroke once, not once per cursor.
            KeyCode::Char(c) => {
                self.for_each_cursor(|ed| ed.insert_char_at_cursor(c));
                self.insert_text.push(c); // `".` last-insert register
            }
            _ => {}
        }

        // Auto-completion: after a word keystroke, a deletion, or a horizontal
        // move, recompute the popup (open / refresh / close based on the new
        // prefix). Cheap when the engine is disabled or the prefix is too short;
        // skipped while arming a `<C-r>` register insert (not a prefix edit).
        // A manual session refreshes on the same edits even with `auto` off — and
        // through the *manual* path, so the session keeps bypassing `min_chars` and
        // keeps its preselection rather than degrading to a noselect auto popup
        // halfway through.
        if !self.awaiting_register
            && matches!(
                key.code,
                KeyCode::Char(_)
                    | KeyCode::Backspace
                    | KeyCode::Delete
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
            )
        {
            if manual_session {
                self.complete_manual_trigger();
            } else if self.complete_config.auto {
                self.complete_trigger();
            }
        }

        // Signature-help auto-trigger (opt-in): a `(` / `,` (the server's advertised
        // trigger chars) fires `textDocument/signatureHelp` as you type the call. A
        // no-op unless the host pushed a trigger set (enabled + supported).
        self.signature_after_insert(&key);

        // Keep the active tabstop's mirrors in sync with whatever was just typed.
        if self.snippet_active() {
            self.snippet_sync();
        }
    }

    /// Insert (or, in Replace mode, overtype) one character at the current cursor
    /// and advance past it. The per-cursor primitive [`handle_insert`] runs at
    /// every cursor via [`Editor::for_each_cursor`].
    fn insert_char_at_cursor(&mut self, c: char) {
        let opts = self.buffer().options;
        // Auto-pairs intercept (Insert mode only — Replace overtypes literally).
        // When it handles the key it has already moved the cursor / inserted the
        // pair, so there is nothing more to do.
        if self.mode == Mode::Insert && opts.autopairs && self.autopair_insert(c) {
            return;
        }
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
        // smartindent electric dedent: a closing bracket typed as the first
        // non-blank char of a line re-indents that line to its opener's level.
        if self.mode == Mode::Insert && opts.smartindent && is_close_bracket(c) {
            self.smartindent_close(c);
        }
    }

    /// Auto-pairs handling for a typed `c` in Insert mode (the caller gates on the
    /// buffer's `autopairs` option). Returns `true` when it fully handled the
    /// keystroke — inserted a delimiter pair and parked the cursor between its
    /// halves, or stepped past an already-present closer — and `false` to fall
    /// through to the ordinary single-character insert.
    ///
    /// The auto-inserted closer is *not* recorded in the `".` accumulator; dot-
    /// repeat replays the raw keystrokes and re-runs auto-pairs, so re-recording
    /// the closer would double it.
    fn autopair_insert(&mut self, c: char) -> bool {
        let next = self.char_after_cursor();
        // Step past a closer/quote the cursor already sits on, so typing the
        // closer of an auto-inserted pair "types through" it instead of doubling.
        if (is_close_bracket(c) || is_quote(c)) && next == Some(c) {
            self.cursor.col += c.len_utf8();
            return true;
        }
        if let Some(close) = close_of(c) {
            // Don't pair an opener butted against a word char (`(` typed before
            // `foo` stays a lone `(`) — the common auto-pairs guard.
            if next.is_some_and(is_word_char) {
                return false;
            }
            self.insert_pair(c, close);
            return true;
        }
        if is_quote(c) {
            // No pair for an apostrophe inside/next to a word (`don't`), before a
            // word char, or when we appear to be closing a string already open on
            // this line (an odd count of this quote before the cursor).
            if self.char_before_cursor().is_some_and(is_word_char)
                || next.is_some_and(is_word_char)
                || self.quote_count_before_cursor(c) % 2 == 1
            {
                return false;
            }
            self.insert_pair(c, c);
            return true;
        }
        false
    }

    /// Insert the `open`/`close` delimiter pair at the cursor and park the cursor
    /// between them.
    fn insert_pair(&mut self, open: char, close: char) {
        let at = self.cursor_char();
        self.buffer_mut().insert_char(at, open);
        self.buffer_mut().insert_char(at + open.len_utf8(), close);
        self.cursor.col += open.len_utf8();
        self.buffer_mut().modified = true;
    }

    /// The character immediately to the right of the cursor on the current line
    /// (the cell the cursor sits on), or `None` at end-of-line.
    fn char_after_cursor(&self) -> Option<char> {
        let s = self.buffer().line(self.cursor.line);
        s.get(self.cursor.col..)
            .and_then(|rest| rest.chars().next())
    }

    /// The character immediately to the left of the cursor on the current line,
    /// or `None` at column 0.
    fn char_before_cursor(&self) -> Option<char> {
        let s = self.buffer().line(self.cursor.line);
        s.get(..self.cursor.col)
            .and_then(|head| head.chars().next_back())
    }

    /// Count of unescaped `q` quotes on the current line before the cursor — an
    /// odd count means we're likely *inside* a `q`-delimited string, so a typed
    /// `q` closes it rather than opening a fresh pair.
    fn quote_count_before_cursor(&self, q: char) -> usize {
        let s = self.buffer().line(self.cursor.line);
        let Some(prefix) = s.get(..self.cursor.col) else {
            return 0;
        };
        let mut count = 0usize;
        let mut escaped = false;
        for ch in prefix.chars() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == q {
                count += 1;
            }
        }
        count
    }

    /// `<BS>` between an empty auto-paired delimiter pair (`(|)`, `"|"`, …):
    /// delete both halves at once. Returns `true` when it handled the delete. The
    /// caller gates on the `autopairs` option and a non-zero cursor column.
    fn autopair_backspace(&mut self) -> bool {
        let (Some(prev), Some(next)) = (self.char_before_cursor(), self.char_after_cursor()) else {
            return false;
        };
        let paired = close_of(prev) == Some(next) || (is_quote(prev) && prev == next);
        if !paired {
            return false;
        }
        let line_start = self.buffer().line_start(self.cursor.line);
        let from = self.cursor.col - prev.len_utf8();
        let to = self.cursor.col + next.len_utf8();
        self.buffer_mut().remove(line_start + from..line_start + to);
        self.cursor.col = from;
        self.buffer_mut().modified = true;
        self.trim_insert_text(1); // the opener char the user had typed
        true
    }

    /// `smartindent` electric dedent: after a closing bracket is typed as the
    /// first non-blank character of its line, re-indent the line to the column of
    /// the line holding its matching opener, and keep the cursor just past the
    /// closer. The caller gates on the `smartindent` option.
    fn smartindent_close(&mut self, c: char) {
        let s = self.buffer().line(self.cursor.line);
        let closer_at = self.cursor.col - c.len_utf8();
        // Only when nothing but whitespace precedes the just-typed closer.
        if !s.get(..closer_at).unwrap_or("").trim().is_empty() {
            return;
        }
        let Some(open_line) = self.matching_open_line(c, closer_at) else {
            return;
        };
        let tabstop = self.buffer().options.effective_tabstop();
        let width = indent_width(&self.buffer().line(open_line), tabstop);
        let col = self.set_line_indent(self.cursor.line, width);
        self.cursor.col = col + c.len_utf8();
    }

    /// The line holding the unmatched opener that pairs with the closer `c` typed
    /// at byte column `closer_at` on the cursor line — a backward bracket-depth
    /// scan from just before the closer. `None` when the brackets don't balance.
    /// Strings and comments aren't recognized; this is a best-effort smartindent
    /// heuristic, not a parser.
    fn matching_open_line(&self, c: char, closer_at: usize) -> Option<usize> {
        let open = match c {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            _ => return None,
        };
        let mut depth = 1i32;
        let mut upto = closer_at; // the cursor line is scanned only up to the closer
        let mut l = self.cursor.line as isize;
        while l >= 0 {
            let s = self.buffer().line(l as usize);
            let slice = s.get(..upto).unwrap_or(s.as_str());
            for ch in slice.chars().rev() {
                if ch == c {
                    depth += 1;
                } else if ch == open {
                    depth -= 1;
                    if depth == 0 {
                        return Some(l as usize);
                    }
                }
            }
            upto = usize::MAX; // whole line for every line above the cursor line
            l -= 1;
        }
        None
    }

    /// Insert the named register's text at every cursor — the `<C-r>{register}`
    /// action. An empty or absent register inserts nothing (vim beeps; we no-op).
    /// The text goes in verbatim, including any newlines a linewise register
    /// carries, so it splits the line exactly where the register's contents end
    /// (no auto-indent, unlike a typed `<CR>`). The insert session already holds
    /// the undo snapshot, so this groups into the surrounding insert.
    fn insert_register(&mut self, name: char) {
        let Some((text, _kind)) = self.register_text(Some(name)) else {
            return;
        };
        self.insert_text_session(&text);
    }

    /// Insert `text` at every cursor as part of the current insert session — the
    /// shared body of `<C-r>{register}` and `<C-r><C-w>`. Records the text once in
    /// the `".` last-insert accumulator (not once per cursor), matching how a
    /// typed character is recorded.
    fn insert_text_session(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.for_each_cursor(|ed| ed.insert_register_text(text));
        self.insert_text.push_str(text); // `".` last-insert register
    }

    /// Insert raw `text` (newlines and all) at the cursor and leave the cursor
    /// just past it — the per-cursor primitive behind [`Editor::insert_register`],
    /// run at every cursor via [`Editor::for_each_cursor`].
    fn insert_register_text(&mut self, text: &str) {
        let at = self.cursor_char();
        self.buffer_mut().insert(at, text);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        // Land past the inserted text so continued typing follows it. Insert-mode
        // placement (`set_cursor_char_insert`) lets the cursor sit at end-of-line.
        self.set_cursor_char_insert(at + text.len());
    }

    /// Insert a tab at the cursor. The width it advances by is the buffer's
    /// resolved [`softtabstop`](crate::options::BufferOptions::effective_softtabstop)
    /// (the `softtabstop → shiftwidth → tabstop` chain), measured from the
    /// cursor's current virtual column so a partial tab only fills the remaining
    /// cells. With `expandtab` the fill is spaces; otherwise it's real tabs (each
    /// jumping a `tabstop` boundary) plus any trailing spaces.
    ///
    /// `<BS>` undoes such a fill as a unit — see [`softtab_backspace`], which
    /// works off the whitespace actually in the line rather than off any marker
    /// left here, so indentation nobody typed with `<Tab>` collapses the same way.
    ///
    /// [`softtab_backspace`]: Editor::softtab_backspace
    fn insert_tab(&mut self) {
        let opts = self.buffer().options;
        let unit = opts.effective_softtabstop();
        let start = self.cursor_virtcol();
        let target = start - (start % unit) + unit; // next multiple of the unit
        let ws = fill_indent(start, target, opts.effective_tabstop(), opts.expandtab);
        let at = self.cursor_char();
        // `ws` is ASCII (tabs/spaces), so its byte length is its column advance.
        let n = ws.len();
        self.buffer_mut().insert(at, &ws);
        self.cursor.col += n;
        self.buffer_mut().modified = true;
        // The expanded tab is part of the `".` last-insert register.
        self.insert_text.push_str(&ws);
    }

    fn insert_backspace(&mut self) {
        if self.cursor.col > 0 {
            if self.buffer().options.autopairs && self.autopair_backspace() {
                return;
            }
            if self.softtab_backspace() {
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

    /// `<BS>` over blanks: delete back to the previous [`softtabstop`] boundary rather
    /// than one column at a time — the mirror of the fill [`insert_tab`] lays
    /// down. Vim's `ins_bs` rule, and deliberately *not* keyed on who wrote the
    /// whitespace: auto-indent, the file on disk and a typed run of spaces all
    /// collapse a unit at a time, which is what makes `<BS>` on an `o`-opened
    /// indented line dedent one level.
    ///
    /// It applies only when the character *before* the cursor is a blank (a word
    /// character rubs out singly, as always), the walk back stops at the first
    /// non-blank so it never eats real text, and deleting a `\t` that straddles
    /// the boundary pads the remainder back out with spaces. Returns `true` when
    /// it handled the delete.
    ///
    /// [`insert_tab`]: Editor::insert_tab
    /// [`softtabstop`]: crate::options::BufferOptions::effective_softtabstop
    fn softtab_backspace(&mut self) -> bool {
        let opts = self.buffer().options;
        let unit = opts.effective_softtabstop();
        if unit <= 1 {
            return false;
        }
        let ts = opts.effective_tabstop();
        let s = self.buffer().line(self.cursor.line);
        let prev_col = unicode::prev_grapheme(&s, self.cursor.col);
        if !is_blank(&s[prev_col..self.cursor.col]) {
            return false;
        }
        // The column to land on: the boundary at or below the last cell the
        // character before the cursor occupies (`vcol - 1`).
        let vcol = unicode::virtcol(&s, self.cursor.col, ts);
        let target = ((vcol - 1) / unit) * unit;
        // Walk back over blanks only, never below the target column.
        let (mut col, mut at_vcol) = (self.cursor.col, vcol);
        while at_vcol > target && col > 0 {
            let prev = unicode::prev_grapheme(&s, col);
            if !is_blank(&s[prev..col]) {
                break;
            }
            col = prev;
            at_vcol = unicode::virtcol(&s, col, ts);
        }
        if col == self.cursor.col {
            return false;
        }
        // A tab can straddle the boundary — deleting it lands below `target`, so
        // pad the difference back out with spaces (vim does the same).
        let pad = " ".repeat(target.saturating_sub(at_vcol));
        let line_start = self.buffer().line_start(self.cursor.line);
        let removed = s[col..self.cursor.col].chars().count();
        let range = line_start + col..line_start + self.cursor.col;
        self.buffer_mut().remove(range);
        if !pad.is_empty() {
            self.buffer_mut().insert(line_start + col, &pad);
        }
        self.cursor.col = col + pad.len();
        self.buffer_mut().modified = true;
        // `".` last-insert register: the blanks go, any padding takes their place.
        self.trim_insert_text(removed);
        self.insert_text.push_str(&pad);
        true
    }
}

/// Whether `s` is entirely blanks (spaces and tabs) — the run `<BS>` collapses
/// by a soft-tab unit and `<Tab>` fills with.
fn is_blank(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b == b' ' || b == b'\t')
}
