//! Cursor placement, grapheme stepping, `curswant` column memory, and viewport
//! scrolling helpers.

use super::*;
use crate::unicode;

impl Editor {
    pub(crate) fn text_height(&self) -> usize {
        // The window's own rows minus a bordered float's one-cell inset (top and
        // bottom) and its status line *when it has one*. The vertical analog of
        // `text_width`: it must match the `height` `view::window_view` projects so
        // the cursor-visibility math scrolls at the real bottom of the text area.
        // The status line is gated on `window_statusline_visible` — exactly as the
        // view projection is — so a float (no status line) uses its full inset
        // height, and a tiled window subtracts its status row only when one shows.
        // The panel is already excluded from the window rect by `relayout`, so it
        // is not subtracted again here.
        let w = self.windows.cur();
        let inset = matches!(&w.float, Some(cfg) if cfg.border != BorderStyle::None) as usize;
        let status = usize::from(self.window_statusline_visible(w.float.is_some()));
        w.rect
            .height
            .saturating_sub(2 * inset)
            .saturating_sub(status)
            .max(1)
    }

    /// The focused window's text-area width in screen cells: its rect width minus
    /// a bordered float's one-cell inset (on each side) and the number gutter. The
    /// horizontal analog of [`text_height`], feeding the `leftcol` scroll math.
    /// Matches the `width` `view::window_view` projects, so on-screen and policy
    /// agree.
    pub(crate) fn text_width(&self) -> usize {
        let w = self.windows.cur();
        let inset = matches!(&w.float, Some(cfg) if cfg.border != BorderStyle::None) as usize;
        let options = w.options;
        let rect_width = w.rect.width;
        let line_count = self.buffer().line_count();
        let number_width = self.number_width_for(options, line_count);
        rect_width
            .saturating_sub(2 * inset)
            .saturating_sub(number_width)
    }

    /// The last real line index (`line_count - 1`, saturating). The rope's phantom
    /// trailing line is never included.
    pub(crate) fn last_line(&self) -> usize {
        self.buffer().line_count().saturating_sub(1)
    }

    pub(crate) fn cursor_char(&self) -> usize {
        self.buffer().byte_at(self.cursor.line, self.cursor.col)
    }

    pub(crate) fn char_at(&self, idx: usize) -> char {
        // Non-boundary bytes (inside a multi-byte char) read as blank rather
        // than panicking; cursor/operator positions are kept on boundaries.
        self.buffer().text.get_char(idx).unwrap_or(' ')
    }

    /// Byte offset one grapheme-cluster forward from `idx` over the whole buffer.
    /// The trailing `\n` of each line is itself a single-byte grapheme.
    pub(crate) fn next_grapheme_idx(&self, idx: usize) -> usize {
        let line = self.buffer().byte_to_line(idx);
        let start = self.buffer().line_start(line);
        let s = self.buffer().line_cow(line);
        let rel = idx - start;
        if rel < s.len() {
            start + unicode::next_grapheme(&s, rel)
        } else {
            (idx + 1).min(self.buffer().len_bytes())
        }
    }

    /// Byte offset one grapheme-cluster backward from `idx` over the whole buffer.
    pub(crate) fn prev_grapheme_idx(&self, idx: usize) -> usize {
        if idx == 0 {
            return 0;
        }
        let line = self.buffer().byte_to_line(idx);
        let start = self.buffer().line_start(line);
        let s = self.buffer().line_cow(line);
        let rel = idx - start;
        if rel == 0 {
            idx - 1
        } else {
            start + unicode::prev_grapheme(&s, rel.min(s.len()))
        }
    }

    /// Snap an absolute byte offset down to a grapheme boundary.
    pub(crate) fn grapheme_floor_abs(&self, idx: usize) -> usize {
        let line = self.buffer().byte_to_line(idx);
        let start = self.buffer().line_start(line);
        let s = self.buffer().line_cow(line);
        let rel = idx.saturating_sub(start).min(s.len());
        start + unicode::floor_grapheme(&s, rel)
    }

    /// Snap an absolute byte offset up to a grapheme boundary.
    pub(crate) fn grapheme_ceil_abs(&self, idx: usize) -> usize {
        let floored = self.grapheme_floor_abs(idx);
        if floored >= idx {
            floored
        } else {
            self.next_grapheme_idx(floored)
        }
    }

    /// The current buffer's `tabstop`: cells a tab expands to, the grid tabs
    /// render on and the cursor snaps to. Floored at 1 so a degenerate `0`
    /// (set via `:set tabstop=0`) never divides by zero.
    pub fn tabstop(&self) -> usize {
        self.buffer().options.effective_tabstop()
    }

    /// Virtual (screen) column of the cursor on its current line.
    pub(crate) fn cursor_virtcol(&self) -> usize {
        let s = self.buffer().line_cow(self.cursor.line);
        unicode::virtcol(&s, self.cursor.col, self.tabstop())
    }

    /// Advance `count` grapheme clusters forward from byte offset `from`, never
    /// passing `limit`. Returns the new offset and how many clusters were crossed.
    pub(crate) fn advance_graphemes(
        &self,
        mut from: usize,
        count: usize,
        limit: usize,
    ) -> (usize, usize) {
        let mut crossed = 0;
        while crossed < count && from < limit {
            let next = self.next_grapheme_idx(from).min(limit);
            if next == from {
                break;
            }
            from = next;
            crossed += 1;
        }
        (from, crossed)
    }

    /// Snap the cursor column down to the nearest grapheme boundary (a no-op for
    /// ASCII), so byte offsets handed to the rope are always valid.
    fn snap_cursor(&mut self) {
        let s = self.buffer().line_cow(self.cursor.line);
        self.cursor.col = unicode::floor_grapheme(&s, self.cursor.col.min(s.len()));
    }

    pub(crate) fn last_char_idx(&self) -> usize {
        // The trailing '\n' is never a valid cursor position.
        self.buffer().len_bytes().saturating_sub(1)
    }

    pub(crate) fn line_len(&self) -> usize {
        self.buffer().line_len(self.cursor.line)
    }

    pub(crate) fn first_non_blank(&self, line: usize) -> usize {
        let s = self.buffer().line_cow(line);
        s.bytes().take_while(|b| *b == b' ' || *b == b'\t').count()
    }

    pub(crate) fn set_cursor_char(&mut self, idx: usize) {
        let idx = self
            .buffer()
            .text
            .floor_char_boundary(idx.min(self.last_char_idx()));
        let line = self.buffer().byte_to_line(idx);
        self.cursor.line = line;
        self.cursor.col = idx - self.buffer().line_start(line);
        self.snap_cursor();
    }

    pub(crate) fn set_cursor_char_insert(&mut self, idx: usize) {
        let idx = self
            .buffer()
            .text
            .floor_char_boundary(idx.min(self.buffer().len_bytes()));
        let line = self.buffer().byte_to_line(idx);
        self.cursor.line = line;
        self.cursor.col = idx - self.buffer().line_start(line);
        self.snap_cursor();
    }

    pub(crate) fn move_vertical(&mut self, delta: i64, allow_eol: bool) {
        let new = (self.cursor.line as i64 + delta).max(0) as usize;
        self.cursor.line = new.min(self.last_line());
        self.settle_desired_col(allow_eol);
        self.preserve_desired = true;
    }

    /// Place the cursor on the current line at the remembered desired *virtual*
    /// column (or end-of-line when `$`-sticky), clamped to the line and a grapheme
    /// boundary.
    pub(crate) fn settle_desired_col(&mut self, allow_eol: bool) {
        let s = self.buffer().line_cow(self.cursor.line);
        // Furthest valid resting byte: past-end for insert/allow_eol, otherwise
        // the start of the last grapheme (normal mode can't rest past EOL).
        let max_byte = if allow_eol {
            s.len()
        } else {
            unicode::prev_grapheme(&s, s.len())
        };
        let target = if self.desired_eol {
            max_byte
        } else {
            unicode::byte_at_virtcol(&s, self.desired_col, self.tabstop()).min(max_byte)
        };
        self.cursor.col = unicode::floor_grapheme(&s, target);
    }

    pub(crate) fn clamp_cursor(&mut self) {
        let last_line = self.last_line();
        if self.cursor.line > last_line {
            self.cursor.line = last_line;
        }
        let len = self.line_len();
        let max_col = if self.mode.is_insert() {
            len
        } else {
            len.saturating_sub(1)
        };
        if self.cursor.col > max_col {
            self.cursor.col = max_col;
        }
        self.snap_cursor();
    }

    pub(crate) fn scroll_half(&mut self, down: bool) {
        let half = (self.text_height() / 2).max(1) as i64;
        self.scroll_by(if down { half } else { -half });
    }

    pub(crate) fn scroll_page(&mut self, down: bool) {
        let page = self.text_height().saturating_sub(2).max(1) as i64;
        self.scroll_by(if down { page } else { -page });
    }

    /// Move the viewport one line — `<C-e>` (down) / `<C-y>` (up) — *without*
    /// touching the cursor, returning whether `top` actually moved. `<C-e>` can
    /// scroll until the last buffer line reaches the top row; `<C-y>` stops at the
    /// first. The pure-viewport primitive behind [`Self::scroll_line`].
    fn scroll_view_line(&mut self, down: bool) -> bool {
        let new_top = if down {
            (self.top + 1).min(self.last_line())
        } else {
            self.top.saturating_sub(1)
        };
        if new_top == self.top {
            return false;
        }
        self.scroll_from = Some((self.top, self.cursor.line));
        self.top = new_top;
        true
    }

    /// `<C-e>` / `<C-y>`: scroll the viewport one line, keeping the cursor on its
    /// buffer line unless the scroll pushes it off-screen — then pull it to the
    /// nearest visible edge (scrolloff is 0), at its remembered desired column.
    /// Unlike [`Self::scroll_by`] (`<C-d>`/`<C-f>`), the cursor does *not* travel
    /// with the view while it stays visible.
    pub(crate) fn scroll_line(&mut self, down: bool) {
        if !self.scroll_view_line(down) {
            return;
        }
        let bottom = self.top + self.text_height() - 1;
        if self.cursor.line < self.top {
            self.cursor.line = self.top;
        } else if self.cursor.line > bottom {
            self.cursor.line = bottom.min(self.last_line());
        } else {
            return; // cursor still on screen — leave it (and its curswant) put
        }
        self.settle_desired_col(false);
        self.preserve_desired = true;
    }

    /// Scroll the viewport by `delta` lines, vim-style: move both `top` and the
    /// cursor together so the cursor keeps its screen row. Records the pre-move
    /// `(top, cursor.line)` in `scroll_from`; `input` turns that into a
    /// `PendingScroll` if `top` actually changed.
    fn scroll_by(&mut self, delta: i64) {
        self.scroll_from = Some((self.top, self.cursor.line));
        let last = self.last_line() as i64;
        self.top = (self.top as i64 + delta).clamp(0, last) as usize;
        self.move_vertical(delta, false);
        self.clamp_cursor();
    }

    pub(crate) fn ensure_visible(&mut self) {
        let th = self.text_height();
        if self.cursor.line < self.top {
            self.top = self.cursor.line;
        } else if self.cursor.line >= self.top + th {
            self.top = self.cursor.line + 1 - th;
        }
        // Horizontal scroll follows the cursor on the same beat as the vertical
        // one, so every motion that calls `ensure_visible` also keeps the cursor's
        // column on screen under `nowrap`.
        self.ensure_visible_horizontal();
    }

    /// Keep the cursor's screen column within the focused window's text area by
    /// adjusting [`Editor::leftcol`] — the horizontal analog of [`ensure_visible`]
    /// (vim's `nowrap` `w_leftcol`). Honors `sidescroll` (the scroll step: `0`
    /// recenters the cursor, `> 0` scrolls just enough to bring it to the edge) and
    /// `sidescrolloff` (the margin kept between the cursor and the edge). A no-op
    /// for a degenerate (zero-width) text area.
    fn ensure_visible_horizontal(&mut self) {
        let tw = self.text_width();
        if tw == 0 {
            return;
        }
        let opts = self.windows.cur().options;
        // The margin can't claim more than half the window, or the left and right
        // bounds would cross.
        let so = opts.sidescrolloff.min(tw.saturating_sub(1) / 2);
        let recenter = opts.sidescroll == 0;
        let vc = self.cursor_virtcol();
        let left = self.leftcol;

        if vc < left + so {
            // Cursor at/past the left margin: scroll left.
            self.leftcol = if recenter {
                vc.saturating_sub(tw / 2)
            } else {
                vc.saturating_sub(so)
            };
        } else if vc + so + 1 > left + tw {
            // Cursor at/past the right margin: scroll right.
            self.leftcol = if recenter {
                vc.saturating_sub(tw / 2)
            } else {
                (vc + so + 1).saturating_sub(tw)
            };
        }
    }
}
