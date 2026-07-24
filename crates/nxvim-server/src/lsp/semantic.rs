//! LSP semantic tokens: the whole-buffer `textDocument/semanticTokens/full`
//! result decoded and projected over the treesitter highlight floor (ADR 0001
//! bridge #2).
//!
//! The flow mirrors diagnostics — an async, server-side enrichment cached
//! per-buffer and projected into nxvim's own highlight layer at the right
//! priority — but it lands in the *highlight* layer rather than a sibling redraw
//! key: the decoded spans become a third source of [`HlInterval`]s that
//! [`EditHost::highlights_for`](crate::treesitter) merges, at
//! [`SEMANTIC_HL_PRIORITY`](nxvim_core::SEMANTIC_HL_PRIORITY) — above the
//! treesitter floor, below user extmarks. A token whose `@lsp.*` group does not
//! resolve to a style in the active theme is **dropped** from the projection, so
//! an undefined group never wins the merge and blanks the syntactic color
//! underneath (neovim sidesteps the same trap by not applying an undefined
//! hl_group).

use std::collections::BTreeMap;

use nxvim_core::{Buffer, BufferId};
use nxvim_lsp::lsp_types::{SemanticToken, SemanticTokensEdit};
use nxvim_lsp::{PositionEncoding, SemanticLegend, SemanticTokensData};
use nxvim_lua::SemanticTokenData;

use super::{byte_col, LspReqKind, SemanticSpan};
use crate::extmarks::HlInterval;
use crate::EditHost;

/// Flatten the decoded per-line [`SemanticSpan`]s into the flat
/// [`SemanticTokenData`] list the `nx._semantic_tokens` mirror holds (one entry
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
    /// Issue a semantic-tokens request for `buffer`, if its server is up, finished
    /// `initialize`, and advertised a legend. A no-op otherwise (the buffer keeps
    /// the treesitter floor alone). Whole-buffer, so — unlike the cursor-anchored
    /// features — it is fired on open and after each change, and the reply is
    /// stale-dropped on a content (`tick`) change, not a cursor move.
    ///
    /// Once the buffer has cached a `result_id`, the request is `full/delta`
    /// (quoting it as `previousResultId`) so the server can ship a diff; with no
    /// cached `result_id` (first request, or the server sent none) it is a whole
    /// `full` request.
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
        let Some(key) = state.server.clone() else {
            return;
        };
        let Some(uri) = state.uri.clone() else {
            return;
        };
        // The server must have finished initialize (so its legend is known) and
        // actually advertise semantic tokens; otherwise there is nothing to ask.
        let Some(rt) = self.lsp_servers.get(&key) else {
            return;
        };
        if rt.legend.is_none() {
            return;
        }
        let supports_delta = rt.semantic_tokens_delta;
        // Send `full/delta` only once we have a cached `result_id` *and* the server
        // advertised delta support; otherwise re-request the whole `full` set.
        let request = match state.semantic.result_id.clone() {
            Some(previous_result_id) if supports_delta => {
                nxvim_lsp::LspRequest::SemanticTokensDelta {
                    uri,
                    previous_result_id,
                }
            }
            _ => nxvim_lsp::LspRequest::SemanticTokensFull { uri },
        };
        let token = self.register_buffer_scoped_request(LspReqKind::SemanticTokens, buffer);
        self.fx.lsp_request(key, token, request);
    }

    /// Cache a `semanticTokens/full` or `full/delta` reply for the buffer it was
    /// requested for, decoding the (possibly delta-patched) packed tokens against
    /// that buffer's server legend + encoding into the per-line spans the
    /// projection reads. Dropped when the buffer is gone, its server has no legend,
    /// or its content changed since the request (`req_tick` mismatch — a fresh
    /// request is already in flight, computed against the new text).
    ///
    /// A [`SemanticTokensData::Full`] replaces the cached token set wholesale; a
    /// [`SemanticTokensData::Delta`] splices its edits into the cached set and
    /// re-decodes. A delta that arrives with no cached base to patch (the
    /// `result_id` the request quoted is gone) can't be applied: the cache is
    /// cleared and a fresh `full` request issued, so the buffer recovers rather
    /// than painting against a phantom base.
    pub(crate) fn on_semantic_tokens_reply(
        &mut self,
        buffer: BufferId,
        req_tick: u64,
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
        let Some(key) = state.server.as_ref() else {
            return;
        };
        let Some(rt) = self.lsp_servers.get(key) else {
            return;
        };
        let Some(legend) = rt.legend.as_ref() else {
            return;
        };
        let encoding = rt.encoding;
        let client_id = rt.client_id;

        let (result_id, tokens) = match data {
            SemanticTokensData::Full { result_id, tokens } => (result_id, tokens),
            SemanticTokensData::Delta { result_id, edits } => {
                let base = &state.semantic.tokens;
                // The request quoted a `result_id`, so the cache should hold its
                // base. If it doesn't (no base to patch), the delta is unusable:
                // clear and re-request a full set rather than splice into nothing.
                if base.is_empty() && !edits.is_empty() {
                    let state = self.lsp_states.get_mut(&buffer).expect("checked above");
                    state.semantic = Default::default();
                    self.lsp_dirty = true;
                    self.request_semantic_tokens(buffer);
                    return;
                }
                (result_id, apply_token_edits(base, &edits))
            }
        };

        let spans = decode_tokens(&tokens, legend, encoding, buf);
        // Mirror the decoded tokens into `nx._semantic_tokens[bufnr]` so the
        // synchronous `vim.lsp.semantic_tokens.get_at_pos` can read them from pure
        // Lua (the diagnostics-mirror analogue), then cache the spans for the paint.
        let mirror = semantic_mirror(&spans, client_id);
        let state = self.lsp_states.get_mut(&buffer).expect("checked above");
        state.semantic.result_id = result_id;
        state.semantic.tokens = tokens;
        state.semantic.spans = spans;
        let _ = self.lua.set_semantic_tokens(buffer.0, &mirror);
        self.lsp_dirty = true;
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
        let Some(spans) = state.semantic.spans.get(&line_idx) else {
            return Vec::new();
        };
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
                    priority: nxvim_core::SEMANTIC_HL_PRIORITY,
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
/// which [`resolve_capture`](nxvim_core::highlight) prepends. `None` when the
/// token's `tokenType` index is outside the legend (nothing to classify it as).
fn classify(
    tok: &SemanticToken,
    legend: &SemanticLegend,
) -> Option<(String, Vec<String>, Vec<String>)> {
    let ty = legend.token_types.get(tok.token_type as usize)?.clone();
    let mut mods = Vec::new();
    let mut groups = Vec::new();
    for (bit, modifier) in legend.token_modifiers.iter().enumerate() {
        if tok.token_modifiers_bitset & (1 << bit) != 0 {
            groups.push(format!("lsp.typemod.{ty}.{modifier}"));
            mods.push(modifier.clone());
        }
    }
    groups.push(format!("lsp.type.{ty}"));
    Some((ty, mods, groups))
}
