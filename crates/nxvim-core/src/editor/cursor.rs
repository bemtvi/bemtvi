//! Cursor placement, grapheme stepping, `curswant` column memory, and viewport
//! scrolling helpers.

use super::*;
use crate::unicode;

/// The effective `'scrolloff'` margin for a `th`-row text area: the option clamped
/// to half the height so a top *and* a bottom margin can both fit (vim clamps the
/// same way). `0` (the default) reduces every caller to the historical no-margin
/// behavior. Shared by the viewport math and every scroll that has to leave the
/// cursor where [`Editor::ensure_visible`] will accept it, so the two never disagree
/// about where the band is.
pub(crate) fn scroll_margin(scrolloff: usize, th: usize) -> usize {
    scrolloff.min(th.saturating_sub(1) / 2)
}

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
        // The focused window lives in the focused layer, so its statusline gate uses
        // that layer's region (which carries any per-dock `'laststatus'` override).
        let status =
            usize::from(self.window_statusline_visible(self.focused_region(), w.float.is_some()));
        // `'padding'` insets the whole content box (top + bottom), matching
        // `window_view`; subtract it before the status line so the math agrees.
        w.rect
            .height
            .saturating_sub(2 * inset)
            .saturating_sub(w.options.padding.vertical())
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
        let options = &w.options;
        let rect_width = w.rect.width;
        let line_count = self.buffer().line_count();
        let number_width = self.number_width_for(options, line_count);
        // `'padding'` insets the whole content box (left + right), matching
        // `window_view`, before the gutter is carved off.
        rect_width
            .saturating_sub(2 * inset)
            .saturating_sub(options.padding.horizontal())
            .saturating_sub(number_width)
            .saturating_sub(options.signcolumn.floor_cells())
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
    pub(crate) fn snap_cursor(&mut self) {
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

    /// Place the cursor at byte `idx`, first clamping it to `max` and snapping to
    /// a char boundary. The shared body behind `set_cursor_char` (clamps to the
    /// last addressable char) and `set_cursor_char_insert` (allows the
    /// past-the-end insert column).
    fn set_cursor_clamped(&mut self, idx: usize, max: usize) {
        let idx = self.buffer().text.floor_char_boundary(idx.min(max));
        let line = self.buffer().byte_to_line(idx);
        self.cursor.line = line;
        self.cursor.col = idx - self.buffer().line_start(line);
        self.snap_cursor();
    }

    pub(crate) fn set_cursor_char(&mut self, idx: usize) {
        let max = self.last_char_idx();
        self.set_cursor_clamped(idx, max);
    }

    pub(crate) fn set_cursor_char_insert(&mut self, idx: usize) {
        let max = self.buffer().len_bytes();
        self.set_cursor_clamped(idx, max);
    }

    /// Settle the cursor on byte `byte`: place it there, refresh the desired
    /// (virtual) column, clear the keep-at-EOL flag, and scroll it into view.
    /// The shared landing tail used by jump/change/mark navigation.
    pub(crate) fn settle_cursor_byte(&mut self, byte: usize) {
        self.set_cursor_char(byte);
        self.desired_col = self.cursor_virtcol();
        self.desired_eol = false;
        self.ensure_visible();
    }

    /// Land the cursor at `(line, col)`, clamping the line to the last line and
    /// the column to that line's last real character (the trailing `\n` excluded),
    /// then run the shared settle tail. Used by jumplist / change-list navigation,
    /// which store raw `(line, col)` pairs that may be stale against an edited
    /// buffer. (Distinct from [`Editor::land_cursor`], which clamps to the full
    /// line length and threads deferred-open landing.)
    pub(crate) fn settle_cursor_at(&mut self, line: usize, col: usize) {
        let line = line.min(self.last_line());
        let max_col = self.buffer().line(line).trim_end_matches('\n').len();
        let col = col.min(max_col);
        self.settle_cursor_byte(self.buffer().byte_at(line, col));
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
        // Insert mode and terminal-job mode both let the cursor sit one past the last
        // char — in a terminal that "next write position" is exactly where the child's
        // cursor is (after the last typed char), not on top of it. An `i_CTRL-O`
        // one-shot (`insert_normal`) does too, behaving like `virtualedit=onemore` so
        // an EOL-append column survives the Normal command and resuming Insert lands
        // past the last char.
        let max_col = if self.mode.is_insert()
            || self.mode == crate::mode::Mode::Terminal
            || self.insert_normal.is_some()
        {
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
    /// scroll until the last buffer line reaches its `'scrolloff'` margin from the
    /// top row ([`max_scroll_top`](Self::max_scroll_top) — the last row itself when
    /// the margin is off); `<C-y>` stops at the first. The pure-viewport primitive
    /// behind [`Self::scroll_line`].
    fn scroll_view_line(&mut self, down: bool) -> bool {
        let new_top = if down {
            // At (or past) the limit the scroll simply stops — never step *back* up,
            // which is what a bare `.min(limit)` would do from a deeper `top`.
            if self.top >= self.max_scroll_top(self.text_height()) {
                return false;
            }
            self.top + 1
        } else {
            self.top.saturating_sub(1)
        };
        if new_top == self.top {
            return false;
        }
        self.top = new_top;
        true
    }

    /// `<C-e>` / `<C-y>`: scroll the viewport one line, keeping the cursor on its
    /// buffer line unless the scroll pushes it within the `'scrolloff'` margin of the
    /// edge — then pull it back to the margin boundary
    /// ([`keep_cursor_in_scroll_margin`](Self::keep_cursor_in_scroll_margin)). Unlike
    /// [`Self::scroll_by`] (`<C-d>`/`<C-f>`), the cursor does *not* travel with the
    /// view while it stays outside the margin.
    pub(crate) fn scroll_line(&mut self, down: bool) {
        if !self.scroll_view_line(down) {
            return;
        }
        let th = self.text_height();
        self.keep_cursor_in_scroll_margin(th);
    }

    /// Pull the cursor into the focused window's `'scrolloff'` band after a
    /// viewport-only scroll moved `top` under it (`<C-e>`/`<C-y>`, the mouse wheel):
    /// a cursor left inside the margin — or scrolled off-screen entirely — is parked
    /// exactly `scrolloff` rows in from the edge it drifted toward, at its remembered
    /// desired column. A no-op while the cursor is still outside the margin, which
    /// leaves it (and its `curswant`) put.
    ///
    /// Parking it on the *margin boundary* rather than the visible edge is
    /// load-bearing, not cosmetic: the per-redraw
    /// [`ensure_visible`](Self::ensure_visible) enforces the same margin, so a cursor
    /// left inside it snaps `top` straight back — the viewport visibly bounces and
    /// the window stops scrolling altogether once the cursor is within `scrolloff`
    /// rows of the edge.
    pub(crate) fn keep_cursor_in_scroll_margin(&mut self, th: usize) {
        let so = scroll_margin(self.windows.cur().options.scrolloff, th);
        let top_edge = self.top + so;
        let bottom = (self.top + th).saturating_sub(1).saturating_sub(so);
        if self.cursor.line < top_edge {
            self.cursor.line = top_edge.min(self.last_line());
        } else if self.cursor.line > bottom {
            self.cursor.line = bottom.min(self.last_line());
        } else {
            return; // cursor still outside the margin — leave it (and its curswant) put
        }
        self.settle_desired_col(false);
        self.preserve_desired = true;
    }

    /// Scroll the viewport by `delta` lines, vim-style: move both `top` and the
    /// cursor together so the cursor keeps its screen row. The pre-move viewport is
    /// snapshotted by [`input`](Self::input), which turns it into a `PendingScroll`
    /// once `top` has moved more than a line.
    fn scroll_by(&mut self, delta: i64) {
        let last = self.last_line() as i64;
        self.top = (self.top as i64 + delta).clamp(0, last) as usize;
        self.move_vertical(delta, false);
        self.clamp_cursor();
    }

    /// The `z`-family viewport repositioning (`zt`/`zz`/`zb` and the
    /// first-non-blank `z<CR>`/`z.`/`z-`). With a `count` the cursor first moves to
    /// that line (1-based), as in vim; then `top` is set so the cursor's line sits
    /// at the top, center, or bottom of the text area. The pre-move `top` is
    /// snapshotted by [`input`](Editor::input), so a move of more than a line
    /// animates like the other scrolls.
    pub(crate) fn view_reposition(
        &mut self,
        place: super::command::ViewPlace,
        first_nonblank: bool,
        count: Option<usize>,
    ) {
        use super::command::ViewPlace;

        if let Some(n) = count {
            self.cursor.line = n.saturating_sub(1).min(self.last_line());
        }
        if first_nonblank {
            // `z<CR>`/`z.`/`z-` land on the line's first non-blank; leaving
            // `preserve_desired` false lets `input` reset curswant to that column.
            self.cursor.col = self.first_non_blank(self.cursor.line);
            self.clamp_cursor();
        } else {
            // `zt`/`zz`/`zb` keep the cursor's remembered column — settling it onto
            // the new line when a count moved there, and leaving it put otherwise.
            if count.is_some() {
                self.settle_desired_col(false);
            }
            self.preserve_desired = true;
        }

        let th = self.text_height();
        let line = self.cursor.line;
        self.top = match place {
            ViewPlace::Top => line,
            ViewPlace::Center => line.saturating_sub(th / 2),
            ViewPlace::Bottom => (line + 1).saturating_sub(th),
        }
        .min(self.last_line());
    }

    pub(crate) fn ensure_visible(&mut self) {
        let th = self.text_height();
        // `'scrolloff'`: keep at least this many text rows above and below the cursor.
        let so = scroll_margin(self.windows.cur().options.scrolloff, th);

        // The largest useful `top`: the last buffer line resting on the bottom text
        // row. The bottom scrolloff margin is only honored while real content sits
        // below the cursor to justify it — never scroll past this to open blank rows
        // under end-of-file (vim pins the last line to the bottom edge and lets the
        // cursor sit inside the margin there).
        let max_top = self.scroll_top_for_bottom(self.last_line(), th);

        // The band of acceptable `top` values that keeps the cursor `so` rows in from
        // either text edge: `top_max` keeps `so` rows above it (else scroll up),
        // `top_min` keeps `so` rows below it (else scroll down), capped by `max_top`.
        // With `so == 0` this is exactly `[scroll_top_for_bottom(cursor, th),
        // cursor.line]` — the historical bottom-scroll / cursor-above-top behavior.
        let top_max = self.scroll_top_for_row_above(self.cursor.line, so);
        let top_min = self
            .scroll_top_for_bottom(self.cursor.line, th - so)
            .min(max_top);
        // Panic-safe clamp: a pathological `virt_lines` layout could invert the
        // bounds, in which case keeping the cursor visible (the bottom target) wins.
        self.top = if top_min <= top_max {
            self.top.clamp(top_min, top_max)
        } else {
            top_min
        };

        // Horizontal scroll follows the cursor on the same beat as the vertical
        // one, so every motion that calls `ensure_visible` also keeps the cursor's
        // column on screen under `nowrap`.
        self.ensure_visible_horizontal();
    }

    /// Scroll the focused window to show as much of the current Visual selection
    /// as possible — used by `gv`. The plain [`ensure_visible`](Self::ensure_visible)
    /// only keeps the live *cursor* on screen, so after scrolling away a `gv`
    /// would land with the selection's far end pinned to the top edge and the
    /// whole body scrolled off above it. Instead: when the selection fits, reveal
    /// it whole (its first line at the top of the window); when it is taller than
    /// the window, brim the window with its tail (the cursor end on the last row).
    /// A no-op when the selection is already wholly on screen, so a `gv` that
    /// needs no scroll never jerks the viewport. The cursor end stays visible
    /// either way, so the [`ensure_visible`](Self::ensure_visible) that follows in
    /// [`input`](Editor::input) leaves the chosen `top` untouched.
    pub(crate) fn reveal_selection(&mut self) {
        let th = self.text_height();
        if th == 0 {
            return;
        }
        // `gv` pins the live cursor to the selection's far end (`` `> ``), so the
        // span runs from the anchor's line down to the cursor's.
        let start = self.visual_anchor.line.min(self.cursor.line);
        // Already wholly on screen — don't yank the viewport around.
        if start >= self.top && self.cursor_screen_row() < th {
            return;
        }
        // Reveal the whole selection with its first line at the top; if that still
        // leaves the cursor end past the bottom the selection out-sizes the window,
        // so pin the cursor end to the last row (the window then brims with its
        // tail), exactly as a scroll-to-bottom would.
        self.top = start;
        if self.cursor_screen_row() >= th {
            self.top = self.scroll_top_for_bottom(self.cursor.line, th);
        }
    }

    /// The number of display (text) rows buffer `line` occupies in the focused
    /// window: `1` under `nowrap` (or for a line that fits), else its soft-wrap
    /// segment count. The text-row analogue of a line's `virt_lines` count, so the
    /// viewport math counts wrapped lines as the several screen rows they fill.
    pub(crate) fn line_text_rows(&self, line: usize) -> usize {
        let opts = &self.windows.cur().options;
        if !opts.wrap {
            return 1;
        }
        let width = self.text_width();
        let buf = self.buffer();
        if width == 0 || line >= buf.line_count() {
            return 1;
        }
        let text = buf.line_cow(line);
        let tab = buf.options.effective_tabstop();
        let indent = unicode::cont_indent(&text, tab, width, opts.wrap_prefix());
        unicode::wrap_segments_indented(&text, tab, width, indent).len()
    }

    /// Which soft-wrap segment the cursor's column falls in (its display-row offset
    /// within its buffer line): `0` under `nowrap` or on the first segment.
    pub(crate) fn cursor_wrap_seg(&self) -> usize {
        let opts = &self.windows.cur().options;
        if !opts.wrap {
            return 0;
        }
        let width = self.text_width();
        if width == 0 {
            return 0;
        }
        let buf = self.buffer();
        let text = buf.line_cow(self.cursor.line);
        let tab = buf.options.effective_tabstop();
        let indent = unicode::cont_indent(&text, tab, width, opts.wrap_prefix());
        let segs = unicode::wrap_segments_indented(&text, tab, width, indent);
        segs.iter()
            .rposition(|s| self.cursor.col >= s.start_byte)
            .unwrap_or(0)
    }

    /// The cursor's text row within the focused window's text body, counting the
    /// `virt_lines` and soft-wrap rows of every buffer line from `top` up to the
    /// cursor (and the cursor line's own `virt_lines_above` + wrap segment). Equals
    /// `cursor.line - top` with no virtual lines or wrapping. Saturates past the
    /// bottom (the caller compares it against the text height to decide to scroll).
    pub(crate) fn cursor_screen_row(&self) -> usize {
        if self.cursor.line < self.top {
            return 0;
        }
        let buf = self.buffer();
        let virt = buf.virt_lines_by_line();
        let mut rows = 0;
        let mut line = self.top;
        while line < self.cursor.line {
            // A closed fold between `top` and the cursor occupies a single screen
            // row; skip its hidden interior so the count matches the rendered rows.
            if let Some(f) = self.collapsing_fold_at(line) {
                rows += 1;
                line = f.end + 1;
                continue;
            }
            rows += self.line_text_rows(line)
                + virt.get(&line).map_or(0, |r| r.above.len() + r.below.len());
            line += 1;
        }
        // The cursor's own row sits *after* its `virt_lines_above` and the wrap
        // segments of its line above the cursor's segment.
        rows + virt.get(&self.cursor.line).map_or(0, |r| r.above.len()) + self.cursor_wrap_seg()
    }

    /// The largest `top` (≤ `target`) such that buffer line `target`'s cursor row
    /// and every line above it down to `top` — each counted with its `virt_lines`
    /// and soft-wrap rows — fit within `th` screen rows, i.e. the cursor lands on
    /// the bottom text row. The `virt_lines_below` of `target` (and any wrap
    /// segments below the cursor's) may spill past the bottom. Used to scroll down
    /// just enough to reveal a cursor below the fold.
    fn scroll_top_for_bottom(&self, target: usize, th: usize) -> usize {
        let virt = self.buffer().virt_lines_by_line();
        // The target line spends its `virt_lines_above` + the wrap segments up to and
        // including the cursor's at the bottom of the window; deeper segments and its
        // `virt_lines_below` overflow.
        let mut rows = virt.get(&target).map_or(0, |r| r.above.len()) + self.cursor_wrap_seg() + 1;
        let mut top = target;
        while top > 0 {
            let (p, prev_top) = self.rows_of_line_above(top, &virt);
            if rows + p > th {
                break;
            }
            rows += p;
            top = prev_top;
        }
        top
    }

    /// One upward step of a scroll-top walk: the screen rows the buffer line just
    /// above `top` occupies, and the line the walk continues from. A closed fold is
    /// ONE screen row and the walk steps over its whole hidden range to its start —
    /// keeping the scroll math in step with the folded rendering; an unfolded line
    /// counts its soft-wrap rows plus its `virt_lines` above and below (`virt` is
    /// the caller's [`Buffer::virt_lines_by_line`] map, built once per walk).
    /// Caller ensures `top > 0`.
    fn rows_of_line_above(
        &self,
        top: usize,
        virt: &std::collections::BTreeMap<usize, crate::extmark::VirtLineRows>,
    ) -> (usize, usize) {
        let prev = top - 1;
        match self.collapsing_fold_at(prev) {
            Some(f) => (1, f.start),
            None => (
                self.line_text_rows(prev)
                    + virt.get(&prev).map_or(0, |r| r.above.len() + r.below.len()),
                prev,
            ),
        }
    }

    /// The largest `top` (≤ `target`) that keeps at least `above` screen rows above
    /// the cursor's row on line `target` — the top-margin (`'scrolloff'`) analogue of
    /// [`scroll_top_for_bottom`](Self::scroll_top_for_bottom). Walks up from `target`,
    /// counting each preceding line's `virt_lines` and soft-wrap rows (a closed fold
    /// as one row), until `above` rows have accumulated or the buffer top is reached.
    /// With `above == 0` it returns `target` (the cursor on the top text row, no
    /// margin) so a zero `scrolloff` leaves the historical behavior untouched.
    fn scroll_top_for_row_above(&self, target: usize, above: usize) -> usize {
        // The cursor sits on `target`, so the wrap segments preceding it already
        // count toward the margin.
        self.scroll_top_for_row_above_seg(target, above, self.cursor_wrap_seg())
    }

    /// [`scroll_top_for_row_above`](Self::scroll_top_for_row_above) measured from the
    /// `seg`-th display row of `target` rather than the cursor's own row — `seg = 0`
    /// asks for the margin above the *line's first* row, which is what a scroll limit
    /// (as opposed to a cursor-visibility bound) wants.
    fn scroll_top_for_row_above_seg(&self, target: usize, above: usize, seg: usize) -> usize {
        let virt = self.buffer().virt_lines_by_line();
        // Rows already above the row within its own line: the line's `virt_lines_above`
        // and the wrap segments preceding `seg`.
        let mut rows = virt.get(&target).map_or(0, |r| r.above.len()) + seg;
        let mut top = target;
        while rows < above && top > 0 {
            let (p, prev_top) = self.rows_of_line_above(top, &virt);
            rows += p;
            top = prev_top;
        }
        top
    }

    /// The deepest `top` an explicit viewport scroll (`<C-e>`, the mouse wheel) may
    /// reach in the focused window: far enough down that the **last** buffer line
    /// still keeps its `'scrolloff'` rows above it. Scrolling stops there rather than
    /// walking the last line up to the top row — past this point the cursor is pinned
    /// to the last line with no room left to carry it down, so
    /// [`ensure_visible`](Self::ensure_visible) would enforce the same margin and drag
    /// `top` straight back, turning every further notch into a bounce.
    ///
    /// Measured from the last line's first display row (`seg = 0`), so it is never
    /// deeper than the bound `ensure_visible` computes from the cursor's actual wrap
    /// segment. With `scrolloff` 0 it is the last line itself — vim's `<C-e>` walking
    /// the buffer's end to the top row, unchanged.
    pub(crate) fn max_scroll_top(&self, th: usize) -> usize {
        let so = scroll_margin(self.windows.cur().options.scrolloff, th);
        self.scroll_top_for_row_above_seg(self.last_line(), so, 0)
    }

    /// Keep the cursor's screen column within the focused window's text area by
    /// adjusting [`Editor::leftcol`] — the horizontal analog of [`ensure_visible`]
    /// (vim's `nowrap` `w_leftcol`). Honors `sidescroll` (the scroll step: `0`
    /// recenters the cursor, `> 0` scrolls just enough to bring it to the edge) and
    /// `sidescrolloff` (the margin kept between the cursor and the edge). A no-op
    /// for a degenerate (zero-width) text area.
    fn ensure_visible_horizontal(&mut self) {
        // Soft-wrap lays long lines across rows instead of panning, so there is no
        // horizontal scroll: keep `leftcol` pinned at the left edge.
        if self.windows.cur().options.wrap {
            self.leftcol = 0;
            return;
        }
        let tw = self.text_width();
        if tw == 0 {
            return;
        }
        let opts = &self.windows.cur().options;
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
