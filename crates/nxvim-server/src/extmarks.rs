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

use crate::EditHost;
use nxvim_core::BufferId;

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
        let mut out: Vec<HlInterval<'a>> = buf
            .extmarks
            .iter_all()
            .enumerate()
            .filter_map(|(i, m)| clip(m, base_order + i as u32))
            .collect();
        // The per-frame ephemeral marks a decoration provider placed this redraw
        // layer *above* the persistent set: continue the `order` past the
        // persistent marks so an ephemeral mark wins ties at equal priority.
        if let Some(eph) = self.ephemeral_extmarks.get(&buffer) {
            let off = base_order + buf.extmarks.iter_all().count() as u32;
            out.extend(
                eph.iter_all()
                    .enumerate()
                    .filter_map(|(j, m)| clip(m, off + j as u32)),
            );
        }
        out
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
