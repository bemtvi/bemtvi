//! The shared fuzzy matcher — one nucleo-class ranker behind every
//! **static-source** float-list consumer (`nx.ui.select`, the static picker
//! sources, and later completion). See
//! `docs/specs/2026-06-14-nx-ui-float-widget.md` → *Matching*: the widget filters
//! and ranks locally as the query changes and highlights the matched characters,
//! so a static-source picker never re-enters Lua while you type (ADR 0002 rule 4).
//!
//! This module is **pure** — `&str` in, ranked indices out — so it sits inside the
//! synchronous, I/O-free [`nxvim-core`](crate), next to its only consumer the way
//! the command oracle lives in `editor::command`. A **dynamic** source (live grep)
//! bypasses this entirely; the widget forwards each query change to the source.

use std::ops::Range;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// Fuzzy-rank `candidates` against `query`, best first.
///
/// Returns, for each candidate that matches, its index into `candidates` paired
/// with the **matched-character spans** to highlight — half-open ranges of
/// **`char` positions** (not byte offsets) into that candidate, coalesced and in
/// order. Candidates that do not match are dropped. Ranking is by descending
/// match score, ties broken by original order so the result is stable as items
/// stream in.
///
/// An empty `query` matches everything in original order with no spans — the
/// "just opened, nothing typed yet" view.
pub fn rank(query: &str, candidates: &[&str]) -> Vec<(usize, Vec<Range<usize>>)> {
    rank_scored(query, candidates)
        .into_iter()
        .map(|(i, _, spans)| (i, spans))
        .collect()
}

/// Like [`rank`], but keeps each match's **raw fuzzy score** — `(index, score,
/// spans)`, best score first. The score lets a consumer *blend* fuzzy quality with
/// its own signal (the completion merge adds a small per-source bias so `lsp`/
/// snippet rows edge out equally-good buffer words, without a strong match from a
/// lower-ranked source being buried). An empty query returns score `0` for every
/// candidate in original order.
pub fn rank_scored(query: &str, candidates: &[&str]) -> Vec<(usize, u32, Vec<Range<usize>>)> {
    if query.is_empty() {
        return (0..candidates.len()).map(|i| (i, 0, Vec::new())).collect();
    }

    let mut matcher = Matcher::new(Config::DEFAULT);
    // `Smart` case/normalization: case-insensitive until the query has an
    // uppercase char, and unicode-normalized — the fzf-like default users expect.
    let pattern = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);

    let mut char_buf: Vec<char> = Vec::new();
    let mut positions: Vec<u32> = Vec::new();
    let mut scored: Vec<(usize, u32, Vec<Range<usize>>)> = Vec::with_capacity(candidates.len());

    for (i, cand) in candidates.iter().enumerate() {
        let haystack = Utf32Str::new(cand, &mut char_buf);
        positions.clear();
        if let Some(score) = pattern.indices(haystack, &mut matcher, &mut positions) {
            positions.sort_unstable();
            positions.dedup();
            scored.push((i, score, coalesce(&positions)));
        }
    }

    // Higher score first; equal scores keep input order (stable streaming).
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    scored
}

/// Collapse a sorted, de-duplicated list of matched `char` positions into the
/// minimal set of half-open ranges (`[start, end)`), so adjacent matches render
/// as one highlighted run.
fn coalesce(positions: &[u32]) -> Vec<Range<usize>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    for &p in positions {
        let p = p as usize;
        match ranges.last_mut() {
            Some(last) if last.end == p => last.end = p + 1,
            _ => ranges.push(p..p + 1),
        }
    }
    ranges
}
