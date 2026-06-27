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

    /// Apply a named `cmdline` action, dispatched by a `cmdline`-bucket keymap (the
    /// `c`-bucket default maps in `prelude/keymap.lua`, or a user override) while the
    /// command line is open. The rebindable control keys: `cancel` abandons the line,
    /// `submit` runs it, `backspace`/`delete` edit around the cursor, `left`/`right`/
    /// `to_start`/`to_end` move the command cursor, `history_prev`/`history_next`
    /// recall, and `insert_register` arms `<C-r>` (the following register-name key is
    /// then read raw — a fixed-grammar literal arg, not a keymap). Typed text is the
    /// residual fallthrough ([`handle_command`](Self::handle_command)), not an action,
    /// so the hot path — typing a command — stays core-direct. An unknown name fails
    /// loud per the no-silent-stub rule.
    pub fn apply_cmdline_action(&mut self, action: &str) -> Result<(), String> {
        // `cancel` / `submit` change the mode (closing the line), so they return
        // before the trailing incsearch refresh.
        match action {
            // `<Tab>`: cycle the wildmenu selection forward while it is open, else
            // open / refresh it (the wildmenu sibling of the insert-mode completion
            // trigger). `<S-Tab>` (`complete_prev`) is its backward twin.
            "complete" => {
                if self.cmdline_menu_open() {
                    self.cmdline_complete_next();
                } else {
                    self.cmdline_complete_trigger();
                }
                return Ok(());
            }
            "complete_prev" => {
                if self.cmdline_menu_open() {
                    self.cmdline_complete_prev();
                } else {
                    self.cmdline_complete_trigger();
                }
                return Ok(());
            }
            // With the popup open, the history keys overload to wildmenu navigation:
            // `<C-p>`/`<Up>` (prev) and `<C-n>`/`<Down>` (next) cycle the selection
            // instead of recalling history (vim's wildmenu key sharing).
            "history_prev" if self.cmdline_menu_open() => {
                self.cmdline_complete_prev();
                return Ok(());
            }
            "history_next" if self.cmdline_menu_open() => {
                self.cmdline_complete_next();
                return Ok(());
            }
            // With the completion popup open, `<Esc>` closes it first — a second
            // `<Esc>` then cancels the line (vim's wildmenu dismissal). If the wildmenu
            // had previewed a selection into the line, restore the user's typed text
            // before closing (so dismissing the menu un-does the preview).
            "cancel" if self.cmdline_menu_open() => {
                self.cmdline_complete_revert();
                self.close_cmdline_menu();
                return Ok(());
            }
            "cancel" => {
                self.cancel_cmdline();
                return Ok(());
            }
            "submit" => {
                self.submit_cmdline();
                return Ok(());
            }
            // Backspacing an empty command line exits, like cancel — but a scripted
            // `vim.ui.input` prompt stays open when backspaced past its start (only
            // `cancel` ends an input), so a user who clears the line can keep typing.
            // The ex/search lines keep vim's empty-exit.
            "backspace" if self.cmdline.is_empty() => {
                if !matches!(self.cmdline_kind, CmdlineKind::Prompt) {
                    self.cancel_cmdline();
                }
                return Ok(());
            }
            _ => {}
        }
        match action {
            "backspace" => self.cmdline_backspace(),
            "delete" => self.cmdline_delete(),
            "left" => self.cmdline_cursor_left(),
            "right" => self.cmdline_cursor_right(),
            "to_start" => self.cmdline_col = 0,
            "to_end" => self.cmdline_col = self.cmdline.len(),
            "history_prev" => self.cmdline_history_prev(),
            "history_next" => self.cmdline_history_next(),
            // `<C-r>` arms register insertion: the next key is read raw (the matcher
            // bypasses it via `cmdline_reads_raw`) and names the register — handled
            // in `handle_command`.
            "insert_register" => self.awaiting_register = true,
            other => return Err(format!("unknown cmdline action {other:?}")),
        }
        // The command line still has focus: refresh the live incsearch preview
        // for the just-edited search pattern (a no-op for an ex command line).
        if let CmdlineKind::Search(dir) = self.cmdline_kind {
            self.update_incsearch_preview(dir);
        }
        // An edit / cursor move refreshes an open completion popup against the new
        // token (a no-op when no cmdline menu is open — typing never opens it).
        self.cmdline_complete_refresh();
        Ok(())
    }

    /// Run the open command line (`submit`): an ex command, a search, or hand a
    /// scripted prompt's typed text to its waiting callback, then restore the line's
    /// origin mode. A confirm dialog never reaches here — it is resolved by
    /// [`handle_confirm`](Self::handle_confirm) on the raw-read path.
    fn submit_cmdline(&mut self) {
        // Accept the highlighted wildmenu row (rewriting the command-name token) before
        // the line resolves, so `<CR>` accepts-then-executes; a noselect popup (nothing
        // highlighted) leaves the typed line unchanged. Either way the popup closes.
        self.cmdline_complete_accept();
        self.close_cmdline_menu();
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
                // Commit from the saved origin, not the incsearch preview hop, so the
                // count search lands deterministically (and identically to the
                // no-incsearch path).
                self.cursor = self.search_origin;
                self.submit_search(&text, dir);
            }
            CmdlineKind::Prompt => {
                // Record the submission in the prompt's history ring (if it opted into
                // one) before handing the typed line to the waiting `vim.ui.input`
                // callback (the server drains `prompt_results` and fires it).
                self.remember_prompt(&text);
                self.cmdline_prompt.clear();
                self.prompt_results.push(Some(text));
            }
            CmdlineKind::Confirm => {}
        }
    }

    /// Whether the command line consumes the next key **raw**, ahead of the keymap
    /// matcher — its two fixed-grammar sub-states: a `vim.fn.confirm` dialog (every
    /// key is a fixed prompt-alphabet answer, resolved by [`handle_confirm`]) and the
    /// `<C-r>{register}` read (the key after `<C-r>` names a register, a literal arg
    /// like `"{reg}`). The server's `feed_matcher` checks this so neither sub-state
    /// routes through the `cmdline` keymap bucket; everything reaches
    /// [`handle_command`](Self::handle_command).
    ///
    /// [`handle_confirm`]: Self::handle_confirm
    pub fn cmdline_reads_raw(&self) -> bool {
        self.mode == Mode::Command
            && (self.awaiting_register || matches!(self.cmdline_kind, CmdlineKind::Confirm))
    }

    /// The command line's residual key handling — the keys that are **not** `cmdline`
    /// maps. Reached from [`Editor::input`] for an unmapped key (a typed character),
    /// and on the raw-read path ([`cmdline_reads_raw`](Self::cmdline_reads_raw)) for a
    /// confirm answer or a `<C-r>{register}` name. Every nameable control key is a
    /// `cmdline` keymap ([`apply_cmdline_action`](Self::apply_cmdline_action)); an
    /// unmapped/disabled control key is inert here.
    pub(crate) fn handle_command(&mut self, key: Key) {
        // A `vim.fn.confirm` dialog resolves on a single keypress, not a typed line.
        if matches!(self.cmdline_kind, CmdlineKind::Confirm) {
            self.handle_confirm(key);
            return;
        }
        // `<C-r>{register}`: the keystroke after `<C-r>` (the `insert_register`
        // action) names the register whose text is inserted at the command cursor. A
        // non-register key cancels, inserting nothing (vim).
        if self.awaiting_register {
            self.awaiting_register = false;
            if key.ctrl && key.code == KeyCode::Char('w') {
                // `<C-r><C-w>`: the word under the buffer cursor, not a register.
                if let Some(word) = self.word_under_cursor() {
                    self.cmdline_insert_str(&word);
                }
            } else if let Some(name) = key.as_char() {
                self.cmdline_insert_register(name);
            }
            if let CmdlineKind::Search(dir) = self.cmdline_kind {
                self.update_incsearch_preview(dir);
            }
            return;
        }
        // The residual text fallthrough: an unmapped printable key inserts into the
        // command line (the hot path stays core-direct, never round-tripping Lua).
        if let KeyCode::Char(c) = key.code {
            if !key.ctrl {
                self.cmdline_insert(c);
                if let CmdlineKind::Search(dir) = self.cmdline_kind {
                    self.update_incsearch_preview(dir);
                }
                // Narrow an open completion popup against the new prefix (no-op when
                // none is open — typing never opens the wildmenu, only `<Tab>` does).
                self.cmdline_complete_refresh();
            }
        }
    }

    /// Abandon the open command line and return to normal mode. For a search
    /// prompt this also rewinds the cursor to where the search began, undoing any
    /// incsearch preview hop (vim's `<Esc>`-cancels-search behavior). `pub` so the
    /// dock-navigation path can close the line directly, and the server can dismiss
    /// it when a file-argument `<Tab>` hands off to the picker (the `cancel` action
    /// is the keymap-driven entry).
    pub fn cancel_cmdline(&mut self) {
        // Abandoning the line also dismisses any open completion popup.
        self.close_cmdline_menu();
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
    pub fn open_prompt(
        &mut self,
        label: String,
        default: String,
        history_key: Option<String>,
        complete: bool,
        complete_docs: bool,
    ) {
        self.cmdline_return_mode = Mode::Normal;
        self.mode = Mode::Command;
        self.cmdline = default;
        self.cmdline_col = self.cmdline.len();
        self.cmdline_kind = CmdlineKind::Prompt;
        self.cmdline_prompt = label;
        // The prompt's history namespace (`nx.ui.input{ history = … }`): drives
        // `<Up>`/`<Down>` recall and what `submit` records. Reset the browse position
        // so the first `<Up>` starts at the newest entry (a prompt opening fresh).
        self.prompt_history_key = history_key;
        self.hist_idx = None;
        // Whether this prompt opted into `<Tab>` autocomplete (`complete = fn`) and
        // whether its wildmenu shows the side docs pane (`complete_docs`).
        self.prompt_complete_active = complete;
        self.prompt_complete_docs = complete_docs;
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

    /// Insert the named register's text at the command cursor — the command-line
    /// `<C-r>{register}`. An empty / absent register inserts nothing. The command
    /// line is a single editable line, so embedded newlines (a linewise register's
    /// trailing break, or a multi-line yank) are dropped rather than splitting it.
    fn cmdline_insert_register(&mut self, name: char) {
        let Some((text, _kind)) = self.register_text(Some(name)) else {
            return;
        };
        self.cmdline_insert_str(&text);
    }

    /// Insert each char of `text` at the command cursor. The command line is a
    /// single editable line, so embedded newlines (a linewise register's trailing
    /// break, or a multi-line yank) are dropped rather than splitting it. Shared
    /// by `<C-r>{register}` and `<C-r><C-w>`.
    fn cmdline_insert_str(&mut self, text: &str) {
        for c in text.chars().filter(|&c| c != '\n') {
            self.cmdline_insert(c);
        }
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
    /// search patterns for `/`,`?`, and — for a `vim.ui.input` prompt — the ring of
    /// its `history` namespace (empty when it opted into none).
    fn active_history(&self) -> &[String] {
        match self.cmdline_kind {
            CmdlineKind::Ex => &self.ex_history,
            CmdlineKind::Search(_) => &self.search_history,
            CmdlineKind::Confirm => &[],
            CmdlineKind::Prompt => self
                .prompt_history_key
                .as_ref()
                .and_then(|k| self.prompt_history.get(k))
                .map_or(&[], Vec::as_slice),
        }
    }

    /// Record a submitted `nx.ui.input` line in its history namespace, skipping an
    /// empty line or a consecutive duplicate (mirroring [`remember_ex`]). A no-op
    /// when the prompt opted into no history (`prompt_history_key` is `None`).
    ///
    /// [`remember_ex`]: Self::remember_ex
    fn remember_prompt(&mut self, text: &str) {
        let Some(key) = self.prompt_history_key.clone() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let cap = self.options.history;
        let ring = self.prompt_history.entry(key).or_default();
        if ring.last().map(String::as_str) != Some(text) {
            ring.push(text.to_string());
            if ring.len() > cap {
                ring.drain(0..ring.len() - cap);
            }
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

    /// The display width of the prompt the client renders ahead of the editable
    /// command line — the multi-char [`Editor::cmdline_prompt`] label (`nx.ui.input`
    /// / `confirm`) when one is set, else the single-char [`Editor::cmdline_prefix`]
    /// (`:` / `/` / `?`). The command-line wildmenu anchors *past* this so the list
    /// lines up with the token in the line, not with the prompt — without it a
    /// multi-char prompt (e.g. `dap> `) slides the popup left by its whole width.
    pub(crate) fn cmdline_prompt_width(&self) -> usize {
        let label = self.cmdline_prompt();
        if label.is_empty() {
            crate::unicode::display_width(&self.cmdline_prefix().to_string())
        } else {
            crate::unicode::display_width(label)
        }
    }

    /// The command cursor's position as a character offset from the start of
    /// [`Editor::cmdline`], for the client to place the terminal cursor.
    pub(crate) fn cmdline_cursor(&self) -> usize {
        self.cmdline[..self.cmdline_col].chars().count()
    }
}
