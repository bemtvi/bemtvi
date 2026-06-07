//! Editor-side treesitter sync: per-buffer parse/span bookkeeping, deciding
//! what to send the syntax worker each frame, ingesting its span replies, and
//! projecting cached spans into the redraw `highlights` payload.

use crate::filetype_of;
use crate::redraw::StyleTable;
use crate::syntax::SyntaxEvent;
use crate::Server;
use nxvim_core::{unicode, BufferId};
use nxvim_rpc::syntax::{encode_edits, EditWire, SpanWire};
use rmpv::Value;
use std::collections::HashMap;

/// A cached highlight span in buffer coordinates: a byte range within a line.
#[derive(Clone)]
pub(crate) struct ByteSpan {
    start: usize,
    end: usize,
    group: String,
}

/// Per-buffer treesitter sync bookkeeping. One of these per open buffer, keyed
/// by [`BufferId`] in [`Server::syntax_states`], so a buffer keeps its parse
/// state and span cache while another is in the window — switching back paints
/// instantly instead of re-parsing.
#[derive(Default)]
pub(crate) struct SyntaxState {
    /// Detected filetype/language, `None` when the buffer has no known grammar.
    language: Option<&'static str>,
    /// Has the worker been sent the full text (`ts_open`) for the current content?
    opened: bool,
    /// `changedtick` of the last `ts_open`/`ts_edit` we sent.
    last_tick: u64,
    /// A request is in flight; coalesce further edits until its reply lands.
    pending: bool,
    /// Last viewport `[first, last)` we requested, to detect scroll-only changes.
    last_view: (usize, usize),
    /// Latest spans from the worker, keyed by absolute buffer line.
    spans: HashMap<usize, Vec<ByteSpan>>,
}

impl Server {
    /// Handle a message from the syntax process. A restart forces a re-`open`;
    /// `ts_highlights` updates the span cache and repaints.
    pub(crate) fn on_syntax_event(&mut self, event: SyntaxEvent) {
        match event {
            SyntaxEvent::Restarted => {
                // A fresh worker holds no buffers, so every cached state is moot:
                // drop them all and let the next sync re-`open` the current buffer
                // (others re-open when next switched to).
                self.syntax_states.clear();
                self.syntax_dirty = true;
            }
            SyntaxEvent::Disabled => {
                // The supervisor gave up (worker won't spawn or keeps crashing).
                // Tell the user once — buffers stay editable, just un-highlighted.
                self.editor
                    .echo("treesitter: syntax worker unavailable, highlighting disabled");
                self.syntax_dirty = true;
            }
            // `ts_highlights` updates the cache; any other notification (e.g.
            // `ts_error` — a grammar that wouldn't load/parse) is ignored, so the
            // buffer simply stays un-highlighted and editing is unaffected.
            SyntaxEvent::Notification { method, params } if method == "ts_highlights" => {
                self.store_spans(&params);
                self.syntax_dirty = true;
            }
            SyntaxEvent::Notification { .. } => {}
        }
    }

    /// Decide what (if anything) to send the syntax process this frame for the
    /// *current* buffer: an `open` (first sync / resync / language change), an
    /// `edit` (text deltas), or a `view` (scroll only). Coalesces while a request
    /// is pending. Each buffer's state is keyed independently, so switching back
    /// to a buffer reuses its cached parse rather than re-opening.
    pub(crate) fn sync_syntax(&mut self, height: usize) {
        // Forget any buffers the editor has since deleted (frees worker memory).
        self.reap_closed_buffers();

        let buffer = self.editor.current_buffer_id();
        let language = filetype_of(self.editor.buffer().path.as_deref());
        // Language gone (no path / unknown extension): nothing to highlight.
        let Some(language) = language else {
            if let Some(state) = self.syntax_states.get_mut(&buffer) {
                state.language = None;
            }
            return;
        };
        self.syntax.ensure_started();

        // Work on this buffer's state as an owned local (so we can freely borrow
        // `self.editor` / `self.syntax` meanwhile), then put it back.
        let mut state = self.syntax_states.remove(&buffer).unwrap_or_default();
        let id = buffer.0;

        let line_count = self.editor.buffer().line_count();
        // Highlight a one-screen overscan above and below the viewport, so the
        // lines a scroll reveals are already cached and colored — no white flash
        // during the smooth-scroll animation (whose band spans up to ~2 screens).
        let first = self.editor.top.saturating_sub(height).min(line_count);
        let last = (self.editor.top + 2 * height).min(line_count);
        let tick = self.editor.buffer().changedtick;
        let language_changed = state.language != Some(language);
        state.language = Some(language);

        // A fresh language or un-opened buffer needs a full open.
        if language_changed || !state.opened {
            let _ = self.editor.buffer_mut().take_edits(); // superseded by full open
            let text = self.editor.buffer().text.to_string();
            self.syntax.open(id, tick, language, &text, first, last);
            state.opened = true;
            state.last_tick = tick;
            state.last_view = (first, last);
            state.pending = true;
        } else if tick != state.last_tick {
            // Text changed. Skip if a request is already in flight (the deltas
            // stay journaled and flush when its reply arrives).
            if !state.pending {
                let batch = self.editor.buffer_mut().take_edits();
                if batch.resync {
                    let text = self.editor.buffer().text.to_string();
                    self.syntax.open(id, tick, language, &text, first, last);
                } else {
                    self.syntax
                        .edit(id, tick, edits_value(&batch.edits), first, last);
                }
                state.last_tick = tick;
                state.last_view = (first, last);
                state.pending = true;
            }
        } else if (first, last) != state.last_view && !state.pending {
            // Text unchanged: re-query only if the viewport scrolled.
            self.syntax.view(id, first, last);
            state.last_view = (first, last);
            state.pending = true;
        }

        self.syntax_states.insert(buffer, state);
    }

    /// Send `ts_close` for, and drop the state of, every buffer the worker still
    /// tracks that the editor no longer has open (deleted via `:bdelete`).
    pub(crate) fn reap_closed_buffers(&mut self) {
        let live = self.editor.buffer_ids();
        let dead: Vec<BufferId> = self
            .syntax_states
            .keys()
            .copied()
            .filter(|id| !live.contains(id))
            .collect();
        for id in dead {
            self.syntax_states.remove(&id);
            self.syntax.close(id.0);
        }
    }

    /// Replace a buffer's span cache from its `ts_highlights` reply, routing by
    /// the reply's `buffer` id. A reply for an unknown buffer (e.g. one closed
    /// while the request was in flight) is dropped.
    pub(crate) fn store_spans(&mut self, params: &[Value]) {
        let Some(Value::Map(map)) = params.first() else {
            return;
        };
        let buffer = BufferId(u64_at(map, "buffer", 0));
        // The buffer the reply is for must still be open; its line count bounds
        // which line keys we accept, so a bogus `line` (e.g. `u64::MAX` from a
        // buggy/hostile worker) can't seed a junk entry that lives forever.
        let Some(line_count) = self.editor.line_count_of(buffer) else {
            return;
        };
        let Some(state) = self.syntax_states.get_mut(&buffer) else {
            return;
        };
        state.pending = false;
        let spans = map
            .iter()
            .find(|(k, _)| k.as_str() == Some("spans"))
            .and_then(|(_, v)| v.as_array());
        let mut cache: HashMap<usize, Vec<ByteSpan>> = HashMap::new();
        if let Some(spans) = spans {
            // Decode through the shared `SpanWire` so the wire tuple shape stays
            // in lockstep with the worker's encoder.
            for span in spans.iter().filter_map(SpanWire::decode) {
                if span.line >= line_count {
                    continue; // out-of-range line: never displayed, don't cache it
                }
                cache.entry(span.line).or_default().push(ByteSpan {
                    start: span.start_byte,
                    end: span.end_byte,
                    group: span.group,
                });
            }
        }
        state.spans = cache;
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
        buffer: nxvim_core::BufferId,
        numbers: &[Option<usize>],
        styles: &mut StyleTable,
    ) -> Value {
        // Spans for this window's buffer (absent until its first `ts_highlights`
        // reply lands, or for a buffer with no grammar). Two windows onto the
        // same buffer share one `SyntaxState`, each slicing its own rows.
        let spans_by_line = self.syntax_states.get(&buffer).map(|state| &state.spans);
        let buf = self.editor.buffer_of(buffer);
        let rows = numbers
            .iter()
            .map(|num| match num {
                Some(n) => {
                    let line_idx = n - 1;
                    let Some(spans) = spans_by_line.and_then(|m| m.get(&line_idx)) else {
                        return Value::Array(Vec::new());
                    };
                    let Some(text) = buf.map(|b| b.line(line_idx)) else {
                        return Value::Array(Vec::new());
                    };
                    let row = spans
                        .iter()
                        .map(|s| {
                            let start = unicode::virtcol(&text, s.start, unicode::TABSTOP);
                            let end = unicode::virtcol(&text, s.end, unicode::TABSTOP);
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
                    Value::Array(row)
                }
                None => Value::Array(Vec::new()),
            })
            .collect();
        Value::Array(rows)
    }
}

/// Encode buffer edit deltas for the `ts_edit` message: each is a 10-element
/// array `[start_byte, old_end_byte, new_end_byte, start_row, start_col,
/// old_end_row, old_end_col, new_end_row, new_end_col, text]`.
fn edits_value(edits: &[nxvim_core::BufferEdit]) -> Value {
    // Go through the shared `EditWire` so the wire tuple shape is defined once,
    // in `nxvim-rpc`, and can't drift from the worker's decoder.
    let wire: Vec<EditWire> = edits
        .iter()
        .map(|e| EditWire {
            start_byte: e.start_byte,
            old_end_byte: e.old_end_byte,
            new_end_byte: e.new_end_byte,
            start_point: e.start_point,
            old_end_point: e.old_end_point,
            new_end_point: e.new_end_point,
            text: e.text.clone(),
        })
        .collect();
    encode_edits(&wire)
}

/// Read a `u64` field from a msgpack map slice, falling back to `default` when
/// the key is absent or not an integer.
fn u64_at(map: &[(Value, Value)], key: &str, default: u64) -> u64 {
    map.iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or(default)
}
