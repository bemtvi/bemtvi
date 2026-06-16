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
    /// The token being completed — the leading command name typed so far. Core
    /// fuzzy-ranks the catalog labels against this.
    pub prefix: String,
    /// Byte offset in [`Editor::cmdline`] where the token starts. Accepting a row
    /// replaces `[anchor .. cmdline_col)` with the chosen command (Phase 2).
    pub anchor: usize,
    /// Display width of the command-line text before the token — the column the
    /// menu anchors under (after the `:` prompt char).
    pub anchor_width: usize,
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
    /// cmdline menu) when the engine is disabled, the line is not an ex command line,
    /// or the cursor is past the command name (argument territory — Phase 1 offers
    /// names only).
    pub(crate) fn cmdline_complete_trigger(&mut self) {
        if !self.cmdcomplete.enabled || !matches!(self.cmdline_kind, CmdlineKind::Ex) {
            self.close_cmdline_menu();
            return;
        }
        let Some((anchor, anchor_width, prefix)) = self.cmdline_command_token() else {
            self.close_cmdline_menu();
            return;
        };
        self.cmdline_complete_request = Some(CmdlineCompleteReq {
            line: self.cmdline.clone(),
            col: self.cmdline_cursor(),
            prefix,
            anchor,
            anchor_width,
        });
    }

    /// Re-run the trigger when a cmdline menu is already open (a content edit
    /// narrowed / changed the token). A no-op when no cmdline menu is open — typing
    /// before `<Tab>` never opens the menu (on-demand activation).
    pub(crate) fn cmdline_complete_refresh(&mut self) {
        if self.cmdline_menu_open() {
            self.cmdline_complete_trigger();
        }
    }

    /// Accept the highlighted command-line completion row: replace the command-name
    /// token `[anchor .. cmdline_col)` with the chosen command and place the cursor at
    /// its end (so further typing — an argument — appends past the accepted name).
    /// Returns whether a row was accepted; `false` (a noselect popup, nothing
    /// highlighted) leaves the line untouched so the caller runs it as typed. The
    /// popup itself is closed by [`cmdline_complete_take_accept`] on success — the
    /// caller closes any still-open noselect popup separately.
    pub(crate) fn cmdline_complete_accept(&mut self) -> bool {
        let Some((anchor, insert)) = self.cmdline_complete_take_accept() else {
            return false;
        };
        self.cmdline
            .replace_range(anchor..self.cmdline_col, &insert);
        self.cmdline_col = anchor + insert.len();
        true
    }

    /// The leading **command-name** token of the ex command line left of the cursor,
    /// as `(anchor_byte, anchor_display_width, prefix)`. The command name is the run
    /// of chars from the first ASCII-alphabetic char (after an optional leading range
    /// `'<,'>` / `%` / `1,$` …) up to the cursor. `None` when whitespace separates
    /// the command name from the cursor — the cursor is in the command's arguments,
    /// which Phase 1 does not complete.
    fn cmdline_command_token(&self) -> Option<(usize, usize, String)> {
        let upto = &self.cmdline[..self.cmdline_col];
        let start = upto.find(|c: char| c.is_ascii_alphabetic())?;
        let token = &upto[start..];
        if token.contains(char::is_whitespace) {
            return None;
        }
        let anchor_width = crate::unicode::display_width(&upto[..start]);
        Some((start, anchor_width, token.to_string()))
    }
}
