//! The incremental parse + highlight engine.
//!
//! Per buffer the engine keeps a **shadow rope** and a persistent **parse tree**.
//! Edits arrive as deltas: the shadow is patched in place, the old tree is
//! `edit`ed and reparsed **incrementally**, so per-edit cost scales with the edit
//! — not the file. Highlights are extracted by running the grammar's query over
//! just the requested line range.

use std::collections::HashMap;
use std::ops::Range;
use std::path::PathBuf;

use ropey::{LineType, Rope};
use streaming_iterator::StreamingIterator;
use tree_sitter::{InputEdit, Node, Parser, Point, QueryCursor, Tree};

use crate::loader::Grammar;

const LINE_TYPE: LineType = LineType::LF_CR;

// The edit and span shapes are the shared syntax-wire types, defined once in
// `nxvim-rpc` so this worker and the server can't drift apart (see that module).
pub use nxvim_rpc::syntax::{EditWire, SpanWire};

/// Per-buffer parse state.
struct BufferState {
    shadow: Rope,
    parser: Parser,
    tree: Option<Tree>,
    language: String,
}

impl BufferState {
    /// Reparse from the shadow, reusing the old tree when present (incremental).
    fn reparse(&mut self) {
        let shadow = &self.shadow;
        let mut callback = |byte: usize, _: Point| -> &[u8] { read_chunk(shadow, byte) };
        // Keep the last good tree if the parse yields `None` (a timeout/cancel):
        // overwriting it with `None` would throw away all incremental reuse and
        // leave the buffer un-highlightable until a full re-open. Today no
        // timeout/cancellation is configured so `parse_with_options` always
        // returns `Some`, but this stays correct if one is ever added.
        if let Some(tree) = self
            .parser
            .parse_with_options(&mut callback, self.tree.as_ref(), None)
        {
            self.tree = Some(tree);
        }
    }
}

/// Owns every buffer's parse state and a lazily-populated grammar cache.
pub struct Engine {
    data_dir: PathBuf,
    /// `None` means a previous load attempt for that language failed; we don't
    /// retry it (so a missing grammar costs one failed `dlopen`, not one per key).
    grammars: HashMap<String, Option<Grammar>>,
    buffers: HashMap<u64, BufferState>,
}

impl Engine {
    pub fn new(data_dir: PathBuf) -> Self {
        Engine {
            data_dir,
            grammars: HashMap::new(),
            buffers: HashMap::new(),
        }
    }

    /// Lazily load (and cache) the grammar for `lang`. `Ok(None)` means it has
    /// failed to load before and shouldn't be retried.
    fn grammar(&mut self, lang: &str) -> Result<Option<&Grammar>, String> {
        if !self.grammars.contains_key(lang) {
            match Grammar::load(&self.data_dir, lang) {
                Ok(g) => {
                    self.grammars.insert(lang.to_string(), Some(g));
                }
                Err(e) => {
                    self.grammars.insert(lang.to_string(), None);
                    return Err(format!("{e:#}"));
                }
            }
        }
        Ok(self.grammars.get(lang).and_then(Option::as_ref))
    }

    /// (Re)initialize a buffer from full text and do the initial parse. Returns
    /// an error string if the language has no usable grammar.
    pub fn open(&mut self, buffer: u64, lang: &str, text: &str) -> Result<(), String> {
        // Touch the grammar cache so a missing grammar reports once.
        let language = match self.grammar(lang)? {
            Some(g) => g.language.clone(),
            None => return Err(format!("no grammar for '{lang}'")),
        };
        let mut parser = Parser::new();
        parser
            .set_language(&language)
            .map_err(|e| format!("set_language: {e}"))?;
        let mut state = BufferState {
            shadow: Rope::from_str(text),
            parser,
            tree: None,
            language: lang.to_string(),
        };
        state.reparse();
        self.buffers.insert(buffer, state);
        Ok(())
    }

    /// Apply edit deltas to a buffer's shadow + tree, then reparse incrementally.
    pub fn edit(&mut self, buffer: u64, edits: &[EditWire]) {
        let Some(state) = self.buffers.get_mut(&buffer) else {
            return; // never opened; the editor opens before editing
        };
        for e in edits {
            // The byte offsets come off the wire; an out-of-range, mis-ordered,
            // or mid-codepoint range would panic ropey and (under the worker's
            // catch_unwind) leave the shadow and tree half-mutated, poisoning the
            // buffer for every later edit. Validate against the live shadow and
            // drop a delta that doesn't fit rather than trust it. `try_*` is a
            // second guard so a mutation can still never panic.
            let len = state.shadow.len();
            let valid = e.start_byte <= e.old_end_byte
                && e.old_end_byte <= len
                && state.shadow.is_char_boundary(e.start_byte)
                && state.shadow.is_char_boundary(e.old_end_byte);
            if !valid {
                continue;
            }
            // Patch the shadow: remove the old range, insert the new bytes.
            if e.old_end_byte > e.start_byte
                && state
                    .shadow
                    .try_remove(e.start_byte..e.old_end_byte)
                    .is_err()
            {
                continue;
            }
            if !e.text.is_empty() && state.shadow.try_insert(e.start_byte, &e.text).is_err() {
                continue;
            }
            if let Some(tree) = state.tree.as_mut() {
                tree.edit(&InputEdit {
                    start_byte: e.start_byte,
                    old_end_byte: e.old_end_byte,
                    new_end_byte: e.new_end_byte,
                    start_position: point(e.start_point),
                    old_end_position: point(e.old_end_point),
                    new_end_position: point(e.new_end_point),
                });
            }
        }
        state.reparse();
    }

    /// Forget a buffer's shadow text and parse tree (the editor deleted it).
    pub fn close(&mut self, buffer: u64) {
        self.buffers.remove(&buffer);
    }

    /// Whether a buffer is known (opened) and which language it uses.
    pub fn language_of(&self, buffer: u64) -> Option<&str> {
        self.buffers.get(&buffer).map(|b| b.language.as_str())
    }

    /// Extract highlight spans for the visible line range `[first_line, last_line)`.
    pub fn highlights(
        &mut self,
        buffer: u64,
        first_line: usize,
        last_line: usize,
    ) -> Vec<SpanWire> {
        let Some(state) = self.buffers.get(&buffer) else {
            return Vec::new();
        };
        let Some(tree) = state.tree.as_ref() else {
            return Vec::new();
        };
        let Some(Some(grammar)) = self.grammars.get(&state.language) else {
            return Vec::new();
        };
        extract_spans(grammar, tree, &state.shadow, first_line, last_line)
    }
}

/// Run the highlights query over the byte range covering the visible lines and
/// resolve the captures into per-line byte spans (most-specific capture wins).
fn extract_spans(
    grammar: &Grammar,
    tree: &Tree,
    rope: &Rope,
    first_line: usize,
    last_line: usize,
) -> Vec<SpanWire> {
    let line_count = rope.len_lines(LINE_TYPE).saturating_sub(1);
    let last_line = last_line.min(line_count);
    if first_line >= last_line {
        return Vec::new();
    }
    let lo = rope.line_to_byte_idx(first_line, LINE_TYPE);
    let hi = rope.line_to_byte_idx(last_line, LINE_TYPE);

    let query = &grammar.query;
    let names = query.capture_names();
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(lo..hi);

    // Collect captures intersecting the viewport as (start, end, group).
    let mut raw: Vec<(usize, usize, &str)> = Vec::new();
    let provider =
        |node: Node| std::iter::once(node_bytes(rope, node.start_byte()..node.end_byte()));
    let mut caps = cursor.captures(query, tree.root_node(), provider);
    while let Some((m, idx)) = caps.next() {
        let cap = m.captures[*idx];
        let name = names[cap.index as usize];
        if name.starts_with('_') {
            continue; // internal/predicate capture, not a highlight group
        }
        let (s, e) = (cap.node.start_byte(), cap.node.end_byte());
        if e > s {
            raw.push((s, e, name));
        }
    }
    drop(caps);

    // Broadest spans first so narrower (more specific) captures overwrite them.
    raw.sort_by_key(|(s, e, _)| (std::cmp::Reverse(e - s), *s));

    let mut out = Vec::new();
    for line in first_line..last_line {
        let line_start = rope.line_to_byte_idx(line, LINE_TYPE);
        let text = rope.line(line, LINE_TYPE).to_string();
        let content_len = text.trim_end_matches(['\n', '\r']).len();
        if content_len == 0 {
            continue;
        }
        let mut groups: Vec<Option<&str>> = vec![None; content_len];
        for &(s, e, name) in &raw {
            if e <= line_start || s >= line_start + content_len {
                continue;
            }
            let cs = s.saturating_sub(line_start).min(content_len);
            let ce = (e - line_start).min(content_len);
            if cs < ce {
                for slot in &mut groups[cs..ce] {
                    *slot = Some(name);
                }
            }
        }
        // Coalesce runs of the same group into spans.
        let mut i = 0;
        while i < content_len {
            match groups[i] {
                Some(g) => {
                    let start = i;
                    while i < content_len && groups[i] == Some(g) {
                        i += 1;
                    }
                    out.push(SpanWire {
                        line,
                        start_byte: start,
                        end_byte: i,
                        group: g.to_string(),
                    });
                }
                None => i += 1,
            }
        }
    }
    out
}

/// Bytes of `rope[range]`, walking chunks (no whole-buffer materialization).
fn node_bytes(rope: &Rope, range: Range<usize>) -> Vec<u8> {
    let mut out = Vec::with_capacity(range.len());
    let mut b = range.start;
    while b < range.end {
        let (chunk, start) = rope.chunk(b);
        if chunk.is_empty() {
            break;
        }
        let from = b - start;
        let to = (range.end - start).min(chunk.len());
        out.extend_from_slice(&chunk.as_bytes()[from..to]);
        b = start + chunk.len();
    }
    out
}

/// The chunk of `rope` starting at byte `byte` (for tree-sitter's read callback).
fn read_chunk(rope: &Rope, byte: usize) -> &[u8] {
    if byte >= rope.len() {
        return &[];
    }
    let (chunk, start) = rope.chunk(byte);
    &chunk.as_bytes()[byte - start..]
}

fn point((row, col): (usize, usize)) -> Point {
    Point::new(row, col)
}
