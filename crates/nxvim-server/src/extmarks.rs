//! Extmark → redraw projection: clip each buffer's hl_group extmarks to a line
//! and priority-merge them with the treesitter highlight spans into a single
//! non-overlapping span list the client paints first-wins.
//!
//! The client ([`nxvim_tui`]'s `cell_style`) takes the **first** span covering a
//! cell and assumes spans don't overlap, so overlap resolution happens **here**:
//! [`merge_intervals`] paints intervals by `(priority, order)` and emits only the
//! winning segments. Treesitter highlights sit at
//! [`nxvim_core::TS_HL_PRIORITY`] (100); extmarks default to
//! [`nxvim_core::DEFAULT_PRIORITY`] (4096), so a plugin / semantic-token mark
//! wins over the base syntax color unless it asks for a lower priority.
//!
//! Unlike the treesitter spans (memoized per `(changedtick, viewport)` in
//! [`crate::treesitter`]), extmark spans are read **live** from the buffer's
//! [`ExtmarkStore`](nxvim_core::ExtmarkStore) every frame: the set is small and
//! always reflects the current marks, so there is no cache to stale.

use crate::redraw::StyleTable;
use crate::EditHost;
use nxvim_core::{unicode, BufferId, HlMode, VirtChunk, VirtTextPos, WinHl};
use rmpv::Value;

/// Placement `pos` tags on the `virt_text` wire array (mirror the client's
/// `VIRT_POS_*`). The shape `[pos, col, hl_mode, chunks]` is fixed across all
/// positions, so adding one is a server-emit + client-render change, never a
/// re-parse.
const POS_EOL: u64 = 0;
const POS_INLINE: u64 = 1;
const POS_OVERLAY: u64 = 2;
const POS_RIGHT_ALIGN: u64 = 3;
const POS_WIN_COL: u64 = 4;

/// One highlight interval over a single line, in **byte offsets within that
/// line**. `priority`/`order` decide who wins where intervals overlap (higher
/// `priority`, then higher `order`, paints on top). `capture` selects the style
/// resolver: treesitter spans are captures (`@`-fallback lookup), extmark groups
/// are resolved as direct highlight groups.
pub(crate) struct HlInterval<'a> {
    pub start: usize,
    pub end: usize,
    pub group: &'a str,
    pub priority: u32,
    pub order: u32,
    pub capture: bool,
}

impl EditHost {
    /// Every hl_group extmark of `buffer` clipped to line `line_idx` (whose
    /// content occupies byte range `[line_start, line_start + line_len)` in the
    /// rope), as line-relative intervals. Point marks (no `end`) and marks
    /// without an `hl_group` contribute nothing visible in v1 and are skipped.
    /// `base_order` offsets the per-mark `order` so extmarks sort above the
    /// treesitter spans they are merged with.
    pub(crate) fn extmark_intervals<'a>(
        &'a self,
        buffer: BufferId,
        line_idx: usize,
        line_start: usize,
        line_len: usize,
        base_order: u32,
    ) -> Vec<HlInterval<'a>> {
        let _ = line_idx;
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return Vec::new();
        };
        let line_end = line_start + line_len;
        // Clip one mark's byte range to this line's content; a multi-line mark
        // contributes its overlap with each line it crosses. Point marks and marks
        // with no `hl_group` render nothing and are dropped.
        let clip = move |m: &'a nxvim_core::Extmark, order: u32| -> Option<HlInterval<'a>> {
            let group = m.hl_group.as_deref()?;
            let end = m.end?.min(line_end);
            let start = m.start.max(line_start);
            // Lazy `.then(|| …)`, not `.then_some(…)`: when the mark doesn't touch
            // this line (`start >= end`) the byte subtractions below would
            // underflow, so they must not be evaluated.
            (start < end).then(|| HlInterval {
                start: start - line_start,
                end: end - line_start,
                group,
                priority: m.priority,
                order,
                capture: false,
            })
        };
        let out: Vec<HlInterval<'a>> = buf
            .extmarks
            .iter_all()
            .enumerate()
            .filter_map(|(i, m)| clip(m, base_order + i as u32))
            .collect();
        out
    }

    /// The browser (`not(feature = "native")`) twin of
    /// [`highlights_for`](crate::EditHost::highlights_for): the per-row highlight
    /// spans the wasm renderer paints as an *overlay* on top of its JS-side
    /// treesitter colors. The native projection bakes treesitter spans, semantic
    /// tokens, extmarks and control-char `SpecialKey` into one merged span list;
    /// the wasm build has no server-side treesitter (it highlights code JS-side) and
    /// substitutes control chars JS-side (`special_key`), so this projects only the
    /// genuinely server-sourced overlays — extmark highlights (the `nx.decor` /
    /// `nx.buf.set_extmark` layer) and LSP semantic tokens — which the JS side can't
    /// reproduce on its own.
    ///
    /// Returns the same `[[start, end, group, style_id], …]`-per-row shape as
    /// `highlights_for`, in display columns clipped/rebased to each row's wrap
    /// segment, so the renderer overlays them exactly like the native client paints
    /// them. A terminal window short-circuits to its vt100 palette spans (as
    /// `highlights_for` does), so a `:terminal` keeps its colors here too.
    #[cfg(not(feature = "native"))]
    pub(crate) fn overlay_highlights_for(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        let numbers: Vec<Option<usize>> = segs.iter().map(|s| s.line).collect();
        if let Some(term) = self.terminal_highlights(buffer, &numbers, styles) {
            return term;
        }
        let Some(b) = self.editor.buffer_of(buffer) else {
            return Value::Array(segs.iter().map(|_| Value::Array(Vec::new())).collect());
        };
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let text = b.line_cow(line_idx);
                let tab = b.options.effective_tabstop();

                // Semantic tokens sit above extmarks' base order, matching the native
                // layering (semantic at SEMANTIC_HL_PRIORITY, extmarks at their own
                // priority); no treesitter spans exist on this build, so base order is 0.
                let sem = self.semantic_intervals(buffer, line_idx, 0);
                let mut intervals = sem;
                intervals.extend(self.extmark_intervals(
                    buffer,
                    line_idx,
                    b.line_start(line_idx),
                    text.len(),
                    intervals.len() as u32,
                ));
                if intervals.is_empty() {
                    return Value::Array(Vec::new());
                }
                let mut vc = unicode::LineVirtcol::new(&text, tab);
                let row = merge_intervals(&intervals)
                    .into_iter()
                    .filter_map(|(sb, eb, group, capture)| {
                        let (start, end) = seg.clip(vc.at(sb), vc.at(eb))?;
                        let resolved = if capture {
                            self.resolve_capture_winhl(winhl, group)
                        } else {
                            self.resolve_winhl(winhl, group)
                        };
                        let style_id = match resolved {
                            Some(style) => Value::from(styles.intern(style) as u64),
                            None => Value::Nil,
                        };
                        Some(Value::Array(vec![
                            Value::from(start as u64),
                            Value::from(end as u64),
                            Value::from(group),
                            style_id,
                        ]))
                    })
                    .collect();
                Value::Array(row)
            })
            .collect();
        Value::Array(rows)
    }
}

impl EditHost {
    /// Build the per-row `virt_text` payload: for each visible row, the extmark
    /// virtual-text placements anchored on that buffer line, as an array of
    /// `[pos, col, hl_mode, [[text, style_id], …]]` (empty for rows with none).
    /// `pos` is `0`=eol / `1`=inline / `2`=overlay / `3`=right_align / `4`=win_col;
    /// `col` is the screen column the inline/overlay text anchors at (the fixed
    /// window column for win_col; `0` for eol/right_align). `hl_mode` is
    /// `0`=replace / `1`=combine / `2`=blend. Each chunk's `style_id` indexes the
    /// per-frame `styles` palette when its `hl_group` resolves (`Nil` otherwise, so
    /// the client paints in normal colors). Marks on a line are emitted in
    /// `(start byte, priority, id)` order so stacked virtual text is stable — for two
    /// marks at the same anchor that makes `priority` the tie-break, so a
    /// higher-priority mark draws later (to the right for concatenated eol text).
    ///
    /// `selection` is the focused window's per-row visual-selection spans (aligned
    /// with `numbers`); a mark with `virt_text_hide` set is **omitted** on any row
    /// the selection covers, matching neovim's "hide the virtual text when the
    /// background text is selected".
    ///
    /// Read live from the buffer's [`ExtmarkStore`](nxvim_core::ExtmarkStore), like
    /// the hl spans.
    pub(crate) fn virt_text_for(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        selection: &[Option<(usize, usize)>],
        styles: &mut StyleTable,
    ) -> Value {
        let nil_rows = || Value::Array(segs.iter().map(|_| Value::Nil).collect());
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return nil_rows();
        };
        // Bucket virt_text marks by their anchor buffer line (0-based). Cheap: the
        // mark set is small and scanned once per frame.
        use std::collections::HashMap;
        let mut by_line: HashMap<usize, Vec<&nxvim_core::Extmark>> = HashMap::new();
        for m in buf.extmarks.iter_all() {
            let Some(decor) = m.decor.as_deref() else {
                continue;
            };
            if decor.virt_text.is_empty() {
                continue;
            }
            by_line
                .entry(buf.byte_to_line(m.start))
                .or_default()
                .push(m);
        }
        if by_line.is_empty() {
            return Value::Array(segs.iter().map(|_| Value::Array(Vec::new())).collect());
        }
        let tabstop = buf.options.effective_tabstop();
        let rows = segs
            .iter()
            .enumerate()
            .map(|(row, seg)| {
                let Some(n) = seg.line else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let Some(marks) = by_line.get(&line_idx) else {
                    return Value::Array(Vec::new());
                };
                // `virt_text_hide`: when this row's background text is covered by the
                // visual selection, a mark that opted into hiding draws nothing.
                let row_selected = selection.get(row).copied().flatten().is_some();
                let mut marks = marks.clone();
                marks.sort_by_key(|m| (m.start, m.priority, m.id));
                // Inline placements need the line text + its start byte to map the
                // mark's byte anchor to a screen column (the same tab/wide-char
                // `virtcol` the inlay hints and hl spans use). Computed once per row.
                let line_start = buf.line_start(line_idx);
                let text = buf.line(line_idx);
                // Screen column of a mark's byte anchor within this line (the
                // tab/wide-char `virtcol` the inlay hints + hl spans share); used by
                // the inline and overlay positions.
                let anchor_col = |m: &nxvim_core::Extmark| -> u64 {
                    let byte_col = m.start.saturating_sub(line_start).min(text.len());
                    unicode::virtcol(&text, byte_col, tabstop) as u64
                };
                let placements: Vec<Value> = marks
                    .iter()
                    .filter_map(|m| {
                        // Every mark in `marks` has `decor` (the bucket only kept
                        // marks whose decor carries virt_text).
                        let decor = m.decor.as_deref().expect("virt_text mark has decor");
                        // Hide this mark on a selected row when it opted in.
                        if decor.virt_text_hide && row_selected {
                            return None;
                        }
                        // Place each position on the right soft-wrap segment, rebased
                        // to row-local columns: inline/overlay ride the row holding
                        // their anchor column; eol / right_align sit after the line's
                        // text, so on its last display row; a fixed win_col shows on
                        // the line's first row. A position that misses this row's
                        // segment is skipped (`None`).
                        let (pos, col) = match decor.virt_text_pos {
                            VirtTextPos::Eol => seg.is_last().then_some((POS_EOL, 0))?,
                            VirtTextPos::Inline => {
                                (POS_INLINE, seg.clip_col(anchor_col(m) as usize)? as u64)
                            }
                            VirtTextPos::Overlay => {
                                (POS_OVERLAY, seg.clip_col(anchor_col(m) as usize)? as u64)
                            }
                            VirtTextPos::RightAlign => {
                                seg.is_last().then_some((POS_RIGHT_ALIGN, 0))?
                            }
                            // A fixed window column, independent of the mark anchor;
                            // show it once, on the line's first display row.
                            VirtTextPos::WinCol(n) => {
                                seg.is_first().then_some((POS_WIN_COL, n as u64))?
                            }
                        };
                        Some(self.virt_placement_value(
                            pos,
                            col,
                            hl_mode_code(decor.hl_mode),
                            &decor.virt_text,
                            winhl,
                            styles,
                        ))
                    })
                    .collect();
                Value::Array(placements)
            })
            .collect();
        Value::Array(rows)
    }

    /// Encode one virtual-text placement as `[pos, col, hl_mode, chunks]`, resolving
    /// each chunk's `hl_group` to a frame-palette style id (`Nil` when the group is
    /// absent or unresolved — the client then paints in normal colors).
    fn virt_placement_value(
        &self,
        pos: u64,
        col: u64,
        hl_mode: u64,
        chunks: &[VirtChunk],
        winhl: &WinHl,
        styles: &mut StyleTable,
    ) -> Value {
        Value::Array(vec![
            Value::from(pos),
            Value::from(col),
            Value::from(hl_mode),
            self.virt_chunks_value(chunks, winhl, styles),
        ])
    }

    /// Resolve a chunk run to the wire form `[[text, style_id], …]`: each chunk's
    /// `hl_group` interned into the per-frame `styles` palette (`Nil` when absent or
    /// unresolved, so the client paints in normal colors). Shared by `virt_text` and
    /// `virt_lines`.
    pub(crate) fn virt_chunks_value(
        &self,
        chunks: &[VirtChunk],
        winhl: &WinHl,
        styles: &mut StyleTable,
    ) -> Value {
        let chunks: Vec<Value> = chunks
            .iter()
            .map(|c| {
                let style_id = match c.hl_group.as_deref() {
                    Some(group) => match self.resolve_winhl(winhl, group) {
                        Some(style) => Value::from(styles.intern(style) as u64),
                        None => Value::Nil,
                    },
                    None => Value::Nil,
                };
                Value::Array(vec![Value::from(c.text.as_str()), style_id])
            })
            .collect();
        Value::Array(chunks)
    }

    /// Build the per-row `virt_lines` payload from the view's interleaved layout:
    /// for each visible screen row, the chunk run `[[text, style_id], …]` when that
    /// row is a **virtual line** (a `RowKind::VirtLine` in the core layout), else
    /// `Nil`. Unlike `virt_text_for`, the *placement* (which rows are virtual, and in
    /// what order) is already decided in core — this only resolves the chunk styles.
    pub(crate) fn virt_lines_value(
        &self,
        virt_lines: &[Option<Vec<VirtChunk>>],
        winhl: &WinHl,
        styles: &mut StyleTable,
    ) -> Value {
        Value::Array(
            virt_lines
                .iter()
                .map(|row| match row {
                    Some(chunks) => self.virt_chunks_value(chunks, winhl, styles),
                    None => Value::Nil,
                })
                .collect(),
        )
    }

    /// Per visible row, the extmark gutter sign (`sign_text`) as the wire cell
    /// `[glyph, 0, style_id]` (the same shape diagnostic signs use; `0` is the
    /// non-diagnostic severity code) paired with the mark's `priority` for the
    /// cross-source merge, or `None` when the row carries no sign. A sign shows on
    /// the line's first display row only (like the number); the highest-priority
    /// `sign_text` mark on the line wins (ties → the most recent / highest id).
    /// `sign_hl_group` resolves to a frame style id via the shared `nx.decor`
    /// palette (`Nil` ⇒ the client paints the glyph in normal colors). Read live
    /// from the buffer's extmark store, like `virt_text_for`; shared by both builds.
    pub(crate) fn extmark_sign_cells(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Vec<Option<(Value, u32)>> {
        let none_rows = || segs.iter().map(|_| None).collect();
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return none_rows();
        };
        // Bucket sign marks by their anchor buffer line (0-based); cheap (small set).
        use std::collections::HashMap;
        let mut by_line: HashMap<usize, Vec<&nxvim_core::Extmark>> = HashMap::new();
        for m in buf.extmarks.iter_all() {
            if m.decor.as_deref().is_some_and(|d| d.sign_text.is_some()) {
                by_line
                    .entry(buf.byte_to_line(m.start))
                    .or_default()
                    .push(m);
            }
        }
        if by_line.is_empty() {
            return none_rows();
        }
        segs.iter()
            .map(|seg| {
                let n = seg.line?;
                // The number sits on the line's first display row only; so does its sign.
                if !seg.is_first() {
                    return None;
                }
                let marks = by_line.get(&(n - 1))?;
                let best = marks.iter().max_by_key(|m| (m.priority, m.id))?;
                let decor = best.decor.as_deref()?;
                let glyph = decor.sign_text.clone()?;
                let style_id = match decor
                    .sign_hl_group
                    .as_deref()
                    .and_then(|g| self.resolve_winhl(winhl, g))
                {
                    Some(style) => Value::from(styles.intern(style) as u64),
                    None => Value::Nil,
                };
                let cell = Value::Array(vec![Value::from(glyph), Value::from(0u64), style_id]);
                Some((cell, best.priority))
            })
            .collect()
    }

    /// The merged gutter sign per visible row — extmark `sign_text` marks combined
    /// with the LSP diagnostic signs into the single sign cell the column paints.
    /// Per row the highest-`priority` source wins; diagnostics sit at the fixed
    /// [`DIAGNOSTIC_SIGN_PRIORITY`] (so an explicit extmark sign at the default
    /// extmark priority shows over a diagnostic; a plugin that wants the diagnostic
    /// to win sets its mark below that). Returns `None` on a row with no sign.
    /// Diagnostics are native-only; extmark signs project on both builds.
    pub(crate) fn merged_sign_cells(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Vec<Option<Value>> {
        let ext = self.extmark_sign_cells(buffer, winhl, segs, styles);
        #[cfg(feature = "native")]
        let diag: Vec<Value> = match self.diagnostics_signs_for(buffer, winhl, segs, styles) {
            Value::Array(a) => a,
            _ => segs.iter().map(|_| Value::Nil).collect(),
        };
        #[cfg(not(feature = "native"))]
        let diag: Vec<Value> = segs.iter().map(|_| Value::Nil).collect();

        segs.iter()
            .enumerate()
            .map(|(i, _)| {
                let d = diag.get(i).filter(|v| !v.is_nil());
                let e = ext.get(i).and_then(Option::as_ref);
                match (d, e) {
                    (Some(dv), Some((ev, prio))) => Some(if *prio > DIAGNOSTIC_SIGN_PRIORITY {
                        ev.clone()
                    } else {
                        dv.clone()
                    }),
                    (Some(dv), None) => Some(dv.clone()),
                    (None, Some((ev, _))) => Some(ev.clone()),
                    (None, None) => None,
                }
            })
            .collect()
    }
}

/// The fixed `priority` a diagnostic gutter sign carries when it competes with an
/// extmark `sign_text` mark for the row's single sign cell. Below the default
/// extmark priority (4096) so an explicit plugin sign wins by default.
pub(crate) const DIAGNOSTIC_SIGN_PRIORITY: u32 = 10;

/// The per-row merged signs as the wire array (`[glyph, code, style_id]` or `Nil`),
/// the shape the `diagnostics_signs` redraw key has always carried.
pub(crate) fn signs_value(cells: &[Option<Value>]) -> Value {
    Value::Array(
        cells
            .iter()
            .map(|c| c.clone().unwrap_or(Value::Nil))
            .collect(),
    )
}

/// The rendered sign-column width in cells from the merged signs and the window's
/// `'signcolumn'` policy. One sign cell is 2 cells (vim); at most one sign per row
/// today, so the busiest visible row has 0 or 1 sign. `no` → 0; `auto` collapses to
/// 0 with no sign else `clamp(1, min, max)`; `yes` reserves its `min` even when
/// clean. Mirrors the old diagnostics-only `sign_width_for`, now sign-source-agnostic.
pub(crate) fn sign_width_from_cells(
    cells: &[Option<Value>],
    signcolumn: nxvim_core::SignColumn,
) -> u16 {
    use nxvim_core::SignColumn;
    let max_signs: u16 = u16::from(cells.iter().any(Option::is_some));
    let cols = match signcolumn {
        SignColumn::No => 0,
        SignColumn::Auto { min, max } => {
            if max_signs == 0 {
                0
            } else {
                max_signs.clamp(min, max)
            }
        }
        SignColumn::Yes { min, max } => max_signs.clamp(min, max),
    };
    cols * 2
}

/// The wire code for a [`HlMode`]: `0`=replace, `1`=combine, `2`=blend.
fn hl_mode_code(mode: HlMode) -> u64 {
    match mode {
        HlMode::Replace => 0,
        HlMode::Combine => 1,
        HlMode::Blend => 2,
    }
}

/// Resolve a set of possibly-overlapping highlight intervals into a
/// non-overlapping, byte-ascending segment list: each output segment is the
/// stretch where one interval wins (highest `(priority, order)` among those
/// covering it). Adjacent segments with the same winning group are coalesced.
/// Gaps (bytes no interval covers) are omitted, so the result is exactly the
/// painted spans — and never overlaps, satisfying the client's first-wins
/// contract.
pub(crate) fn merge_intervals<'a>(
    intervals: &[HlInterval<'a>],
) -> Vec<(usize, usize, &'a str, bool)> {
    if intervals.is_empty() {
        return Vec::new();
    }
    // Boundary points partition the line into segments over which the covering
    // set — and thus the winner — is constant.
    let mut points: Vec<usize> = Vec::with_capacity(intervals.len() * 2);
    for iv in intervals {
        points.push(iv.start);
        points.push(iv.end);
    }
    points.sort_unstable();
    points.dedup();

    let mut out: Vec<(usize, usize, &'a str, bool)> = Vec::new();
    for win in points.windows(2) {
        let (p, q) = (win[0], win[1]);
        if p >= q {
            continue;
        }
        // The interval covering [p, q) with the highest (priority, order) paints
        // this segment; ties favor the later-added interval (extmarks over the
        // treesitter spans they merge with).
        let winner = intervals
            .iter()
            .filter(|iv| iv.start <= p && iv.end >= q)
            .max_by_key(|iv| (iv.priority, iv.order));
        let Some(iv) = winner else {
            continue; // a gap between intervals
        };
        match out.last_mut() {
            Some(last) if last.1 == p && last.2 == iv.group && last.3 == iv.capture => last.1 = q,
            _ => out.push((p, q, iv.group, iv.capture)),
        }
    }
    out
}
