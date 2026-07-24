//! LSP inlay hints: the whole-buffer `textDocument/inlayHint` result decoded and
//! projected as **inline** virtual text over the buffer's own glyphs.
//!
//! The request/decode/cache flow mirrors semantic tokens — an async, server-side
//! enrichment cached per-buffer, requested on change, stale-dropped on a content
//! (`tick`) change — but the projection lands in a sibling redraw key
//! (`inlay_hints`) the client paints *inline*: each hint's text is inserted at its
//! screen column, shifting the real glyphs (and the cursor) right, rather than
//! recoloring an existing cell (semantic tokens) or sitting at end-of-line
//! (diagnostic virtual text).
//!
//! Unlike semantic tokens, inlay hints are **opt-in**: a buffer carries no hints
//! until `vim.lsp.inlay_hint.enable(true)` flips its [`inlay_enabled`] flag.

use std::collections::BTreeMap;

use nxvim_core::unicode;
use nxvim_core::BufferId;
use nxvim_core::WinHl;
use nxvim_lsp::lsp_types::{Position, Range};
use nxvim_lsp::{InlayHintData, ServerKey};
use nxvim_lua::InlayHintMirrorData;
use rmpv::Value;

use super::{byte_col, InlayHintSpan, InlayHintsCache, InlayResolveTarget, LspReqKind};
use crate::redraw::StyleTable;
use crate::EditHost;

/// Flatten the decoded per-line [`InlayHintSpan`]s into the flat
/// [`InlayHintMirrorData`] list the `nx._inlay_hints` mirror holds (one entry per
/// hint, in line then column order), tagging each with the owning `client_id` — the
/// shape `vim.lsp.inlay_hint.get` returns. A still-unresolved lazy hint (empty
/// `text`) is omitted: it paints nothing and has no label to read yet.
fn inlay_mirror(cache: &InlayHintsCache, client_id: u64) -> Vec<InlayHintMirrorData> {
    cache
        .hints
        .iter()
        .flat_map(|(&line, spans)| {
            spans
                .iter()
                .filter(|s| !s.text.is_empty())
                .map(move |s| InlayHintMirrorData {
                    line: line as u32,
                    col: s.byte_col as u32,
                    label: s.text.clone(),
                    kind: s.kind,
                    client_id,
                })
        })
        .collect()
}

impl EditHost {
    /// Issue an inlay-hint request for `buffer`, if it has the feature enabled and
    /// its server is up, finished `initialize`, and advertised `inlayHintProvider`.
    /// A no-op otherwise (the buffer shows no hints). Whole-buffer, so — like
    /// semantic tokens — it is fired on enable and after each change, and the reply
    /// is stale-dropped on a content (`tick`) change, not a cursor move.
    pub(crate) fn request_inlay_hints(&mut self, buffer: BufferId) {
        let Some(state) = self.lsp_states.get(&buffer) else {
            return;
        };
        if !state.inlay_enabled {
            return;
        }
        let Some(key) = state.server.clone() else {
            return;
        };
        let Some(uri) = state.uri.clone() else {
            return;
        };
        let Some(rt) = self.lsp_servers.get(&key) else {
            return;
        };
        if !rt.inlay_hints {
            return;
        }
        // Whole-buffer range: `(0,0)` .. `(line_count, 0)`. A viewport-scoped range
        // is a Phase-2 follow-up (recorded as an approximation).
        let line_count = self.editor.buffer_of(buffer).map_or(0, |b| b.line_count());
        let range = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: line_count as u32,
                character: 0,
            },
        };
        let token = self.register_buffer_scoped_request(LspReqKind::InlayHints, buffer);
        self.fx
            .lsp_request(key, token, nxvim_lsp::LspRequest::InlayHint { uri, range });
    }

    /// Cache an `inlayHint` reply for the buffer it was requested for, decoding each
    /// hint's `character` against that buffer's server encoding into a line-local
    /// byte column and bucketing by line. Dropped when the buffer is gone, its
    /// server is unknown, or its content changed since the request (`req_tick`
    /// mismatch — a fresh request is already in flight against the newer text).
    pub(crate) fn on_inlay_hints_reply(
        &mut self,
        buffer: BufferId,
        req_tick: u64,
        hints: Vec<InlayHintData>,
    ) {
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return;
        };
        if buf.changedtick != req_tick {
            return; // computed against superseded text; the newer request wins.
        }
        let Some(state) = self.lsp_states.get(&buffer) else {
            return;
        };
        let Some(key) = state.server.as_ref() else {
            return;
        };
        let Some(rt) = self.lsp_servers.get(key) else {
            return;
        };
        let encoding = rt.encoding;
        let client_id = rt.client_id;
        let server_key = key.clone();

        let mut by_line: BTreeMap<usize, Vec<InlayHintSpan>> = BTreeMap::new();
        for hint in hints {
            let line_idx = hint.line as usize;
            if line_idx >= buf.line_count() {
                continue; // a hint past the buffer's editable lines: drop it.
            }
            // An eager hint paints its label; a *lazy* one (empty label but
            // resolvable — `resolve_data`) is kept as an empty placeholder and
            // resolved below. An empty label with no resolve data paints nothing.
            if hint.label.is_empty() && hint.resolve_data.is_none() {
                continue;
            }
            let text = buf.line(line_idx);
            let byte = byte_col(encoding, &text, hint.character as usize);
            by_line.entry(line_idx).or_default().push(InlayHintSpan {
                byte_col: byte,
                text: sanitize_label(&hint.label),
                kind: hint.kind,
                resolve_data: hint.resolve_data,
            });
        }
        // Keep each line's hints in column order so the client inserts them
        // left-to-right (and a multi-hint line paints deterministically). Resolves
        // are issued *after* the sort, so the `(line, idx)` each one records stays
        // valid against the cached vec the reply will index.
        for spans in by_line.values_mut() {
            spans.sort_by_key(|s| s.byte_col);
        }

        let state = self.lsp_states.get_mut(&buffer).expect("checked above");
        state.inlay.hints = by_line;
        // Push the read mirror (`nx._inlay_hints`) and then resolve any lazy
        // placeholders; a resolve reply refreshes the mirror again as it fills in.
        let mirror = inlay_mirror(&state.inlay, client_id);
        let _ = self.lua.set_inlay_hints(buffer.0, &mirror);
        self.issue_inlay_resolves(buffer, &server_key, req_tick);
        self.lsp_dirty = true;
    }

    /// Issue an `inlayHint/resolve` for every lazy placeholder cached for `buffer`,
    /// recording each in `inlay_resolves` so the reply fills the right span. Called
    /// straight after caching a reply; a no-op when no hint was lazy. The `tick`
    /// guards staleness — a resolve whose buffer changed before its reply lands is
    /// dropped (the whole cache was already replaced by the newer request).
    fn issue_inlay_resolves(&mut self, buffer: BufferId, server_key: &ServerKey, tick: u64) {
        // Collect (line, idx, hint-json) for every placeholder first — issuing
        // borrows `self.fx` mutably, so the cache read must finish beforehand.
        let mut jobs: Vec<(usize, usize, nxvim_lsp::serde_json::Value)> = Vec::new();
        if let Some(state) = self.lsp_states.get(&buffer) {
            for (&line, spans) in &state.inlay.hints {
                for (idx, span) in spans.iter().enumerate() {
                    if let Some(data) = &span.resolve_data {
                        jobs.push((line, idx, data.clone()));
                    }
                }
            }
        }
        for (line, idx, hint) in jobs {
            self.inlay_resolve_seq += 1;
            let cb_id = self.inlay_resolve_seq;
            self.inlay_resolves.insert(
                cb_id,
                InlayResolveTarget {
                    buffer,
                    tick,
                    line,
                    idx,
                },
            );
            let token = nxvim_lsp::ReqToken {
                kind: LspReqKind::ResolveInlayHint.as_u16(),
                generation: 0,
                cb_id,
            };
            self.fx.lsp_request(
                server_key.clone(),
                token,
                nxvim_lsp::LspRequest::ResolveInlayHint { hint },
            );
        }
    }

    /// Fill a lazy inlay hint's label from its `inlayHint/resolve` reply, routed by
    /// the `cb_id` the request carried (so concurrent resolves don't collide).
    /// Dropped if the target is unknown (a superseded resolve), the buffer is gone,
    /// its content changed since the request (`tick` mismatch — the placeholder was
    /// already replaced), or the resolved label is empty (nothing to paint). On
    /// success the span's `text` is filled and the `get` mirror refreshed.
    pub(crate) fn on_inlay_hint_resolved(&mut self, cb_id: u64, label: Option<String>) {
        let Some(target) = self.inlay_resolves.remove(&cb_id) else {
            return;
        };
        let Some(label) = label.filter(|l| !l.is_empty()) else {
            return;
        };
        let Some(buf) = self.editor.buffer_of(target.buffer) else {
            return;
        };
        if buf.changedtick != target.tick {
            return; // the placeholder belonged to a now-superseded reply.
        }
        let Some(state) = self.lsp_states.get(&target.buffer) else {
            return;
        };
        let client_id = state
            .server
            .as_ref()
            .and_then(|k| self.lsp_servers.get(k))
            .map(|rt| rt.client_id);
        let Some(client_id) = client_id else {
            return;
        };
        let state = self
            .lsp_states
            .get_mut(&target.buffer)
            .expect("checked above");
        let Some(span) = state
            .inlay
            .hints
            .get_mut(&target.line)
            .and_then(|spans| spans.get_mut(target.idx))
        else {
            return;
        };
        span.text = sanitize_label(&label);
        span.resolve_data = None;
        let mirror = inlay_mirror(&state.inlay, client_id);
        let _ = self.lua.set_inlay_hints(target.buffer.0, &mirror);
        self.lsp_dirty = true;
    }

    /// Build the per-row `inlay_hints` redraw payload from a row→buffer-line
    /// mapping (`numbers`, 1-based, `None` for filler): each visible row's inline
    /// hints as `[col, text, style_id]` in **screen columns**, sorted left to right.
    /// The LSP byte anchor is converted to a screen column with the same tab/
    /// wide-char `virtcol` the highlights and diagnostics use, so a hint lands
    /// between the right glyphs. `style_id` indexes the per-frame `styles` palette
    /// when the `LspInlayHint` group resolves (`Nil` otherwise, so the client falls
    /// back to a built-in dim foreground). An empty inner array for a row with no
    /// hints (or while the buffer has inlay hints disabled).
    pub(crate) fn inlay_hints_for(
        &self,
        buffer: BufferId,
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        let enabled = self
            .lsp_states
            .get(&buffer)
            .is_some_and(|s| s.inlay_enabled);
        let state = enabled.then(|| self.lsp_states.get(&buffer)).flatten();
        let Some(state) = state else {
            // One empty entry per row so the client's `inlay_hints[row]` index stays
            // aligned with `numbers`/`highlights`.
            return Value::Array(segs.iter().map(|_| Value::Array(Vec::new())).collect());
        };
        let buf = self.editor.buffer_of(buffer);
        let tabstop = buf
            .map(|b| b.options.effective_tabstop())
            .unwrap_or(unicode::TABSTOP);
        let style_id = match self.resolve_winhl(winhl, "LspInlayHint") {
            Some(style) => Value::from(styles.intern(style) as u64),
            None => Value::Nil,
        };
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let Some(spans) = state.inlay.hints.get(&line_idx) else {
                    return Value::Array(Vec::new());
                };
                let Some(text) = buf.map(|b| b.line(line_idx)) else {
                    return Value::Array(Vec::new());
                };
                let hints = spans
                    .iter()
                    // Skip an unresolved lazy placeholder (empty `text`): it has no
                    // label to paint yet — its `inlayHint/resolve` will fill it.
                    .filter(|s| !s.text.is_empty())
                    // Place the hint on the wrap segment that holds its anchor column,
                    // rebased to row-local columns (so it rides the right continuation
                    // row); a hint outside this row's segment is skipped.
                    .filter_map(|s| {
                        let col = unicode::virtcol(&text, s.byte_col.min(text.len()), tabstop);
                        let col = seg.clip_col(col)?;
                        Some(Value::Array(vec![
                            Value::from(col as u64),
                            Value::from(s.text.clone()),
                            style_id.clone(),
                        ]))
                    })
                    .collect();
                Value::Array(hints)
            })
            .collect();
        Value::Array(rows)
    }
}

/// Strip terminal control characters (and flatten newlines) from a hint label —
/// it is untrusted server text the client paints inline, so an escape sequence or
/// embedded newline must not reach the terminal or break the row.
fn sanitize_label(label: &str) -> String {
    label
        .chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}
