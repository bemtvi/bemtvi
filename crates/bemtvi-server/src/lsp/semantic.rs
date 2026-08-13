//! LSP semantic tokens: the whole-buffer `textDocument/semanticTokens/full`
//! result decoded and projected over the treesitter highlight floor (ADR 0001
//! bridge #2).
//!
//! The flow mirrors diagnostics — an async, server-side enrichment cached
//! per-buffer and projected into bemtvi's own highlight layer at the right
//! priority — but it lands in the *highlight* layer rather than a sibling redraw
//! key: the decoded spans become a third source of [`HlInterval`]s that
//! [`EditHost::highlights_for`](crate::treesitter) merges, at
//! [`SEMANTIC_HL_PRIORITY`](bemtvi_core::SEMANTIC_HL_PRIORITY) — above the
//! treesitter floor, below user extmarks. A token whose `@lsp.*` group does not
//! resolve to a style in the active theme is **dropped** from the projection, so
//! an undefined group never wins the merge and blanks the syntactic color
//! underneath (neovim sidesteps the same trap by not applying an undefined
//! hl_group).

use std::collections::BTreeMap;

use bemtvi_core::{Buffer, BufferId};
use bemtvi_lsp::lsp_types::{SemanticToken, SemanticTokensEdit};
use bemtvi_lsp::{PositionEncoding, SemanticLegend, SemanticTokensData, ServerKey};
use bemtvi_lua::SemanticTokenData;

use super::{byte_col, LspReqKind, SemanticSpan};
use crate::extmarks::HlInterval;
use crate::EditHost;

/// Flatten the decoded per-line [`SemanticSpan`]s into the flat
/// [`SemanticTokenData`] list the `btv._semantic_tokens` mirror holds (one entry
/// per token, in line then column order), tagging each with the owning
/// `client_id` — the shape `vim.lsp.semantic_tokens.get_at_pos` returns.
fn semantic_mirror(
    spans: &BTreeMap<usize, Vec<SemanticSpan>>,
    client_id: u64,
) -> Vec<SemanticTokenData> {
    spans
        .iter()
        .flat_map(|(&line, line_spans)| {
            line_spans.iter().map(move |s| SemanticTokenData {
                line: line as u32,
                start_col: s.start as u32,
                end_col: s.end as u32,
                token_type: s.ty.clone(),
                modifiers: s.mods.clone(),
                client_id,
            })
        })
        .collect()
}

impl EditHost {
    /// Issue a semantic-tokens request for `buffer` to **every** attached server
    /// that is up, finished `initialize`, and advertised a legend. A no-op when none
    /// has (the buffer keeps the treesitter floor alone). Whole-buffer, so — unlike
    /// the cursor-anchored features — it is fired on open and after each change, and
    /// each reply is stale-dropped on a content (`tick`) change, not a cursor move.
    ///
    /// Every capable server is asked because the caches are per server and the
    /// projection concatenates them: asking only the first would silently drop a
    /// second server's tokens, which is the whole `pyright` + `ruff` failure in
    /// miniature. Each request carries **that** server's own delta cursor — a
    /// `result_id` is meaningful only to the server that issued it.
    ///
    /// Once a server has cached a `result_id`, its request is `full/delta` (quoting
    /// it as `previousResultId`) so it can ship a diff; with no cached `result_id`
    /// (first request, or the server sent none) it is a whole `full` request.
    pub(crate) fn request_semantic_tokens(&mut self, buffer: BufferId) {
        // Disabled editor-wide, or this buffer was `stop`ped: nothing to fetch (the
        // projection is hidden anyway, and a later enable re-requests).
        if !self.semantic_tokens_enabled {
            return;
        }
        let Some(state) = self.lsp_states.get(&buffer) else {
            return;
        };
        if !state.semantic_on() {
            return;
        }
        let Some(uri) = state.uri.clone() else {
            return;
        };
        // Per server: its own cached `result_id` and its own delta support.
        let requests: Vec<(ServerKey, bemtvi_lsp::LspRequest)> = self
            .lsp_capable_servers(buffer, LspReqKind::SemanticTokens)
            .into_iter()
            .filter_map(|(key, _enc)| {
                let rt = self.lsp_servers.get(&key)?;
                // No legend ⇒ nothing to decode a reply against; skip this server.
                rt.legend.as_ref()?;
                let supports_delta = rt.semantic_tokens_delta;
                let cached = state.doc(&key).and_then(|d| d.semantic.result_id.clone());
                let uri = uri.clone();
                let request = match cached {
                    Some(previous_result_id) if supports_delta => {
                        bemtvi_lsp::LspRequest::SemanticTokensDelta {
                            uri,
                            previous_result_id,
                        }
                    }
                    _ => bemtvi_lsp::LspRequest::SemanticTokensFull { uri },
                };
                Some((key, request))
            })
            .collect();
        for (key, request) in requests {
            let token = self.register_multi_request(LspReqKind::SemanticTokens, buffer, &key);
            self.fx.lsp_request(key, token, request);
        }
    }

    /// Cache a `semanticTokens/full` or `full/delta` reply under the server that
    /// produced it, decoding the (possibly delta-patched) packed tokens against
    /// **that** server's legend + encoding into the per-line spans the projection
    /// reads. Dropped when the buffer is gone, the server is no longer attached or
    /// has no legend, or the content changed since the request (`req_tick` mismatch
    /// — a fresh request is already in flight, computed against the new text).
    ///
    /// `key` is the server the request was *sent* to, carried through the pending
    /// request rather than re-derived: the token-type indices are per-legend, so
    /// decoding one server's reply against another's legend paints plausible-looking
    /// nonsense rather than failing visibly.
    ///
    /// A [`SemanticTokensData::Full`] replaces that server's cached token set
    /// wholesale; a [`SemanticTokensData::Delta`] splices its edits into it and
    /// re-decodes. A delta that arrives with no cached base to patch (the
    /// `result_id` the request quoted is gone) can't be applied: the cache is
    /// cleared and a fresh `full` request issued, so the buffer recovers rather
    /// than painting against a phantom base.
    pub(crate) fn on_semantic_tokens_reply(
        &mut self,
        buffer: BufferId,
        req_tick: u64,
        key: ServerKey,
        data: SemanticTokensData,
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
        let Some(doc) = state.doc(&key) else {
            return;
        };
        let Some(rt) = self.lsp_servers.get(&key) else {
            return;
        };
        let Some(legend) = rt.legend.as_ref() else {
            return;
        };
        let encoding = rt.encoding;

        let (result_id, tokens) = match data {
            SemanticTokensData::Full { result_id, tokens } => (result_id, tokens),
            SemanticTokensData::Delta { result_id, edits } => {
                let base = &doc.semantic.tokens;
                // The request quoted a `result_id`, so the cache should hold its
                // base. If it doesn't (no base to patch), the delta is unusable:
                // clear and re-request a full set rather than splice into nothing.
                if base.is_empty() && !edits.is_empty() {
                    let state = self.lsp_states.get_mut(&buffer).expect("checked above");
                    if let Some(doc) = state.doc_mut(&key) {
                        doc.semantic = Default::default();
                    }
                    self.lsp_dirty = true;
                    self.request_semantic_tokens(buffer);
                    return;
                }
                (result_id, apply_token_edits(base, &edits))
            }
        };

        let spans = decode_tokens(&tokens, legend, encoding, buf);
        let state = self.lsp_states.get_mut(&buffer).expect("checked above");
        if let Some(doc) = state.doc_mut(&key) {
            doc.semantic.result_id = result_id;
            doc.semantic.tokens = tokens;
            doc.semantic.spans = spans;
        }
        // Re-mirror the buffer's tokens into `btv._semantic_tokens[bufnr]` so the
        // synchronous `vim.lsp.semantic_tokens.get_at_pos` can read them from pure
        // Lua (the diagnostics-mirror analogue). Rebuilt across ALL servers, not
        // just the one that answered: the mirror is one flat list per buffer, so
        // pushing only this reply's tokens would erase the other server's.
        self.push_semantic_mirror(buffer);
        self.lsp_dirty = true;
    }

    /// Rebuild `btv._semantic_tokens[bufnr]` from every attached server's cache, each
    /// entry tagged with its producing `client_id`, in line-then-column order.
    pub(crate) fn push_semantic_mirror(&mut self, buffer: BufferId) {
        let Some(state) = self.lsp_states.get(&buffer) else {
            return;
        };
        let mut mirror: Vec<SemanticTokenData> = state
            .servers()
            .filter_map(|(key, doc)| {
                let client_id = self.lsp_servers.get(key)?.client_id;
                Some(semantic_mirror(&doc.semantic.spans, client_id))
            })
            .flatten()
            .collect();
        mirror.sort_by_key(|t| (t.line, t.start_col, t.end_col, t.client_id));
        let _ = self.lua.set_semantic_tokens(buffer.0, &mirror);
    }

    /// The semantic-token highlight intervals on buffer line `line_idx`, ready to
    /// merge with the treesitter / extmark intervals in
    /// [`highlights_for`](crate::treesitter). Each cached token paints its
    /// most-specific candidate group that resolves to a style in the active theme;
    /// a token whose candidates all fail to resolve is **omitted**, so it never
    /// overrides the treesitter span beneath. `base_order` offsets the per-token
    /// `order` so these sort deterministically among themselves (priority already
    /// separates them from the other sources).
    pub(crate) fn semantic_intervals(
        &self,
        buffer: BufferId,
        line_idx: usize,
        base_order: u32,
    ) -> Vec<HlInterval<'_>> {
        // The editor-wide gate (`vim.lsp.semantic_tokens.enable(false)`) hides the
        // paint everywhere without dropping any cache.
        if !self.semantic_tokens_enabled {
            return Vec::new();
        }
        let Some(state) = self.lsp_states.get(&buffer) else {
            return Vec::new();
        };
        // A `vim.lsp.semantic_tokens.stop(buf)` hides this buffer's paint (the cache
        // survives, so a later `start` repaints from it without a round-trip).
        if !state.semantic_on() {
            return Vec::new();
        }
        // Read across every attached server: each caches the tokens it published, so
        // a buffer served by two of them paints both sets. Sorted left to right so
        // the merged intervals' `order` is a stable function of the text rather than
        // of which server answered first.
        let mut spans: Vec<&SemanticSpan> = state
            .servers()
            .filter_map(|(_, d)| d.semantic.spans.get(&line_idx))
            .flatten()
            .collect();
        if spans.is_empty() {
            return Vec::new();
        }
        spans.sort_by_key(|s| (s.start, s.end));
        spans
            .iter()
            .enumerate()
            .filter_map(|(i, s)| {
                // First candidate (most specific) that resolves to a real style
                // wins; none resolving ⇒ drop the token (treesitter shows through).
                let group = s
                    .groups
                    .iter()
                    .find(|g| self.editor.highlights.resolve_capture(g).is_some())?;
                Some(HlInterval {
                    start: s.start,
                    end: s.end,
                    group: group.as_str(),
                    priority: bemtvi_core::SEMANTIC_HL_PRIORITY,
                    order: base_order + i as u32,
                    capture: true,
                })
            })
            .collect()
    }
}

/// Apply a `full/delta` reply's edits to the previously cached token set,
/// returning the new set. The protocol's edits index the *flat integer* array
/// (each [`SemanticToken`] is five integers); an edit replaces
/// `[start, start+delete_count)` of that flat array with its `data`. Edits are
/// applied in ascending `start` order, rebuilding the array segment by segment so
/// earlier edits never shift the indices of later ones (the same scheme neovim's
/// `semantic_tokens.lua` uses). The result is re-chunked back into tokens.
fn apply_token_edits(base: &[SemanticToken], edits: &[SemanticTokensEdit]) -> Vec<SemanticToken> {
    let old = flatten_tokens(base);
    let mut sorted: Vec<&SemanticTokensEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start);

    let mut out: Vec<u32> = Vec::new();
    let mut idx = 0usize;
    for edit in sorted {
        let start = (edit.start as usize).min(old.len());
        // Copy the untouched run before this edit, then its replacement data.
        if idx < start {
            out.extend_from_slice(&old[idx..start]);
        }
        if let Some(data) = &edit.data {
            out.extend(flatten_tokens(data));
        }
        // `delete_count` is an untrusted server `u32`; saturate the add so a huge
        // value can't overflow `usize` on a 32-bit (wasm) target before the `.min`.
        idx = start
            .saturating_add(edit.delete_count as usize)
            .min(old.len());
    }
    out.extend_from_slice(&old[idx..]);
    chunk_tokens(&out)
}

/// Flatten tokens to the protocol's packed integer array (five per token).
fn flatten_tokens(tokens: &[SemanticToken]) -> Vec<u32> {
    let mut out = Vec::with_capacity(tokens.len() * 5);
    for t in tokens {
        out.extend_from_slice(&[
            t.delta_line,
            t.delta_start,
            t.length,
            t.token_type,
            t.token_modifiers_bitset,
        ]);
    }
    out
}

/// Re-chunk a packed integer array into tokens (five per token). A trailing
/// remainder shorter than five integers is dropped — a malformed splice can't
/// describe a partial token, so the unfinished tail is discarded rather than
/// fabricated.
fn chunk_tokens(data: &[u32]) -> Vec<SemanticToken> {
    data.chunks_exact(5)
        .map(|c| SemanticToken {
            delta_line: c[0],
            delta_start: c[1],
            length: c[2],
            token_type: c[3],
            token_modifiers_bitset: c[4],
        })
        .collect()
}

/// Decode a packed `semanticTokens/full` token array into per-line highlight
/// spans. The protocol encodes each token as deltas from the previous one
/// (`deltaLine`, `deltaStart`, `length`, `tokenType`, `tokenModifiers`); this
/// walks them to absolute `(line, startChar, length)`, converts the char offsets
/// to line-local bytes through the negotiated `encoding`, resolves each token's
/// candidate `@lsp.*` capture names from the `legend`, and buckets the result by
/// line. Tokens are single-line (we advertise no multiline support); one whose
/// `tokenType` is outside the legend is skipped (unclassifiable).
fn decode_tokens(
    tokens: &[SemanticToken],
    legend: &SemanticLegend,
    encoding: PositionEncoding,
    buffer: &Buffer,
) -> BTreeMap<usize, Vec<SemanticSpan>> {
    let mut spans: BTreeMap<usize, Vec<SemanticSpan>> = BTreeMap::new();
    let mut line: u32 = 0;
    let mut start_char: u32 = 0;
    for tok in tokens {
        // Absolute position: a non-zero deltaLine moves down and resets the
        // column to deltaStart; a zero deltaLine advances the column on the line.
        // The deltas are untrusted server `u32`s, so accumulate with saturating
        // adds: a malformed reply (a near-`u32::MAX` delta) must not overflow-panic
        // the server thread (a one-line-DoS, like the inverted-range clamp in
        // [`lsp_range_to_bytes_in`](super::lsp_range_to_bytes_in)). A saturated value
        // lands far past end-of-buffer and is dropped by the bounds checks below.
        if tok.delta_line > 0 {
            line = line.saturating_add(tok.delta_line);
            start_char = tok.delta_start;
        } else {
            start_char = start_char.saturating_add(tok.delta_start);
        }
        if tok.length == 0 {
            continue;
        }
        let line_idx = line as usize;
        if line_idx >= buffer.line_count() {
            continue;
        }
        let text = buffer.line(line_idx);
        let start = byte_col(encoding, &text, start_char as usize);
        let end = byte_col(
            encoding,
            &text,
            start_char.saturating_add(tok.length) as usize,
        );
        if end <= start {
            continue;
        }
        let Some((ty, mods, groups)) = classify(tok, legend) else {
            continue;
        };
        spans.entry(line_idx).or_default().push(SemanticSpan {
            start,
            end,
            groups,
            ty,
            mods,
        });
    }
    spans
}

/// Classify a token against the legend into its `(type, modifiers, groups)`: the
/// bare type name and active modifier names (for the `get_at_pos` mirror), plus
/// the candidate `@lsp.*` highlight-capture names it could paint as — ordered
/// most-specific first: `lsp.typemod.<type>.<modifier>` for each active modifier
/// (legend-bit order), then `lsp.type.<type>`. Group names omit the leading `@`,
/// which [`resolve_capture`](bemtvi_core::highlight) prepends. `None` when the
/// token's `tokenType` index is outside the legend (nothing to classify it as).
fn classify(
    tok: &SemanticToken,
    legend: &SemanticLegend,
) -> Option<(String, Vec<String>, Vec<String>)> {
    let ty = legend.token_types.get(tok.token_type as usize)?.clone();
    let mut mods = Vec::new();
    let mut groups = Vec::new();
    // The bitset is a u32, so only the first 32 legend entries can ever be set; an
    // out-of-spec server advertising more would drive `1 << bit` past the type's
    // width (a debug panic, a silent wrap to bit 0 in release). Names past bit 31
    // are unreachable by construction, so they are skipped, not classified.
    for (bit, modifier) in legend.token_modifiers.iter().enumerate().take(32) {
        if tok.token_modifiers_bitset & (1 << bit) != 0 {
            groups.push(format!("lsp.typemod.{ty}.{modifier}"));
            mods.push(modifier.clone());
        }
    }
    groups.push(format!("lsp.type.{ty}"));
    Some((ty, mods, groups))
}
