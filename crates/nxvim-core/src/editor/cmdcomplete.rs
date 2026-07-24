//! Command-line completion — the unified float-list widget's **fifth orchestration**
//! (`nx.cmdline_complete`). Pressing `<Tab>` while an ex command line (`:`) is open
//! offers a fuzzy list of matching command names (with a docs/params preview pane in
//! a later phase); a plugin-registered command appears in the list exactly as a
//! built-in does.
//!
//! Unlike the insert-mode completion engine ([`super::complete`], whose prefix comes
//! from a buffer line and whose accept edits the buffer), this engine completes a
//! token of the command line ([`Editor::cmdline`]). The candidate set is **policy**
//! owned by the bundled `nx.cmdline_complete` Lua plugin (the curated command catalog
//! merged with `nx.user_command.get()`), so core stays out of "what commands exist":
//! core extracts the token being completed, ranks + renders the menu (reusing the
//! `Menu` / [`MenuView`](crate::view::MenuView) widget), and applies the accept; the
//! server fetches the catalog candidates and hands them to
//! [`Editor::open_cmdline_menu`].
//!
//! The catalog filter is a microsecond table scan, so — unlike the async insert
//! sources (rg / lsp) — there is no streaming / generation machinery here: `<Tab>`
//! (and each edit while the menu is open) sets [`Editor::cmdline_complete_request`],
//! the server resolves it in one round-trip, and the menu is rebuilt.

use super::menu::MenuKind;
use super::*;

/// `nx.cmdline_complete.setup{}` configuration. Off until a config arrives, so an
/// editor with no command-line completion behaves byte-for-byte as before.
#[derive(Clone, Debug, Default)]
pub(crate) struct CmdlineCompleteConfig {
    pub enabled: bool,
    /// Show the docs/params preview pane beside the menu (Phase 3). `false` in
    /// Phase 1 (names only).
    pub docs: bool,
}

/// A pending command-line completion request: core extracts the token being
/// completed and the server fetches the catalog candidates for `line` / `col` from
/// the Lua source, then hands them back to [`Editor::open_cmdline_menu`] with the
/// `anchor` / `prefix` core computed. `None` when idle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CmdlineCompleteReq {
    /// The whole command line (the source decides command-vs-argument from it).
    pub line: String,
    /// The command cursor as a char offset (for the source's context decision).
    pub col: usize,
    /// The token being completed — the command name, or (once whitespace separates
    /// it) the current argument word. Core fuzzy-ranks the catalog labels against
    /// this.
    pub prefix: String,
    /// Byte offset in [`Editor::cmdline`] where the token starts. Accepting a row
    /// replaces `[anchor .. cmdline_col)` with the chosen command (Phase 2).
    pub anchor: usize,
    /// Display width of the command-line text before the token — the column the
    /// menu anchors under (after the `:` prompt char).
    pub anchor_width: usize,
    /// Whether this request **narrows an already-open** wildmenu (an edit) rather than
    /// opening it (the initial `<Tab>`). Only consulted for prompt completion, where
    /// the server queries the initial request at once but debounces refreshes. Always
    /// `false` for the ex catalog path (its source is a microsecond table scan).
    pub refresh: bool,
}

impl Editor {
    /// Apply an `nx.cmdline_complete.setup{}` config (enable the engine).
    pub fn configure_cmdline_complete(&mut self, docs: bool) {
        self.cmdcomplete = CmdlineCompleteConfig {
            enabled: true,
            docs,
        };
    }

    /// Whether the command-line completion docs pane is enabled (Phase 3).
    pub fn cmdline_complete_docs(&self) -> bool {
        self.cmdcomplete.docs
    }

    /// Whether a command-line completion menu is currently open.
    pub(crate) fn cmdline_menu_open(&self) -> bool {
        self.menu_kind() == Some(MenuKind::Cmdline)
    }

    /// `<Tab>` (the `complete` cmdline action) / a content edit while the menu is
    /// open: compute the token being completed and queue a [`CmdlineCompleteReq`]
    /// for the server to resolve against the catalog. A no-op (closing any open
    /// cmdline menu) when the engine is disabled or the line is not an ex command
    /// line. The token is either the leading command name or — once whitespace
    /// separates it — the current argument word (the source decides what to offer
    /// for it from the whole `line`; `:set` arguments complete option names, other
    /// commands' arguments return nothing yet, which closes the menu).
    pub(crate) fn cmdline_complete_trigger(&mut self) {
        // A scripted prompt (`nx.ui.input{ complete = fn }`) routes to its own
        // per-call source rather than the ex catalog — different token model (a
        // trailing identifier run, not a command-name/argument split) and a different
        // request field the server resolves against the prompt's `complete` callback.
        if matches!(self.cmdline_kind, CmdlineKind::Prompt) {
            self.prompt_complete_trigger(false);
            return;
        }
        if !self.cmdcomplete.enabled || !matches!(self.cmdline_kind, CmdlineKind::Ex) {
            self.close_cmdline_menu();
            return;
        }
        let Some((anchor, anchor_width, prefix)) = self.cmdline_complete_token() else {
            self.close_cmdline_menu();
            return;
        };
        self.cmdline_complete_request = Some(CmdlineCompleteReq {
            line: self.cmdline.clone(),
            col: self.cmdline_cursor(),
            prefix,
            anchor,
            anchor_width,
            refresh: false,
        });
    }

    /// Stamp a [`Editor::prompt_complete_request`] for the open `nx.ui.input` prompt
    /// (a no-op when it opted into no `complete` source). The completed token is the
    /// **trailing identifier run** before the cursor (word chars + `_`, breaking on
    /// `.`/space/punctuation) — the natural unit for a REPL word/member completion —
    /// rather than the ex command-name/argument split. Accepting a row replaces
    /// `[anchor .. cmdline_col)`; the server fills the candidates from the prompt's
    /// own `complete` callback.
    fn prompt_complete_trigger(&mut self, refresh: bool) {
        if !self.prompt_complete_active {
            self.close_cmdline_menu();
            return;
        }
        let (anchor, anchor_width, prefix) = self.prompt_complete_token();
        self.prompt_complete_request = Some(CmdlineCompleteReq {
            line: self.cmdline.clone(),
            col: self.cmdline_cursor(),
            prefix,
            anchor,
            anchor_width,
            refresh,
        });
    }

    /// The token being completed in an `nx.ui.input` prompt: the trailing run of
    /// identifier characters (`is_alphanumeric` or `_`) immediately before the cursor,
    /// as `(anchor_byte, anchor_display_width, prefix)`. An empty run (the cursor sits
    /// right after a `.`/space/`(`) anchors at the cursor with an empty prefix, so the
    /// source's full candidate set shows (e.g. `os.<Tab>` lists every member).
    fn prompt_complete_token(&self) -> (usize, usize, String) {
        let upto = &self.cmdline[..self.cmdline_col];
        let start = upto
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
            .last()
            .map_or(self.cmdline_col, |(i, _)| i);
        let anchor_width = crate::unicode::display_width(&upto[..start]);
        (start, anchor_width, upto[start..].to_string())
    }

    /// Open / rebuild the prompt wildmenu from `candidates` the server resolved from
    /// the prompt's `complete` source. Re-extracts the token from the **current** line
    /// (so an async source that resolved a tick late ranks against the freshest
    /// prefix), then reuses the shared [`Editor::open_cmdline_menu`]. Shows the side
    /// docs pane when the prompt opted into it (`complete_docs`). A no-op off a prompt
    /// line (the prompt closed before the async candidates arrived).
    ///
    /// Each candidate is `(label, insert, doc, range)`; `range` is the adapter-specified
    /// replace span as `(start_char, len_char)` — a 0-based char offset into the line and
    /// a char count (the DAP `CompletionItem.start`/`length`). When present it is
    /// converted to a byte `(start, end)` span against the current line and overrides the
    /// trailing-identifier token for that row; `None` falls back to the token.
    pub fn open_prompt_complete_menu(&mut self, candidates: Vec<super::CmdlineCandidate>) {
        if !matches!(self.cmdline_kind, CmdlineKind::Prompt) {
            return;
        }
        let (anchor, anchor_width, prefix) = self.prompt_complete_token();
        // Map each candidate's `(start_char, len_char)` to a byte span in the line.
        let candidates: Vec<super::CmdlineCandidate> = candidates
            .into_iter()
            .map(|(label, insert, doc, range)| {
                let bytes = range.map(|(start_char, len_char)| {
                    self.cmdline_char_span_to_bytes(start_char, len_char)
                });
                (label, insert, doc, bytes)
            })
            .collect();
        let docs = self.prompt_complete_docs;
        self.open_cmdline_menu(anchor, anchor_width, &prefix, candidates, docs);
    }

    /// Convert a `(start_char, len_char)` span — a 0-based char offset into
    /// [`Editor::cmdline`] and a char count — into a byte `(start, end)` span, clamped
    /// to the line. Char-boundary aware so a multibyte line still replaces whole
    /// characters.
    fn cmdline_char_span_to_bytes(&self, start_char: usize, len_char: usize) -> (usize, usize) {
        let bounds: Vec<usize> = self
            .cmdline
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(self.cmdline.len()))
            .collect();
        let at = |c: usize| bounds.get(c).copied().unwrap_or(self.cmdline.len());
        (at(start_char), at(start_char + len_char))
    }

    /// Re-run the trigger when a cmdline menu is already open (a content edit
    /// narrowed the token). A no-op when no cmdline menu is open — typing before
    /// `<Tab>` never opens the menu (on-demand activation).
    ///
    /// An edit only **narrows the same token**: it re-ranks when the token start is
    /// unchanged (e.g. `:e<Tab>` then `d` → `edit`). An edit that moves the token
    /// start — typing *past* the completed word into a new token, the space after a
    /// command name being the common case — does **not** auto-open a completion for
    /// the new token; it closes the wildmenu, and the user re-opens with an explicit
    /// `<Tab>`. This is what keeps the space after `:e` from launching the file
    /// picker (or `:set ` from popping its option list) before the user asks for it.
    pub(crate) fn cmdline_complete_refresh(&mut self) {
        if !self.cmdline_menu_open() {
            return;
        }
        // A real edit commits any previewed selection (the line is the user's own text
        // again) — drop the revert snapshot before re-resolving.
        self.cmdline_complete_saved = None;
        // A prompt completion re-queries on every edit (live narrowing as you type the
        // word): there's no "typed past the token into a new one" file-picker hazard
        // to guard against, so it re-triggers unconditionally while the menu is open.
        // It's a refresh (narrowing an open menu), so the server debounces it.
        if matches!(self.cmdline_kind, CmdlineKind::Prompt) {
            self.prompt_complete_trigger(true);
            return;
        }
        let same_token =
            self.cmdline_complete_token().map(|(anchor, ..)| anchor) == self.cmdline_menu_anchor();
        if same_token {
            self.cmdline_complete_trigger();
        } else {
            self.close_cmdline_menu();
        }
    }

    /// Preview the highlighted wildmenu row in the command line: rewrite the
    /// command-name token `[anchor .. cmdline_col)` to the selected command (saving
    /// the user's typed line once, so `<Esc>` can restore it) **without** closing the
    /// menu or executing. Called after each wildmenu navigation so what `<CR>` will
    /// run is always what the line shows. A no-op while the popup is noselect (nothing
    /// highlighted yet).
    pub(crate) fn cmdline_complete_preview(&mut self) {
        // Snapshot the originally-typed line on the first preview; on later previews
        // restore it first. Cycling rows must restore to the *originally typed* text
        // (not the last row), and restoring also keeps each row's explicit `replace`
        // span — indexed against that original line — valid as the user cycles.
        match self.cmdline_complete_saved.clone() {
            None => self.cmdline_complete_saved = Some((self.cmdline.clone(), self.cmdline_col)),
            Some((line, col)) => {
                self.cmdline = line;
                self.cmdline_col = col;
            }
        }
        let Some((start, end, insert)) = self.cmdline_complete_selected() else {
            return;
        };
        self.cmdline.replace_range(start..end, &insert);
        self.cmdline_col = start + insert.len();
    }

    /// Restore the command line to what the user typed before the wildmenu previewed
    /// a selection (the `<Esc>`-dismisses-wildmenu path). A no-op when no preview was
    /// applied (a noselect popup, or no cmdline menu) — `<Esc>` then just closes the
    /// popup, leaving the typed line untouched.
    pub(crate) fn cmdline_complete_revert(&mut self) {
        if let Some((line, col)) = self.cmdline_complete_saved.take() {
            self.cmdline = line;
            self.cmdline_col = col;
        }
    }

    /// Replace the current command-line **argument** token with `text`, keeping the
    /// command line open. This is the file-picker handoff's "paste the chosen path":
    /// `<Tab>` on `:e <arg>` opens the picker over the still-open line, and confirming
    /// a file calls here to drop its path into the argument — the user then runs the
    /// line with `<CR>` (the picker never auto-executes). A no-op off an ex command
    /// line or when there is no argument token (e.g. still in the command name).
    pub fn cmdline_replace_arg(&mut self, text: &str) {
        if !matches!(self.cmdline_kind, CmdlineKind::Ex) {
            return;
        }
        let Some((anchor, _, _)) = self.cmdline_complete_token() else {
            return;
        };
        // The line was not edited while the picker grabbed input, so the token still
        // ends at the cursor; replace `[anchor .. cmdline_col)` and park the cursor
        // past the pasted path so further typing appends to it.
        self.cmdline.replace_range(anchor..self.cmdline_col, text);
        self.cmdline_col = anchor + text.len();
    }

    /// Accept the highlighted command-line completion row: replace the command-name
    /// token `[anchor .. cmdline_col)` with the chosen command and place the cursor at
    /// its end (so further typing — an argument — appends past the accepted name).
    /// Returns whether a row was accepted; `false` (a noselect popup, nothing
    /// highlighted) leaves the line untouched so the caller runs it as typed. The
    /// popup itself is closed by [`cmdline_complete_take_accept`] on success — the
    /// caller closes any still-open noselect popup separately.
    pub(crate) fn cmdline_complete_accept(&mut self) -> bool {
        // Restore the originally-typed line (a preview may have rewritten it) so the
        // accepted row's `replace` span — indexed against that line — applies correctly.
        if let Some((line, col)) = self.cmdline_complete_saved.take() {
            self.cmdline = line;
            self.cmdline_col = col;
        }
        let Some((start, end, insert)) = self.cmdline_complete_take_accept() else {
            return false;
        };
        self.cmdline.replace_range(start..end, &insert);
        self.cmdline_col = start + insert.len();
        true
    }

    /// The token being completed in the ex command line left of the cursor, as
    /// `(anchor_byte, anchor_display_width, prefix)`. Two cases:
    ///
    /// - **No whitespace yet** (still typing the command name): the token runs from
    ///   the first ASCII-alphabetic char (after an optional leading range `'<,'>` /
    ///   `%` / `1,$` …) up to the cursor. `None` when there is no command name yet.
    /// - **Whitespace seen** (in the arguments): the token is the current argument
    ///   word — from just after the last whitespace up to the cursor (empty when the
    ///   cursor sits right after a space, so `:set <Tab>` offers every option). The
    ///   source decides what to offer for it from the whole `line`.
    ///
    /// `anchor` is the byte offset of the token start; accepting a row replaces
    /// `[anchor .. cmdline_col)`.
    fn cmdline_complete_token(&self) -> Option<(usize, usize, String)> {
        let upto = &self.cmdline[..self.cmdline_col];
        // An argument word: everything after the last run of whitespace. `rfind`
        // lands on the last whitespace char; the token starts just past it.
        let start = if let Some(ws) = upto.rfind(char::is_whitespace) {
            ws + upto[ws..].chars().next().map_or(1, char::len_utf8)
        } else {
            // The command name (no whitespace yet): skip a leading range.
            upto.find(|c: char| c.is_ascii_alphabetic())?
        };
        let anchor_width = crate::unicode::display_width(&upto[..start]);
        Some((start, anchor_width, upto[start..].to_string()))
    }
}
