//! The renderable view of the editor: semantic regions, not a baked grid.
//!
//! The core no longer lays out a flat screen (status/command lines are not
//! painted into text rows). Instead it produces a [`View`] describing *what* to
//! show in each region, and the client arranges those regions with its own
//! widgets. This keeps layout and styling a UI concern while the core stays the
//! single source of truth for content, scrolling, and cursor placement.
//!
//! Columns are byte offsets (ropey's native metric and vim's column model);
//! `cursor_screen_col` additionally carries the cursor's screen-cell column,
//! accounting for wide characters and tabs.

use crate::editor::Editor;
use crate::mode::Mode;
use crate::unicode;

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
    /// Cursor's screen-cell column on its line (wide-char and tab aware). Used
    /// by clients to place the terminal cursor; `cursor_col` stays the byte
    /// column for the ruler and `nvim_win_get_cursor`.
    pub cursor_screen_col: usize,
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
    /// Per visible row (aligned with `lines`), the half-open screen-column span
    /// `[start, end)` to paint as the visual-mode selection, or `None` when that
    /// row carries no selection. All `None` outside visual modes. `end` may
    /// exceed the row's text width to mark a selected newline (one extra cell) or
    /// to fill a linewise selection to the viewport edge.
    pub selection: Vec<Option<(usize, usize)>>,
}

impl View {
    pub(crate) fn from_editor(ed: &Editor) -> View {
        let (width, height) = ed.dims();
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

        let selection = selection_spans(ed, width, line_count);

        let file_name = ed
            .buffer
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[No Name]".to_string());

        let cursor_screen_col = {
            let line = ed.buffer.line(ed.cursor.line);
            unicode::virtcol(&line, ed.cursor.col, unicode::TABSTOP)
        };

        View {
            lines,
            cursor_row: ed
                .cursor
                .line
                .saturating_sub(ed.top)
                .min(height.saturating_sub(1)),
            cursor_col: ed.cursor.col,
            cursor_screen_col,
            mode_label: ed.mode.label().to_string(),
            command_mode: ed.mode == Mode::Command,
            cmdline: ed.cmdline.clone(),
            message: ed.message.clone(),
            file_name,
            modified: ed.buffer.modified,
            cursor_line: ed.cursor.line + 1,
            selection,
        }
    }
}

/// Compute, for each of the `height` visible rows, the half-open screen-column
/// span to highlight as the visual selection (or `None`). Returns all-`None`
/// outside visual modes.
fn selection_spans(ed: &Editor, width: usize, line_count: usize) -> Vec<Option<(usize, usize)>> {
    let (_, height) = ed.dims();
    let mut spans = vec![None; height];
    if !ed.mode.is_visual() {
        return spans;
    }

    // Order the two ends of the selection by buffer position.
    let a = ed.visual_anchor();
    let c = ed.cursor;
    let (start, end) = if (a.line, a.col) <= (c.line, c.col) {
        (a, c)
    } else {
        (c, a)
    };
    let linewise = ed.mode == Mode::VisualLine;

    for (row, span) in spans.iter_mut().enumerate() {
        let buf_line = ed.top + row;
        if buf_line >= line_count || buf_line < start.line || buf_line > end.line {
            continue;
        }
        let text = ed.buffer.line(buf_line);

        if linewise {
            // Whole line, filled to the viewport edge — as vim paints it.
            *span = Some((0, width));
            continue;
        }

        // Charwise: clip the inclusive [start, end] region to this row.
        let lo = if buf_line == start.line { start.col } else { 0 };
        let start_col = unicode::virtcol(&text, lo, unicode::TABSTOP);
        let end_col = if buf_line == end.line {
            // Include the grapheme under the trailing cursor.
            let hi = unicode::next_grapheme(&text, end.col.min(text.len()));
            unicode::virtcol(&text, hi, unicode::TABSTOP)
        } else {
            // The selection continues onto the next line: highlight the text and
            // one extra cell standing in for the selected newline.
            unicode::virtcol(&text, text.len(), unicode::TABSTOP) + 1
        };
        *span = Some((start_col, end_col));
    }

    spans
}
