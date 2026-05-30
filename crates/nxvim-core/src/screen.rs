//! Rendering the editor state into a flat grid of text rows.
//!
//! The server serializes a [`Screen`] into RPC redraw events; the client paints
//! it. Keeping rendering here (in the core) means every client — terminal today,
//! a native GUI later — sees identical, already-laid-out output.
//!
//! Limitations (intentional for now): one display cell per `char` (no
//! wide-char / tab-width handling yet) and no syntax highlight attributes.

use crate::editor::Editor;
use crate::mode::Mode;

/// A fully laid-out screen: `height` rows of exactly `width` columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Screen {
    pub width: usize,
    pub height: usize,
    /// One string per row; each is padded/truncated to `width` display cells.
    pub lines: Vec<String>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    /// Short mode code (vim's `mode()`), e.g. `"n"`, `"i"`, `"c"`.
    pub mode: String,
}

impl Screen {
    pub(crate) fn from_editor(ed: &Editor) -> Screen {
        let (width, height) = ed.dims();
        let text_height = ed.text_height();
        let mut lines = Vec::with_capacity(height);

        let line_count = ed.buffer.line_count();
        for row in 0..text_height {
            let idx = ed.top + row;
            if idx < line_count {
                lines.push(fit(&ed.buffer.line(idx), width));
            } else {
                lines.push(fit("~", width));
            }
        }

        lines.push(fit(&status_line(ed), width));
        lines.push(fit(&bottom_line(ed), width));

        let (cursor_row, cursor_col) = if ed.mode == Mode::Command {
            (height.saturating_sub(1), 1 + ed.cmdline.chars().count())
        } else {
            let row = ed.cursor.line.saturating_sub(ed.top);
            (row.min(text_height.saturating_sub(1)), ed.cursor.col.min(width.saturating_sub(1)))
        };

        Screen {
            width,
            height,
            lines,
            cursor_row,
            cursor_col,
            mode: ed.mode.short_code().to_string(),
        }
    }
}

fn status_line(ed: &Editor) -> String {
    let name = ed
        .buffer
        .path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "[No Name]".to_string());
    let modified = if ed.buffer.modified { " [+]" } else { "" };
    let left = format!(" {}  {}{}", ed.mode.label(), name, modified);
    let right = format!("{},{} ", ed.cursor.line + 1, ed.cursor.col + 1);

    let total = ed.dims().0;
    let pad = total.saturating_sub(left.chars().count() + right.chars().count());
    format!("{left}{}{right}", " ".repeat(pad))
}

fn bottom_line(ed: &Editor) -> String {
    if ed.mode == Mode::Command {
        format!(":{}", ed.cmdline)
    } else {
        ed.message.clone()
    }
}

/// Truncate or right-pad `s` to exactly `width` display cells.
fn fit(s: &str, width: usize) -> String {
    let mut out = String::with_capacity(width);
    let mut count = 0;
    for c in s.chars() {
        if count >= width {
            break;
        }
        // Render tabs as a single space for now (no elastic tab stops yet).
        out.push(if c == '\t' { ' ' } else { c });
        count += 1;
    }
    if count < width {
        out.push_str(&" ".repeat(width - count));
    }
    out
}
