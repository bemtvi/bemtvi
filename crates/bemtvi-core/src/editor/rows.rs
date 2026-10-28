//! Screen-row geometry of one window: how many display rows each buffer line
//! fills (its soft-wrap segments, its `virt_lines`, a closed fold as one row),
//! and the scroll bounds that follow from it. The viewport math counts in these
//! rows, never in buffer lines, so a wrapped line scrolls as the several rows it
//! fills.
//!
//! Built for the focused window by [`Editor::row_geom`] and for any other by
//! [`Editor::window_row_geom`], so a window you are not in (the wheel scrolls one)
//! is measured exactly as the focused one is.

use super::fold::{Fold, FoldState};
use super::windows::Window;
use super::*;
use crate::extmark::VirtLineRows;
use crate::options::WindowOptions;
use crate::unicode;
use std::collections::BTreeMap;

pub(crate) struct RowGeom<'a> {
    buf: &'a Buffer,
    opts: &'a WindowOptions,
    folds: &'a FoldState,
    /// The text-area width in cells, the wrap width.
    width: usize,
    virt: BTreeMap<usize, VirtLineRows>,
}

impl<'a> RowGeom<'a> {
    fn new(buf: &'a Buffer, w: &'a Window, width: usize) -> Self {
        RowGeom {
            buf,
            opts: &w.options,
            folds: &w.folds,
            width,
            virt: buf.virt_lines_by_line(),
        }
    }

    /// The soft-wrap segment starts of `line`, or `None` when it cannot wrap
    /// (`nowrap`, a zero-width window, or past the buffer's end).
    fn segment_starts(&self, line: usize) -> Option<Vec<usize>> {
        if !self.opts.wrap || self.width == 0 || line >= self.buf.line_count() {
            return None;
        }
        let text = self.buf.line_cow(line);
        let tab = self.buf.options.effective_tabstop();
        let wo = self.opts.wrap_opts();
        let indent = unicode::cont_indent(&text, tab, self.width, wo);
        Some(
            unicode::window_wrap_segments(&text, tab, self.width, indent, wo)
                .iter()
                .map(|s| s.start_byte)
                .collect(),
        )
    }

    /// The text rows buffer `line` occupies: `1` under `nowrap` (or for a line
    /// that fits), else its soft-wrap segment count.
    pub(crate) fn line_rows(&self, line: usize) -> usize {
        self.segment_starts(line).map_or(1, |s| s.len())
    }

    /// Which soft-wrap segment byte `col` of `line` falls in (its display-row
    /// offset within the line): `0` under `nowrap` or on the first segment.
    pub(crate) fn wrap_seg(&self, line: usize, col: usize) -> usize {
        self.segment_starts(line)
            .and_then(|s| s.iter().rposition(|&start| col >= start))
            .unwrap_or(0)
    }

    /// The `virt_lines` rows drawn above `line`.
    pub(crate) fn virt_above(&self, line: usize) -> usize {
        self.virt.get(&line).map_or(0, |r| r.above.len())
    }

    /// Every screen row `line` occupies: its text rows plus its `virt_lines`.
    pub(crate) fn line_total_rows(&self, line: usize) -> usize {
        self.line_rows(line)
            + self
                .virt
                .get(&line)
                .map_or(0, |r| r.above.len() + r.below.len())
    }

    /// The closed fold that collapses `line` on screen, if folding is enabled.
    pub(crate) fn fold_at(&self, line: usize) -> Option<Fold> {
        if !self.opts.foldenable {
            return None;
        }
        self.folds.collapsing_at(line)
    }

    /// One upward step of a scroll-top walk: the screen rows the buffer line just
    /// above `top` occupies, and the line the walk continues from. A closed fold is
    /// ONE screen row and the walk steps over its whole hidden range to its start —
    /// keeping the scroll math in step with the folded rendering. Caller ensures
    /// `top > 0`.
    fn rows_of_line_above(&self, top: usize) -> (usize, usize) {
        let prev = top - 1;
        match self.fold_at(prev) {
            Some(f) => (1, f.start),
            None => (self.line_total_rows(prev), prev),
        }
    }

    /// The largest `top` (≤ `target`) such that display row `seg` of buffer line
    /// `target` and every line above it down to `top` fit within `th` screen rows,
    /// i.e. that row lands on the bottom text row. The target's `virt_lines_below`
    /// (and its segments below `seg`) may spill past the bottom.
    pub(crate) fn top_for_bottom(&self, target: usize, seg: usize, th: usize) -> usize {
        let mut rows = self.virt_above(target) + seg + 1;
        let mut top = target;
        while top > 0 {
            let (p, prev_top) = self.rows_of_line_above(top);
            if rows + p > th {
                break;
            }
            rows += p;
            top = prev_top;
        }
        top
    }

    /// The largest `top` (≤ `target`) that keeps at least `above` screen rows above
    /// display row `seg` of line `target` — the top-margin (`'scrolloff'`) analogue
    /// of [`top_for_bottom`](Self::top_for_bottom). With `above == 0` it returns
    /// `target`.
    pub(crate) fn top_for_row_above(&self, target: usize, seg: usize, above: usize) -> usize {
        // The rows already above `seg` within its own line count toward the margin.
        let mut rows = self.virt_above(target) + seg;
        let mut top = target;
        while rows < above && top > 0 {
            let (p, prev_top) = self.rows_of_line_above(top);
            rows += p;
            top = prev_top;
        }
        top
    }

    /// The band of `top` values that keeps display row `seg` of line `line` `so`
    /// screen rows in from either text edge of a `th`-row window, as
    /// `(top_min, top_max)`: `top_max` keeps `so` rows above it (else scroll up),
    /// `top_min` keeps `so` rows below it (else scroll down). With `so == 0` this is
    /// exactly "the row on screen".
    ///
    /// `top_min` is capped by `max_top`, the largest useful `top`: the bottom margin
    /// is only honored while real content sits below to justify it — never scroll
    /// past it to open blank rows under end-of-file (vim pins the last line to the
    /// bottom edge and lets the cursor sit inside the margin there).
    pub(crate) fn band(
        &self,
        line: usize,
        seg: usize,
        th: usize,
        so: usize,
        max_top: usize,
    ) -> (usize, usize) {
        let top_max = self.top_for_row_above(line, seg, so);
        let top_min = self.top_for_bottom(line, seg, th - so).min(max_top);
        (top_min, top_max)
    }

    /// Where a viewport-only scroll (`<C-e>`/`<C-y>`, the wheel) that left the
    /// view at `top` must park a cursor on `line`, or `None` when it is still
    /// outside the `so`-row margin and stays put. `col_on(line)` is the column the
    /// cursor would take on a line (its remembered desired column), since the wrap
    /// segment it lands on moves the band. `max_top` is the largest useful `top`
    /// (see [`band`](Self::band)).
    ///
    /// The band is measured in screen rows, so under `wrap` (or `virt_lines`) a
    /// count of buffer lines from the edge can land far off-screen. The walk starts
    /// at the edge the cursor drifted past and steps inward a line at a time (a
    /// closed fold as one) until `top` sits in the cursor's band; a line taller
    /// than the window stops it at the far edge.
    pub(crate) fn park(
        &self,
        top: usize,
        th: usize,
        so: usize,
        max_top: usize,
        line: usize,
        col_on: impl Fn(usize) -> usize,
    ) -> Option<usize> {
        let fits = |l: usize| {
            let (lo, hi) = self.band(l, self.wrap_seg(l, col_on(l)), th, so, max_top);
            (lo..=hi).contains(&top)
        };
        if fits(line) {
            return None;
        }
        let last = self.buf.line_count().saturating_sub(1);
        let (_, top_max) = self.band(line, self.wrap_seg(line, col_on(line)), th, so, max_top);
        let down = top > top_max;
        let mut l = if down {
            top
        } else {
            line.min(top + th).min(last)
        };
        while !fits(l) {
            let next = if down {
                self.fold_at(l).map_or(l, |f| f.end) + 1
            } else {
                let p = l.saturating_sub(1);
                self.fold_at(p).map_or(p, |f| f.start)
            };
            if (down && next > last) || (!down && next < top) || next == l {
                break;
            }
            l = next;
        }
        Some(l)
    }
}

impl Editor {
    /// The [`RowGeom`] of the focused window.
    pub(crate) fn row_geom(&self) -> RowGeom<'_> {
        RowGeom::new(self.buffer(), self.windows.cur(), self.text_width())
    }

    /// The [`RowGeom`] of window `win` in whichever tree holds it, or `None` for
    /// an unknown window. Measured from the window's own options, folds, buffer and
    /// width, so it agrees with [`row_geom`](Self::row_geom) for the focused one.
    pub(crate) fn window_row_geom(&self, win: WindowId) -> Option<RowGeom<'_>> {
        let (_, tree) = self.tree_of_window(win)?;
        let w = tree.get(win);
        let buf = &self.buffers.get(w.buffer).buffer;
        Some(RowGeom::new(buf, w, self.window_text_width(w, buf)))
    }
}
