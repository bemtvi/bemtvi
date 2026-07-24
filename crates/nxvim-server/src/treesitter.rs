//! Editor-side treesitter projection: query the editor's in-process engine for
//! the current buffer's viewport, memoize the result per `(changedtick,
//! viewport)`, and project cached spans into the redraw `highlights` payload.
//!
//! The parse tree and incremental reparse live **inside** the editor now
//! ([`nxvim_core::Editor`] owns a `SyntaxEngine`); there is no worker process and
//! no RPC — `editor.highlights()` returns spans correct for the same frame as the
//! edit. This module is just the slim cache + the byte→screen-column projection
//! that the redraw needs.
//!
//! The one place that *does* span frames is a **large file**: the engine bounds each
//! parse to a per-frame deadline, so a file too big to parse in one frame is parsed
//! progressively — [`EditHost::refresh_highlights`] bypasses the memo while the
//! engine reports [`ts_parse_pending`](nxvim_core::Editor::ts_parse_pending), and the
//! redraw re-arms a short timer to resume on the next frame, until the parse
//! converges and the file colours in. Normal-sized files never take that path.

use crate::redraw::StyleTable;
use crate::EditHost;
use nxvim_core::{unicode, view::WindowView, BufferId, WinHl};
use rmpv::Value;
use std::collections::HashMap;

/// Language names from a query file's `; inherits: a,b,c` modeline(s) — the
/// languages whose same-named query this one builds on. Only the leading comment
/// block is scanned (the modeline is conventionally the first line); scanning stops
/// at the first non-comment line, so a stray `inherits:` deeper in the file is
/// ignored. Returns them in declared order; empty when there is no modeline.
fn parse_inherits(text: &str) -> Vec<String> {
    let mut langs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if !line.starts_with(';') {
            break; // past the leading comment block — modelines live only there
        }
        let body = line.trim_start_matches(';').trim();
        if let Some(rest) = body.strip_prefix("inherits:") {
            langs.extend(
                rest.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from),
            );
        }
    }
    langs
}

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
    /// `(changedtick, first_line, last_line, language)` the cached spans were
    /// computed for, or `None` before the first fetch. The language is part of
    /// the key so a filetype change that leaves the text untouched (`:set
    /// filetype=…`, `nx.bo.filetype` / `nx.bo.ts_highlight`) still invalidates the
    /// memo and re-queries the engine — those don't bump `changedtick`.
    key: Option<(u64, usize, usize, Option<String>)>,
    /// Latest spans from the engine, keyed by absolute buffer line.
    spans: HashMap<usize, Vec<ByteSpan>>,
    /// Absolute buffer lines a line-background capture (`@markup.raw.block`) covers,
    /// from the same engine query as `spans`. Projected by
    /// [`line_bg_for`](crate::EditHost::line_bg_for) into the `line_bg` layer so a
    /// markdown fenced code block reads as a solid region under the per-token syntax
    /// the winner-takes-cell merge otherwise paints over its background.
    pub(crate) block_bg_lines: std::collections::HashSet<usize>,
}

impl EditHost {
    /// Refresh the highlight memo for **every visible buffer** from the editor's
    /// engine, if its content or viewport changed since the last fetch. Called from
    /// [`EditHost::redraw`] just before projecting, so the spans painted this frame
    /// reflect the keypress that triggered it.
    ///
    /// Every window in the frame is serviced — not only the focused one — so a
    /// grabbing float (or any unfocused split) never leaves the buffer behind it
    /// dark. When a buffer is shown in two windows its query range is the union of
    /// both viewports, so one fetch serves both.
    pub(crate) fn refresh_highlights(&mut self, windows: &[WindowView]) {
        // Forget memos for buffers the editor has since deleted.
        self.reap_closed_buffers();

        // Union the line range each visible window needs onto its buffer. Each window
        // overscans a screen above and below its own viewport, so the lines a scroll
        // reveals are already cached and colored — no white flash during the
        // smooth-scroll animation (whose band spans up to ~2 screens).
        let mut ranges: HashMap<BufferId, (usize, usize)> = HashMap::new();
        for win in windows {
            let Some(line_count) = self.editor.line_count_of(win.buffer) else {
                continue;
            };
            let height = win.rect.height.saturating_sub(1); // minus the status row
            let top = self.editor.window_top(win.id);
            let first = top.saturating_sub(height).min(line_count);
            let last = (top + 2 * height).min(line_count);
            ranges
                .entry(win.buffer)
                .and_modify(|(f, l)| {
                    *f = (*f).min(first);
                    *l = (*l).max(last);
                })
                .or_insert((first, last));
        }

        for (buffer, (first, last)) in ranges {
            self.refresh_buffer_highlights(buffer, first, last);
        }
    }

    /// Refresh one buffer's highlight memo for the absolute line range `first..last`,
    /// re-querying the engine only on a memo miss (its content, the range, or its
    /// language changed since the last fetch).
    fn refresh_buffer_highlights(&mut self, buffer: BufferId, first: usize, last: usize) {
        // Buffer-open half of the query bridge: resolve this language's runtimepath
        // queries (`queries/` + `after/queries`, `;; extends`) onto the engine once,
        // before its first highlight. Guarded per-language, so it's a cheap no-op on
        // every later frame.
        if let Some(lang) = self.editor.ts_language_for(buffer) {
            self.resolve_runtimepath_queries(&lang);
        }
        let Some(changedtick) = self.editor.changedtick_of(buffer) else {
            return;
        };
        let key = (
            changedtick,
            first,
            last,
            self.editor.ts_language_for(buffer),
        );

        // While a large file's parse is still in flight, bypass the memo every frame:
        // the re-query below resumes the parse one budget further and picks up the
        // spans the growing tree now exposes. A memo hit here (the key is unchanged —
        // same text, same viewport) would freeze the buffer half-parsed and dark. Once
        // the parse converges `parse_pending` goes false and the memo takes over again.
        let pending = self.editor.ts_parse_pending(buffer);

        // Memo hit: the spans are already current for this content + viewport.
        if !pending && self.syntax_states.get(&buffer).and_then(|s| s.key.as_ref()) == Some(&key) {
            return;
        }

        // Miss: re-query the engine (this also drains the buffer's edit journal
        // into the engine and reparses incrementally) and re-index by line.
        let spans = self.editor.highlights(buffer, first, last);
        // Read the line-background lines this same query produced (markdown fenced
        // code blocks) — the engine stashed them during `highlights` above.
        let block_bg_lines = self
            .editor
            .line_background_lines(buffer)
            .into_iter()
            .collect();
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
        state.block_bg_lines = block_bg_lines;
    }

    /// Resolve a language's runtimepath treesitter queries once and push them to
    /// the engine — the buffer-open half of the query-resolution bridge (ADR 0001,
    /// bridge #4). For each engine query (`highlights` / `indents` / `injections`),
    /// [`collect_query_parts`](Self::collect_query_parts) gathers the engine's
    /// bundled base, every runtimepath `queries/<lang>/<name>.scm` and
    /// `after/queries/<lang>/<name>.scm`, **and** — following `; inherits:`
    /// modelines — the same for each inherited language, then installs the
    /// concatenation via
    /// [`set_resolved_ts_query`](nxvim_core::Editor::set_resolved_ts_query) (which
    /// keeps it only when it differs from the base — a no-op for an uncustomized
    /// language).
    ///
    /// This is what lets a config `queries/ecma/injections.scm` reach `javascript`
    /// (whose bundled `injections.scm` is just `; inherits: ecma,jsx`), and what
    /// merges `after/queries` `;; extends` overlays. Modeline comments (`;; extends`
    /// / `; inherits:`) are valid query comments, so they concatenate harmlessly.
    /// The full neovim precedence (exact replace-vs-extend ordering across the
    /// runtimepath) is approximated by pure additive concatenation — enough for the
    /// overlay and inherits cases configs actually ship. Guarded by
    /// `resolved_ts_langs` so resolution runs at most once per language, not per
    /// frame.
    fn resolve_runtimepath_queries(&mut self, lang: &str) {
        if !self.resolved_ts_langs.insert(lang.to_string()) {
            return; // already resolved this language
        }
        let rtp = self.lua.runtimepath().to_vec();
        let mut applied = false;
        for name in ["highlights", "indents", "injections", "textobjects"] {
            let mut visited = std::collections::HashSet::new();
            let parts = self.collect_query_parts(lang, name, &rtp, &mut visited);
            if parts.is_empty() {
                continue;
            }
            let merged = parts.join("\n");
            // Skip the install when the resolved text is just the engine's own base
            // (no inherits/overlay added anything) — the engine would no-op anyway,
            // but this also keeps the memo intact for the common uncustomized case.
            if self.editor.ts_base_query(lang, name).as_deref() == Some(merged.as_str()) {
                continue;
            }
            self.editor.set_resolved_ts_query(lang, name, Some(merged));
            applied = true;
        }
        // A new overlay changes what the engine paints / which layers it injects, so
        // drop the highlight memo: open buffers of this language re-query next frame.
        if applied {
            self.syntax_states.clear();
        }
    }

    /// Gather the query texts for `(lang, name)` in merge order — the engine's
    /// bundled base, then this language's runtimepath `queries/` and
    /// `after/queries/` files — and, for every `; inherits: a,b` modeline found in
    /// any of them, the same gathered for each inherited language **first** (so a
    /// language's own patterns override what it inherits). `visited` guards the
    /// inherit graph against cycles. Empty when nothing exists for the language.
    fn collect_query_parts(
        &self,
        lang: &str,
        name: &str,
        rtp: &[std::path::PathBuf],
        visited: &mut std::collections::HashSet<String>,
    ) -> Vec<String> {
        if !visited.insert(lang.to_string()) {
            return Vec::new(); // already pulled in via another inherit edge
        }
        // This language's own texts: bundled base, then runtimepath `queries/` and
        // `after/queries/` (each in runtimepath order).
        let mut own: Vec<String> = Vec::new();
        if let Some(base) = self.editor.ts_base_query(lang, name) {
            own.push(base);
        }
        for sub in ["queries", "after/queries"] {
            for dir in rtp {
                let path = dir.join(sub).join(lang).join(format!("{name}.scm"));
                if let Ok(text) = std::fs::read_to_string(&path) {
                    own.push(text);
                }
            }
        }
        // Inherited languages (from any `; inherits:` modeline) contribute first.
        let mut parts: Vec<String> = Vec::new();
        for inherited in own.iter().flat_map(|t| parse_inherits(t)) {
            parts.extend(self.collect_query_parts(&inherited, name, rtp, visited));
        }
        parts.extend(own);
        parts
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
        winhl: &WinHl,
        segs: &[crate::redraw::RowSeg],
        styles: &mut StyleTable,
    ) -> Value {
        // A terminal buffer's "syntax" is its vt100 grid colors, not treesitter:
        // project the per-cell fg/bg/attrs into the same span shape and skip the
        // tree query entirely (the buffer has no grammar anyway). Terminals never
        // wrap, so the segment clip is the identity — pass the row→line mapping.
        let numbers: Vec<Option<usize>> = segs.iter().map(|s| s.line).collect();
        if let Some(term) = self.terminal_highlights(buffer, &numbers, styles) {
            return term;
        }
        // Spans for this window's buffer (absent until its first refresh, or for
        // a buffer with no grammar). Two windows onto the same buffer share one
        // `SyntaxState`, each slicing its own rows.
        let spans_by_line = self.syntax_states.get(&buffer).map(|state| &state.spans);
        let buf = self.editor.buffer_of(buffer);
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let Some(b) = buf else {
                    return Value::Array(Vec::new());
                };
                let ts_spans = spans_by_line.and_then(|m| m.get(&line_idx));
                let text = b.line_cow(line_idx);
                let tab = b.options.effective_tabstop();
                let ts_len = ts_spans.map_or(0, |s| s.len()) as u32;

                // Unprintable control chars are shown as `^X` / `<xx>` tokens
                // (see `unicode::display_line`, applied to the wire text in
                // `lines_value`); overlay `SpecialKey` on those cells so the
                // substitution is visibly distinct from real text. Forces the
                // merge path even with no other highlight source.
                let control = unicode::unprintable_positions(&text);

                // LSP semantic tokens (ADR 0001 bridge #2): a third highlight
                // source at SEMANTIC_HL_PRIORITY, between treesitter and extmarks.
                // Built before the extmark scan so its count offsets the extmark
                // orders, keeping the source layering deterministic.
                let sem = self.semantic_intervals(buffer, line_idx, ts_len);

                // Fast path: no extmarks *and* no semantic tokens on this line ⇒
                // emit the (already non-overlapping) treesitter spans verbatim,
                // byte-identical to the pre-extmark projection.
                let ext = self.extmark_intervals(
                    buffer,
                    line_idx,
                    b.line_start(line_idx),
                    text.len(),
                    // treesitter spans take orders [0, n); semantic + extmarks above.
                    ts_len + sem.len() as u32,
                );
                if ext.is_empty() && sem.is_empty() && control.is_empty() {
                    let Some(spans) = ts_spans else {
                        return Value::Array(Vec::new());
                    };
                    let mut vc = unicode::LineVirtcol::new(&text, tab);
                    let row = spans
                        .iter()
                        .filter_map(|s| {
                            // Clip each full-line span to this row's wrap segment and
                            // rebase to row-local columns (so a wrapped continuation
                            // row paints only its slice, at the right column).
                            let (start, end) = seg.clip(vc.at(s.start), vc.at(s.end))?;
                            let style_id = match self.resolve_capture_winhl(winhl, &s.group) {
                                Some(style) => Value::from(styles.intern(style) as u64),
                                None => Value::Nil,
                            };
                            Some(Value::Array(vec![
                                Value::from(start as u64),
                                Value::from(end as u64),
                                Value::from(s.group.as_str()),
                                style_id,
                            ]))
                        })
                        .collect();
                    return Value::Array(row);
                }

                // Merge path: treesitter spans (priority TS_HL_PRIORITY), semantic
                // tokens (SEMANTIC_HL_PRIORITY), and the line's extmarks
                // (DEFAULT_PRIORITY), resolved into non-overlapping winning segments.
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
                intervals.extend(sem);
                intervals.extend(ext);
                // The control-char overlay wins over every other source (it isn't
                // real content), so it paints at the top priority.
                for &(sb, eb) in &control {
                    intervals.push(crate::extmarks::HlInterval {
                        start: sb,
                        end: eb,
                        group: "SpecialKey",
                        priority: nxvim_core::SPECIAL_KEY_PRIORITY,
                        order: 0,
                        capture: false,
                    });
                }
                let mut vc = unicode::LineVirtcol::new(&text, tab);
                let row = crate::extmarks::merge_intervals(&intervals)
                    .into_iter()
                    .filter_map(|(sb, eb, group, capture)| {
                        // Clip the merged span to this row's wrap segment, rebased.
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
