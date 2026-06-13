//! Command-line mode: entry, in-line editing, history recall, and the scripted
//! `vim.ui.input` prompt / `confirm` dialogs.

use super::*;
use crate::input::{Key, KeyCode};
use crate::mode::Mode;

impl Editor {
    pub(crate) fn enter_command(&mut self) {
        // `:` pressed in Visual mode operates on the selection: vim stamps the
        // `'<` / `'>` selection marks and prefills the line with `'<,'>` so the
        // typed command (`:'<,'>d`, `:'<,'>s/…`) addresses exactly those lines.
        let from_visual = self.mode.is_visual();
        if from_visual {
            self.record_visual_marks();
        }
        self.cmdline_return_mode = self.command_return_mode();
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Ex;
        self.hist_idx = None;
        self.message.clear();
        self.reset_pending();
        if from_visual {
            self.cmdline.push_str("'<,'>");
            self.cmdline_col = self.cmdline.len();
        }
    }

    /// Open the command line as a `/` (forward) or `?` (backward) search prompt.
    /// Same `Mode::Command` machinery as `:`; the kind routes `<CR>` to a search
    /// instead of an ex-command. `count` is the prefix on the opening `/`,`?`
    /// (`3/foo` finds the 3rd match), stashed for submit since `reset_pending`
    /// clears it.
    pub(crate) fn enter_search(&mut self, dir: SearchDir, count: usize) {
        self.cmdline_return_mode = self.search_return_mode();
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Search(dir);
        self.pending_search_count = count.max(1);
        self.hist_idx = None;
        self.search_origin = self.cursor;
        self.message.clear();
        self.reset_pending();
    }

    /// The type character of the open command line, for `vim.fn.getcmdtype()`:
    /// `:` for an ex command, `/` / `?` for a forward / backward search, `@` for
    /// a scripted `input()` / `confirm()` prompt — and `""` when no command line
    /// is open. `cmdline_kind` lingers after the line closes (it's set on entry,
    /// not cleared on exit), so the open check gates on the mode, not the kind.
    pub fn cmdline_type(&self) -> &'static str {
        if self.mode != Mode::Command {
            return "";
        }
        match self.cmdline_kind {
            CmdlineKind::Ex => ":",
            CmdlineKind::Search(SearchDir::Forward) => "/",
            CmdlineKind::Search(SearchDir::Backward) => "?",
            CmdlineKind::Prompt | CmdlineKind::Confirm => "@",
        }
    }

    pub(crate) fn handle_command(&mut self, key: Key) {
        // A `vim.fn.confirm` dialog resolves on a single keypress, not a typed
        // line, so it owns the key ahead of the line-editing path below.
        if matches!(self.cmdline_kind, CmdlineKind::Confirm) {
            self.handle_confirm(key);
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.cancel_cmdline();
                return;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.cmdline);
                self.cmdline_col = 0;
                let kind = self.cmdline_kind;
                self.mode = self.cmdline_return_mode;
                match kind {
                    CmdlineKind::Ex => {
                        self.remember_ex(&text);
                        self.execute_ex(&text);
                    }
                    CmdlineKind::Search(dir) => {
                        // Commit from the saved origin, not the incsearch preview
                        // hop, so the count search lands deterministically (and
                        // identically to the no-incsearch path).
                        self.cursor = self.search_origin;
                        self.submit_search(&text, dir);
                    }
                    CmdlineKind::Prompt => {
                        // Hand the typed line to the waiting `vim.ui.input` callback
                        // (the server drains `prompt_results` and fires it).
                        self.cmdline_prompt.clear();
                        self.prompt_results.push(Some(text));
                    }
                    // A confirm dialog is resolved by `handle_confirm` (routed at
                    // the top of this fn), so it never reaches the line-submit path.
                    CmdlineKind::Confirm => {}
                }
                return;
            }
            // Backspacing an empty command line exits, like Esc. With text, it
            // deletes the char before the cursor (a no-op at the very start).
            KeyCode::Backspace if self.cmdline.is_empty() => {
                self.cancel_cmdline();
                return;
            }
            KeyCode::Backspace => self.cmdline_backspace(),
            // `<Del>` removes the char *under* the cursor.
            KeyCode::Delete => self.cmdline_delete(),
            // Within-line cursor motion: arrows by a char, Home/End (and the
            // vim-cmdline `<C-b>`/`<C-e>`) to the ends.
            KeyCode::Left => self.cmdline_cursor_left(),
            KeyCode::Right => self.cmdline_cursor_right(),
            KeyCode::Home => self.cmdline_col = 0,
            KeyCode::End => self.cmdline_col = self.cmdline.len(),
            KeyCode::Char('b') if key.ctrl => self.cmdline_col = 0,
            KeyCode::Char('e') if key.ctrl => self.cmdline_col = self.cmdline.len(),
            // Command-history recall (`<Up>`/`<C-p>` older, `<Down>`/`<C-n>`
            // newer), over whichever history the open prompt's kind selects.
            KeyCode::Up => self.cmdline_history_prev(),
            KeyCode::Down => self.cmdline_history_next(),
            KeyCode::Char('p') if key.ctrl => self.cmdline_history_prev(),
            KeyCode::Char('n') if key.ctrl => self.cmdline_history_next(),
            KeyCode::Char(c) if !key.ctrl => self.cmdline_insert(c),
            _ => {}
        }
        // The command line still has focus: refresh the live incsearch preview
        // for the just-edited search pattern (a no-op for an ex command line).
        if let CmdlineKind::Search(dir) = self.cmdline_kind {
            self.update_incsearch_preview(dir);
        }
    }

    /// Abandon the open command line and return to normal mode. For a search
    /// prompt this also rewinds the cursor to where the search began, undoing any
    /// incsearch preview hop (vim's `<Esc>`-cancels-search behavior).
    fn cancel_cmdline(&mut self) {
        if matches!(self.cmdline_kind, CmdlineKind::Search(_)) {
            self.cursor = self.search_origin;
            self.clamp_cursor();
            // Cancelling a `d/`-style search also abandons the pending operator.
            self.search_operator = None;
        }
        if matches!(self.cmdline_kind, CmdlineKind::Prompt) {
            // A cancelled `vim.ui.input` delivers `nil` to its callback (neovim's
            // `on_confirm(nil)` on `<Esc>`).
            self.cmdline_prompt.clear();
            self.prompt_results.push(None);
        }
        self.mode = self.cmdline_return_mode;
        self.cmdline.clear();
        self.cmdline_col = 0;
    }

    /// The mode an *ex* command line (`:`) opened now should restore when it
    /// closes: back to [`Mode::MultiCursor`] when opened from placement mode (so a
    /// `:`-command can keep dropping cursors), else [`Mode::Normal`] — including
    /// from Visual, which vim leaves on `:` (the `'<,'>` range carries the
    /// selection into the command).
    fn command_return_mode(&self) -> Mode {
        if self.mode == Mode::MultiCursor {
            Mode::MultiCursor
        } else {
            Mode::Normal
        }
    }

    /// The mode a `/`,`?` search command line opened now should restore on close.
    /// Unlike `:`, vim keeps the *selection live* through a visual-mode search —
    /// the moving end hops to the match while the anchor holds — so a search
    /// opened from Visual / Visual-Line returns to that mode (the preserved
    /// [`Editor::visual_anchor`] then spans anchor→match). Placement mode is
    /// likewise kept so a search can hop between cursor drops; everything else
    /// returns to [`Mode::Normal`].
    fn search_return_mode(&self) -> Mode {
        match self.mode {
            Mode::Visual | Mode::VisualLine | Mode::MultiCursor => self.mode,
            _ => Mode::Normal,
        }
    }

    /// Open the command line as a scripted `vim.ui.input` prompt (Phase 8): a
    /// [`CmdlineKind::Prompt`] showing `label`, prefilled with `default` and the
    /// cursor at its end. `<CR>` / `<Esc>` deliver the result through
    /// [`Editor::prompt_results`]. The server calls this when it drains a queued
    /// `vim.ui.input` request, then fires the registered Lua callback on submit.
    pub fn open_prompt(&mut self, label: String, default: String) {
        self.cmdline_return_mode = Mode::Normal;
        self.mode = Mode::Command;
        self.cmdline = default;
        self.cmdline_col = self.cmdline.len();
        self.cmdline_kind = CmdlineKind::Prompt;
        self.cmdline_prompt = label;
    }

    /// Open the command line as a `vim.fn.confirm` button dialog: a
    /// [`CmdlineKind::Confirm`] showing `label` (the message plus rendered
    /// buttons). It has no editable line — a single keypress matching one of
    /// `accelerators` (lowercase, in button order) resolves to that button's
    /// 1-based index, `<CR>` selects `default` (1-based; `0` cancels), and
    /// `<Esc>` / `<C-c>` cancel with `0`. The chosen index is delivered as a
    /// string through [`Editor::prompt_results`] (the channel `Prompt` uses; the
    /// server forwards it to the blocked `vim.fn.confirm`). The server calls this
    /// when it drains a queued confirm request.
    pub fn open_confirm(&mut self, label: String, accelerators: Vec<String>, default: i64) {
        self.cmdline_return_mode = Mode::Normal;
        self.mode = Mode::Command;
        self.cmdline.clear();
        self.cmdline_col = 0;
        self.cmdline_kind = CmdlineKind::Confirm;
        self.cmdline_prompt = label;
        self.confirm_accelerators = accelerators;
        self.confirm_default = default;
    }

    /// Resolve an open confirm dialog from a single keypress, pushing the chosen
    /// 1-based index (or `0` to cancel) as a string onto [`Editor::prompt_results`]
    /// and returning to normal mode. An unrecognized key is ignored (the dialog
    /// stays open), matching neovim's "press one of the listed keys" behavior.
    fn handle_confirm(&mut self, key: Key) {
        let index = match key.code {
            KeyCode::Esc => Some(0),
            KeyCode::Char('c') if key.ctrl => Some(0),
            KeyCode::Enter => Some(self.confirm_default),
            KeyCode::Char(c) if !key.ctrl => {
                let pressed = c.to_ascii_lowercase().to_string();
                self.confirm_accelerators
                    .iter()
                    .position(|acc| *acc == pressed)
                    .map(|i| i as i64 + 1)
            }
            _ => None,
        };
        if let Some(index) = index {
            self.mode = Mode::Normal;
            self.cmdline_prompt.clear();
            self.confirm_accelerators.clear();
            self.prompt_results.push(Some(index.to_string()));
        }
    }

    /// The `vim.ui.input` / `vim.fn.confirm` prompt label (empty unless a
    /// [`CmdlineKind::Prompt`] or [`CmdlineKind::Confirm`] is open), projected
    /// into the [`View`] so the client renders it ahead of the editable line in
    /// place of the single-char [`Editor::cmdline_prefix`].
    pub(crate) fn cmdline_prompt(&self) -> &str {
        if matches!(
            self.cmdline_kind,
            CmdlineKind::Prompt | CmdlineKind::Confirm
        ) {
            &self.cmdline_prompt
        } else {
            ""
        }
    }

    /// Insert `c` at the command cursor and step the cursor past it.
    fn cmdline_insert(&mut self, c: char) {
        self.cmdline.insert(self.cmdline_col, c);
        self.cmdline_col += c.len_utf8();
    }

    /// Delete the char before the command cursor (`<BS>`); a no-op at the start.
    fn cmdline_backspace(&mut self) {
        if let Some(prev) = self.cmdline_prev_boundary() {
            self.cmdline.remove(prev);
            self.cmdline_col = prev;
        }
    }

    /// Delete the char under the command cursor (`<Del>`); a no-op at the end.
    fn cmdline_delete(&mut self) {
        if self.cmdline_col < self.cmdline.len() {
            self.cmdline.remove(self.cmdline_col);
        }
    }

    /// Move the command cursor one char left (`<Left>`).
    fn cmdline_cursor_left(&mut self) {
        if let Some(prev) = self.cmdline_prev_boundary() {
            self.cmdline_col = prev;
        }
    }

    /// Move the command cursor one char right (`<Right>`).
    fn cmdline_cursor_right(&mut self) {
        if let Some(c) = self.cmdline[self.cmdline_col..].chars().next() {
            self.cmdline_col += c.len_utf8();
        }
    }

    /// Byte offset of the char boundary immediately before the command cursor,
    /// or `None` when it's already at the start. (Char-aware so multibyte input
    /// in the command line edits one whole character at a time.)
    fn cmdline_prev_boundary(&self) -> Option<usize> {
        self.cmdline[..self.cmdline_col]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
    }

    /// The history list for the open command line's kind: ex commands for `:`,
    /// search patterns for `/`,`?`. A `vim.ui.input` prompt has no history.
    fn active_history(&self) -> &[String] {
        match self.cmdline_kind {
            CmdlineKind::Ex => &self.ex_history,
            CmdlineKind::Search(_) => &self.search_history,
            CmdlineKind::Confirm => &[],
            CmdlineKind::Prompt => &[],
        }
    }

    /// `<Up>`/`<C-p>` in the command line — recall the previous history entry
    /// (the newest first), replacing the typed line. A no-op with an empty
    /// history.
    fn cmdline_history_prev(&mut self) {
        let len = self.active_history().len();
        if len == 0 {
            return;
        }
        let idx = match self.hist_idx {
            None => len - 1,
            Some(i) => i.saturating_sub(1),
        };
        self.hist_idx = Some(idx);
        self.cmdline = self.active_history()[idx].clone();
        self.cmdline_col = self.cmdline.len();
    }

    /// `<Down>`/`<C-n>` in the command line — move to a newer history entry, or
    /// back to an empty line once past the newest.
    fn cmdline_history_next(&mut self) {
        let len = self.active_history().len();
        match self.hist_idx {
            Some(i) if i + 1 < len => {
                self.hist_idx = Some(i + 1);
                self.cmdline = self.active_history()[i + 1].clone();
                self.cmdline_col = self.cmdline.len();
            }
            Some(_) => {
                self.hist_idx = None;
                self.cmdline.clear();
                self.cmdline_col = 0;
            }
            None => {}
        }
    }

    /// The command-line prompt character for the current [`CmdlineKind`]: `:` for
    /// an ex command, `/` / `?` for a forward / backward search. The client draws
    /// it at the head of the command line.
    pub(crate) fn cmdline_prefix(&self) -> char {
        match self.cmdline_kind {
            CmdlineKind::Ex => ':',
            CmdlineKind::Search(dir) => dir.prefix(),
            // A `vim.ui.input` prompt and a `vim.fn.confirm` dialog render their
            // multi-char label via `cmdline_prompt()` instead; the single-char
            // prefix is unused (a space keeps the projection well-formed).
            CmdlineKind::Prompt | CmdlineKind::Confirm => ' ',
        }
    }

    /// The command cursor's position as a character offset from the start of
    /// [`Editor::cmdline`], for the client to place the terminal cursor.
    pub(crate) fn cmdline_cursor(&self) -> usize {
        self.cmdline[..self.cmdline_col].chars().count()
    }
}
