//! The snippet expansion *session* — driving the cursor through an expanded
//! snippet's tabstops.
//!
//! The pure grammar parse lives in [`crate::snippet`]; this is the stateful half
//! that lives on the [`Editor`]. Expanding a snippet replaces a range with the
//! literal text and anchors every tabstop occurrence as a **range extmark** in
//! [`SNIPPET_NS`], so the buffer's single edit choke point keeps each tabstop's
//! byte range correct as the user types — the same trick multi-cursor placement
//! uses for secondary cursors. Those marks carry an `hl_group`
//! (`SnippetTabstop` / `SnippetTabstopActive`), so the existing extmark→highlight
//! projection paints the placeholders with no extra plumbing.
//!
//! `<Tab>` / `<S-Tab>` jump between tabstops; every mirror of the active tabstop
//! is kept in sync after each insert-mode edit; the session ends on `<Esc>` to
//! Normal or when the final `$0` stop is reached.
//!
//! ## Why the active tabstop tracks its left edge by hand
//!
//! Extmark gravity is neovim's fixed default: a range's start is right-gravity and
//! its end left-gravity, so text typed at *either* boundary lands *outside* the
//! range. That is exactly wrong for the tabstop you are filling in — typing at an
//! empty tabstop would fall outside it. So the active stop's left edge is tracked
//! as a plain byte offset ([`SnippetSession::anchor`]) that does **not** slide when
//! you type at it; its live region is `anchor..cursor`, and its extmark is reset to
//! that range after each edit (for the highlight and as the mirror source).

use super::*;
use crate::extmark::{DEFAULT_PRIORITY, SNIPPET_NS};
use crate::input::{Key, KeyCode};
use crate::mode::Mode;
use crate::snippet::ParsedSnippet;

/// The tabstop-jump keys, configurable via `nx.snippet.setup{ jump_next, jump_prev }`.
#[derive(Debug, Clone)]
pub struct SnippetKeys {
    pub next: Vec<Key>,
    pub prev: Vec<Key>,
}

impl Default for SnippetKeys {
    fn default() -> Self {
        SnippetKeys {
            next: vec![Key::new(KeyCode::Tab)],
            prev: vec![Key {
                shift: true,
                ..Key::new(KeyCode::Tab)
            }],
        }
    }
}

/// One tabstop within a live session: its occurrence extmarks (`marks[0]` is the
/// primary/editable occurrence, any rest are mirrors).
struct SessionStop {
    index: u32,
    marks: Vec<u64>,
}

/// An in-flight snippet expansion. `stops` is in tab order (1, 2, …, then the
/// final `$0`); `current` indexes the stop the cursor is parked on; `anchor` is
/// the live left edge of the current stop's editable region (see the module
/// docs on gravity).
pub struct SnippetSession {
    stops: Vec<SessionStop>,
    current: usize,
    anchor: usize,
}

/// Which way a tabstop jump moves.
#[derive(Clone, Copy)]
pub(crate) enum JumpDir {
    Next,
    Prev,
}

impl Editor {
    /// Whether a snippet expansion is currently being filled in.
    pub fn snippet_active(&self) -> bool {
        self.snippet.is_some()
    }

    /// Set the tabstop-jump keys (`nx.snippet.setup`). An empty list keeps the
    /// default for that direction.
    pub fn set_snippet_keys(&mut self, next: Vec<Key>, prev: Vec<Key>) {
        if !next.is_empty() {
            self.snippet_keys.next = next;
        }
        if !prev.is_empty() {
            self.snippet_keys.prev = prev;
        }
    }

    /// Classify a key against the active session's jump bindings. `None` when no
    /// session is live or the key isn't a jump key (so it edits normally).
    pub(crate) fn snippet_jump_for(&self, key: &Key) -> Option<JumpDir> {
        self.snippet.as_ref()?;
        if self.snippet_keys.next.iter().any(|k| k == key) {
            Some(JumpDir::Next)
        } else if self.snippet_keys.prev.iter().any(|k| k == key) {
            Some(JumpDir::Prev)
        } else {
            None
        }
    }

    /// Expand `parsed` over the buffer range `anchor..replace_end`, entering Insert
    /// mode at the first tabstop. Continuation lines are re-indented to the anchor
    /// line's indent. Folds into the surrounding insert undo group when one is open
    /// (a completion-accept mid-insert), else opens its own. A snippet with no
    /// editable tabstop (only literal text, or only `$0`) just inserts and parks the
    /// cursor — no session is started.
    pub fn expand_snippet(&mut self, anchor: usize, replace_end: usize, parsed: ParsedSnippet) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        // Re-indent continuation lines to the anchor line's leading whitespace.
        let line = self.buffer().byte_to_line(anchor);
        let indent = self.line_leading_ws(line);
        let (text, spans) = reindent(&parsed.text, &parsed.stops, &indent);

        // One undo step: open a group unless we're already inside an insert session.
        if !self.snapshot_taken {
            self.push_undo();
            self.snapshot_taken = true;
        }
        // End any prior session before its marks are orphaned by the edit.
        self.end_snippet();
        let text_len = text.len();
        self.buffer_mut().remove(anchor..replace_end.max(anchor));
        self.buffer_mut().insert(anchor, &text);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.mode = Mode::Insert;

        // Anchor every tabstop occurrence as a range extmark and build the session.
        let mut stops = Vec::new();
        for (stop, ranges) in parsed.stops.iter().zip(&spans) {
            let marks = ranges
                .iter()
                .map(|r| self.set_snippet_mark(None, anchor + r.start, anchor + r.end, false))
                .collect();
            stops.push(SessionStop {
                index: stop.index,
                marks,
            });
        }

        // No editable stop, or only the final `$0`: park the cursor and stop here.
        match stops.iter().position(|s| s.index != 0) {
            None => {
                let at = stops
                    .iter()
                    .find(|s| s.index == 0)
                    .and_then(|s| self.snippet_mark_range(s.marks[0]))
                    .map_or(anchor + text_len, |(s, _)| s);
                for s in &stops {
                    self.drop_snippet_marks(&s.marks);
                }
                self.set_cursor_char_insert(at);
            }
            Some(idx) => {
                self.snippet = Some(SnippetSession {
                    stops,
                    current: idx,
                    anchor: 0,
                });
                self.snippet_place_current();
            }
        }
        self.ensure_visible();
    }

    /// Jump to the next/previous tabstop. Returns whether a session was active (and
    /// thus the key was consumed). Landing on the final `$0` ends the session.
    pub(crate) fn snippet_jump(&mut self, dir: JumpDir) -> bool {
        let Some(sess) = self.snippet.as_ref() else {
            return false;
        };
        let len = sess.stops.len();
        let target = match dir {
            JumpDir::Next => (sess.current + 1).min(len - 1),
            JumpDir::Prev => sess.current.saturating_sub(1),
        };
        self.snippet.as_mut().unwrap().current = target;
        let final_stop = self.snippet.as_ref().unwrap().stops[target].index == 0;
        self.snippet_place_current();
        if final_stop {
            self.end_snippet();
        }
        self.ensure_visible();
        true
    }

    /// Park the cursor on the current stop's primary occurrence (at its end, so any
    /// default text is kept), set the live left-edge anchor, and recolour so only
    /// the current stop's marks read as active.
    fn snippet_place_current(&mut self) {
        let Some(sess) = self.snippet.as_ref() else {
            return;
        };
        let cur = sess.current;
        let recolor: Vec<(u64, bool)> = sess
            .stops
            .iter()
            .enumerate()
            .flat_map(|(i, s)| s.marks.iter().map(move |&m| (m, i == cur)))
            .collect();
        for (mark, active) in recolor {
            self.recolor_snippet_mark(mark, active);
        }
        let primary = self.snippet.as_ref().unwrap().stops[cur].marks[0];
        if let Some((start, end)) = self.snippet_mark_range(primary) {
            self.snippet.as_mut().unwrap().anchor = start;
            self.set_cursor_char_insert(end);
        }
    }

    /// After an insert-mode edit, grow the active tabstop's region to `anchor..cursor`
    /// and copy that text into its mirror occurrences. Edits before the anchor (a
    /// mirror sync to the left) shift the anchor and cursor so they keep tracking the
    /// live region. A no-op when no session is active.
    pub(crate) fn snippet_sync(&mut self) {
        let Some(sess) = self.snippet.as_ref() else {
            return;
        };
        let cur = sess.current;
        let mut anchor = sess.anchor;
        let mut cursor = self.cursor_char();
        // A backspace past the left edge collapses the anchor onto the cursor.
        if cursor < anchor {
            anchor = cursor;
        }
        let content = self.buffer().text.slice(anchor..cursor).to_string();
        let primary = self.snippet.as_ref().unwrap().stops[cur].marks[0];
        let mirrors: Vec<u64> = self.snippet.as_ref().unwrap().stops[cur].marks[1..].to_vec();

        // Apply mirror replacements high-offset-first so an earlier edit never shifts
        // a later one; an edit wholly left of the anchor shifts the live region.
        let mut edits: Vec<(usize, usize)> = mirrors
            .iter()
            .filter_map(|&m| self.snippet_mark_range(m))
            .filter(|&(s, e)| self.buffer().text.slice(s..e) != content.as_str())
            .collect();
        edits.sort_by_key(|&(s, _)| std::cmp::Reverse(s));
        for (s, e) in edits {
            self.buffer_mut().remove(s..e);
            if !content.is_empty() {
                self.buffer_mut().insert(s, &content);
            }
            if s < anchor {
                let delta = content.len() as isize - (e - s) as isize;
                anchor = (anchor as isize + delta).max(0) as usize;
                cursor = (cursor as isize + delta).max(0) as usize;
            }
        }
        self.buffer_mut().normalize();
        // Reset the primary mark to the live region (gravity would drop boundary
        // edits) and re-park the cursor, which mirror edits to the left may have shifted.
        self.set_snippet_mark(Some(primary), anchor, cursor, true);
        let s = self.snippet.as_mut().unwrap();
        s.anchor = anchor;
        self.set_cursor_char_insert(cursor);
    }

    /// Tear down the session: remove every tabstop extmark and clear the state.
    pub(crate) fn end_snippet(&mut self) {
        if let Some(sess) = self.snippet.take() {
            for s in &sess.stops {
                self.drop_snippet_marks(&s.marks);
            }
        }
    }

    // --- extmark helpers --------------------------------------------------

    /// Create (`id == None`) or replace a tabstop range extmark, returning its id.
    fn set_snippet_mark(&mut self, id: Option<u64>, start: usize, end: usize, active: bool) -> u64 {
        let hl = if active {
            "SnippetTabstopActive"
        } else {
            "SnippetTabstop"
        };
        let bid = self.cur_buffer();
        self.buffers.get_mut(bid).buffer.extmarks.set(
            SNIPPET_NS,
            id,
            start,
            Some(end),
            Some(hl.to_string()),
            DEFAULT_PRIORITY,
            None,
        )
    }

    fn recolor_snippet_mark(&mut self, id: u64, active: bool) {
        if let Some((start, end)) = self.snippet_mark_range(id) {
            self.set_snippet_mark(Some(id), start, end, active);
        }
    }

    fn snippet_mark_range(&self, id: u64) -> Option<(usize, usize)> {
        self.buffer()
            .extmarks
            .get(SNIPPET_NS, id)
            .map(|m| (m.start, m.end.unwrap_or(m.start)))
    }

    fn drop_snippet_marks(&mut self, ids: &[u64]) {
        let bid = self.cur_buffer();
        for &id in ids {
            self.buffers
                .get_mut(bid)
                .buffer
                .extmarks
                .del(SNIPPET_NS, id);
        }
    }

    /// The leading-whitespace prefix of `line` (spaces and tabs), as a string.
    fn line_leading_ws(&self, line: usize) -> String {
        let s = self.buffer().line_cow(line);
        s.chars().take_while(|&c| c == ' ' || c == '\t').collect()
    }
}

/// Re-indent `text` so every line after the first is prefixed with `indent`, and
/// remap each tabstop's byte spans to the new offsets. Returns the new text and a
/// per-stop list of remapped spans (parallel to `stops`).
fn reindent(
    text: &str,
    stops: &[crate::snippet::TabStop],
    indent: &str,
) -> (String, Vec<Vec<std::ops::Range<usize>>>) {
    if indent.is_empty() || !text.contains('\n') {
        let spans = stops.iter().map(|s| s.spans.clone()).collect();
        return (text.to_string(), spans);
    }
    // Map each original byte offset to its offset in the re-indented text.
    let mut map = vec![0usize; text.len() + 1];
    let mut out = String::new();
    let mut prev_nl = false;
    for (i, ch) in text.char_indices() {
        if prev_nl {
            out.push_str(indent);
        }
        map[i] = out.len();
        out.push(ch);
        prev_nl = ch == '\n';
    }
    map[text.len()] = out.len();
    let spans = stops
        .iter()
        .map(|s| s.spans.iter().map(|r| map[r.start]..map[r.end]).collect())
        .collect();
    (out, spans)
}
