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
        }
    }

    pub fn is_insert(self) -> bool {
        matches!(self, Mode::Insert | Mode::Replace)
    }

    pub fn is_visual(self) -> bool {
        matches!(self, Mode::Visual | Mode::VisualLine)
    }
}
