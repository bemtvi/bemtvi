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

/// Everything but the chunks of one `virt_text` placement: where it draws
/// (`pos`/`col`), how its highlight merges with what's under it (`hl_mode`), and
/// whether its chunks take their group's foreground only
/// ([`virt_text_fg_only`](nxvim_core::VirtDecor::virt_text_fg_only)).
struct Placement {
    pos: u64,
    col: u64,
    hl_mode: u64,
    fg_only: bool,
}

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

/// The byte range `[lo, hi)` that the visible rows span, or `None` when no row maps
/// to a buffer line.
///
/// Every per-frame bucketing pass below keys marks by their **anchor line**, so a
/// mark anchored outside the visible rows cannot contribute to the frame. Pruning on
/// the byte range costs an integer compare, whereas deriving a mark's line costs a
/// rope lookup (`byte_to_line`) — so with a few thousand marks and ~50 visible rows
/// this is the difference between thousands of rope lookups per frame and a handful.
/// It is what made a buffer full of git signs / inlay hints / line highlights cost
/// ~6x one full of plain highlight marks. See
/// `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
fn viewport_byte_range(
    buf: &nxvim_core::Buffer,
    segs: &[crate::redraw::RowSeg],
) -> Option<(usize, usize)> {
    let mut first = usize::MAX;
    let mut last = 0usize;
    for seg in segs {
        if let Some(n) = seg.line {
            let idx = n - 1;
            first = first.min(idx);
            last = last.max(idx);
        }
    }
    if first == usize::MAX {
        return None;
    }
    let lo = buf.line_start(first);
    // Exclusive at the start of the row after the last visible one, so an anchor on
    // the last visible line (including its trailing newline) is still inside.
    let hi = if last + 1 < buf.line_count() {
        buf.line_start(last + 1)
    } else {
        buf.len_bytes()
    };
    Some((lo, hi))
}

/// The buffer's highlight-bearing extmarks, prepared **once per frame** so the
/// per-row projection can find the marks touching its line without re-scanning the
/// whole store.
///
/// [`EditHost::extmark_intervals`] is called once per visible row, and used to scan
/// every mark in the buffer on each call — so one window cost O(rows x marks), about
/// 250 000 mark visits per frame with a few thousand marks. That was 77% of the
/// extmark cost of a keystroke; see
/// `docs/plans/2026-08-07-incremental-buffer-mirror.md`.
///
/// Only marks that can actually paint are kept — a `hl_group` *and* a range `end`,
/// which is exactly what the old per-mark clip required — each paired with its
/// `iter_all` enumerate index so the deterministic source-layering order the scan
/// produced is preserved byte-for-byte.
pub(crate) struct HlMarkIndex<'a> {
    /// `(enumerate order, start, end, mark)`, sorted by `start`.
    marks: Vec<(u32, usize, usize, &'a nxvim_core::Extmark)>,
    /// `max_end[i]` is the largest `end` among `marks[..=i]`, so a query can walk
    /// back from the last mark starting before the line and stop as soon as no
    /// earlier mark can still reach it. That is immediate for the usual
    /// non-overlapping mark set and stays correct for arbitrarily nested ranges,
    /// which a fixed look-back window would get wrong.
    max_end: Vec<usize>,
}

impl<'a> HlMarkIndex<'a> {
    pub(crate) fn build(buf: &'a nxvim_core::Buffer) -> Self {
        let mut marks: Vec<(u32, usize, usize, &'a nxvim_core::Extmark)> = buf
            .extmarks
            .iter_all()
            .enumerate()
            .filter_map(|(i, m)| {
                // Mirrors the clip's own guards: no group or no range ⇒ paints nothing.
                let end = m.end?;
                m.hl_group.as_deref()?;
                Some((i as u32, m.start, end, m))
            })
            .collect();
        marks.sort_by_key(|(order, start, _, _)| (*start, *order));
        let mut max_end = Vec::with_capacity(marks.len());
        let mut running = 0usize;
        for (_, _, end, _) in &marks {
            running = running.max(*end);
            max_end.push(running);
        }
        Self { marks, max_end }
    }

    /// Marks overlapping the byte range `[lo, hi)`, in the store's original order.
    fn overlapping(&self, lo: usize, hi: usize) -> Vec<(u32, &'a nxvim_core::Extmark)> {
        // Everything starting at or after `hi` is out; the rest is a prefix.
        let mut i = self.marks.partition_point(|(_, start, _, _)| *start < hi);
        let mut out = Vec::new();
        while i > 0 {
            i -= 1;
            if self.max_end[i] <= lo {
                break;
            }
            let (order, _, end, m) = self.marks[i];
            if end > lo {
                out.push((order, m));
            }
        }
        // Walking backwards found them newest-first; the clip below assigns orders
        // from the stored enumerate index, but callers merge on `order`, so restore
        // ascending order to keep the emitted interval list stable.
        out.reverse();
        out
    }
}

impl EditHost {
    /// Every hl_group extmark touching the line occupying byte range
    /// `[line_start, line_start + line_len)` in the rope, as line-relative
    /// intervals. Point marks (no `end`) and marks without an `hl_group` contribute
    /// nothing visible and are skipped. `base_order` offsets the per-mark `order` so
    /// extmarks sort above the treesitter spans they are merged with.
    ///
    /// `index` is the caller's per-frame [`HlMarkIndex`] — built once before the row
    /// loop rather than rebuilt here, since this runs once per visible row and used
    /// to re-scan the whole store each time.
    pub(crate) fn extmark_intervals<'a>(
        &'a self,
        index: &HlMarkIndex<'a>,
        line_start: usize,
        line_len: usize,
        base_order: u32,
    ) -> Vec<HlInterval<'a>> {
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
        index
            .overlapping(line_start, line_end)
            .into_iter()
            .filter_map(|(order, m)| clip(m, base_order + order))
            .collect()
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
        // Built once for the whole frame; see `HlMarkIndex`.
        let mark_index = HlMarkIndex::build(b);
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
                    &mark_index,
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
        // Bucket virt_text marks by their anchor buffer line (0-based), considering
        // only marks anchored in the visible range — see `viewport_byte_range`.
        let Some((lo, hi)) = viewport_byte_range(buf, segs) else {
            return nil_rows();
        };
        use std::collections::HashMap;
        let mut by_line: HashMap<usize, Vec<&nxvim_core::Extmark>> = HashMap::new();
        for m in buf.extmarks.iter_all() {
            if m.start < lo || m.start >= hi {
                continue;
            }
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
                            Placement {
                                pos,
                                col,
                                hl_mode: hl_mode_code(decor.hl_mode),
                                fg_only: decor.virt_text_fg_only,
                            },
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

    /// The **line-background** layer (neovim's `line_hl_group`, `hl_eol` semantics):
    /// per visible screen row whose buffer line carries a `line_hl_group` extmark,
    /// the pair `[row, style_id]` — the group resolved (with `winhighlight` remap)
    /// into this frame's `styles` palette. The client paints each row's background
    /// across the full text width *before* the text, the way `'cursorline'` does, so
    /// syntax spans / selection / search compose on top. Emitted on **both** builds
    /// (the marker is a core extmark; the doc-float renderer sets it): an empty array
    /// when no visible line carries one, keeping the wire shape stable.
    ///
    /// Wrapping is handled for free: [`RowSeg::line`](crate::redraw::RowSeg::line)
    /// maps each *screen* row to its buffer line, so every wrapped continuation row of
    /// a marked line gets the same background. Where several `line_hl_group` marks
    /// anchor on one line the highest [`priority`](nxvim_core::Extmark::priority) wins.
    pub(crate) fn line_bg_for(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        let empty = || Value::Array(Vec::new());
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return empty();
        };
        // Bucket line_hl_group marks by their anchor buffer line (0-based), keeping
        // the highest-priority group per line, and considering only marks anchored in
        // the visible range — see `viewport_byte_range`.
        let Some((lo, hi)) = viewport_byte_range(buf, segs) else {
            return empty();
        };
        use std::collections::HashMap;
        let mut by_line: HashMap<usize, (&str, u32)> = HashMap::new();
        for m in buf.extmarks.iter_all() {
            if m.start < lo || m.start >= hi {
                continue;
            }
            let Some(group) = m.decor.as_deref().and_then(|d| d.line_hl_group.as_deref()) else {
                continue;
            };
            let line = buf.byte_to_line(m.start);
            by_line
                .entry(line)
                .and_modify(|e| {
                    if m.priority >= e.1 {
                        *e = (group, m.priority);
                    }
                })
                .or_insert((group, m.priority));
        }
        // Treesitter line-background lines (a markdown fenced code block's
        // `@markup.raw.block`) tint the whole line the same way a `line_hl_group`
        // extmark does — the block background the winner-takes-cell syntax merge
        // otherwise drops on tokenized cells. A real `line_hl_group` extmark on the
        // same line already present above wins (it was set explicitly); these fill in
        // the rest at the treesitter priority.
        // Native-only: the `syntax_states` highlight memo (and its `block_bg_lines`)
        // exists only in the native build — on wasm, highlighting is a JS-side overlay,
        // so there is no such tree-sitter block background to fill in here.
        #[cfg(feature = "native")]
        if let Some(st) = self.syntax_states.get(&buffer) {
            for &line in &st.block_bg_lines {
                by_line
                    .entry(line)
                    .or_insert(("@markup.raw.block", nxvim_core::TS_HL_PRIORITY));
            }
        }
        if by_line.is_empty() {
            return empty();
        }
        let rows: Vec<Value> = segs
            .iter()
            .enumerate()
            .filter_map(|(row, seg)| {
                let group = by_line.get(&(seg.line? - 1))?.0;
                // A `line_hl_group` is a named highlight group (not a treesitter
                // capture), so resolve it directly — the same `resolve_winhl` the
                // extmark `hl_group` projection uses for its direct groups.
                let style = self.resolve_winhl(winhl, group)?;
                Some(Value::Array(vec![
                    Value::from(row as u64),
                    Value::from(styles.intern(style) as u64),
                ]))
            })
            .collect();
        Value::Array(rows)
    }

    /// Encode one virtual-text placement as `[pos, col, hl_mode, chunks]`, resolving
    /// each chunk's `hl_group` to a frame-palette style id (`Nil` when the group is
    /// absent or unresolved — the client then paints in normal colors).
    fn virt_placement_value(
        &self,
        at: Placement,
        chunks: &[VirtChunk],
        winhl: &WinHl,
        styles: &mut StyleTable,
    ) -> Value {
        Value::Array(vec![
            Value::from(at.pos),
            Value::from(at.col),
            Value::from(at.hl_mode),
            self.virt_chunks_value_fg(chunks, at.fg_only, winhl, styles),
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
        self.virt_chunks_value_fg(chunks, false, winhl, styles)
    }

    /// [`virt_chunks_value`](Self::virt_chunks_value) with the mark's
    /// [`virt_text_fg_only`](nxvim_core::VirtDecor::virt_text_fg_only): when set, each
    /// resolved style keeps its foreground (and its attributes) but drops `bg` /
    /// `reverse`, so the chunk paints as a glyph on whatever surface is under it
    /// instead of as a band of the group's own background. Resolved here rather than
    /// in the client so every front end gets it from one place — the wire still
    /// carries a plain interned style.
    fn virt_chunks_value_fg(
        &self,
        chunks: &[VirtChunk],
        fg_only: bool,
        winhl: &WinHl,
        styles: &mut StyleTable,
    ) -> Value {
        let chunks: Vec<Value> = chunks
            .iter()
            .map(|c| {
                let style_id = match c.hl_group.as_deref() {
                    Some(group) => match self.resolve_winhl(winhl, group) {
                        Some(mut style) => {
                            if fg_only {
                                style.bg = None;
                                style.reverse = false;
                            }
                            Value::from(styles.intern(style) as u64)
                        }
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
        virt_lines: &[Option<&[VirtChunk]>],
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
        // Bucket sign marks by their anchor buffer line (0-based), considering only
        // marks anchored in the visible range so the rope lookup is paid per visible
        // mark rather than per mark in the buffer.
        let Some((lo, hi)) = viewport_byte_range(buf, segs) else {
            return none_rows();
        };
        use std::collections::HashMap;
        let mut by_line: HashMap<usize, Vec<&nxvim_core::Extmark>> = HashMap::new();
        for m in buf.extmarks.iter_all() {
            if m.start < lo || m.start >= hi {
                continue;
            }
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
    /// Both sources project on both builds — the diagnostic render store
    /// (`diagnostics_signs_for`) is tick-driven and feature-agnostic, so the wasm
    /// build (LSP or `nx.diagnostic.set`) gets the gutter signs too.
    pub(crate) fn merged_sign_cells(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Vec<Option<Value>> {
        let ext = self.extmark_sign_cells(buffer, winhl, segs, styles);
        let diag: Vec<Value> = match self.diagnostics_signs_for(buffer, winhl, segs, styles) {
            Value::Array(a) => a,
            _ => segs.iter().map(|_| Value::Nil).collect(),
        };

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

    /// Augment the per-row `virt_text` payload with `line_fill` overlays: for each row
    /// whose line carries a `line_fill` mark, append an `Overlay` placement whose chunk
    /// is the fill text repeated to cover the window's text body (`text_width` cells —
    /// over-provisioned, the client clips it to the body). This reuses the existing
    /// `virt_text` wire so no client change is needed. The fill shows on the line's
    /// first display row only; the highest-priority `line_fill` mark on a line wins. A
    /// no-op (returns the input untouched) when no mark carries a fill.
    ///
    /// The overlay starts **past the line's own text** (its end-of-line screen column,
    /// `tabstop`-aware), not at column 0: on the blank line a plain rule sits on that
    /// is column 0 and the fill spans the row, while a line carrying a label keeps it
    /// and the fill runs out from it — the labelled rule (`─ pyright ─────`) the doc
    /// float heads each server's hover section with. Every client already pads an
    /// overlay anchored past end-of-text to its column, so this needs no client change.
    #[allow(clippy::too_many_arguments)] // the window's render facts, like its siblings
    pub(crate) fn apply_line_fill(
        &self,
        virt_text: Value,
        buffer: BufferId,
        segs: &[crate::redraw::RowSeg],
        text_width: usize,
        tabstop: usize,
        winhl: &WinHl,
        styles: &mut StyleTable,
    ) -> Value {
        if text_width == 0 {
            return virt_text;
        }
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return virt_text;
        };
        // The highest-priority line_fill mark per anchor line (ties → highest id).
        use std::collections::HashMap;
        let mut by_line: HashMap<usize, &nxvim_core::Extmark> = HashMap::new();
        for m in buf.extmarks.iter_all() {
            if m.decor.as_deref().is_some_and(|d| d.line_fill.is_some()) {
                by_line
                    .entry(buf.byte_to_line(m.start))
                    .and_modify(|best| {
                        if (m.priority, m.id) > (best.priority, best.id) {
                            *best = m;
                        }
                    })
                    .or_insert(m);
            }
        }
        if by_line.is_empty() {
            return virt_text;
        }
        let Value::Array(mut rows) = virt_text else {
            return virt_text;
        };
        for (i, seg) in segs.iter().enumerate() {
            let Some(n) = seg.line else { continue };
            if !seg.is_first() {
                continue;
            }
            let Some(m) = by_line.get(&(n - 1)) else {
                continue;
            };
            let fill = match m.decor.as_deref().and_then(|d| d.line_fill.as_ref()) {
                Some(f) if !f.text.is_empty() => f,
                _ => continue,
            };
            // Start past the line's own text so a label survives (column 0 on the blank
            // line a plain rule sits on), and repeat the fill text to cover the body
            // width (clipped client-side); a multi-cell glyph just over-provisions a
            // little.
            let line = buf.line(n - 1);
            let from = nxvim_core::unicode::virtcol(&line, line.len(), tabstop);
            let chunk = VirtChunk {
                text: fill.text.repeat(text_width.saturating_sub(from).max(1)),
                hl_group: fill.hl_group.clone(),
            };
            let placement = self.virt_placement_value(
                Placement {
                    pos: POS_OVERLAY,
                    col: from as u64,
                    hl_mode: 0,
                    fg_only: false,
                },
                &[chunk],
                winhl,
                styles,
            );
            if let Some(Value::Array(row)) = rows.get_mut(i) {
                row.push(placement);
            }
        }
        Value::Array(rows)
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
