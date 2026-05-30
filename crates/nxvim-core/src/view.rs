//! The renderable view of the editor: semantic regions, not a baked grid.
//!
//! The core no longer lays out a flat screen (status/command lines are not
//! painted into text rows). Instead it produces a [`View`] describing *what* to
//! show in each region, and the client arranges those regions with its own
//! widgets. This keeps layout and styling a UI concern while the core stays the
//! single source of truth for content, scrolling, and cursor placement.
//!
//! Columns are byte offsets (ropey's native metric and vim's column model). One
//! display cell per byte for now — no wide-char/tab-width handling yet.

use crate::editor::Editor;
use crate::mode::Mode;

/// A snapshot of everything a client needs to draw a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Visible text rows (the text viewport). Empty rows below the buffer are
    /// the literal string `"~"`, as in vim.
    pub lines: Vec<String>,
    /// Cursor position within the text viewport (row relative to the top of the
    /// visible window; `col` is a byte/column offset within the line).
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// Uppercase mode name for the status line, e.g. `"NORMAL"`.
    pub mode_label: String,
    /// True while in command-line mode; the cursor then belongs to the command
    /// region, which the client owns.
    pub command_mode: bool,
    /// Command-line contents (text after the leading `:`).
    pub cmdline: String,
    /// Transient status message (shown on the command line when not typing one).
    pub message: String,
    /// File name for the status line (`"[No Name]"` when unset).
    pub file_name: String,
    pub modified: bool,
    /// 1-based cursor line, for the status-line ruler.
    pub cursor_line: usize,
}

impl View {
    pub(crate) fn from_editor(ed: &Editor) -> View {
        let (_, height) = ed.dims();
        let line_count = ed.buffer.line_count();

        let mut lines = Vec::with_capacity(height);
        for row in 0..height {
            let idx = ed.top + row;
            if idx < line_count {
                lines.push(ed.buffer.line(idx));
            } else {
                lines.push("~".to_string());
            }
        }

        let file_name = ed
            .buffer
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[No Name]".to_string());

        View {
            lines,
            cursor_row: ed.cursor.line.saturating_sub(ed.top).min(height.saturating_sub(1)),
            cursor_col: ed.cursor.col,
            mode_label: ed.mode.label().to_string(),
            command_mode: ed.mode == Mode::Command,
            cmdline: ed.cmdline.clone(),
            message: ed.message.clone(),
            file_name,
            modified: ed.buffer.modified,
            cursor_line: ed.cursor.line + 1,
        }
    }
}
