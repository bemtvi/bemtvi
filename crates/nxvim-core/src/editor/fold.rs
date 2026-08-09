//! Per-window code folds.
//!
//! Phase 1 implements **manual** folds: ranges created with `zf` / `:fold` and
//! opened/closed/deleted with the `z` family (`zo`/`zc`/`za`/`zR`/`zM`/`zd`/…).
//! The fold *structure* is a flat set of inclusive buffer-line ranges nested by
//! containment, and each window owns its own [`FoldState`] — vim's per-window
//! fold model, so the same buffer folds independently in two windows.
//!
//! This is the spine the later sources reuse: tree-sitter / indent / LSP folds
//! (which compute the same ranges from buffer content), fold-aware motion and
//! scrolling, and the fold-column gutter all build on the model and the `z`
//! commands defined here. Rendering reads [`FoldState::collapsed_regions`] to
//! collapse a closed fold into a single placeholder row (see `crate::view`).

use super::*;

/// One fold: an inclusive 0-based buffer-line range `[start, end]` (always
/// spanning at least two lines) and whether it is currently closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fold {
    /// First line of the fold (0-based). This is the line shown when the fold is
    /// closed (carrying the fold's placeholder text).
    pub(crate) start: usize,
    /// Last line of the fold (0-based, inclusive). `end > start` always.
    pub(crate) end: usize,
    /// Whether the fold is currently collapsed.
    pub(crate) closed: bool,
}

impl Fold {
    /// Whether buffer line `line` lies within this fold's range.
    pub(crate) fn contains(&self, line: usize) -> bool {
        line >= self.start && line <= self.end
    }

    /// Number of buffer lines this fold spans (`end - start + 1`).
    pub(crate) fn line_count(&self) -> usize {
        self.end - self.start + 1
    }
}

/// The resolved fold *source* — `'foldmethod'` with `expr` split by whether its
/// `'foldexpr'` is the canonical tree-sitter one nxvim computes natively. This is
/// what the recompute dispatches on (and keys its cache by), so a `foldexpr`
/// change between the tree-sitter and a generic expr re-folds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FoldSource {
    /// Hand-built folds; never recomputed.
    Manual,
    /// `foldmethod=indent`.
    Indent,
    /// `foldmethod=marker` — folds bounded by the `'foldmarker'` strings.
    Marker,
    /// `foldmethod=expr` with the native tree-sitter `foldexpr`.
    Treesitter,
    /// `foldmethod=expr` with a generic Lua `foldexpr`. nxvim-core can't run Lua,
    /// so the server evaluates the expression per line (with `v:lnum` bound) and
    /// pushes the per-line values via [`Editor::set_foldexpr_values`]; the structure
    /// is built from them by [`Editor::compute_generic_expr_folds`].
    GenericExpr,
    /// `foldmethod=expr` with the LSP `foldexpr` marker (`nx.lsp.foldexpr`). The
    /// server requests `textDocument/foldingRange` and pushes the line ranges via
    /// [`Editor::set_lsp_folds`]; the structure is built from them by
    /// [`Editor::compute_lsp_folds`] (containment depth → levels, like tree-sitter).
    Lsp,
}

/// Server-pushed fold data for an externally-computed source (see
/// [`Editor::external_folds`]). Tagged with the `changedtick` it was computed for
/// so a stale push (data for a since-edited buffer) is ignored until the server
/// re-pushes for the current tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExternalFolds {
    /// The buffer's `changedtick` this data was computed against.
    pub(crate) changedtick: u64,
    /// The raw per-source payload.
    pub(crate) data: ExternalFoldData,
}

/// The raw payload of [`ExternalFolds`], one variant per externally-computed
/// source. Core turns it into `(start, end, level)` ranges itself (applying
/// `'foldnestmax'`/`'foldminlines'`) so all the fold semantics stay in one place.
///
/// The generic `'foldexpr'` used to live here too, as one value string per line
/// replaced wholesale on every edit. It now rides the per-line fold input cache
/// instead, which splices the rows an edit touched and leaves only those needing
/// re-evaluation — the difference between one Lua call per buffer line per keystroke
/// and one per *changed* line. LSP folds stay here: `foldingRange` answers for the
/// whole document at once, so there is no per-line input to splice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExternalFoldData {
    /// LSP `foldingRange` line spans, each an inclusive 0-based `[start, end]`.
    Lsp(Vec<(usize, usize)>),
}

/// The inputs that determine a *computed* fold structure. Cached on
/// [`FoldState`] so an unchanged buffer/options pair doesn't recompute its folds
/// every tick (vim's per-`(buf, changedtick, method)` caching). `'foldlevel'` is
/// deliberately absent: it changes only which folds display *closed*, applied
/// separately by [`FoldState::apply_foldlevel`] without rebuilding the structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FoldKey {
    /// The buffer's `changedtick` the structure was computed at.
    changedtick: u64,
    /// The fold source the structure was computed for.
    source: FoldSource,
    /// The effective `'shiftwidth'` (the indent fold's level divisor).
    shiftwidth: usize,
    /// The effective `'tabstop'` — the other half of an indent level, since it
    /// decides what a leading tab is *worth* in display columns. Without it here,
    /// `:set tabstop=` re-folded nothing on a tab-indented buffer: the structure
    /// cache matched (no text had changed) and the recompute was skipped.
    tabstop: usize,
    /// `'foldnestmax'` (the depth cap).
    foldnestmax: usize,
    /// `'foldminlines'` (the minimum span a fold must have to exist).
    foldminlines: usize,
}

/// A window's folds. Stored sorted so an *outer* fold always precedes the inner
/// folds nested inside it — ascending `start`, and for equal starts the wider
/// (larger `end`) first. Nesting is defined purely by containment (no explicit
/// parent pointers); the set is small, so each query recomputes it.
///
/// For `manual` folds the set is built by hand (`zf`/`:fold`). For a *computed*
/// method (`indent`/…) it is regenerated from buffer content by
/// [`Editor::refresh_folds`], guarded by [`FoldState::cache`] so an unchanged
/// buffer skips the work; manual `zo`/`zc` overrides are preserved across a
/// recompute by matching ranges.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FoldState {
    folds: Vec<Fold>,
    /// The inputs the current computed structure was built from, or `None` for a
    /// manual / never-computed set. Compared in [`Editor::refresh_folds`] to skip
    /// recomputing an unchanged buffer.
    cache: Option<FoldKey>,
}

impl FoldState {
    /// Re-establish the outer-before-inner order after a mutation.
    fn sort(&mut self) {
        self.folds
            .sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    }

    /// Whether this window has no folds.
    pub(crate) fn is_empty(&self) -> bool {
        self.folds.is_empty()
    }

    /// The folds as `(start, end, closed)` tuples (outer-before-inner) for shada
    /// persistence — see [`crate::editor::persist::FileFolds`].
    pub(crate) fn exported(&self) -> Vec<(usize, usize, bool)> {
        self.folds
            .iter()
            .map(|f| (f.start, f.end, f.closed))
            .collect()
    }

    /// Replace the fold set with persisted `(start, end, closed)` ranges (a shada
    /// restore). Skips degenerate spans (`end <= start`); leaves the `cache` clear
    /// (these are manual folds, never recomputed).
    pub(crate) fn restore(&mut self, folds: &[(usize, usize, bool)]) {
        self.folds = folds
            .iter()
            .filter(|&&(start, end, _)| end > start)
            .map(|&(start, end, closed)| Fold { start, end, closed })
            .collect();
        self.sort();
    }

    /// Create a manual fold over `[start, end]`, created **closed** (as `zf`
    /// does). A span under two lines is rejected; an exact duplicate of an
    /// existing fold is re-closed rather than added twice. Returns whether a fold
    /// now exists for the range.
    fn create(&mut self, start: usize, end: usize) -> bool {
        if end <= start {
            return false;
        }
        if let Some(f) = self
            .folds
            .iter_mut()
            .find(|f| f.start == start && f.end == end)
        {
            f.closed = true;
            return true;
        }
        self.folds.push(Fold {
            start,
            end,
            closed: true,
        });
        self.sort();
        true
    }

    /// Index of the innermost fold containing `line` (the smallest span), or
    /// `None` when no fold covers it.
    fn innermost_at(&self, line: usize) -> Option<usize> {
        self.folds
            .iter()
            .enumerate()
            .filter(|(_, f)| f.contains(line))
            .min_by_key(|(_, f)| f.line_count())
            .map(|(i, _)| i)
    }

    /// The fold that actually collapses `line` on screen: the **outermost closed**
    /// fold among those containing `line`. Walking containing folds from outermost
    /// inward, the first closed one hides everything below it, so a larger closed
    /// fold wins over a nested one. `None` when `line` is fully visible.
    pub(crate) fn collapsing_at(&self, line: usize) -> Option<Fold> {
        self.folds
            .iter()
            .filter(|f| f.closed && f.contains(line))
            // `folds` is sorted outer-first; the widest closed one is the collapser.
            .max_by_key(|f| f.line_count())
            .copied()
    }

    /// The closed folds that actually collapse text — outermost-only,
    /// non-overlapping, sorted by `start`, and clamped to `line_count`. A closed
    /// fold nested inside another closed fold is omitted (its lines are already
    /// hidden). This is what the renderer walks to drop hidden lines and emit one
    /// placeholder row per region.
    pub(crate) fn collapsed_regions(&self, line_count: usize) -> Vec<Fold> {
        let mut out: Vec<Fold> = Vec::new();
        let mut covered_end: Option<usize> = None;
        for f in self.folds.iter().filter(|f| f.closed) {
            if f.start >= line_count {
                continue;
            }
            // Already inside the last emitted region → nested, skip.
            if covered_end.is_some_and(|ce| f.start <= ce) {
                continue;
            }
            let end = f.end.min(line_count.saturating_sub(1));
            if end <= f.start {
                continue;
            }
            covered_end = Some(end);
            out.push(Fold {
                start: f.start,
                end,
                closed: true,
            });
        }
        out
    }

    /// Open one level at `line`: reveal the outermost closed fold covering it.
    /// Inner folds keep their own state (revealed but possibly still closed).
    /// Returns whether anything opened.
    fn open_one(&mut self, line: usize) -> bool {
        let Some(target) = self.collapsing_at(line) else {
            return false;
        };
        for f in &mut self.folds {
            if f.start == target.start && f.end == target.end {
                f.closed = false;
            }
        }
        true
    }

    /// Close one level at `line`: collapse the innermost *open* fold covering it.
    /// Returns the closed fold's start line (where vim parks the cursor), or
    /// `None` when no open fold covers `line`.
    fn close_one(&mut self, line: usize) -> Option<usize> {
        let idx = self
            .folds
            .iter()
            .enumerate()
            .filter(|(_, f)| !f.closed && f.contains(line))
            .min_by_key(|(_, f)| f.line_count())
            .map(|(i, _)| i)?;
        self.folds[idx].closed = true;
        Some(self.folds[idx].start)
    }

    /// Recursively open every fold covering `line` (`zO`).
    fn open_recursive(&mut self, line: usize) {
        for f in &mut self.folds {
            if f.contains(line) {
                f.closed = false;
            }
        }
    }

    /// Recursively close every fold covering `line` (`zC`); returns the outermost
    /// such fold's start line for cursor parking, or `None` when none cover it.
    fn close_recursive(&mut self, line: usize) -> Option<usize> {
        let mut start = None;
        for f in &mut self.folds {
            if f.contains(line) {
                f.closed = true;
                start = Some(start.map_or(f.start, |s: usize| s.min(f.start)));
            }
        }
        start
    }

    /// Open every fold in the window (`zR`).
    fn open_all(&mut self) {
        for f in &mut self.folds {
            f.closed = false;
        }
    }

    /// Close every fold in the window (`zM`).
    fn close_all(&mut self) {
        for f in &mut self.folds {
            f.closed = true;
        }
    }

    /// Delete the innermost fold at `line` (`zd`); returns whether one was removed.
    fn delete_at(&mut self, line: usize) -> bool {
        match self.innermost_at(line) {
            Some(idx) => {
                self.folds.remove(idx);
                true
            }
            None => false,
        }
    }

    /// Delete every fold in the window (`zE`).
    fn delete_all(&mut self) {
        self.folds.clear();
    }

    /// The `'foldcolumn'` marker string (width `width`) for buffer `line`: `-` on
    /// an open fold's first line, `│` within an open fold, and `+` for the closed
    /// fold that collapses the line (only while `foldenable`). Outer folds take the
    /// leftmost cells; a closed fold's `+` ends the column (its inner folds are
    /// hidden). Blank when no fold covers the line. This is the per-row string the
    /// client paints in its fold gutter.
    pub(crate) fn column_marker(&self, line: usize, width: usize, foldenable: bool) -> String {
        if width == 0 {
            return String::new();
        }
        let mut cells = vec![' '; width];
        // `folds` is sorted outer-first, so the filter preserves nesting order. A
        // closed fold breaks immediately, so the enumerate index tracks the column
        // exactly (it only advances on the open arm, which continues).
        for (i, f) in self.folds.iter().filter(|f| f.contains(line)).enumerate() {
            if i >= width {
                break;
            }
            if foldenable && f.closed {
                cells[i] = '+';
                break;
            }
            cells[i] = if f.start == line { '-' } else { '│' };
        }
        cells.into_iter().collect()
    }

    /// Start line of the nearest fold beginning strictly below `line` (`zj`).
    fn next_fold_start(&self, line: usize) -> Option<usize> {
        self.folds
            .iter()
            .map(|f| f.start)
            .filter(|&s| s > line)
            .min()
    }

    /// End line of the nearest fold ending strictly above `line` (`zk`).
    fn prev_fold_end(&self, line: usize) -> Option<usize> {
        self.folds.iter().map(|f| f.end).filter(|&e| e < line).max()
    }

    /// The nesting level of the fold at `idx` — `1` plus the number of folds that
    /// strictly contain it (vim's 1-based fold level). For a computed source this
    /// equals the line's indent depth, since nesting follows containment.
    fn level_at(&self, idx: usize) -> usize {
        let f = self.folds[idx];
        1 + self
            .folds
            .iter()
            .filter(|o| {
                o.start <= f.start && o.end >= f.end && (o.start, o.end) != (f.start, f.end)
            })
            .count()
    }

    /// Replace the fold set with a freshly-computed structure (the `(start, end,
    /// level)` ranges a computed source produced), recording `key` so an unchanged
    /// buffer skips the next recompute. A fold's closed state defaults to "closed
    /// when its level is deeper than `'foldlevel'`", but a manual `zo`/`zc` override
    /// on an identical range that survived the recompute is carried over (vim keeps
    /// a hand-toggled computed fold's state across an edit).
    fn rebuild_computed(
        &mut self,
        ranges: Vec<(usize, usize, usize)>,
        foldlevel: usize,
        key: FoldKey,
    ) {
        let old = std::mem::take(&mut self.folds);
        self.folds = ranges
            .into_iter()
            .map(|(start, end, level)| {
                let closed = old
                    .iter()
                    .find(|o| o.start == start && o.end == end)
                    .map_or(level > foldlevel, |o| o.closed);
                Fold { start, end, closed }
            })
            .collect();
        self.sort();
        self.cache = Some(key);
    }

    /// Re-derive every fold's closed state from `'foldlevel'`: a fold displays
    /// closed exactly when its [level](FoldState::level_at) is deeper than
    /// `foldlevel`. Changing `foldlevel` resets all open/close state (vim's
    /// behavior — it overrides any manual `zo`/`zc`), so this ignores prior state.
    fn apply_foldlevel(&mut self, foldlevel: usize) {
        let levels: Vec<usize> = (0..self.folds.len()).map(|i| self.level_at(i)).collect();
        for (f, level) in self.folds.iter_mut().zip(levels) {
            f.closed = level > foldlevel;
        }
    }
}

impl Editor {
    /// The focused window's fold state (immutable).
    fn folds(&self) -> &FoldState {
        &self.windows.cur().folds
    }

    /// The focused window's fold state (mutable).
    fn folds_mut(&mut self) -> &mut FoldState {
        &mut self.windows.cur_mut().folds
    }

    /// Whether folding is enabled for the focused window (`'foldenable'`). A
    /// closed fold only collapses on screen while this is on; `zn`/`zi` toggle it.
    fn foldenable(&self) -> bool {
        self.windows.cur().options.foldenable
    }

    /// Turn `'foldenable'` on for the focused window. The fold-creating and
    /// fold-closing commands (`zf`/`zF`/`zc`/`zC`/`za`-close/`zM`) all do this in
    /// vim, so that operating on folds while it was off (e.g. after `zn`) brings
    /// folding back rather than silently doing nothing.
    fn enable_folding(&mut self) {
        self.windows.cur_mut().options.foldenable = true;
    }

    /// The fold that collapses the focused window's cursor line on screen, if any.
    /// Used by the `z` commands and fold-aware motion.
    pub(crate) fn cursor_collapsing_fold(&self) -> Option<Fold> {
        self.collapsing_fold_at(self.cursor.line)
    }

    /// The closed fold collapsing `line` on screen in the focused window (honoring
    /// `'foldenable'`), or `None` when the line is fully visible.
    pub(crate) fn collapsing_fold_at(&self, line: usize) -> Option<Fold> {
        if !self.foldenable() {
            return None;
        }
        self.folds().collapsing_at(line)
    }

    /// The first display line of `line`: the start of the closed fold collapsing
    /// it, or `line` itself when visible. Snapping the cursor through this keeps it
    /// off a fold's hidden interior — it always lands on the fold's header line.
    pub(crate) fn fold_line_start(&self, line: usize) -> usize {
        self.collapsing_fold_at(line).map_or(line, |f| f.start)
    }

    /// The last line of the closed fold collapsing `line`, or `line` itself.
    pub(crate) fn fold_line_end(&self, line: usize) -> usize {
        self.collapsing_fold_at(line).map_or(line, |f| f.end)
    }

    /// The visible line `count` lines below `line`, counting each closed fold as a
    /// single line (vim's fold-aware `j`). Clamped at the last line.
    pub(crate) fn line_below_folds(&self, line: usize, count: usize) -> usize {
        let last = self.last_line();
        let mut l = self.fold_line_start(line);
        for _ in 0..count {
            let end = self.fold_line_end(l);
            if end >= last {
                break;
            }
            l = self.fold_line_start(end + 1);
        }
        l
    }

    /// The visible line `count` lines above `line`, counting each closed fold as a
    /// single line (vim's fold-aware `k`). Clamped at the first line.
    pub(crate) fn line_above_folds(&self, line: usize, count: usize) -> usize {
        let mut l = self.fold_line_start(line);
        for _ in 0..count {
            if l == 0 {
                break;
            }
            l = self.fold_line_start(l - 1);
        }
        l
    }

    /// Create a manual fold over the inclusive line range `[first, last]` and park
    /// the cursor on its first line (vim's `zf` / `:fold`). A degenerate range
    /// (under two lines) is a silent no-op, matching vim.
    pub(crate) fn create_fold(&mut self, first: usize, last: usize) {
        let (lo, hi) = (first.min(last), first.max(last));
        if self.folds_mut().create(lo, hi) {
            // `zf`/`zF` set 'foldenable' so a fold created while it was off shows.
            self.enable_folding();
            self.cursor.line = lo.min(self.last_line());
            self.cursor.col = self.first_non_blank(self.cursor.line);
            self.clamp_cursor();
        }
    }

    /// Park the cursor on `line`'s first non-blank — where the `z` open/close
    /// commands leave it after changing fold state.
    fn settle_on_fold_line(&mut self, line: usize) {
        self.cursor.line = line.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
    }

    /// `zo` — open one fold level under the cursor.
    pub(crate) fn fold_open(&mut self) {
        let line = self.cursor.line;
        self.folds_mut().open_one(line);
    }

    /// `zc` — close one fold level under the cursor, parking on the fold's start.
    pub(crate) fn fold_close(&mut self) {
        self.enable_folding();
        let line = self.cursor.line;
        if let Some(start) = self.folds_mut().close_one(line) {
            self.settle_on_fold_line(start);
        }
    }

    /// `za` — toggle the fold under the cursor (open if collapsed, else close).
    pub(crate) fn fold_toggle(&mut self) {
        if self.cursor_collapsing_fold().is_some() {
            self.fold_open();
        } else {
            self.fold_close();
        }
    }

    /// `zO` — open every fold under the cursor, recursively.
    pub(crate) fn fold_open_recursive(&mut self) {
        let line = self.cursor.line;
        self.folds_mut().open_recursive(line);
    }

    /// `zC` — close every fold under the cursor, recursively, parking on the
    /// outermost fold's start.
    pub(crate) fn fold_close_recursive(&mut self) {
        self.enable_folding();
        let line = self.cursor.line;
        if let Some(start) = self.folds_mut().close_recursive(line) {
            self.settle_on_fold_line(start);
        }
    }

    /// `zR` — open all folds in the window.
    pub(crate) fn fold_open_all(&mut self) {
        self.folds_mut().open_all();
    }

    /// `zM` — close all folds in the window, parking on the cursor's enclosing
    /// fold start so the cursor stays on a visible line.
    pub(crate) fn fold_close_all(&mut self) {
        self.enable_folding();
        self.folds_mut().close_all();
        if let Some(f) = self.cursor_collapsing_fold() {
            self.settle_on_fold_line(f.start);
        }
    }

    /// `zd` — delete the innermost fold under the cursor.
    pub(crate) fn fold_delete(&mut self) {
        let line = self.cursor.line;
        self.folds_mut().delete_at(line);
    }

    /// `zE` — delete every fold in the window.
    pub(crate) fn fold_delete_all(&mut self) {
        self.folds_mut().delete_all();
    }

    /// `zn` / `zN` / `zi` — set or toggle `'foldenable'` for the focused window.
    pub(crate) fn set_foldenable(&mut self, on: Option<bool>) {
        let opt = &mut self.windows.cur_mut().options.foldenable;
        *opt = on.unwrap_or(!*opt);
    }

    /// `zj` — move to the start of the next fold below the cursor (a no-op when
    /// there is none). Lands on the line's first non-blank, like other line jumps.
    fn fold_next(&mut self) {
        let line = self.cursor.line;
        if let Some(start) = self.folds().next_fold_start(line) {
            self.settle_on_fold_line(self.fold_line_start(start));
        }
    }

    /// `zk` — move to the end of the previous fold above the cursor (a no-op when
    /// there is none).
    fn fold_prev(&mut self) {
        let line = self.cursor.line;
        if let Some(end) = self.folds().prev_fold_end(line) {
            self.settle_on_fold_line(self.fold_line_start(end));
        }
    }

    /// `zF` / `{count}zF` — create a fold over `count` lines from the cursor
    /// (default 1, which is a degenerate single-line span and so a no-op, as in
    /// vim). The fold spans `[cursor, cursor + count - 1]`.
    fn create_fold_lines(&mut self, count: usize) {
        let first = self.cursor.line;
        let last = (first + count.saturating_sub(1)).min(self.last_line());
        self.create_fold(first, last);
    }

    /// Dispatch a resolved [`FoldCmd`] (the `z`-family fold commands) to its
    /// `fold_*` method. `count` is the command's resolved count, used only by
    /// `zF`.
    pub(crate) fn execute_fold(&mut self, cmd: FoldCmd, count: usize) {
        match cmd {
            FoldCmd::Open => self.fold_open(),
            FoldCmd::Close => self.fold_close(),
            FoldCmd::Toggle => self.fold_toggle(),
            FoldCmd::OpenRecursive => self.fold_open_recursive(),
            FoldCmd::CloseRecursive => self.fold_close_recursive(),
            FoldCmd::OpenAll => self.fold_open_all(),
            FoldCmd::CloseAll => self.fold_close_all(),
            FoldCmd::Delete => self.fold_delete(),
            FoldCmd::DeleteAll => self.fold_delete_all(),
            FoldCmd::CreateLines => self.create_fold_lines(count),
            FoldCmd::Enable(on) => self.set_foldenable(on),
            FoldCmd::Next => self.fold_next(),
            FoldCmd::Prev => self.fold_prev(),
        }
    }

    /// Rebuild the focused window's *computed* folds when their inputs changed.
    ///
    /// For `'foldmethod=manual'` this is a no-op (manual folds are hand-built and
    /// never recomputed). For a computed source (`indent` / the tree-sitter
    /// `foldexpr`) it compares the current `(changedtick, source, indent-options)`
    /// against the cached [`FoldKey`] and recomputes only on a mismatch — so an
    /// unchanged buffer pays nothing. After a rebuild the cursor is snapped out of
    /// any line a closed fold now hides, onto the fold's header (vim parks the
    /// cursor on a visible line).
    ///
    /// Driven from the input loop (after every keystroke that may have edited the
    /// buffer) and from the option setters that change a fold input
    /// (`foldmethod`/`foldexpr`/`shiftwidth`/`tabstop`/`foldnestmax`/`foldminlines`).
    /// Operates on the focused window only; a non-focused window onto the same
    /// buffer recomputes when it next gains focus.
    pub(crate) fn refresh_folds(&mut self) {
        let source = self.fold_source();
        if source == FoldSource::Manual {
            return;
        }
        let key = self.fold_key(source);
        if self.windows.cur().folds.cache == Some(key) {
            return;
        }
        let foldlevel = self.windows.cur().options.foldlevel;
        let ranges = match source {
            FoldSource::Indent => self.compute_indent_folds(),
            FoldSource::Marker => self.compute_marker_folds(),
            FoldSource::Treesitter => match self.compute_treesitter_folds() {
                Some(r) => r,
                // The grammar / parse isn't ready yet — leave the folds untouched and
                // don't cache, so the next tick retries once it loads.
                None => return,
            },
            // The generic-`foldexpr` and LSP structures are computed *outside* core
            // (the server can't be reached from here) and pushed into `external_folds`.
            // Build from that data only when it matches the current `changedtick`;
            // otherwise leave the folds and don't cache, so we retry once the server
            // pushes a fresh result (a `set_*` push busts the cache to force the rebuild).
            FoldSource::GenericExpr => match self.compute_generic_expr_folds() {
                Some(r) => r,
                None => return,
            },
            FoldSource::Lsp => match self.compute_lsp_folds() {
                Some(r) => r,
                None => return,
            },
            FoldSource::Manual => return,
        };
        self.windows
            .cur_mut()
            .folds
            .rebuild_computed(ranges, foldlevel, key);
        self.snap_cursor_to_fold_header();
    }

    /// Snap the focused cursor out of any line a closed fold now hides, onto that
    /// fold's header — vim keeps the cursor on a visible line after folds change.
    fn snap_cursor_to_fold_header(&mut self) {
        let line = self.cursor.line;
        let start = self.fold_line_start(line);
        if start != line {
            self.cursor.line = start;
            self.cursor.col = self.first_non_blank(start);
            self.clamp_cursor();
        }
    }

    /// The focused buffer's resolved [`FoldSource`] — `'foldmethod'`, with `expr`
    /// split by whether `'foldexpr'` is the canonical tree-sitter one.
    fn fold_source(&self) -> FoldSource {
        match self.buffer().options.foldmethod {
            crate::options::FoldMethod::Manual => FoldSource::Manual,
            crate::options::FoldMethod::Indent => FoldSource::Indent,
            crate::options::FoldMethod::Marker => FoldSource::Marker,
            crate::options::FoldMethod::Expr => {
                let expr = self.foldexpr();
                if is_treesitter_foldexpr(expr) {
                    FoldSource::Treesitter
                } else if is_lsp_foldexpr(expr) {
                    FoldSource::Lsp
                } else {
                    FoldSource::GenericExpr
                }
            }
        }
    }

    /// Set the focused window's `'foldlevel'` and re-fold accordingly. For a
    /// computed source this re-derives which folds display closed (deeper than the
    /// new level); for `manual` it only stores the value (manual folds keep their
    /// own `zo`/`zc` state). Shared by `:set foldlevel=N` and the `vim.wo` bridge.
    pub(crate) fn set_foldlevel(&mut self, level: usize) {
        self.windows.cur_mut().options.foldlevel = level;
        if self.fold_source() == FoldSource::Manual {
            return;
        }
        // Make sure the structure is current before re-deriving closed state.
        self.refresh_folds();
        self.windows.cur_mut().folds.apply_foldlevel(level);
        self.snap_cursor_to_fold_header();
    }

    /// The cache key for the focused buffer's computed folds (see [`FoldKey`]).
    fn fold_key(&self, source: FoldSource) -> FoldKey {
        let bo = &self.buffer().options;
        FoldKey {
            changedtick: self.buffer().changedtick,
            source,
            shiftwidth: bo.effective_shiftwidth(),
            tabstop: bo.effective_tabstop(),
            foldnestmax: bo.foldnestmax,
            foldminlines: bo.foldminlines,
        }
    }

    /// Compute tree-sitter folds for the focused buffer: the engine's `@fold` node
    /// ranges (`folds.scm`) turned into per-line levels by containment depth, then
    /// into nested ranges via [`ranges_from_levels`]. `None` when tree-sitter folds
    /// aren't *available* (no grammar / no `folds.scm` / parse not ready) so the
    /// caller leaves existing folds alone and retries; `Some(vec![])` when the query
    /// loaded but found nothing foldable.
    fn compute_treesitter_folds(&mut self) -> Option<Vec<(usize, usize, usize)>> {
        let buf = self.current_buffer_id();
        // Sync the parse and query the `@fold` ranges in one call.
        let ranges = self.ts_folds(buf);
        // Distinguish "no grammar / parse not ready" from "loaded, found nothing".
        if !self.ts_folds_available(buf) {
            return None;
        }
        let bo = &self.buffer().options;
        // Containment depth → per-line levels → nested ranges, shared with the LSP
        // source (both deliver opaque line spans rather than per-line levels).
        let spans: Vec<(usize, usize)> = ranges.iter().map(|r| (r.start, r.end)).collect();
        Some(ranges_from_containment(
            &spans,
            self.buffer().line_count(),
            bo.foldnestmax,
            bo.foldminlines,
        ))
    }

    /// The focused buffer's generic `'foldexpr'` that needs (re)evaluation, or
    /// `None`. `Some((buf, changedtick, expr, line_count))` when `foldmethod=expr`
    /// resolves to a generic Lua foldexpr (not the native tree-sitter / LSP markers)
    /// and no fresh result is stored for the buffer's current `changedtick`. The
    /// server drives [`LuaRuntime::eval_foldexpr_lines`](nxvim_lua) from this and
    /// pushes the values back via [`Editor::set_foldexpr_values`]; once a current
    /// result is stored this returns `None`, so the foldexpr isn't re-evaluated
    /// every frame (only after an edit or a foldexpr change).
    pub fn pending_foldexpr(&self) -> Option<(BufferId, u64, String, usize, usize)> {
        if self.fold_source() != FoldSource::GenericExpr {
            return None;
        }
        let buf = self.current_buffer_id();
        let tick = self.buffer().changedtick;
        let expr = self.foldexpr().to_string();
        let line_count = self.buffer().line_count();
        // The cache is spliced by `refresh_folds`, which runs on the input hook
        // *before* the redraw this is driven from, so the unevaluated rows here are
        // exactly the ones the last edit touched. An absent (or differently-derived)
        // cache means the whole buffer needs evaluating.
        let Some(FoldInputs {
            tick: cached_tick,
            params: FoldInputParams::Expr { expr: cached_expr },
            lines: FoldLines::Expr(values),
        }) = self.fold_inputs.get(&buf)
        else {
            return Some((buf, tick, expr, 0, line_count));
        };
        if *cached_tick != tick || *cached_expr != expr || values.len() != line_count {
            return Some((buf, tick, expr, 0, line_count));
        }
        let first = values.iter().position(Option::is_none)?;
        // The contiguous span covering every hole. An edit produces one run, so this
        // is normally exactly the inserted rows; a batch that produced several runs
        // re-evaluates the rows between them too, which is correct (if not minimal)
        // and keeps the server's side a single range.
        let last = values.iter().rposition(Option::is_none)?;
        Some((buf, tick, expr, first, last - first + 1))
    }

    /// Whether buffer `buf` resolves its folds from LSP `foldingRange` —
    /// `foldmethod=expr` with the LSP foldexpr marker (`nx.lsp.foldexpr`). The
    /// server gates its `textDocument/foldingRange` requests on this (a buffer that
    /// doesn't want LSP folds is never queried). Buffer-parameterized (not just the
    /// focused buffer) so the server can ask about any attached buffer.
    pub fn buffer_wants_lsp_folds(&self, buf: BufferId) -> bool {
        let is_expr = self
            .buffer_of(buf)
            .is_some_and(|b| b.options.foldmethod == crate::options::FoldMethod::Expr);
        let expr = self.foldexprs.get(&buf).map(String::as_str).unwrap_or("");
        is_expr && is_lsp_foldexpr(expr)
    }

    /// Snapshot each window's **manual** folds for shada, keyed by the path of the
    /// buffer it shows (only `foldmethod=manual` folds persist — computed sources
    /// regenerate on open). Files restored but not reopened this session are carried
    /// forward from [`Editor::pending_folds`]. First window per path wins.
    pub(crate) fn export_folds(&self) -> Vec<crate::editor::persist::FileFolds> {
        use crate::editor::persist::FileFolds;
        let mut out: Vec<FileFolds> = Vec::new();
        let mut seen: std::collections::HashSet<std::path::PathBuf> =
            std::collections::HashSet::new();
        for win in self.windows.all_windows() {
            let Some(ob) = self.buffers.map.get(&win.buffer) else {
                continue;
            };
            if ob.buffer.options.foldmethod != crate::options::FoldMethod::Manual {
                continue;
            }
            let Some(path) = ob
                .buffer
                .path
                .as_ref()
                .filter(|p| !p.as_os_str().is_empty())
            else {
                continue;
            };
            let folds = win.folds.exported();
            if folds.is_empty() || !seen.insert(path.clone()) {
                continue;
            }
            out.push(FileFolds {
                path: path.clone(),
                folds,
            });
        }
        for (path, folds) in &self.pending_folds {
            if seen.insert(path.clone()) {
                out.push(FileFolds {
                    path: path.clone(),
                    folds: folds.clone(),
                });
            }
        }
        out
    }

    /// Restore the focused window's manual folds from any shada-restored set for
    /// the buffer it now shows, draining that set (one-shot per path). Only applies
    /// when the window has no folds of its own yet (a session that already created
    /// folds wins) and the buffer is `foldmethod=manual`. Called wherever a buffer
    /// becomes the focused window's — the fold analogue of
    /// [`Editor::seed_pending_file_marks`].
    pub(crate) fn seed_pending_folds(&mut self) {
        if self.pending_folds.is_empty() {
            return;
        }
        if self.buffer().options.foldmethod != crate::options::FoldMethod::Manual {
            return;
        }
        let Some(path) = self
            .buffer()
            .path
            .as_ref()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| crate::editor::normalize_path(p))
        else {
            return;
        };
        let Some(folds) = self.pending_folds.remove(&path) else {
            return;
        };
        if self.windows.cur().folds.is_empty() {
            self.windows.cur_mut().folds.restore(&folds);
        }
    }

    /// Whether buffer `buf` wants LSP folds but has no fresh `foldingRange` result
    /// for its current `changedtick` yet. The server uses this to decide whether to
    /// (re)issue a `textDocument/foldingRange` request — so a request fires both
    /// when the buffer's content changes (the tick moves, staling the old result)
    /// and when `foldmethod`/`foldexpr` is set to the LSP source (no result at all).
    pub fn needs_lsp_fold_request(&self, buf: BufferId) -> bool {
        if !self.buffer_wants_lsp_folds(buf) {
            return false;
        }
        let tick = self.buffer_of(buf).map_or(0, |b| b.changedtick);
        !self
            .external_folds
            .get(&buf)
            .is_some_and(|e| e.changedtick == tick && matches!(e.data, ExternalFoldData::Lsp(_)))
    }

    /// The focused buffer's `'foldexpr'` (empty when unset).
    pub(crate) fn foldexpr(&self) -> &str {
        self.effective_foldexpr(self.current_buffer_id())
    }

    /// `buf`'s *effective* `'foldexpr'`: its own expression, or the global value when it
    /// has none (empty ⇒ no expression). The per-buffer form [`Editor::foldexpr`] reads
    /// for the focused buffer and the server mirrors into `vim.bo.foldexpr`.
    ///
    /// The global fallback is what makes the usual config pair — `vim.opt.foldmethod =
    /// "expr"` next to `vim.opt.foldexpr = …` — apply to every buffer rather than only
    /// the one the config ran in.
    pub fn effective_foldexpr(&self, buf: BufferId) -> &str {
        self.foldexprs
            .get(&buf)
            .map(String::as_str)
            .unwrap_or(&self.foldexpr_global)
    }

    /// Set (or, with an empty string, clear) the **global value** of `'foldexpr'` — the
    /// fallback a buffer with no expression of its own folds by. The `:setglobal` /
    /// `vim.go` half of [`Editor::set_foldexpr`]; rebuilds the focused window's folds,
    /// since the buffer it shows may resolve through this.
    pub(crate) fn set_foldexpr_global(&mut self, value: &str) {
        self.foldexpr_global = value.to_string();
        self.external_folds.clear();
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }

    /// The global value of `'foldexpr'` (empty ⇒ none), for the `:setglobal fde?` readout
    /// and the `vim.go` mirror.
    pub fn foldexpr_global(&self) -> &str {
        &self.foldexpr_global
    }

    /// Set the focused buffer's `'foldexpr'`. Empty clears it. The expression
    /// drives `foldmethod=expr`: the canonical tree-sitter / LSP markers fold
    /// natively, and any other value is a generic Lua `'foldexpr'` the server
    /// evaluates per line. The stale externally-pushed data is dropped so the new
    /// expression's result can't be confused with the old one's, and the structure
    /// is rebuilt for the new expression.
    pub(crate) fn set_foldexpr(&mut self, value: &str) {
        let buf = self.current_buffer_id();
        if value.is_empty() {
            self.foldexprs.remove(&buf);
        } else {
            self.foldexprs.insert(buf, value.to_string());
        }
        // The previous expr's pushed values no longer describe this expr; clear them
        // (and bust the structure cache) so we don't fold by stale data while the
        // server re-evaluates.
        self.external_folds.remove(&buf);
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }

    /// Store the server's per-line `'foldexpr'` values for `buf` (computed at
    /// `changedtick`) and rebuild the focused window's folds from them. Called by
    /// the server after evaluating a generic Lua `foldexpr` (which nxvim-core can't
    /// run). Busts the structure cache so the push is honored even when the
    /// `changedtick` is unchanged (e.g. the first evaluation after an edit already
    /// cached an empty set while the server caught up).
    /// `first` is the 0-based row `values[0]` belongs to — the range
    /// [`pending_foldexpr`](Self::pending_foldexpr) asked for. A push whose
    /// `changedtick` no longer matches the buffer (the text moved on while the
    /// server was evaluating) is dropped: the next frame asks again against the
    /// current text rather than splicing values derived from stale lines.
    pub fn set_foldexpr_values(
        &mut self,
        buf: BufferId,
        changedtick: u64,
        first: usize,
        values: Vec<String>,
    ) {
        let current = self
            .buffer_of(buf)
            .map(|b| (b.changedtick, b.line_count()))
            .unwrap_or((0, 0));
        if current.0 != changedtick {
            return;
        }
        let params = FoldInputParams::Expr {
            expr: self.effective_foldexpr(buf).to_string(),
        };
        // A push covering the whole buffer can seed the cache outright; a partial one
        // only makes sense against a cache that is already the right shape.
        let entry = self.fold_inputs.entry(buf).or_insert_with(|| FoldInputs {
            tick: changedtick,
            params: params.clone(),
            lines: FoldLines::Expr(vec![None; current.1]),
        });
        if entry.params != params || entry.tick != changedtick {
            if first != 0 || values.len() != current.1 {
                return;
            }
            *entry = FoldInputs {
                tick: changedtick,
                params,
                lines: FoldLines::Expr(vec![None; current.1]),
            };
        }
        if let FoldLines::Expr(slots) = &mut entry.lines {
            for (i, v) in values.into_iter().enumerate() {
                if let Some(slot) = slots.get_mut(first + i) {
                    *slot = Some(v);
                }
            }
        }
        self.rebuild_pushed_folds(buf);
    }

    /// Store the server's LSP `foldingRange` line spans for `buf` (computed at
    /// `changedtick`) and rebuild the focused window's folds. Mirrors
    /// [`Editor::set_foldexpr_values`] for the LSP source.
    pub fn set_lsp_folds(&mut self, buf: BufferId, changedtick: u64, ranges: Vec<(usize, usize)>) {
        self.external_folds.insert(
            buf,
            ExternalFolds {
                changedtick,
                data: ExternalFoldData::Lsp(ranges),
            },
        );
        self.rebuild_pushed_folds(buf);
    }

    /// Rebuild the focused window's folds after a server push, but only when `buf`
    /// is the focused buffer and uses an externally-computed source — a push for a
    /// background buffer is stored for when it next gains focus (the same
    /// focused-window-only scope as the indent/tree-sitter recompute).
    fn rebuild_pushed_folds(&mut self, buf: BufferId) {
        if buf != self.current_buffer_id() {
            return;
        }
        if !matches!(
            self.fold_source(),
            FoldSource::GenericExpr | FoldSource::Lsp
        ) {
            return;
        }
        // Bust the structure cache so `refresh_folds` rebuilds from the new data even
        // when the `changedtick` it keys on hasn't moved.
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }

    /// The focused buffer's externally-pushed fold data, but only when it was
    /// computed for the buffer's *current* `changedtick` — a stale push (for a
    /// since-edited buffer) reads as "no data yet", so the caller leaves the folds
    /// alone and waits for the server to re-push.
    fn fresh_external_folds(&self) -> Option<&ExternalFoldData> {
        let buf = self.current_buffer_id();
        let ext = self.external_folds.get(&buf)?;
        (ext.changedtick == self.buffer().changedtick).then_some(&ext.data)
    }

    /// Build the focused buffer's generic-`foldexpr` folds from the server-pushed
    /// per-line values (vim's `fold-expr` value grammar), applying `'foldnestmax'`
    /// and `'foldminlines'`. `None` when no fresh values have been pushed yet (or
    /// the pushed data is for the LSP source) — leave folds alone and retry.
    fn compute_generic_expr_folds(&mut self) -> Option<Vec<(usize, usize, usize)>> {
        let params = FoldInputParams::Expr {
            expr: self.foldexpr().to_string(),
        };
        let bo = &self.buffer().options;
        let (nestmax, foldminlines, line_count) =
            (bo.foldnestmax, bo.foldminlines, self.buffer().line_count());
        let Some(FoldLines::Expr(values)) = self.sync_fold_inputs(&params) else {
            return None;
        };
        // Any row still unevaluated: leave the folds alone and retry once the server
        // has filled it in (it reads the same cache through `pending_foldexpr`).
        if values.iter().any(Option::is_none) {
            return None;
        }
        Some(ranges_from_foldexpr_values(
            values,
            line_count,
            nestmax,
            foldminlines,
        ))
    }

    /// Build the focused buffer's LSP folds from the server-pushed `foldingRange`
    /// spans — containment depth → per-line levels (the same shape as tree-sitter),
    /// then nested ranges. `None` when no fresh ranges have been pushed yet.
    fn compute_lsp_folds(&self) -> Option<Vec<(usize, usize, usize)>> {
        let ExternalFoldData::Lsp(spans) = self.fresh_external_folds()?;
        let bo = &self.buffer().options;
        Some(ranges_from_containment(
            spans,
            self.buffer().line_count(),
            bo.foldnestmax,
            bo.foldminlines,
        ))
    }

    /// Compute `'foldmethod=indent'` folds for the focused buffer: each line's fold
    /// level is its leading-indent display width divided by `'shiftwidth'` (capped
    /// at `'foldnestmax'`), and a blank line takes the *lower* of the levels of the
    /// non-blank lines around it — so trailing blanks fall out of a fold while
    /// blanks *between* same-level lines stay in (vim's `fold-indent` rule). The
    /// per-line level array is folded into nested ranges by [`ranges_from_levels`].
    fn compute_indent_folds(&mut self) -> Vec<(usize, usize, usize)> {
        let bo = &self.buffer().options;
        let (sw, nestmax, foldminlines) = (
            bo.effective_shiftwidth().max(1),
            bo.foldnestmax,
            bo.foldminlines,
        );
        let params = FoldInputParams::Indent {
            tabstop: bo.effective_tabstop(),
        };
        let Some(FoldLines::Indent(cols)) = self.sync_fold_inputs(&params) else {
            return Vec::new();
        };
        // Per-line indent level; blank lines are marked and resolved afterward so a
        // blank line never starts or ends a fold on its own.
        let mut levels = Vec::with_capacity(cols.len());
        let mut blank = Vec::with_capacity(cols.len());
        for c in cols {
            levels.push(c.map_or(0, |c| (c / sw).min(nestmax)));
            blank.push(c.is_none());
        }
        // Resolve blank lines to `min(prev_nonblank, next_nonblank)` — the level
        // that keeps an interior blank inside its block but drops a trailing blank
        // out of the fold above it.
        resolve_masked_levels(&mut levels, &blank);
        ranges_from_levels(&levels, foldminlines)
    }

    /// Bring the focused buffer's cached per-line fold inputs up to date under
    /// `params` and hand them back.
    ///
    /// The cache is what stops a computed fold source from costing a pass over the
    /// whole buffer on every keystroke: each source derives one value per line from
    /// that line's **text alone**, so an edit invalidates exactly the rows it
    /// touched. The fold edit journal names those rows and the rest are spliced
    /// through untouched. What remains per keystroke is deriving levels from the
    /// cached values, which is integer work with no rope reads and no allocation.
    ///
    /// Rebuilt from text rather than spliced when there is no cache yet, when the
    /// derivation parameters changed (`:set tabstop=`, `:set foldmarker=`), when the
    /// batch is a `resync` (undo/redo, reload — the rows are meaningless), when the
    /// batch cannot be folded into one forward-moving span, or when the spliced
    /// length does not match the buffer's. That last check is the safety net: any
    /// mistake in the span arithmetic shows up as a length mismatch and falls back to
    /// the correct-by-construction path instead of producing wrong folds.
    fn sync_fold_inputs(&mut self, params: &FoldInputParams) -> Option<&FoldLines> {
        let id = self.current_buffer_id();
        // Drained unconditionally, so the journal cannot accumulate behind a rebuild.
        let batch = self.take_fold_edits_of(id).unwrap_or_default();
        let tick = self.buffer().changedtick;
        let cached = self.fold_inputs.get(&id);
        let mut lines = match cached {
            Some(c) if c.params == *params && !batch.resync => {
                if c.tick == tick && batch.edits.is_empty() {
                    return self.fold_inputs.get(&id).map(|c| &c.lines);
                }
                Some(c.lines.clone())
            }
            _ => None,
        };
        if let Some(l) = lines.as_mut() {
            let buf = self.buffer();
            // The journal's points are *positions*, so the rows they touch are
            // inclusive at both ends; the end-exclusive spans run one past. An edit
            // ending at the very end of the buffer has its `old_end_point` on the
            // rope's phantom trailing line, which is not cached, so the old span can
            // run exactly one row past the cached length — clamping there is the
            // conversion, not a fudge.
            let spliced = crate::buffer::fold_edit_rows(&batch.edits).is_some_and(|(s, oe, ne)| {
                if s > l.len() || oe > l.len() {
                    return false;
                }
                l.splice(buf, params, s, (oe + 1).min(l.len()), ne + 1);
                true
            });
            if !spliced || l.len() != buf.line_count() {
                lines = None;
            }
        }
        let lines = lines.unwrap_or_else(|| FoldLines::build(self.buffer(), params));
        self.fold_inputs.insert(
            id,
            FoldInputs {
                tick,
                params: params.clone(),
                lines,
            },
        );
        self.fold_inputs.get(&id).map(|c| &c.lines)
    }

    /// Compute `'foldmethod=marker'` folds for the focused buffer: the literal
    /// `'foldmarker'` start/end strings in the text bound folds (default `{{{`/`}}}`).
    /// Each line's fold level is computed by vim's `foldlevelMarker` rule
    /// ([`marker_line_levels`]) — a start marker raises the level at its line, an end
    /// marker lowers it only *after* its line (so the end-marker line stays in the
    /// fold), and a number after a marker sets an absolute level — then the per-line
    /// levels fold into nested ranges via [`ranges_from_levels`].
    fn compute_marker_folds(&mut self) -> Vec<(usize, usize, usize)> {
        let (open, close) = self.effective_foldmarker();
        let nestmax = self.buffer().options.foldnestmax;
        let foldminlines = self.buffer().options.foldminlines;
        let params = FoldInputParams::Marker { open, close };
        let Some(FoldLines::Marker(per_line)) = self.sync_fold_inputs(&params) else {
            return Vec::new();
        };
        // `run` is vim's `lvl_next`: the fold level carried into the next line. It is
        // kept uncapped so deeply-nested markers still *pair* correctly; only the
        // recorded per-line level is clamped to `'foldnestmax'` (as vim does).
        let mut run = 0usize;
        let levels: Vec<usize> = per_line
            .iter()
            .map(|toks| {
                // A line with no markers is the identity transition, which is almost
                // every line — the reason the cache stores `None` for it.
                let Some(toks) = toks else {
                    return run.min(nestmax);
                };
                let (lvl, next) = marker_levels_from(toks, run);
                run = next;
                lvl.min(nestmax)
            })
            .collect();
        ranges_from_levels(&levels, foldminlines)
    }

    /// The focused buffer's effective `'foldmarker'` — its `(start, end)` override or
    /// vim's default `{{{`/`}}}` when unset.
    pub(crate) fn effective_foldmarker(&self) -> (String, String) {
        self.effective_foldmarker_of(self.current_buffer_id())
    }

    /// `buf`'s *effective* `'foldmarker'` pair: its own, else the global value
    /// (`:setglobal foldmarker=…`), else vim's built-in `{{{`/`}}}`. The per-buffer form
    /// [`Editor::effective_foldmarker`] reads for the focused buffer and the server
    /// mirrors into `vim.bo.foldmarker`.
    pub fn effective_foldmarker_of(&self, buf: BufferId) -> (String, String) {
        self.foldmarkers
            .get(&buf)
            .cloned()
            .or_else(|| self.foldmarker_global.clone())
            .unwrap_or_else(default_foldmarker)
    }

    /// Set the **global value** of `'foldmarker'` — the pair a buffer with none of its
    /// own folds by. The `:setglobal` / `vim.go` half of [`Editor::set_foldmarker`];
    /// rebuilds the focused window's folds, which may resolve through it.
    pub(crate) fn set_foldmarker_global(&mut self, open: &str, close: &str) {
        self.foldmarker_global = Some((open.to_string(), close.to_string()));
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }

    /// Clear the global `'foldmarker'` (back to vim's `{{{`/`}}}`) — `:setglobal fmr&`.
    pub(crate) fn reset_foldmarker_global(&mut self) {
        self.foldmarker_global = None;
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }

    /// The global `'foldmarker'` pair (vim's default when none was set), for the
    /// `:setglobal fmr?` readout and the `vim.go` mirror.
    pub fn foldmarker_global(&self) -> (String, String) {
        self.foldmarker_global
            .clone()
            .unwrap_or_else(default_foldmarker)
    }

    /// Set the focused buffer's `'foldmarker'` to the `(start, end)` pair and refold.
    /// The markers don't enter the [`FoldKey`] cache key, so the structure cache is
    /// busted explicitly to honor the change even on an unchanged `changedtick`.
    pub(crate) fn set_foldmarker(&mut self, start: &str, end: &str) {
        let buf = self.current_buffer_id();
        self.foldmarkers
            .insert(buf, (start.to_string(), end.to_string()));
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }

    /// Reset the focused buffer's `'foldmarker'` to vim's default `{{{`/`}}}` and
    /// refold.
    pub(crate) fn reset_foldmarker(&mut self) {
        let buf = self.current_buffer_id();
        self.foldmarkers.remove(&buf);
        self.windows.cur_mut().folds.cache = None;
        self.refresh_folds();
    }
}

/// Vim's default `'foldmarker'` pair (`{{{` / `}}}`).
fn default_foldmarker() -> (String, String) {
    ("{{{".to_string(), "}}}".to_string())
}

/// Resolve every `mask`ed line's fold level to `min(prev_unmasked,
/// next_unmasked)` — the rule that keeps an interior masked line (a blank line
/// under `foldmethod=indent`, or a `-1` "undefined" foldexpr value) inside the
/// shallower of the blocks bracketing it, while a trailing masked run falls out
/// of the fold above it. A line with no defined neighbour on a side takes the
/// other side, or level 0 when both sides are masked.
fn resolve_masked_levels(levels: &mut [usize], mask: &[bool]) {
    let n = levels.len();
    for i in 0..n {
        if !mask[i] {
            continue;
        }
        let prev = (0..i).rev().find(|&j| !mask[j]).map(|j| levels[j]);
        let next = (i + 1..n).find(|&j| !mask[j]).map(|j| levels[j]);
        levels[i] = match (prev, next) {
            (Some(p), Some(q)) => p.min(q),
            (Some(p), None) => p,
            (None, Some(q)) => q,
            (None, None) => 0,
        };
    }
}

/// The value of the run of leading ASCII digits in `s` (a number following a fold
/// marker, e.g. the `2` in `{{{2`), or `None` when `s` doesn't start with a digit.
fn leading_number(s: &str) -> Option<usize> {
    let digits = s.len() - s.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    (digits > 0).then(|| s[..digits].parse().ok()).flatten()
}

/// Whether `'foldexpr'` is the canonical tree-sitter foldexpr nxvim computes
/// natively — neovim's `v:lua.vim.treesitter.foldexpr()` and the `nx.*` spellings.
/// The trailing `()` and a `v:lua.` prefix are optional, and surrounding
/// whitespace is ignored, so the common config spellings all resolve.
fn is_treesitter_foldexpr(expr: &str) -> bool {
    matches!(
        canonical_foldexpr(expr),
        "vim.treesitter.foldexpr" | "nx.treesitter.foldexpr"
    )
}

/// Normalize a `'foldexpr'` to its bare callable name for the native-marker
/// checks: drop surrounding whitespace, an optional `v:lua.` prefix, and a
/// trailing `()`, so the common config spellings all collapse to one form.
fn canonical_foldexpr(expr: &str) -> &str {
    let e = expr.trim();
    let e = e.strip_prefix("v:lua.").unwrap_or(e);
    e.strip_suffix("()").unwrap_or(e)
}

/// Whether `'foldexpr'` is the canonical LSP foldexpr marker — `nx.lsp.foldexpr`
/// (and the `vim.lsp.foldexpr` alias). Like the tree-sitter marker it is a native
/// reference the fold engine recognizes rather than a per-line Lua call: the
/// server requests `textDocument/foldingRange` and pushes the ranges in.
fn is_lsp_foldexpr(expr: &str) -> bool {
    matches!(
        canonical_foldexpr(expr),
        "vim.lsp.foldexpr" | "nx.lsp.foldexpr"
    )
}

/// Turn opaque foldable line spans (tree-sitter `@fold` node ranges or LSP
/// `foldingRange` results) into nested `(start, end, level)` folds: each line's
/// level is how many spans contain it (containment depth), capped at
/// `'foldnestmax'`, then [`ranges_from_levels`] recovers the fold tree — the same
/// builder the indent / generic-expr sources feed.
fn ranges_from_containment(
    spans: &[(usize, usize)],
    line_count: usize,
    foldnestmax: usize,
    foldminlines: usize,
) -> Vec<(usize, usize, usize)> {
    let mut levels = vec![0usize; line_count];
    for &(start, end) in spans {
        let end = end.min(line_count.saturating_sub(1));
        for level in levels.iter_mut().take(end + 1).skip(start) {
            *level = (*level + 1).min(foldnestmax);
        }
    }
    ranges_from_levels(&levels, foldminlines)
}

/// One parsed vim `'foldexpr'` value (`:h fold-expr`). The expression returns one
/// of these per line; the array is then resolved to per-line fold levels.
enum FoldExprValue {
    /// A literal level (`0`, `1`, …). `0` ⇒ not in a fold.
    Level(usize),
    /// `-1` — undefined; resolved to the lower of the surrounding defined levels.
    Undefined,
    /// `"="` — same level as the previous line.
    Same,
    /// `"aN"` — the previous line's level plus `N`.
    Add(usize),
    /// `"sN"` — the previous line's level minus `N`.
    Sub(usize),
    /// `">N"` — a fold of level `N` *starts* at this line (the line's level is `N`).
    Start(usize),
    /// `"<N"` — a fold of level `N` *ends* at this line (the line is level `N`; the
    /// next line drops to `N-1`).
    End(usize),
}

/// Parse one vim `'foldexpr'` value string. An unrecognized / malformed value is
/// `Level(0)` (vim is lenient here — a bad expr just leaves the line unfolded).
fn parse_foldexpr_value(raw: &str) -> FoldExprValue {
    let v = raw.trim();
    if v == "=" {
        return FoldExprValue::Same;
    }
    let num = |s: &str| s.trim().parse::<usize>().ok();
    match v.as_bytes().first() {
        Some(b'>') => num(&v[1..]).map_or(FoldExprValue::Level(0), FoldExprValue::Start),
        Some(b'<') => num(&v[1..]).map_or(FoldExprValue::Level(0), FoldExprValue::End),
        Some(b'a' | b'A') => num(&v[1..]).map_or(FoldExprValue::Level(0), FoldExprValue::Add),
        Some(b's' | b'S') => num(&v[1..]).map_or(FoldExprValue::Level(0), FoldExprValue::Sub),
        _ => {
            if v == "-1" {
                FoldExprValue::Undefined
            } else {
                FoldExprValue::Level(num(v).unwrap_or(0))
            }
        }
    }
}

/// Resolve a buffer's per-line `'foldexpr'` values (`values[i]` = the expression's
/// result for line `i`) into nested `(start, end, level)` folds. The relative /
/// marker forms (`=`/`aN`/`sN`/`>N`/`<N`) are folded against a running level, `-1`
/// lines take the lower of their nearest defined neighbours (vim's rule), every
/// level is capped at `'foldnestmax'`, and [`ranges_from_levels`] recovers the
/// tree honoring `'foldminlines'`.
fn ranges_from_foldexpr_values(
    values: &[Option<String>],
    line_count: usize,
    foldnestmax: usize,
    foldminlines: usize,
) -> Vec<(usize, usize, usize)> {
    let n = line_count;
    let mut levels = vec![0usize; n];
    let mut undefined = vec![false; n];
    // `run` is the running fold level carried into the next line (what `=`/`aN`/`sN`
    // are relative to, and where a `<N` end drops back to).
    let mut run = 0usize;
    for i in 0..n {
        let raw = values.get(i).and_then(Option::as_deref).unwrap_or("0");
        let value = parse_foldexpr_value(raw);
        let lvl = match value {
            FoldExprValue::Level(k) => k,
            FoldExprValue::Undefined => {
                undefined[i] = true;
                run
            }
            FoldExprValue::Same => run,
            FoldExprValue::Add(k) => run + k,
            FoldExprValue::Sub(k) => run.saturating_sub(k),
            FoldExprValue::Start(k) => k,
            FoldExprValue::End(k) => k,
        }
        .min(foldnestmax);
        levels[i] = lvl;
        // A `<N` end leaves the *following* lines one level shallower; everything
        // else carries this line's level forward.
        run = match value {
            FoldExprValue::End(k) => k.saturating_sub(1).min(foldnestmax),
            _ => lvl,
        };
    }
    // Resolve `-1` lines to `min(prev_defined, next_defined)` — the rule that keeps
    // an undefined line inside the shallower of the blocks bracketing it.
    resolve_masked_levels(&mut levels, &undefined);
    ranges_from_levels(&levels, foldminlines)
}

/// Fold a per-line fold-level array into nested `(start, end, level)` ranges
/// (vim's universal computed-fold model, shared by `indent`/`expr`/tree-sitter).
/// A fold of depth `d` spans the maximal run of consecutive lines whose level is
/// `≥ d`; a rise in level opens folds starting at that line, a drop closes them at
/// the previous line. Only folds spanning more than `foldminlines` lines (and at
/// least two, the model's minimum) are emitted, and duplicate ranges produced by a
/// level jumping by more than one are collapsed to their shallowest level.
fn ranges_from_levels(levels: &[usize], foldminlines: usize) -> Vec<(usize, usize, usize)> {
    let mut out: Vec<(usize, usize, usize)> = Vec::new();
    // `open[k]` is the start line of the currently-open depth-`(k+1)` fold.
    let mut open: Vec<usize> = Vec::new();
    let mut push = |start: usize, end: usize, level: usize| {
        // A fold needs ≥2 lines (the model's invariant) and must clear
        // `'foldminlines'` to be worth displaying closed.
        if end > start && end - start + 1 > foldminlines {
            out.push((start, end, level));
        }
    };
    for (i, &cur) in levels.iter().enumerate() {
        while open.len() > cur {
            let level = open.len();
            let start = open.pop().expect("len checked");
            push(start, i.saturating_sub(1), level);
        }
        while open.len() < cur {
            open.push(i);
        }
    }
    let last = levels.len().saturating_sub(1);
    while let Some(start) = open.pop() {
        let level = open.len() + 1;
        push(start, last, level);
    }
    // A jump of more than one level (e.g. indent 0 → 2) opens several folds at the
    // same line that can close at the same line too, yielding duplicate ranges;
    // keep one per range at its shallowest level (sort puts the smallest level
    // first within an equal `(start, end)` group).
    out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    out.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);
    out
}

/// The parameters a set of cached per-line fold inputs was derived under. A
/// mismatch means the cache describes a different derivation and must be rebuilt —
/// this is what makes `:set tabstop=` / `:set foldmarker=` take effect on text that
/// did not change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FoldInputParams {
    /// `'tabstop'` only: `'shiftwidth'` divides the cached columns at derive time,
    /// so changing it does not invalidate the scan.
    Indent {
        tabstop: usize,
    },
    Marker {
        open: String,
        close: String,
    },
    /// The generic Lua `'foldexpr'` itself: a different expression is a different
    /// derivation, so changing it discards the values rather than reusing them.
    Expr {
        expr: String,
    },
}

/// One marker occurrence: whether it opens (`{{{`) or closes (`}}}`), and the
/// absolute level a trailing number gave it.
pub(crate) type MarkerTok = (bool, Option<usize>);

/// The cached per-line inputs themselves, one entry per editable line.
#[derive(Debug, Clone)]
pub(crate) enum FoldLines {
    /// Leading-whitespace **display columns**, or `None` for a blank line (which
    /// takes its level from its neighbours rather than owning one).
    Indent(Vec<Option<usize>>),
    /// The marker tokens each line carries, in document order — `None` for a line
    /// with none, which is almost every line and is the identity transition.
    Marker(Vec<Option<Box<[MarkerTok]>>>),
    /// The generic `'foldexpr'` value for each line, or `None` for a line whose
    /// value has not been evaluated yet. Unlike the other two this cannot be derived
    /// from text here — nxvim-core cannot run Lua — so a fresh or spliced row is
    /// left `None` and the server fills it in
    /// ([`Editor::pending_foldexpr`] / [`Editor::set_foldexpr_values`]).
    Expr(Vec<Option<String>>),
}

impl FoldLines {
    fn len(&self) -> usize {
        match self {
            FoldLines::Indent(v) => v.len(),
            FoldLines::Marker(v) => v.len(),
            FoldLines::Expr(v) => v.len(),
        }
    }

    /// Replace rows `[start, old_end)` with the same rows re-derived from `buf`'s
    /// current text over `[start, new_end)`.
    fn splice(
        &mut self,
        buf: &Buffer,
        params: &FoldInputParams,
        start: usize,
        old_end: usize,
        new_end: usize,
    ) {
        match (self, params) {
            (FoldLines::Indent(v), FoldInputParams::Indent { tabstop }) => {
                let fresh = (start..new_end).map(|i| indent_cols_of(&buf.line_cow(i), *tabstop));
                v.splice(start..old_end, fresh.collect::<Vec<_>>());
            }
            (FoldLines::Marker(v), FoldInputParams::Marker { open, close }) => {
                let fresh = (start..new_end).map(|i| marker_toks_of(&buf.line_cow(i), open, close));
                v.splice(start..old_end, fresh.collect::<Vec<_>>());
            }
            (FoldLines::Expr(v), FoldInputParams::Expr { .. }) => {
                // No text derivation: the replaced rows simply need re-evaluating.
                v.splice(start..old_end, (start..new_end).map(|_| None));
            }
            // Kind and params disagree — the caller rebuilds rather than splicing.
            _ => {}
        }
    }

    /// Derive the whole set from `buf`'s text, in one sequential pass.
    fn build(buf: &Buffer, params: &FoldInputParams) -> Self {
        match params {
            FoldInputParams::Indent { tabstop } => FoldLines::Indent(
                buf.iter_lines()
                    .map(|l| indent_cols_of(&l, *tabstop))
                    .collect(),
            ),
            FoldInputParams::Marker { open, close } => FoldLines::Marker(
                buf.iter_lines()
                    .map(|l| marker_toks_of(&l, open, close))
                    .collect(),
            ),
            FoldInputParams::Expr { .. } => FoldLines::Expr(vec![None; buf.line_count()]),
        }
    }
}

/// A buffer's cached per-line fold inputs, with what they describe.
#[derive(Debug, Clone)]
pub(crate) struct FoldInputs {
    /// The `changedtick` the inputs describe.
    tick: u64,
    params: FoldInputParams,
    lines: FoldLines,
}

/// The display columns of `line`'s leading whitespace, or `None` when the line is
/// blank. A tab advances to the next `tabstop`. Pure function of the line's text —
/// which is exactly why the result can be cached per line and spliced.
fn indent_cols_of(line: &str, tabstop: usize) -> Option<usize> {
    if line.trim().is_empty() {
        return None;
    }
    let mut cols = 0usize;
    for ch in line.chars() {
        match ch {
            ' ' => cols += 1,
            '\t' => cols += tabstop - (cols % tabstop),
            _ => break,
        }
    }
    Some(cols)
}

/// The fold markers `line` carries, in document order, or `None` when it carries
/// none. Also a pure function of the line's text.
fn marker_toks_of(line: &str, open: &str, close: &str) -> Option<Box<[MarkerTok]>> {
    let mut markers: Vec<(usize, bool)> = line
        .match_indices(open)
        .map(|(p, _)| (p, true))
        .chain(line.match_indices(close).map(|(p, _)| (p, false)))
        .collect();
    if markers.is_empty() {
        return None;
    }
    markers.sort_by_key(|&(p, _)| p);
    Some(
        markers
            .into_iter()
            .map(|(pos, is_open)| {
                let after = pos + if is_open { open.len() } else { close.len() };
                (is_open, leading_number(&line[after..]))
            })
            .collect(),
    )
}

/// Apply one line's marker tokens to the running fold level, returning
/// `(this line's level, the level carried into the next)` — vim's `foldlevelMarker`.
/// A start marker raises the level at its own line, an end marker lowers it only
/// *after* its line (so the marker line stays in the fold), and a number after
/// either sets an absolute level.
fn marker_levels_from(toks: &[MarkerTok], start_lvl: usize) -> (usize, usize) {
    let mut lvl = start_lvl;
    let mut next = start_lvl;
    for &(is_open, num) in toks {
        match (is_open, num) {
            // `{{{N` — absolute open to level N.
            (true, Some(n)) if n > 0 => {
                lvl = n;
                next = n;
            }
            // `{{{` — nest one deeper.
            (true, _) => {
                lvl += 1;
                next += 1;
            }
            // `}}}N` — close down to level N (next line N-1); never opens a fold.
            (false, Some(n)) if n > 0 => {
                lvl = n.min(start_lvl);
                next = n.saturating_sub(1);
            }
            // `}}}` — close one level (the marker line stays in the fold).
            (false, _) => next = next.saturating_sub(1),
        }
    }
    (lvl, next)
}
