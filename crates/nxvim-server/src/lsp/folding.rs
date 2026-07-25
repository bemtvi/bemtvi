//! LSP folding ranges: the whole-buffer `textDocument/foldingRange` result pushed
//! into the core fold engine as the **LSP fold source**.
//!
//! Unlike the cursor-anchored features, this is requested per *buffer* (on open
//! and after each change) — but only while the buffer's `foldmethod=expr` resolves
//! to the LSP foldexpr marker (`nx.lsp.foldexpr`) and its server advertises
//! `foldingRangeProvider`. The reply's line spans are pushed straight into core
//! (`Editor::set_lsp_folds`), which builds the same nested fold structure the
//! indent / tree-sitter sources produce (containment depth → per-line levels), so
//! the buffer folds identically regardless of where the ranges came from. The
//! request is stale-dropped on a content (`tick`) change, like semantic tokens and
//! inlay hints.

use nxvim_core::BufferId;
use nxvim_lsp::FoldRangeData;

use super::LspReqKind;
use crate::EditHost;

impl EditHost {
    /// Issue a `textDocument/foldingRange` request for `buffer`, if it wants LSP
    /// folds (`foldmethod=expr` + the LSP foldexpr marker) and its server is up,
    /// finished `initialize`, and advertised `foldingRangeProvider`. A no-op
    /// otherwise (the buffer simply isn't folded by LSP). Whole-buffer, so — like
    /// semantic tokens — it is fired on open and after each change, and the reply is
    /// stale-dropped on a content (`tick`) change.
    pub(crate) fn request_folding_range(&mut self, buffer: BufferId) {
        if !self.editor.buffer_wants_lsp_folds(buffer) {
            return;
        }
        let Some(state) = self.lsp_states.get(&buffer) else {
            return;
        };
        // Selected by capability, so a second server providing folds is used even
        // when the buffer's first one doesn't.
        let Some((key, _uri, _enc)) = self.lsp_target_for(buffer, LspReqKind::FoldingRange) else {
            return;
        };
        let Some(uri) = state.uri.clone() else {
            return;
        };
        let token = self.register_buffer_scoped_request(LspReqKind::FoldingRange, buffer, &key);
        self.fx
            .lsp_request(key, token, nxvim_lsp::LspRequest::FoldingRange { uri });
    }

    /// Issue a `foldingRange` request for the current buffer when it wants LSP folds
    /// but has no fresh result for its current `changedtick`, and one isn't already
    /// in flight for that buffer+tick. Driven from `redraw` (after `sync_lsp` has
    /// flushed `didChange`), so a request fires on both a content change (the tick
    /// moved) and a config change (`foldmethod`/`foldexpr` just set to the LSP
    /// source) — and retries across frames until the server is initialized. A no-op
    /// for any buffer that doesn't use the LSP fold source.
    pub(crate) fn maybe_request_folding_range(&mut self) {
        let buffer = self.editor.current_buffer_id();
        if !self.editor.needs_lsp_fold_request(buffer) {
            return;
        }
        // Don't pile up requests: skip while one for this buffer+tick is in flight
        // (a fresh reply clears the "needs request" condition by storing its result).
        let tick = self.editor.buffer_of(buffer).map_or(0, |b| b.changedtick);
        if self
            .lsp_buf_requests
            .values()
            .any(|p| p.kind == LspReqKind::FoldingRange && p.buffer == buffer && p.tick == tick)
        {
            return;
        }
        self.request_folding_range(buffer);
    }

    /// Push a `foldingRange` reply into the core fold engine for the buffer it was
    /// requested for. Each range is clamped to the buffer's line count and reduced
    /// to an inclusive `[start, end]` span (a degenerate single-line or inverted
    /// range is dropped — the fold model needs `end > start`). Dropped entirely when
    /// the buffer is gone or its content changed since the request (`req_tick`
    /// mismatch — a fresh request is already in flight against the newer text).
    pub(crate) fn on_folding_range_reply(
        &mut self,
        buffer: BufferId,
        req_tick: u64,
        folds: Vec<FoldRangeData>,
    ) {
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return;
        };
        if buf.changedtick != req_tick {
            return; // computed against superseded text; the newer request wins.
        }
        let last = buf.line_count().saturating_sub(1);
        let ranges: Vec<(usize, usize)> = folds
            .iter()
            .filter_map(|f| {
                let start = f.start as usize;
                let end = (f.end as usize).min(last);
                (end > start).then_some((start, end))
            })
            .collect();
        self.editor.set_lsp_folds(buffer, req_tick, ranges);
        self.lsp_dirty = true;
    }
}
