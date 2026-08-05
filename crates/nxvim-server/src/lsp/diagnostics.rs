//! Diagnostics: the per-buffer `publishDiagnostics` cache projected into the
//! redraw underline spans and the under-cursor message line, the
//! `:LspDiagnostics` location list, and `[d`/`]d` navigation.

use std::collections::HashMap;

use nxvim_core::extmark::DIAGNOSTIC_NS;
use nxvim_core::unicode;
use nxvim_core::Buffer;
use nxvim_core::WinHl;
use nxvim_lsp::lsp_types::Diagnostic;
use nxvim_lsp::PositionEncoding;
use rmpv::Value;

use super::*;
use crate::redraw::StyleTable;
use crate::EditHost;

/// One diagnostic to project, resolved to where it sits in the buffer **right now**.
///
/// A diagnostic arrives as an absolute LSP `(line, character)` range that is only
/// true of the document the server last saw. Every surface needs where it is *now*,
/// which is what `start`/`end` carry: byte offsets read from the diagnostic's
/// [`DIAGNOSTIC_NS`] anchor, so they track the edits made since. Byte offsets also
/// mean the per-server position encoding is resolved once, at anchor time, instead of
/// on every row of every frame.
pub(crate) struct TrackedDiagnostic<'a> {
    /// The diagnostic as published — its message, severity, `source`/`code`, and the
    /// original range (which a request quoting it back to its server must use, since
    /// that range is what the server's own document is indexed by).
    pub(crate) d: &'a Diagnostic,
    /// The server that published it, or `None` for a client-set
    /// (`vim.diagnostic.set`) one. Requests that quote diagnostics back must filter
    /// on this: a diagnostic is not portable between servers.
    pub(crate) server: Option<&'a ServerKey>,
    /// First byte covered, in the current buffer.
    pub(crate) start: usize,
    /// One past the last byte covered (`== start` for a zero-width diagnostic, which
    /// the surfaces widen to one cell so it is still visible).
    pub(crate) end: usize,
}

impl TrackedDiagnostic<'_> {
    /// `1`=error … `4`=hint.
    fn severity(&self) -> u8 {
        severity_code(self.d.severity)
    }

    /// The buffer row this diagnostic **starts** on — the line that owns its gutter
    /// sign and its one inline-message slot.
    fn start_line(&self, buf: &Buffer) -> usize {
        buf.byte_to_line(self.start)
    }

    /// The `[start, end)` **line-local** byte span this diagnostic occupies on buffer
    /// row `line_idx` (whose text is `line`), or `None` if it does not reach that row.
    /// A multi-line span is clipped to the row: `0` before its first line, the line
    /// length after its last.
    fn row_span(&self, buf: &Buffer, line_idx: usize, line: &str) -> Option<(usize, usize)> {
        let row_start = buf.byte_at(line_idx, 0);
        // End-inclusive at both edges: a span ending exactly at end-of-line still
        // belongs to that row, and a zero-width one resting there is still shown.
        let row_end = row_start + line.len();
        if self.end < row_start || self.start > row_end {
            return None;
        }
        let start = self.start.saturating_sub(row_start).min(line.len());
        let end = self.end.saturating_sub(row_start).min(line.len());
        Some((start, end.max(start)))
    }

    /// Whether byte offset `at` rests **on** this diagnostic — the "under the cursor"
    /// test. A zero-width diagnostic still covers the one cell it rests at.
    fn covers(&self, at: usize) -> bool {
        at >= self.start && at < self.end.max(self.start + 1)
    }
}

impl EditHost {
    /// Whether a diagnostic update landing *right now* is **held** instead of
    /// applied: insert mode is active, and the config asks for either a quiet gap
    /// first (`update_in_insert = <ms>`, the default) or nothing at all until
    /// `InsertLeave` (`update_in_insert = false`). A language server republishes
    /// after every `didChange` — i.e. after every keystroke — so applying each
    /// publish makes the squiggles, gutter signs and inline messages churn under the
    /// cursor while you are still mid-word, on errors that exist only because the
    /// line isn't finished.
    ///
    /// Held, not dropped: the newest update is parked
    /// ([`LspServerDoc::pending_diagnostics`] / [`EditHost::pending_client_diagnostics`])
    /// and folded in by [`commit_pending_diagnostics`](Self::commit_pending_diagnostics)
    /// — from the debounce timer once typing goes quiet
    /// ([`arm_diag_debounce`](Self::arm_diag_debounce)), and unconditionally on
    /// `InsertLeave`. What stays on screen meanwhile is the last applied set.
    ///
    /// Deviation from neovim, deliberate: neovim gates only the *display* handlers,
    /// leaving `vim.diagnostic.get` fresh mid-insert. nxvim holds the update one
    /// layer earlier — at the server-side store every surface reads — so the
    /// squiggles, the signs, the under-cursor message, the statusline counts,
    /// `]d` navigation and `:LspDiagnostics` all agree on one set instead of
    /// disagreeing for the duration of an insert. Client-set diagnostics
    /// (`vim.diagnostic.set`) keep their own Lua-side store, which is never held,
    /// so `vim.diagnostic.get` still reports those the moment they are set.
    pub(crate) fn diagnostics_paused(&self) -> bool {
        self.editor.mode.is_insert()
            && (!self.diag_config.update_in_insert || self.diag_config.insert_debounce_ms > 0)
    }

    /// Arm (re-arm) the one-shot that ends a pause while you are still typing.
    ///
    /// A true debounce: every held update replaces the pending timer, so a burst of
    /// keystrokes keeps pushing the deadline out and the display only catches up
    /// once typing goes quiet for `insert_debounce_ms`. Not armed when the pause has
    /// no time limit (`update_in_insert = false` — only `InsertLeave` ends that one)
    /// or no duration (`insert_debounce_ms == 0`, where nothing is ever held).
    ///
    /// Routed through [`apply_loop_op`](Self::apply_loop_op) rather than a native
    /// timer so the browser session debounces on the Worker's wheel identically.
    pub(crate) fn arm_diag_debounce(&mut self) {
        if !self.diag_config.update_in_insert || self.diag_config.insert_debounce_ms == 0 {
            return;
        }
        self.apply_loop_op(nxvim_lua::LoopOp::TimerStart {
            id: crate::DIAG_DEBOUNCE_TIMER_ID,
            delay_ms: self.diag_config.insert_debounce_ms,
            repeat_ms: 0,
        });
    }

    /// The debounce elapsed: typing has been quiet long enough, so apply what it
    /// held. Gated on `update_in_insert` because the setting can have flipped to
    /// "hold until `InsertLeave`" since the timer was armed — the already-scheduled
    /// wake must not then jump the gun (the `InsertLeave` commit still runs).
    pub(crate) fn on_diag_debounce(&mut self) {
        if self.diag_config.update_in_insert {
            self.commit_pending_diagnostics();
        }
    }

    /// Republish `buffer`'s whole LSP diagnostic set into the Lua mirror
    /// (`nx._diagnostics[bufnr]`, what the synchronous `vim.diagnostic.get` reads)
    /// and mark the frame dirty so the coalesced repaint picks the change up.
    ///
    /// The mirror is the buffer's set **merged across servers** — a per-server push
    /// would have `vim.diagnostic.get` report only whichever server published last —
    /// with each server's half tagged by its own `client_id`, so a reader can still
    /// tell the type-checker's errors from the linter's.
    pub(crate) fn push_diagnostics_mirror(&mut self, buffer: nxvim_core::BufferId) {
        let Some(state) = self.lsp_states.get(&buffer) else {
            return;
        };
        let all: Vec<DiagnosticData> = state
            .servers()
            .flat_map(|(k, d)| {
                let client_id = self.lsp_servers.get(k).map(|rt| rt.client_id);
                diagnostic_mirror_data(&d.diagnostics, client_id)
            })
            .collect();
        self.lsp_dirty = true;
        let _ = self.lua.set_diagnostics(buffer.0, &all);
    }

    /// Apply every diagnostic update parked while
    /// [`diagnostics_paused`](Self::diagnostics_paused) held — both the per-server
    /// publishes and the client-set (`vim.diagnostic.set`) store. Called when the
    /// debounce elapses, on the `InsertLeave` edge (before the autocmd fires, so a
    /// handler reading diagnostics sees the resumed set), and when the timing config
    /// changes mid-insert.
    ///
    /// A no-op — no mirror push, no repaint, no timer touched — when nothing was
    /// held, which is the case for every insert session during which nothing
    /// published.
    pub(crate) fn commit_pending_diagnostics(&mut self) {
        let mut committed = !self.pending_client_diagnostics.is_empty();
        let mut refreshed: Vec<nxvim_core::BufferId> = Vec::new();
        for (id, state) in self.lsp_states.iter_mut() {
            let mut changed = false;
            for (_, doc) in state.servers_mut() {
                if let Some(pending) = doc.pending_diagnostics.take() {
                    doc.diagnostics = pending;
                    changed = true;
                }
            }
            if changed {
                refreshed.push(*id);
            }
        }
        committed |= !refreshed.is_empty();
        for id in refreshed {
            self.push_diagnostics_mirror(id);
            self.refresh_diagnostic_marks(id);
        }

        // The client-set store's held writes. An empty held list is a *clear* (the
        // shape `SetClientDiagnostics` gives it), not "nothing held" — the map entry
        // is what records that something was held.
        if !self.pending_client_diagnostics.is_empty() {
            for (buffer, diags) in std::mem::take(&mut self.pending_client_diagnostics) {
                if diags.is_empty() {
                    self.client_diagnostics.remove(&buffer);
                } else {
                    self.client_diagnostics.insert(buffer, diags);
                }
                self.refresh_diagnostic_marks(buffer);
            }
            self.lsp_dirty = true;
        }

        // Nothing is held anymore, so disarm the debounce — an idle session must not
        // be woken for work that has already landed. Harmless when the commit *was*
        // the timer firing (a spent one-shot is already gone).
        if committed {
            self.apply_loop_op(nxvim_lua::LoopOp::TimerStop {
                id: crate::DIAG_DEBOUNCE_TIMER_ID,
            });
        }
    }

    /// Every attached server's diagnostics for `buffer`, each paired with the
    /// server that published it and **that server's** negotiated encoding.
    ///
    /// The pairing is the point: two servers on one buffer may have negotiated
    /// different encodings, so their `character` columns are not comparable and can
    /// only be converted per source. Callers must not flatten this into one encoding.
    fn lsp_diagnostics_of(
        &self,
        buffer: nxvim_core::BufferId,
    ) -> Vec<(&Diagnostic, Option<&ServerKey>, PositionEncoding)> {
        let Some(state) = self.lsp_states.get(&buffer) else {
            return Vec::new();
        };
        state
            .servers()
            .filter_map(|(key, doc)| {
                let encoding = self.lsp_servers.get(key)?.encoding;
                Some(
                    doc.diagnostics
                        .iter()
                        .map(move |d| (d, Some(key), encoding)),
                )
            })
            .flatten()
            .collect()
    }

    /// Every diagnostic to project for `buffer`, **in the canonical merged order**:
    /// each attached server's published set (servers in [`ServerKey`] order), then
    /// the client-set (`vim.diagnostic.set`) one. The order is not cosmetic — it is
    /// the key [`refresh_diagnostic_marks`](Self::refresh_diagnostic_marks) anchors
    /// against, so both must be derived from this one function.
    ///
    /// Unlike a per-server view this also yields client-set diagnostics for a buffer
    /// with *no attached server*, so the render surfaces aren't gated on an LSP.
    fn merged_sources(
        &self,
        buffer: nxvim_core::BufferId,
    ) -> Vec<(&Diagnostic, Option<&ServerKey>, PositionEncoding)> {
        let mut out = self.lsp_diagnostics_of(buffer);
        if let Some(diags) = self.client_diagnostics.get(&buffer) {
            // Client-set diagnostics have no server, so their columns are already
            // nxvim's native bytes — `Utf8` makes the shared conversion the identity.
            out.extend(diags.iter().map(|d| (d, None, PositionEncoding::Utf8)));
        }
        out
    }

    /// Every diagnostic to project for `buffer`, each resolved to the byte span it
    /// occupies **in the buffer as it is now** — the shape every render and
    /// cursor-anchored surface reads. Empty when the buffer has no diagnostics.
    ///
    /// Resolution goes through the [`DIAGNOSTIC_NS`] anchor placed when the set was
    /// applied, so a span follows the text you type around it instead of sitting at
    /// the absolute position a server published minutes (or one held update) ago.
    /// The published range is the fallback for anything unanchored — a buffer whose
    /// marks a destructive reload cleared, or a set the mark refresh hasn't caught up
    /// with — which is exactly the untracked behavior, never a wrong one: the count
    /// guard means a stale anchor set is ignored wholesale rather than read
    /// off-by-one.
    pub(crate) fn diagnostics_merged(
        &self,
        buffer: nxvim_core::BufferId,
    ) -> Vec<TrackedDiagnostic<'_>> {
        let sources = self.merged_sources(buffer);
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return Vec::new();
        };
        // The anchors are addressed by position in the merged list, so they are only
        // trustworthy while the list is the one they were placed from.
        let anchored = self.diag_mark_counts.get(&buffer) == Some(&sources.len());
        sources
            .into_iter()
            .enumerate()
            .map(|(i, (d, server, encoding))| {
                let span = anchored
                    .then(|| buf.extmarks.get(DIAGNOSTIC_NS, i as u64))
                    .flatten()
                    .map(|m| (m.start, m.end.unwrap_or(m.start)))
                    .unwrap_or_else(|| {
                        let r = lsp_range_to_bytes_in(buf, &d.range, encoding);
                        (r.start, r.end)
                    });
                TrackedDiagnostic {
                    d,
                    server,
                    start: span.0,
                    end: span.1.max(span.0),
                }
            })
            .collect()
    }

    /// [`EditHost::diagnostics_merged`] for the current buffer — the merged set the
    /// cursor-anchored surfaces (under-cursor message, `goto`, float, loclist) read.
    pub(crate) fn current_diagnostics_merged(&self) -> Vec<TrackedDiagnostic<'_>> {
        self.diagnostics_merged(self.editor.current_buffer_id())
    }

    /// (Re-)anchor `buffer`'s diagnostics: one [`DIAGNOSTIC_NS`] range extmark per
    /// entry of the merged set, addressed by its position in it. From here on the
    /// buffer's own edit choke point keeps every span correct as the user types —
    /// which is the whole reason diagnostics are stored as marks in neovim too.
    ///
    /// Must be called after **every** change to what the merged set contains — a
    /// publish applied, a held update committed, `vim.diagnostic.set`, a server
    /// detaching — because the anchors are addressed positionally. The recorded count
    /// is what makes a missed call safe rather than silently wrong: a merged list of a
    /// different length ignores the anchors and falls back to published ranges.
    pub(crate) fn refresh_diagnostic_marks(&mut self, buffer: nxvim_core::BufferId) {
        // Resolved against the *current* text before any mutation, so the spans are
        // the ones the marks should be placed at.
        let spans: Vec<(usize, usize)> = {
            let Some(buf) = self.editor.buffer_of(buffer) else {
                return;
            };
            self.merged_sources(buffer)
                .iter()
                .map(|(d, _, encoding)| {
                    let r = lsp_range_to_bytes_in(buf, &d.range, *encoding);
                    (r.start, r.end.max(r.start))
                })
                .collect()
        };
        let Some(buf) = self.editor.buffer_of_mut(buffer) else {
            return;
        };
        buf.extmarks.clear(DIAGNOSTIC_NS, None);
        for (i, (start, end)) in spans.iter().enumerate() {
            // Default gravity (start right, end left) — neovim's for diagnostics: text
            // typed *before* the span carries it along, text typed at its end doesn't
            // stretch it.
            buf.extmarks.set(
                DIAGNOSTIC_NS,
                Some(i as u64),
                *start,
                Some(*end),
                None,
                0,
                None,
            );
        }
        if spans.is_empty() {
            self.diag_mark_counts.remove(&buffer);
        } else {
            self.diag_mark_counts.insert(buffer, spans.len());
        }
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
        for t in self.diagnostics_merged(buffer) {
            let sev = t.severity(); // 1=error … 4=hint
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
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return Value::Array(segs.iter().map(|_| Value::Array(Vec::new())).collect());
        };
        // The LSP-pushed and client-set sets merged, each already resolved to where
        // it sits in the buffer *now* (its anchor's span, not its published range).
        let diags = self.diagnostics_merged(buffer);
        // Per-frame index built once instead of scanning the whole merged list per
        // row: each diagnostic intersects every buffer row its span crosses (exactly
        // when `row_span` returns `Some`). Single-line diagnostics — the overwhelming
        // majority — bucket by that one line; genuinely multi-line ones (rare) stay in
        // a small overflow list scanned per row. `candidates_for` merges the two back
        // into the original merged-list order, so the emitted span order per row is
        // identical to a full-list scan's.
        let index = DiagLineIndex::build(&diags, buf);
        // Tab width is the rendered window's buffer's `tabstop` (it may differ
        // from the current buffer's), so the underline columns line up with the
        // text the client paints for that window.
        let tabstop = buf.options.effective_tabstop();
        let rows = segs
            .iter()
            .map(|seg| {
                let Some(n) = seg.line else {
                    return Value::Array(Vec::new());
                };
                let line_idx = n - 1;
                let text = buf.line(line_idx);
                let spans = index
                    .candidates_for(line_idx as u32)
                    .filter_map(|&i| {
                        let t = &diags[i];
                        let (start_byte, end_byte) = t.row_span(buf, line_idx, &text)?;
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
                        let severity = t.severity();
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
        // Merged LSP + client-set, each at its tracked position; the inline message
        // positions by line only, so only the anchor's start line matters here.
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return Value::Array(segs.iter().map(|_| Value::Nil).collect());
        };
        let diags = self.diagnostics_merged(buffer);
        // Per-frame index of the diagnostics *starting* on each line, in merged
        // order, so `min_by_key` (which returns the first element reaching the
        // minimum) picks the same winner as a per-row `filter`/`min_by_key` would.
        let by_start = DiagStartIndex::build(&diags, buf);
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
                    .map(|&i| &diags[i])
                    .min_by_key(|t| (t.severity(), t.start));
                let Some(t) = best else {
                    return Value::Nil;
                };
                let severity = t.severity();
                let text = format!(
                    "{}{}",
                    self.diag_config.virt_prefix,
                    first_line(&t.d.message)
                );
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
        // Merged LSP + client-set at their tracked positions; the gutter sign
        // positions by line only.
        let Some(buf) = self.editor.buffer_of(buffer) else {
            return Value::Array(segs.iter().map(|_| Value::Nil).collect());
        };
        let diags = self.diagnostics_merged(buffer);
        // Same per-frame start-line index as the virtual-text surface: same
        // "starts on line" filter and same `min_by_key` tie-break.
        let by_start = DiagStartIndex::build(&diags, buf);
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
                    .map(|&i| &diags[i])
                    .min_by_key(|t| (t.severity(), t.start));
                let Some(t) = best else {
                    return Value::Nil;
                };
                let severity = t.severity();
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

    /// The message of the highest-severity diagnostic covering the cursor, for the
    /// message line (shown only when no other message is set, so `:messages` history
    /// stays clean). `None` when the cursor is on no diagnostic. Newlines are
    /// flattened so it fits one line.
    pub(crate) fn diagnostic_under_cursor(&self) -> Option<String> {
        let at = self
            .editor
            .buffer()
            .byte_at(self.editor.cursor.line, self.editor.cursor.col);
        self.current_diagnostics_merged()
            .into_iter()
            .filter(|t| t.covers(at))
            .min_by_key(|t| t.severity())
            .map(|t| first_line(&t.d.message))
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
        let buf = self.editor.buffer();
        items.sort_by_key(|t| t.start);
        let entries = items
            .into_iter()
            .map(|t| {
                let row = t.start_line(buf);
                let byte = t.start - buf.byte_at(row, 0);
                let severity = t.severity();
                let text = format!("{}: {}", severity_short(severity), first_line(&t.d.message),);
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
        let row = self.editor.cursor.line;
        let buf = self.editor.buffer();
        let mut items: Vec<TrackedDiagnostic> = self
            .current_diagnostics_merged()
            .into_iter()
            .filter(|t| t.start_line(buf) == row)
            .collect();
        items.sort_by_key(|t| (t.severity(), t.start));
        let lines = items
            .iter()
            .flat_map(|t| diagnostic_float_lines(t.d))
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
    /// when the buffer has no (matching) diagnostics. Targets each diagnostic's
    /// *tracked* start — where it sits after the edits since it was published — then
    /// `jump_to`s the current file so the move snaps to a valid resting cell (no
    /// file open — same buffer).
    pub(crate) fn diagnostic_goto(&mut self, forward: bool, severity: Option<u8>) {
        // Resolve every (matching) diagnostic to a 0-based (line, byte col) and
        // sort by position, so "next/previous from the cursor" is a list walk.
        let buf = self.editor.buffer();
        let mut positions: Vec<(usize, usize)> = self
            .current_diagnostics_merged()
            .into_iter()
            .filter(|t| severity.is_none_or(|s| t.severity() == s))
            .map(|t| {
                let row = t.start_line(buf);
                (row, t.start - buf.byte_at(row, 0))
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

    /// The diagnostics `server` published that **overlap** the 0-based,
    /// end-exclusive buffer range `(start_row, start_col, end_row, end_col)` — the
    /// `context.diagnostics` its code-action request carries, so a quickfix action
    /// offered over a selection is given the very diagnostics that selection covers.
    /// An empty range (a point at the cursor) reads as one byte wide, which is what
    /// makes the cursor case "the diagnostics under the cursor"; a zero-width
    /// *diagnostic* is likewise treated as one byte wide, so it can still be hit.
    ///
    /// Scoped to one server: the request quotes them straight back, so they must be
    /// that server's own — a diagnostic is not portable between servers (its columns
    /// are in its publisher's encoding, its `code`/`data` are that server's handles
    /// on the problem).
    ///
    /// Selection is by *tracked* span, so a selection covers the diagnostics that sit
    /// under it now; what is sent is the **published** range, which is what the
    /// server's own copy of the document is indexed by.
    pub(crate) fn diagnostics_in_range_from(
        &self,
        server: &ServerKey,
        (s_row, s_col, e_row, e_col): (usize, usize, usize, usize),
    ) -> Vec<Diagnostic> {
        let buffer = self.editor.buffer();
        let lo = buffer.byte_at(s_row, s_col);
        let hi = buffer.byte_at(e_row, e_col).max(lo);
        self.current_diagnostics_merged()
            .into_iter()
            .filter(|t| t.server == Some(server))
            .filter(|t| t.start < hi.max(lo + 1) && lo < t.end.max(t.start + 1))
            .map(|t| t.d.clone())
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
    fn build(diags: &[TrackedDiagnostic], buf: &Buffer) -> Self {
        let mut by_line: HashMap<u32, Vec<usize>> = HashMap::new();
        for (i, t) in diags.iter().enumerate() {
            by_line.entry(t.start_line(buf) as u32).or_default().push(i);
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
    fn build(diags: &[TrackedDiagnostic], buf: &Buffer) -> Self {
        let mut single: HashMap<u32, Vec<usize>> = HashMap::new();
        let mut multi = Vec::new();
        for (i, t) in diags.iter().enumerate() {
            let start = t.start_line(buf);
            if start == buf.byte_to_line(t.end) {
                single.entry(start as u32).or_default().push(i);
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
