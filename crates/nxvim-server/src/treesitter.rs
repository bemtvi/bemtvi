//! Editor-side treesitter projection: query the editor's in-process engine for
//! the current buffer's viewport, memoize the result per `(changedtick,
//! viewport)`, and project cached spans into the redraw `highlights` payload.
//!
//! The parse tree and incremental reparse live **inside** the editor now
//! ([`nxvim_core::Editor`] owns a `SyntaxEngine`); there is no worker process,
//! no RPC, and no async catch-up frame — `editor.highlights()` returns spans
//! correct for the same frame as the edit. This module is just the slim cache +
//! the byte→screen-column projection that the redraw needs.

use crate::redraw::StyleTable;
use crate::Server;
use nxvim_core::{unicode, BufferId};
use rmpv::Value;
use std::collections::HashMap;

/// A cached highlight span in buffer coordinates: a byte range within a line.
#[derive(Clone)]
pub(crate) struct ByteSpan {
    start: usize,
    end: usize,
    group: String,
}

/// Per-buffer highlight memo: the spans last fetched from the engine, indexed by
/// absolute buffer line, plus the `(changedtick, first, last)` key they were
/// fetched for. A redraw that changed neither the text nor the viewport re-uses
/// the cache instead of re-running the query. This is all that survives the old
/// async `SyntaxState` — every `pending`/`opened`/coalescing field is gone now
/// that the engine answers synchronously.
#[derive(Default)]
pub(crate) struct SyntaxState {
    /// `(changedtick, first_line, last_line)` the cached spans were computed for,
    /// or `None` before the first fetch.
    key: Option<(u64, usize, usize)>,
    /// Latest spans from the engine, keyed by absolute buffer line.
    spans: HashMap<usize, Vec<ByteSpan>>,
}

impl Server {
    /// Refresh the current buffer's highlight memo from the editor's engine, if
    /// the content or viewport changed since the last fetch. Called from
    /// [`Server::redraw`] just before projecting, so the spans painted this frame
    /// reflect the keypress that triggered it.
    pub(crate) fn refresh_highlights(&mut self, height: usize) {
        // Forget memos for buffers the editor has since deleted.
        self.reap_closed_buffers();

        let buffer = self.editor.current_buffer_id();
        let line_count = self.editor.buffer().line_count();
        // Highlight a one-screen overscan above and below the viewport, so the
        // lines a scroll reveals are already cached and colored — no white flash
        // during the smooth-scroll animation (whose band spans up to ~2 screens).
        let first = self.editor.top.saturating_sub(height).min(line_count);
        let last = (self.editor.top + 2 * height).min(line_count);
        let key = (self.editor.buffer().changedtick, first, last);

        // Memo hit: the spans are already current for this content + viewport.
        if self.syntax_states.get(&buffer).and_then(|s| s.key) == Some(key) {
            return;
        }

        // Miss: re-query the engine (this also drains the buffer's edit journal
        // into the engine and reparses incrementally) and re-index by line.
        let spans = self.editor.highlights(buffer, first, last);
        let mut by_line: HashMap<usize, Vec<ByteSpan>> = HashMap::new();
        for s in spans {
            by_line.entry(s.line).or_default().push(ByteSpan {
                start: s.start_byte,
                end: s.end_byte,
                group: s.group,
            });
        }
        let state = self.syntax_states.entry(buffer).or_default();
        state.key = Some(key);
        state.spans = by_line;
    }

    /// Drop the highlight memo of every buffer the editor no longer has open
    /// (deleted via `:bdelete`). The engine's own parse state for those buffers
    /// is forgotten by the editor at deletion time.
    pub(crate) fn reap_closed_buffers(&mut self) {
        let live = self.editor.buffer_ids();
        self.syntax_states.retain(|id, _| live.contains(id));
    }

    /// Build a per-row `highlights` payload from a row→buffer-line mapping
    /// (`numbers`, 1-based, `None` for filler): each row's cached byte spans
    /// converted to **screen columns** (tab- and wide-char aware, like the
    /// selection), as `[start_col, end_col, group, style_id]`. `style_id` indexes
    /// into the per-frame `styles` palette when the span's capture resolves
    /// through the registry; it is `Nil` otherwise, so the client falls back to
    /// its built-in theme for that group. Used for both the static viewport and
    /// the scroll-animation band (which share `styles`).
    pub(crate) fn highlights_for(
        &self,
        buffer: BufferId,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        // Spans for this window's buffer (absent until its first refresh, or for
        // a buffer with no grammar). Two windows onto the same buffer share one
        // `SyntaxState`, each slicing its own rows.
        let spans_by_line = self.syntax_states.get(&buffer).map(|state| &state.spans);
        let buf = self.editor.buffer_of(buffer);
        let rows = numbers
            .iter()
            .map(|num| {
                let Some(n) = num else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let Some(b) = buf else {
                    return Value::Array(Vec::new());
                };
                let ts_spans = spans_by_line.and_then(|m| m.get(&line_idx));
                let text = b.line(line_idx);
                let tab = b.options.effective_tabstop();

                // Fast path: no extmarks on this line ⇒ emit the (already
                // non-overlapping) treesitter spans verbatim, byte-identical to
                // the pre-extmark projection.
                let ext = self.extmark_intervals(
                    buffer,
                    line_idx,
                    b.line_start(line_idx),
                    text.len(),
                    // treesitter spans take orders [0, n); extmarks sort above.
                    ts_spans.map_or(0, |s| s.len()) as u32,
                );
                if ext.is_empty() {
                    let Some(spans) = ts_spans else {
                        return Value::Array(Vec::new());
                    };
                    let row = spans
                        .iter()
                        .map(|s| {
                            let start = unicode::virtcol(&text, s.start, tab);
                            let end = unicode::virtcol(&text, s.end, tab);
                            let style_id = match self.editor.highlights.resolve_capture(&s.group) {
                                Some(style) => Value::from(styles.intern(style) as u64),
                                None => Value::Nil,
                            };
                            Value::Array(vec![
                                Value::from(start as u64),
                                Value::from(end as u64),
                                Value::from(s.group.as_str()),
                                style_id,
                            ])
                        })
                        .collect();
                    return Value::Array(row);
                }

                // Merge path: treesitter spans (priority TS_HL_PRIORITY) and the
                // line's extmarks, resolved into non-overlapping winning segments.
                let mut intervals = Vec::new();
                if let Some(spans) = ts_spans {
                    for (i, s) in spans.iter().enumerate() {
                        intervals.push(crate::extmarks::HlInterval {
                            start: s.start,
                            end: s.end,
                            group: s.group.as_str(),
                            priority: nxvim_core::TS_HL_PRIORITY,
                            order: i as u32,
                            capture: true,
                        });
                    }
                }
                intervals.extend(ext);
                let row = crate::extmarks::merge_intervals(&intervals)
                    .into_iter()
                    .map(|(sb, eb, group, capture)| {
                        let start = unicode::virtcol(&text, sb, tab);
                        let end = unicode::virtcol(&text, eb, tab);
                        let resolved = if capture {
                            self.editor.highlights.resolve_capture(group)
                        } else {
                            self.editor.highlights.resolve(group)
                        };
                        let style_id = match resolved {
                            Some(style) => Value::from(styles.intern(style) as u64),
                            None => Value::Nil,
                        };
                        Value::Array(vec![
                            Value::from(start as u64),
                            Value::from(end as u64),
                            Value::from(group),
                            style_id,
                        ])
                    })
                    .collect();
                Value::Array(row)
            })
            .collect();
        Value::Array(rows)
    }
}
