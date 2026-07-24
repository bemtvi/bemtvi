//! Diagnostics: the per-buffer `publishDiagnostics` cache projected into the
//! redraw underline spans and the under-cursor message line, the
//! `:LspDiagnostics` location list, and `[d`/`]d` navigation.

use std::collections::HashMap;

use nxvim_core::unicode;
use nxvim_core::WinHl;
use nxvim_lsp::lsp_types::Diagnostic;
use nxvim_lsp::PositionEncoding;
use rmpv::Value;

use super::*;
use crate::redraw::StyleTable;
use crate::EditHost;

impl EditHost {
    /// The current buffer's cached diagnostics together with its server's
    /// negotiated position encoding, or `None` when the buffer has no attached
    /// server (so callers project nothing). Both borrows are released before any
    /// `&mut self` use.
    pub(crate) fn current_diagnostics(&self) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
        self.diagnostics_of(self.editor.current_buffer_id())
    }

    /// Buffer-addressed form of [`EditHost::current_diagnostics`], for projecting a
    /// non-focused window's own buffer. Same `(diagnostics, encoding)` or `None`
    /// when that buffer has no attached server.
    pub(crate) fn diagnostics_of(
        &self,
        buffer: nxvim_core::BufferId,
    ) -> Option<(&Vec<Diagnostic>, PositionEncoding)> {
        let state = self.lsp_states.get(&buffer)?;
        let key = state.server.as_ref()?;
        let encoding = self.lsp_servers.get(key)?.encoding;
        Some((&state.diagnostics, encoding))
    }

    /// Every diagnostic to project for `buffer`, each paired with the position
    /// encoding its `character` columns are authored in. Two sources are merged:
    /// the LSP server's published set (at the server's *negotiated* encoding) and
    /// the client-set (`vim.diagnostic.set`) set, which has no server and so is
    /// already in nxvim's native bytes — tagged [`PositionEncoding::Utf8`] so the
    /// shared byte-column conversion ([`EditHost::diag_row_span`]) is the identity.
    /// Unlike [`EditHost::diagnostics_of`] this also yields client-set diagnostics
    /// for a buffer with *no attached server*, so the render surfaces aren't gated
    /// on an LSP. Empty when the buffer has neither source.
    pub(crate) fn diagnostics_merged(
        &self,
        buffer: nxvim_core::BufferId,
    ) -> Vec<(&Diagnostic, PositionEncoding)> {
        let mut out = Vec::new();
        if let Some((diags, encoding)) = self.diagnostics_of(buffer) {
            out.extend(diags.iter().map(|d| (d, encoding)));
        }
        if let Some(diags) = self.client_diagnostics.get(&buffer) {
            out.extend(diags.iter().map(|d| (d, PositionEncoding::Utf8)));
        }
        out
    }

    /// [`EditHost::diagnostics_merged`] for the current buffer — the merged set the
    /// cursor-anchored surfaces (under-cursor message, `goto`, float, loclist) read.
    pub(crate) fn current_diagnostics_merged(&self) -> Vec<(&Diagnostic, PositionEncoding)> {
        self.diagnostics_merged(self.editor.current_buffer_id())
    }

    /// Build the per-row `diagnostics` redraw payload from a row→buffer-line
    /// mapping (`numbers`, 1-based, `None` for filler): each visible row's
    /// diagnostic underline spans as `[start_col, end_col, severity, style_id]`
    /// in **screen columns**. Mirrors [`EditHost::highlights_for`] — the LSP
    /// character offsets are converted to bytes through the negotiated encoding,
    /// then bytes to screen columns with the same tab/wide-char `virtcol` the
    /// highlights and selection use, so squiggles line up with the glyphs.
    /// `severity` is `1`=error … `4`=hint; `style_id` indexes the per-frame
    /// `styles` palette when the matching `DiagnosticUnderline*` group resolves
    /// through the registry (`Nil` otherwise, so the client falls back to a
    /// built-in severity color).
    /// Diagnostic counts for `buffer` by severity `[error, warn, info, hint]`,
    /// for the `diagnostics` statusline segment. Zero across the board when the
    /// buffer has no language server / no diagnostics.
    pub(crate) fn diag_counts_for(&self, buffer: nxvim_core::BufferId) -> [usize; 4] {
        let mut counts = [0usize; 4];
        for (d, _) in self.diagnostics_merged(buffer) {
            let sev = super::severity_code(d.severity); // 1=error … 4=hint
            if (1..=4).contains(&sev) {
                counts[(sev - 1) as usize] += 1;
            }
        }
        counts
    }

    pub(crate) fn diagnostics_for(
        &self,
        buffer: nxvim_core::BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        // `vim.diagnostic.config({ underline = false })` hides the squiggles; the
        // message line and the location list (other surfaces) are unaffected.
        if !self.diag_config.underline {
            // One empty entry per row so the client's `diagnostics[row]` index
            // stays aligned with `highlights`/`numbers`.
            return Value::Array(segs.iter().map(|_| Value::Array(Vec::new())).collect());
        }
        let buf = self.editor.buffer_of(buffer);
        // The LSP-pushed and client-set sets merged; each diagnostic carries the
        // encoding its columns are in (the client-set ones are native UTF-8).
        let diags = self.diagnostics_merged(buffer);
        // Per-frame index built once instead of scanning the whole merged list per
        // row: each diagnostic intersects every buffer row in `[start.line,
        // end.line]` (exactly when `diag_row_span` returns `Some`). Single-line
        // diagnostics — the overwhelming majority — bucket by that one line;
        // genuinely multi-line ones (rare) stay in a small overflow list scanned
        // per row. `candidates_for` merges the two back into the original
        // merged-list order, so the emitted span order per row is identical to the
        // old `diags.iter().filter_map(...)` scan.
        let index = DiagLineIndex::build(&diags);
        // Tab width is the rendered window's buffer's `tabstop` (it may differ
        // from the current buffer's), so the underline columns line up with the
        // text the client paints for that window.
        let tabstop = buf
            .map(|b| b.options.effective_tabstop())
            .unwrap_or(unicode::TABSTOP);
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let Some(text) = buf.map(|b| b.line(line_idx)) else {
                    return Value::Array(Vec::new());
                };
                let spans = index
                    .candidates_for(line_idx as u32)
                    .filter_map(|&i| {
                        let (d, encoding) = diags[i];
                        let (start_byte, end_byte) =
                            self.diag_row_span(d, encoding, line_idx, &text)?;
                        let start_col = unicode::virtcol(&text, start_byte, tabstop);
                        let mut end_col = unicode::virtcol(&text, end_byte, tabstop);
                        // A zero-width range (e.g. an empty span at end-of-line)
                        // still needs one underlined cell to be visible.
                        if end_col <= start_col {
                            end_col = start_col + 1;
                        }
                        // Clip the underline to this row's wrap segment, rebased to
                        // row-local columns (so it lands on the right continuation row).
                        let (start_col, end_col) = seg.clip(start_col, end_col)?;
                        let severity = severity_code(d.severity);
                        let style_id = match self.resolve_winhl(winhl, severity_group(severity)) {
                            Some(style) => Value::from(styles.intern(style) as u64),
                            None => Value::Nil,
                        };
                        Some(Value::Array(vec![
                            Value::from(start_col as u64),
                            Value::from(end_col as u64),
                            Value::from(severity as u64),
                            style_id,
                        ]))
                    })
                    .collect();
                Value::Array(spans)
            })
            .collect();
        Value::Array(rows)
    }

    /// Build the per-row `diagnostics_virt` payload: for each visible row, the
    /// inline virtual-text decoration — the most severe diagnostic *starting* on
    /// that buffer line — as `[text, severity, style_id]`, or `Nil` when the row
    /// has none (or virtual text is off). `text` is the config prefix followed by
    /// the diagnostic's first message line; `severity` is `1`=error … `4`=hint;
    /// `style_id` indexes the per-frame `styles` palette when the matching
    /// `DiagnosticVirtualText*` group resolves (`Nil` otherwise, so the client
    /// falls back to a built-in severity color). Mirrors [`EditHost::diagnostics_for`]
    /// but emits one optional decoration per row rather than a span list — the text
    /// is positioned after end-of-line by the client, so no column conversion runs.
    pub(crate) fn diagnostics_virt_text_for(
        &self,
        buffer: nxvim_core::BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        if !self.diag_config.virtual_text {
            // One `Nil` per row so the client's `diagnostics_virt[row]` index
            // stays aligned with `numbers`/`diagnostics`.
            return Value::Array(segs.iter().map(|_| Value::Nil).collect());
        }
        // Merged LSP + client-set; the inline message positions by line only, so
        // the per-diagnostic encoding isn't needed here.
        let diags = self.diagnostics_merged(buffer);
        // Per-frame index of the diagnostics *starting* on each line, in merged
        // order, so `min_by_key` (which returns the first element reaching the
        // minimum) picks the same winner as the old per-row `filter`/`min_by_key`.
        let by_start = DiagStartIndex::build(&diags);
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Nil;
                };
                // The eol message sits after the line's text — on a wrapped line that
                // is the last display row only, so it isn't repeated on every
                // continuation row.
                if !seg.is_last() {
                    return Value::Nil;
                }
                let line = (n - 1) as u32;
                // The most severe diagnostic that *starts* on this row wins the
                // line's one inline slot (ties broken by leftmost column).
                let best = by_start
                    .on_line(line)
                    .map(|&i| diags[i].0)
                    .min_by_key(|d| (severity_code(d.severity), d.range.start.character));
                let Some(d) = best else {
                    return Value::Nil;
                };
                let severity = severity_code(d.severity);
                let text = format!("{}{}", self.diag_config.virt_prefix, first_line(&d.message));
                let style_id = match self.resolve_winhl(winhl, severity_virt_group(severity)) {
                    Some(style) => Value::from(styles.intern(style) as u64),
                    None => Value::Nil,
                };
                Value::Array(vec![
                    Value::from(text),
                    Value::from(severity as u64),
                    style_id,
                ])
            })
            .collect();
        Value::Array(rows)
    }

    /// Build the per-row `diagnostics_signs` payload: for each visible row, the
    /// gutter sign for the most severe diagnostic *starting* on that buffer line —
    /// as `[glyph, severity, style_id]`, or `Nil` when the row has none (or signs
    /// are off). `glyph` is the config (or built-in) per-severity letter; `severity`
    /// is `1`=error … `4`=hint; `style_id` indexes the per-frame `styles` palette
    /// when the matching `DiagnosticSign*` group resolves (`Nil` otherwise, so the
    /// client falls back to a built-in severity color). Mirrors
    /// [`EditHost::diagnostics_virt_text_for`] but addressed to the gutter.
    pub(crate) fn diagnostics_signs_for(
        &self,
        buffer: nxvim_core::BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        if !self.diag_config.signs {
            // One `Nil` per row so the client's `diagnostics_signs[row]` index
            // stays aligned with `numbers`/`diagnostics`.
            return Value::Array(segs.iter().map(|_| Value::Nil).collect());
        }
        // Merged LSP + client-set; the gutter sign positions by line only.
        let diags = self.diagnostics_merged(buffer);
        // Same per-frame start-line index as the virtual-text surface: same
        // "starts on line" filter and same `min_by_key` tie-break.
        let by_start = DiagStartIndex::build(&diags);
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Nil;
                };
                // The gutter sign sits on the line's first display row only (like the
                // number), not repeated down its wrapped continuation rows.
                if !seg.is_first() {
                    return Value::Nil;
                }
                let line = (n - 1) as u32;
                // The most severe diagnostic that *starts* on this row wins the
                // line's sign cell (ties broken by leftmost column).
                let best = by_start
                    .on_line(line)
                    .map(|&i| diags[i].0)
                    .min_by_key(|d| (severity_code(d.severity), d.range.start.character));
                let Some(d) = best else {
                    return Value::Nil;
                };
                let severity = severity_code(d.severity);
                let glyph = self.diag_config.sign_glyph(severity).to_string();
                let style_id = match self.resolve_winhl(winhl, severity_sign_group(severity)) {
                    Some(style) => Value::from(styles.intern(style) as u64),
                    None => Value::Nil,
                };
                Value::Array(vec![
                    Value::from(glyph),
                    Value::from(severity as u64),
                    style_id,
                ])
            })
            .collect();
        Value::Array(rows)
    }

    // The sign-column WIDTH is computed sign-source-agnostically from the merged
    // signs in `crate::extmarks::sign_width_from_cells` (diagnostic + extmark), so
    // there's no diagnostics-only width function here anymore.

    /// The message of the highest-severity diagnostic whose range covers the
    /// cursor, for the message line (shown only when no other message is set, so
    /// `:messages` history stays clean). `None` when the cursor is on no
    /// diagnostic. Newlines are flattened so it fits one line.
    pub(crate) fn diagnostic_under_cursor(&self) -> Option<String> {
        let (row, col) = (self.editor.cursor.line, self.editor.cursor.col);
        let line = self.editor.buffer().line(row);
        self.current_diagnostics_merged()
            .into_iter()
            .filter(|(d, encoding)| {
                self.diag_row_span(d, *encoding, row, &line)
                    // Cover the resting cell of a zero-width range too.
                    .is_some_and(|(s, e)| col >= s && col < e.max(s + 1))
            })
            .min_by_key(|(d, _)| severity_code(d.severity))
            .map(|(d, _)| first_line(&d.message))
    }

    /// The `[start, end)` **byte** span a diagnostic occupies on buffer row
    /// `line_idx` (whose text is `line`), or `None` if it does not reach that
    /// row. Multi-line ends are clipped to the row: `0` before the range's first
    /// line, the line length after its last. The LSP character offsets are
    /// converted to bytes through the negotiated `encoding` (Decision 4).
    pub(crate) fn diag_row_span(
        &self,
        d: &Diagnostic,
        encoding: PositionEncoding,
        line_idx: usize,
        line: &str,
    ) -> Option<(usize, usize)> {
        let (s, e) = (d.range.start, d.range.end);
        let row = line_idx as u32;
        if row < s.line || row > e.line {
            return None;
        }
        let start = if s.line == row {
            byte_col(encoding, line, s.character as usize)
        } else {
            0
        };
        let end = if e.line == row {
            byte_col(encoding, line, e.character as usize)
        } else {
            line.len()
        };
        Some((start, end))
    }

    /// Build the `:LspDiagnostics` location list for the current buffer: one
    /// `severity  line:col  message` row per diagnostic (sorted by position) and
    /// a parallel [`PanelTarget`] list to attach as the panel's jump targets.
    /// `None` when the buffer has no diagnostics.
    /// The current buffer's diagnostics as **location-list entries**
    /// `(path, line, col, text)` (0-based line/col), sorted by position — fed to
    /// [`nxvim_core::Editor::open_location_list`] by `:LspDiagnostics` /
    /// `vim.diagnostic.setloclist`. `None` when there are no diagnostics, or the
    /// buffer has no file path to navigate to.
    pub(crate) fn diagnostics_location_list(&self) -> Option<Vec<nxvim_core::LocListEntry>> {
        let mut items = self.current_diagnostics_merged();
        if items.is_empty() {
            return None;
        }
        // A navigable list needs a file to jump into; a no-path buffer can't have one.
        let path = self.editor.buffer().path.clone()?;
        items.sort_by_key(|(d, _)| (d.range.start.line, d.range.start.character));
        let entries = items
            .into_iter()
            .map(|(d, encoding)| {
                let row = d.range.start.line as usize;
                let character = d.range.start.character as usize;
                let line = self.editor.buffer().line(row);
                let byte = byte_col(encoding, &line, character);
                let severity = severity_code(d.severity);
                let text = format!("{}: {}", severity_short(severity), first_line(&d.message),);
                // The vim quickfix type char drives the row's severity color
                // (1=ERROR→`E` … 4=HINT→`N`, matching `vim.diagnostic.toqflist`).
                let typ = qf_type_char(severity);
                (path.clone(), row, byte, text, typ)
            })
            .collect();
        Some(entries)
    }

    /// `vim.diagnostic.open_float()`: open a float (the bottom panel, the same
    /// surface hover uses) listing every diagnostic on the cursor's line in full —
    /// the multi-line messages with their `source` and `code`, which the inline
    /// virtual text truncates to one line. Diagnostics are sorted by severity then
    /// start column; each is formatted as `E  source: message [code]`, its message
    /// split across as many panel rows as it has lines. A loud no-op (an echoed
    /// message, no panel) when the cursor's line has no diagnostics.
    pub(crate) fn diagnostics_open_float(&mut self) {
        // The cursor line's diagnostics: those *starting* on it (neovim's `lnum`
        // scope), matching the virt-text / sign surfaces. Collected and sorted
        // before any `&mut self` use so the borrow is released for `open_panel`.
        let row = self.editor.cursor.line as u32;
        let mut items: Vec<&Diagnostic> = self
            .current_diagnostics_merged()
            .into_iter()
            .filter(|(d, _)| d.range.start.line == row)
            .map(|(d, _)| d)
            .collect();
        items.sort_by_key(|d| (severity_code(d.severity), d.range.start.character));
        let lines = items
            .iter()
            .flat_map(|d| diagnostic_float_lines(d))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            self.editor.echo("No diagnostics under cursor");
            return;
        }
        self.editor.open_scratch_listing("[Diagnostics]", lines, 0);
    }

    /// `vim.diagnostic.goto_next`/`goto_prev`: move the cursor to the next
    /// (`forward`) or previous diagnostic in the current buffer, wrapping around
    /// the ends. `severity` (1=ERROR…4=HINT) restricts the set when set. A no-op
    /// when the buffer has no (matching) diagnostics. Reuses the same byte-column
    /// conversion the underline path uses, then `jump_to`s the *current* file so
    /// the move snaps to a valid resting cell (no file open — same buffer).
    pub(crate) fn diagnostic_goto(&mut self, forward: bool, severity: Option<u8>) {
        // Resolve every (matching) diagnostic to a 0-based (line, byte col) and
        // sort by position, so "next/previous from the cursor" is a list walk.
        let mut positions: Vec<(usize, usize)> = self
            .current_diagnostics_merged()
            .into_iter()
            .filter(|(d, _)| severity.map_or(true, |s| severity_code(d.severity) == s))
            .map(|(d, encoding)| {
                let row = d.range.start.line as usize;
                let line = self.editor.buffer().line(row);
                (
                    row,
                    byte_col(encoding, &line, d.range.start.character as usize),
                )
            })
            .collect();
        if positions.is_empty() {
            return;
        }
        positions.sort_unstable();
        positions.dedup();

        let cur = (self.editor.cursor.line, self.editor.cursor.col);
        // The next strictly-after (forward) or strictly-before (backward) target,
        // wrapping to the first/last when the cursor is past the last/before the
        // first — neovim's `goto_next`/`goto_prev` wrap behavior.
        let target = if forward {
            positions
                .iter()
                .find(|&&p| p > cur)
                .copied()
                .unwrap_or(positions[0])
        } else {
            positions
                .iter()
                .rev()
                .find(|&&p| p < cur)
                .copied()
                .unwrap_or(positions[positions.len() - 1])
        };

        let (line, byte) = target;
        if let Some(path) = self.editor.buffer().path.clone() {
            self.editor.jump_to(&path, line, byte);
        }
    }

    /// The cached diagnostics **overlapping** the 0-based, end-exclusive buffer range
    /// `(start_row, start_col, end_row, end_col)` — the `context.diagnostics` a
    /// code-action request carries, so a quickfix action offered over a selection is
    /// given the very diagnostics that selection covers. An empty range (a point at
    /// the cursor) reads as one byte wide, which is what makes the cursor case
    /// "the diagnostics under the cursor"; a zero-width *diagnostic* is likewise
    /// treated as one byte wide, so it can still be hit.
    pub(crate) fn diagnostics_in_range(
        &self,
        (s_row, s_col, e_row, e_col): (usize, usize, usize, usize),
    ) -> Vec<Diagnostic> {
        let Some((diags, encoding)) = self.current_diagnostics() else {
            return Vec::new();
        };
        let buffer = self.editor.buffer();
        let lo = buffer.byte_at(s_row, s_col);
        let hi = buffer.byte_at(e_row, e_col).max(lo);
        diags
            .iter()
            .filter(|d| {
                let span = self.lsp_range_to_bytes(&d.range, encoding);
                span.start < hi.max(lo + 1) && lo < span.end.max(span.start + 1)
            })
            .cloned()
            .collect()
    }
}

/// A per-frame index from a buffer line to the merged-list positions of the
/// diagnostics *starting* on that line — the keying both [`EditHost::diagnostics_virt_text_for`]
/// and [`EditHost::diagnostics_signs_for`] use. Built once per call instead of
/// re-scanning the whole merged list for every visible row. Each bucket holds the
/// merged-list indices in their original order, so iterating a bucket and folding
/// it with `min_by_key` (which keeps the first element reaching the minimum)
/// reproduces the old `diags.iter().filter(...).min_by_key(...)` winner exactly.
struct DiagStartIndex {
    by_line: HashMap<u32, Vec<usize>>,
}

impl DiagStartIndex {
    fn build(diags: &[(&Diagnostic, PositionEncoding)]) -> Self {
        let mut by_line: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, (d, _)) in diags.iter().enumerate() {
            by_line.entry(d.range.start.line).or_default().push(i);
        }
        Self { by_line }
    }

    /// The merged-list indices of the diagnostics starting on `line`, in merged
    /// order (empty when none start there).
    fn on_line(&self, line: u32) -> std::slice::Iter<'_, usize> {
        self.by_line.get(&line).map_or([].iter(), |v| v.iter())
    }
}

/// A per-frame index for the underline surface ([`EditHost::diagnostics_for`]),
/// whose selection is span *intersection* (a diagnostic paints on every buffer
/// row in `[start.line, end.line]`), not just its start line. Single-line
/// diagnostics — the common case — bucket by their one line; the rare genuinely
/// multi-line ones live in a small overflow list scanned per row. Both halves
/// store merged-list indices in original order; [`DiagLineIndex::candidates_for`]
/// merges them back into ascending merged-list order, so the emitted span order
/// per row is identical to the old full-list `filter_map` scan.
struct DiagLineIndex {
    single: HashMap<u32, Vec<usize>>,
    multi: Vec<usize>,
}

impl DiagLineIndex {
    fn build(diags: &[(&Diagnostic, PositionEncoding)]) -> Self {
        let mut single: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut multi = Vec::new();
        for (i, (d, _)) in diags.iter().enumerate() {
            if d.range.start.line == d.range.end.line {
                single.entry(d.range.start.line).or_default().push(i);
            } else {
                multi.push(i);
            }
        }
        Self { single, multi }
    }

    /// The merged-list indices of the diagnostics intersecting buffer `row`, in
    /// ascending (= original merged) order. The original scan emitted spans in
    /// merged order, so the single-line bucket (already ordered) is merged with
    /// the row-covering multi-line entries by a single ordered pass.
    fn candidates_for(&self, row: u32) -> impl Iterator<Item = &usize> {
        // Restrict to multi-line diagnostics whose inclusive line range covers
        // `row`; `diag_row_span` still re-checks, so this is purely a prefilter.
        // Most frames have an empty `multi`, so this collapses to the single
        // bucket's iterator.
        let single = self.single.get(&row).map_or([].iter(), |v| v.iter());
        // No multi-line diagnostics: skip the merge entirely (the common path).
        if self.multi.is_empty() {
            return Either::Left(single);
        }
        // Every multi-line diagnostic is offered as a candidate for this row; the
        // caller's `diag_row_span` does the precise inclusive-range coverage test
        // (returning `None` for rows the range doesn't reach), exactly as the old
        // full-list scan relied on. Both halves are ascending, so the union sorts
        // back into the original merged order.
        let mut merged: Vec<&usize> = single.collect();
        merged.extend(self.multi.iter());
        merged.sort_unstable();
        Either::Right(merged.into_iter())
    }
}

/// A two-arm iterator so [`DiagLineIndex::candidates_for`] can return the cheap
/// borrowed single-bucket iterator on the common (no multi-line) path without
/// allocating, and a merged owned iterator only when multi-line diagnostics
/// exist.
enum Either<L, R> {
    Left(L),
    Right(R),
}

impl<'a, L, R> Iterator for Either<L, R>
where
    L: Iterator<Item = &'a usize>,
    R: Iterator<Item = &'a usize>,
{
    type Item = &'a usize;
    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Either::Left(l) => l.next(),
            Either::Right(r) => r.next(),
        }
    }
}

/// Format one diagnostic as the panel rows `vim.diagnostic.open_float` shows: a
/// header `E  source: <first message line> [code]` followed by any remaining
/// message lines verbatim. Every line is control-sanitized like the single-line
/// surfaces ([`first_line`]) — the message text is untrusted server output, and
/// the panel paints it.
fn diagnostic_float_lines(d: &Diagnostic) -> Vec<String> {
    let mut msg_lines = d
        .message
        .lines()
        .map(sanitize_control)
        .filter(|l| !l.trim().is_empty());
    let mut header = format!("{}  ", severity_short(severity_code(d.severity)));
    if let Some(src) = d.source.as_deref().filter(|s| !s.is_empty()) {
        header.push_str(&sanitize_control(src));
        header.push_str(": ");
    }
    header.push_str(&msg_lines.next().unwrap_or_default());
    if let Some(code) = diagnostic_code(d) {
        header.push_str(&format!(" [{code}]"));
    }
    let mut out = vec![header];
    out.extend(msg_lines);
    out
}

/// A diagnostic's `code` rendered for the float header (a number stringified, a
/// string sanitized), or `None` when the server attached none.
fn diagnostic_code(d: &Diagnostic) -> Option<String> {
    use nxvim_lsp::lsp_types::NumberOrString;
    match d.code.as_ref()? {
        NumberOrString::Number(n) => Some(n.to_string()),
        NumberOrString::String(s) => Some(sanitize_control(s)),
    }
}

/// Strip terminal control characters from one line of (untrusted) server text,
/// the per-line half of [`first_line`]'s sanitizing — so a float row carrying a
/// multi-line message can't smuggle an escape sequence to the terminal.
fn sanitize_control(line: &str) -> String {
    line.chars().filter(|c| !c.is_control()).collect()
}
