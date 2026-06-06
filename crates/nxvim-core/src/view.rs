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

/// A scroll gesture for the client to animate. Self-contained: it carries its
/// own band of rendered lines (`lines`) and selection spans covering every row
/// visible during the slide, anchored at `base_line`. The client interpolates
/// `from`→`to` against its local clock and slices `lines` per frame; the main
/// `View` fields stay the *destination* viewport for clients that don't animate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollAnim {
    pub from_top: usize,
    pub to_top: usize,
    pub from_cursor: usize,
    pub to_cursor: usize,
    pub duration_ms: u64,
    /// Buffer-line index of `lines[0]` (= `min(from_top, to_top)`).
    pub base_line: usize,
    /// `|to_top - from_top| + height` rows starting at `base_line`, "~"-padded
    /// past end of buffer.
    pub lines: Vec<String>,
    /// Selection spans aligned with `lines` (same length).
    pub selection: Vec<Option<(usize, usize)>>,
    /// 1-based buffer line number per row (aligned with `lines`), `None` for
    /// `~` filler rows, so the number column slides with the text during the
    /// animation.
    pub numbers: Vec<Option<usize>>,
}

/// The renderable form of the bottom [`Panel`](crate::editor): a title, the
/// visible slice of its content, the cursor's row within that slice, and the
/// content height the client lays the panel out to. `None` in [`View::panel`]
/// when no panel is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PanelView {
    /// Label shown in the panel's title bar (e.g. `Messages`, `Buffers`).
    pub title: String,
    /// The visible content rows (already scrolled and **word-wrapped** to the
    /// panel width); never longer than `height`. The client pads shorter content
    /// with blank rows. A long logical entry occupies several consecutive rows.
    pub lines: Vec<String>,
    /// First display row (within the visible slice) of the selected logical
    /// entry. The client places the editing cursor here.
    pub cursor_row: usize,
    /// Number of consecutive display rows the selected entry occupies in the
    /// visible slice (≥ 1 — more than one when the entry wrapped). The client
    /// highlights `cursor_row .. cursor_row + cursor_span` as the focused line, so
    /// the whole wrapped entry reads as selected.
    pub cursor_span: usize,
    /// Content height in rows (excludes the title row). The client lays the
    /// whole panel out as `height + 1` rows; the editor sized it so the text
    /// window keeps at least one row.
    pub height: usize,
}

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
    /// True while `r` waits for its replacement character (a one-shot replace
    /// that stays in normal mode). Clients show the replace cursor shape while it
    /// holds, mirroring vim's operator-pending feedback.
    pub pending_replace: bool,
    /// Command-line contents (text after the leading prompt char).
    pub cmdline: String,
    /// The command-line prompt character: `:` for an ex command, `/` / `?` for a
    /// forward / backward search. Only meaningful while `command_mode`.
    pub cmdline_prefix: char,
    /// The multi-char prompt label for a `vim.ui.input` prompt (Phase 8), shown
    /// ahead of the editable line in place of `cmdline_prefix`. Empty for
    /// `:`/`/`/`?`. Only meaningful while `command_mode`.
    pub cmdline_prompt: String,
    /// Cursor position within `cmdline` as a character count from its start, so
    /// the client can place the command cursor mid-line after `<Left>`/`<Right>`
    /// edits. Only meaningful while `command_mode`.
    pub cmdline_cursor: usize,
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
    /// Per visible row (aligned with `lines`), the half-open screen-column spans
    /// of every search match on that row — the `Search`/`hlsearch` highlight.
    /// Empty inner vecs for rows with no match; all empty when no search is
    /// active (or it was cleared by `:noh`).
    pub search: Vec<Vec<(usize, usize)>>,
    /// Per visible row, the single match the live `incsearch` preview rests on
    /// (the `IncSearch` highlight), or `None`. All `None` outside an active
    /// incsearch preview.
    pub incsearch: Vec<Option<(usize, usize)>>,
    /// Present only on a redraw caused by a scroll command that moved the
    /// viewport; carries the data a client needs to animate the slide.
    pub scroll: Option<ScrollAnim>,
    /// 1-based buffer line number per visible row (aligned with `lines`), or
    /// `None` for `~` filler rows past the end of the buffer. The client formats
    /// the number column (absolute / relative / hybrid) from these.
    pub numbers: Vec<Option<usize>>,
    /// `:set number` — show the absolute line number.
    pub number: bool,
    /// `:set relativenumber` — show numbers relative to the cursor line.
    pub relativenumber: bool,
    /// Width in cells of the number column (`0` when both options are off).
    pub number_width: usize,
    /// The bottom panel (`:messages`, `:ls`), or `None` when none is open. When
    /// present it has input focus, so the client draws the editing cursor inside
    /// the panel rather than the text window.
    pub panel: Option<PanelView>,
}

impl View {
    pub(crate) fn from_editor(ed: &Editor) -> View {
        // The text window height — the full UI height minus any rows the bottom
        // panel claims (so `lines` is sized to the area the client paints text).
        let height = ed.text_height();
        let line_count = ed.buffer().line_count();
        // Selections fill to the text width — the area past the number gutter.
        let width = ed.text_width();

        let lines = window_lines(ed, ed.top, height, line_count);
        let selection = selection_spans(ed, width, line_count, ed.top, height);
        let (search, incsearch) = ed.search_highlights(ed.top, height);
        let numbers = window_numbers(ed.top, height, line_count);

        let scroll = ed.pending_scroll().map(|ps| {
            let base_line = ps.from_top.min(ps.to_top);
            let count = ps.from_top.abs_diff(ps.to_top) + height;
            ScrollAnim {
                from_top: ps.from_top,
                to_top: ps.to_top,
                from_cursor: ps.from_cursor,
                to_cursor: ps.to_cursor,
                duration_ms: ps.duration_ms,
                base_line,
                lines: window_lines(ed, base_line, count, line_count),
                selection: selection_spans(ed, width, line_count, base_line, count),
                numbers: window_numbers(base_line, count, line_count),
            }
        });

        let file_name = ed
            .buffer()
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "[No Name]".to_string());

        let cursor_screen_col = {
            let line = ed.buffer().line(ed.cursor.line);
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
            pending_replace: ed.pending_replace(),
            cmdline: ed.cmdline.clone(),
            cmdline_prefix: ed.cmdline_prefix(),
            cmdline_prompt: ed.cmdline_prompt().to_string(),
            cmdline_cursor: ed.cmdline_cursor(),
            message: ed.message.clone(),
            file_name,
            modified: ed.buffer().modified,
            cursor_line: ed.cursor.line + 1,
            selection,
            search,
            incsearch,
            scroll,
            numbers,
            number: ed.options.number,
            relativenumber: ed.options.relativenumber,
            number_width: ed.number_width(),
            panel: ed.panel_view(),
        }
    }
}

/// 1-based buffer line number for each of the `count` rows starting at buffer
/// line `base`, `None` for rows past the end of the buffer (the `~` fillers).
fn window_numbers(base: usize, count: usize, line_count: usize) -> Vec<Option<usize>> {
    (0..count)
        .map(|row| {
            let idx = base + row;
            (idx < line_count).then_some(idx + 1)
        })
        .collect()
}

/// Build `count` rendered rows starting at buffer line `base`, padding rows past
/// the end of the buffer with `"~"` (as vim shows below the last line).
fn window_lines(ed: &Editor, base: usize, count: usize, line_count: usize) -> Vec<String> {
    let mut lines = Vec::with_capacity(count);
    for row in 0..count {
        let idx = base + row;
        if idx < line_count {
            lines.push(ed.buffer().line(idx));
        } else {
            lines.push("~".to_string());
        }
    }
    lines
}

/// Compute, for each of the `count` rows starting at buffer line `base`, the
/// half-open screen-column span to highlight as the visual selection (or
/// `None`). Returns all-`None` outside visual modes.
fn selection_spans(
    ed: &Editor,
    width: usize,
    line_count: usize,
    base: usize,
    count: usize,
) -> Vec<Option<(usize, usize)>> {
    let mut spans = vec![None; count];
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
        let buf_line = base + row;
        if buf_line >= line_count || buf_line < start.line || buf_line > end.line {
            continue;
        }
        let text = ed.buffer().line(buf_line);

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
