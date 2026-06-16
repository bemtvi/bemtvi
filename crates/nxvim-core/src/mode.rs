/// The editor's current input mode.
///
/// Mirrors vim's modes. The set is deliberately small for now and will grow
/// (operator-pending, terminal, select, etc.) as the editor matures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Replace,
    Visual,
    VisualLine,
    /// Command-line mode (`:` ex commands).
    Command,
    /// Multi-cursor *placement* mode (nxvim-specific, entered with `<A-c>`).
    /// Motions move only the active (primary) cursor so you can navigate and drop
    /// cursors (`c` / `{count}c{motion}`); leaving with `<Esc>` keeps the placed
    /// cursors and returns to Normal, where motions and edits act on them all.
    MultiCursor,
    /// Terminal-job mode: the current buffer hosts a live PTY child process and
    /// keystrokes are forwarded to it as input bytes (vim/neovim's `t` mode).
    /// `<C-\><C-n>` leaves to Normal, where the terminal buffer reads as ordinary
    /// (read-only) text for scrolling / yanking.
    Terminal,
}

/// Which key-handling context owns input right now — the buffer being edited, or a
/// grabbing widget that routes keys through its **own** keymap bucket.
///
/// The keymap engine selects a trie by this rather than [`Mode`] alone: an
/// `Editing` context uses the per-mode trie with the command-grammar disambiguation
/// oracle and the literal-argument bypass; a widget context uses that widget's
/// dedicated bucket (`vim.keymap.set('picker', …)`) with neither (a widget has no
/// core command grammar). Until a widget is converted to the keymap engine it stays
/// `Editing` and its keys are grabbed in core as before. Phase 1 adds `Picker`; the
/// other grabbing widgets (select / panel / explorer / cmdline) follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyContext {
    /// The buffer — the normal/insert/visual/… per-mode trie applies.
    Editing,
    /// A prompted fuzzy picker (`nx.picker`) grabs input; its `picker` bucket applies.
    Picker,
    /// A promptless selectable list (`nx.ui.select`) grabs input; its `select`
    /// bucket applies. No query — every key is a map (an unmapped key is inert).
    Select,
}

impl Mode {
    /// Short uppercase label shown in the status line, e.g. `NORMAL`.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Insert => "INSERT",
            Mode::Replace => "REPLACE",
            Mode::Visual => "VISUAL",
            Mode::VisualLine => "V-LINE",
            Mode::Command => "COMMAND",
            Mode::MultiCursor => "MULTICURSOR",
            Mode::Terminal => "TERMINAL",
        }
    }

    /// The single-letter mode code used by vim's `mode()` builtin.
    pub fn short_code(self) -> &'static str {
        match self {
            Mode::Normal => "n",
            Mode::Insert => "i",
            Mode::Replace => "R",
            Mode::Visual => "v",
            Mode::VisualLine => "V",
            Mode::Command => "c",
            // No vim equivalent; reads as normal mode for `mode()`-checking scripts.
            Mode::MultiCursor => "n",
            Mode::Terminal => "t",
        }
    }

    /// Whether this is the nxvim-specific multi-cursor *placement* mode.
    pub fn is_multicursor(self) -> bool {
        matches!(self, Mode::MultiCursor)
    }

    pub fn is_insert(self) -> bool {
        matches!(self, Mode::Insert | Mode::Replace)
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine)
    }
}
