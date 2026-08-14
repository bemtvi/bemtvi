//! Normal/visual motions and text-object range resolution.

use super::*;
use crate::mode::Mode;
use crate::unicode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CharClass {
    Blank,
    Word,
    Punct,
}

/// Which column of a *display* row `g0`/`g^`/`g$` target — the within-row
/// analogues of `0`/`^`/`$`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayCol {
    Start,
    FirstNonBlank,
    End,
}

pub(crate) fn char_class(c: char) -> CharClass {
    if c == ' ' || c == '\t' || c == '\n' || c == '\r' {
        CharClass::Blank
    } else if c.is_alphanumeric() || c == '_' {
        CharClass::Word
    } else {
        CharClass::Punct
    }
}

impl Editor {
    /// Build a find-char motion. Forward (`f`/`t`) is inclusive, backward
    /// (`F`/`T`) is exclusive, matching how vim feeds these to an operator.
    fn find_motion(
        &self,
        kind: FindKind,
        target: char,
        count: usize,
        repeat: bool,
    ) -> Option<MotionResult> {
        let pos = self.find_char_target(kind, target, count, repeat)?;
        if kind.forward() {
            Some(MotionResult::inclusive(pos))
        } else {
            Some(MotionResult::exclusive(pos))
        }
    }

    /// Byte offset for a find-char motion on the cursor's line, or `None` if the
    /// target is not found. `f`/`F` land on the target; `t`/`T` stop one
    /// grapheme short. `repeat` (a `;`/`,` replay) hops over an immediately
    /// adjacent match for `t`/`T`, so repeats make progress instead of sticking.
    fn find_char_target(
        &self,
        kind: FindKind,
        target: char,
        count: usize,
        repeat: bool,
    ) -> Option<usize> {
        let line = self.cursor.line;
        let s = self.buffer().line(line);
        let base = self.buffer().line_start(line);
        let cur = self.cursor.col;
        let till = kind.till();
        let char_at = |col: usize| s[col..].chars().next();

        let hit = if kind.forward() {
            let mut col = unicode::next_grapheme(&s, cur);
            if till && repeat && col < s.len() && char_at(col) == Some(target) {
                col = unicode::next_grapheme(&s, col);
            }
            let mut found = 0;
            let mut hit = None;
            while col < s.len() {
                if char_at(col) == Some(target) {
                    found += 1;
                    if found == count {
                        hit = Some(col);
                        break;
                    }
                }
                col = unicode::next_grapheme(&s, col);
            }
            hit.map(|c| {
                if till {
                    unicode::prev_grapheme(&s, c)
                } else {
                    c
                }
            })
        } else {
            if cur == 0 {
                return None;
            }
            let mut col = unicode::prev_grapheme(&s, cur);
            if till && repeat && col > 0 && char_at(col) == Some(target) {
                col = unicode::prev_grapheme(&s, col);
            }
            let mut found = 0;
            let mut hit = None;
            loop {
                if char_at(col) == Some(target) {
                    found += 1;
                    if found == count {
                        hit = Some(col);
                        break;
                    }
                }
                if col == 0 {
                    break;
                }
                col = unicode::prev_grapheme(&s, col);
            }
            hit.map(|c| {
                if till {
                    unicode::next_grapheme(&s, c)
                } else {
                    c
                }
            })
        };
        hit.map(|col| base + col)
    }

    /// Where a [`Motion`] lands, as a [`MotionResult`]. The motion *alphabet*
    /// lives in [`classify_motion`]; this is its effect counterpart — it `match`es
    /// the typed enum exhaustively, so a new motion variant must be handled here
    /// too (the compiler enforces it). Returns `None` only for *execution* misses
    /// (a find/`;`/`,` with no match, or `;`/`,` before any find), never for a
    /// classification disagreement. The count and the un-defaulted `raw` count are
    /// derived from the pending command, exactly as the old caller computed them.
    pub(crate) fn resolve_motion(&self, motion: Motion) -> Option<MotionResult> {
        let line = self.cursor.line;
        let last_line = self.last_line();
        let count = self.effective_count();
        let raw = if self.pending.operator.is_some() {
            self.pending.op_count.or(self.pending.count)
        } else {
            self.pending.count
        };

        let result = match motion {
            Motion::GotoTop => {
                let target_line = raw.map(|n| n - 1).unwrap_or(0).min(last_line);
                let target = self.buffer().line_start(target_line);
                MotionResult::linewise(target, MoveAxis::LineAnchor)
            }
            // `;` repeats the last find-char motion; `,` repeats it reversed.
            Motion::FindRepeat { reverse } => {
                let (kind, target) = self.last_find?;
                let kind = if reverse { kind.reversed() } else { kind };
                return self.find_motion(kind, target, count, true);
            }
            Motion::Find(kind, target) => return self.find_motion(kind, target, count, false),
            // `` `{mark} `` — charwise exclusive to the mark's exact position.
            // `'{mark}` — linewise to the mark's line, landing on first non-blank
            // (the `LineAnchor` axis `gg`/`G` use). An unset mark is `None`: an
            // *execution* miss the caller reports loudly (E20), not a parse miss.
            Motion::MarkJumpExact(name, _) => {
                let pos = self.mark_position(name)?;
                MotionResult::exclusive(self.buffer().byte_at(pos.line, pos.col))
            }
            Motion::MarkJumpLine(name, _) => {
                let pos = self.mark_position(name)?;
                MotionResult::linewise(self.buffer().line_start(pos.line), MoveAxis::LineAnchor)
            }
            Motion::Left => {
                let s = self.buffer().line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::prev_grapheme(&s, col);
                }
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            Motion::Right => {
                let s = self.buffer().line(line);
                let mut col = self.cursor.col;
                for _ in 0..count {
                    col = unicode::next_grapheme(&s, col);
                }
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            Motion::LineStart => MotionResult::exclusive(self.buffer().byte_at(line, 0)),
            Motion::FirstNonBlank => {
                let col = self.first_non_blank(line);
                MotionResult::exclusive(self.buffer().byte_at(line, col))
            }
            Motion::LineEnd => {
                let l = (line + count - 1).min(last_line);
                let s = self.buffer().line(l);
                let col = unicode::prev_grapheme(&s, s.len());
                MotionResult {
                    target: self.buffer().byte_at(l, col),
                    kind: MotionKind::Inclusive,
                    axis: MoveAxis::EndOfLine,
                }
            }
            Motion::Down => {
                // Already on the last line: the motion FAILS (vim's `cursor_down`
                // returns FAIL there and beeps) rather than resolving to a move that
                // goes nowhere. The distinction is invisible for a typed `j` — both
                // leave the cursor put — but it is what lets `100<F3>a` stop at the
                // end of the buffer instead of replaying against the last line 90
                // more times. A count that overshoots still clamps, as vim does.
                if line >= last_line {
                    return None;
                }
                // Fold-aware: each closed fold counts as one line, so `j` steps over
                // a collapsed range in a single move (and never lands in its hidden
                // interior). Falls back to plain stepping when no fold is in the way.
                let l = self.line_below_folds(line, count).min(last_line);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep)
            }
            Motion::Up => {
                // The mirror of `Down`: on the first line there is nowhere to go, so
                // the motion fails instead of resolving to a no-op.
                if line == 0 {
                    return None;
                }
                let l = self.line_above_folds(line, count);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep)
            }
            Motion::DisplayDown => self.display_motion(true, count),
            Motion::DisplayUp => self.display_motion(false, count),
            Motion::DisplayLineStart => self.display_line_motion(DisplayCol::Start),
            Motion::DisplayFirstNonBlank => self.display_line_motion(DisplayCol::FirstNonBlank),
            Motion::DisplayLineEnd => self.display_line_motion(DisplayCol::End),
            Motion::GotoLine => {
                let l = raw.map(|n| n - 1).unwrap_or(last_line).min(last_line);
                MotionResult::linewise(self.buffer().line_start(l), MoveAxis::LineAnchor)
            }
            Motion::Word(big) => self.word_motion(count, big),
            Motion::BackWord(big) => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_backward(idx, big);
                }
                MotionResult::exclusive(idx)
            }
            Motion::EndWord(big) => {
                let mut idx = self.cursor_char();
                for _ in 0..count {
                    idx = self.word_end(idx, big);
                }
                MotionResult::inclusive(idx)
            }
        };
        Some(result)
    }

    /// Resolve a `w`/`W` word motion. Special case: `cw` on a non-blank acts like
    /// `ce` — it changes to the end of the word without swallowing the trailing
    /// space — so it returns an inclusive end-of-word target instead.
    fn word_motion(&self, count: usize, big: bool) -> MotionResult {
        let mut idx = self.cursor_char();
        // `cw` on an empty line changes nothing — vim empties the motion (the
        // line break is never swallowed) but still enters Insert mode.
        if self.pending.operator == Some('c') && self.line_len() == 0 {
            return MotionResult::exclusive(idx);
        }
        if self.pending.operator == Some('c')
            && idx <= self.last_char_idx()
            && char_class(self.char_at(idx)) != CharClass::Blank
        {
            for _ in 0..count {
                idx = self.word_end(idx, big);
            }
            MotionResult::inclusive(idx)
        } else {
            for _ in 0..count {
                idx = self.word_forward(idx, big);
            }
            MotionResult::exclusive(idx)
        }
    }

    /// Apply a motion as plain cursor movement, maintaining vim's `curswant`.
    pub(crate) fn apply_movement(&mut self, m: MotionResult) {
        match m.axis {
            MoveAxis::Horizontal => {
                self.set_cursor_char(m.target);
                self.clamp_cursor();
            }
            MoveAxis::EndOfLine => {
                self.set_cursor_char(m.target);
                self.clamp_cursor();
                self.eol_request = true;
            }
            MoveAxis::LineAnchor => {
                let line = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                // Landing inside a closed fold (e.g. `G`/`gg`/a mark jump into a
                // collapsed range) snaps to the fold's visible header line, as vim
                // does — the cursor is never left on a hidden line.
                self.cursor.line = self.fold_line_start(line);
                self.cursor.col = self.first_non_blank(self.cursor.line);
                self.clamp_cursor();
            }
            MoveAxis::VerticalKeep => {
                let line = self
                    .buffer()
                    .byte_to_line(m.target.min(self.last_char_idx()));
                self.cursor.line = self.fold_line_start(line);
                self.settle_desired_col(false);
                self.preserve_desired = true;
            }
        }
    }

    /// Resolve `gj` / `gk`: move `count` *display* rows down / up, keeping the
    /// cursor's column within the display row (vim's `curswant` in screen cells).
    /// Under `nowrap` (or a zero-width area) this is plain `j` / `k`. When wrapping,
    /// the soft-wrap continuation rows of a buffer line are stepped before crossing
    /// to the next / previous buffer line, so the cursor walks the screen one row at
    /// a time rather than one buffer line at a time.
    fn display_motion(&self, down: bool, count: usize) -> MotionResult {
        let width = self.text_width();
        let wrap = self.windows.cur().options.wrap;
        if !wrap || width == 0 {
            // No wrapping: `gj` / `gk` are exactly `j` / `k`.
            let l = if down {
                (self.cursor.line + count).min(self.last_line())
            } else {
                self.cursor.line.saturating_sub(count)
            };
            return MotionResult::linewise(self.buffer().line_start(l), MoveAxis::VerticalKeep);
        }
        let tab = self.tabstop();
        let opts = &self.windows.cur().options;
        let wp = opts.wrap_prefix();
        let buf = self.buffer();
        // Segment a line with the window's `'breakindent'`/`'showbreak'` continuation
        // indent, so gj/gk step the same display rows the window renders.
        let segs_of = |text: &str| {
            let indent = unicode::cont_indent(text, tab, width, wp);
            unicode::wrap_segments_indented(text, tab, width, indent)
        };
        let seg_count = |line: usize| segs_of(&buf.line_cow(line)).len();

        // The cursor's current display column: its screen column minus the start
        // column of the wrap segment it sits in.
        let text0 = buf.line_cow(self.cursor.line);
        let segs0 = segs_of(&text0);
        let cur_full = unicode::virtcol(&text0, self.cursor.col, tab);
        let cur_seg = segs0
            .iter()
            .rposition(|s| self.cursor.col >= s.start_byte)
            .unwrap_or(0);
        let want = cur_full - segs0[cur_seg].start_col;

        // Walk `count` display rows: step wrap segments within a line, then cross to
        // the next / previous buffer line's first / last segment.
        let mut line = self.cursor.line;
        let mut seg = cur_seg;
        let mut nseg = segs0.len();
        for _ in 0..count {
            if down {
                if seg + 1 < nseg {
                    seg += 1;
                } else if line < self.last_line() {
                    line += 1;
                    nseg = seg_count(line);
                    seg = 0;
                } else {
                    break;
                }
            } else if seg > 0 {
                seg -= 1;
            } else if line > 0 {
                line -= 1;
                nseg = seg_count(line);
                seg = nseg - 1;
            } else {
                break;
            }
        }

        // Land at the target row's start column + the wanted display column, clamped
        // to the row's own extent so it doesn't spill into the next row's content.
        let text = buf.line_cow(line);
        let segs = segs_of(&text);
        let start_col = segs.get(seg).map_or(0, |s| s.start_col);
        let next_start = segs.get(seg + 1).map_or(usize::MAX, |s| s.start_col);
        let screen_col = (start_col + want).min(next_start.saturating_sub(1).max(start_col));
        let byte = unicode::floor_grapheme(&text, unicode::byte_at_virtcol(&text, screen_col, tab));
        MotionResult::horizontal(buf.line_start(line) + byte, MotionKind::Inclusive)
    }

    /// Resolve `g0` / `g^` / `g$`: move within the cursor's current *display* row to
    /// its first column / first non-blank / last column — the within-row siblings of
    /// `gj`/`gk`. Under `nowrap` (or a zero-width area) these are exactly `0` / `^` /
    /// `$`; when wrapping, the bounds are the cursor's own soft-wrap segment rather
    /// than the whole buffer line.
    fn display_line_motion(&self, kind: DisplayCol) -> MotionResult {
        let width = self.text_width();
        let wrap = self.windows.cur().options.wrap;
        let line = self.cursor.line;
        let buf = self.buffer();

        // The display row's byte span within the line: the whole line under `nowrap`,
        // else the soft-wrap segment the cursor sits in.
        let text = buf.line_cow(line);
        let (start_byte, end_byte) = if !wrap || width == 0 {
            (0, text.len())
        } else {
            let tab = self.tabstop();
            let opts = &self.windows.cur().options;
            let indent = unicode::cont_indent(&text, tab, width, opts.wrap_prefix());
            let segs = unicode::wrap_segments_indented(&text, tab, width, indent);
            let seg = segs
                .iter()
                .rposition(|s| self.cursor.col >= s.start_byte)
                .unwrap_or(0);
            (segs[seg].start_byte, segs[seg].end_byte)
        };
        let base = buf.line_start(line);
        match kind {
            DisplayCol::Start => MotionResult::exclusive(base + start_byte),
            DisplayCol::FirstNonBlank => {
                // First non-blank byte within the segment; an all-blank segment lands
                // on its start (vim's `g^` falls back to the first column).
                let seg = &text[start_byte..end_byte];
                let off = seg.find(|c: char| c != ' ' && c != '\t').unwrap_or(0);
                MotionResult::exclusive(base + start_byte + off)
            }
            DisplayCol::End => {
                // Last grapheme of the display row (inclusive, like `$`), never before
                // the row's own start for an empty segment.
                let last = unicode::prev_grapheme(&text, end_byte).max(start_byte);
                MotionResult {
                    target: base + last,
                    kind: MotionKind::Inclusive,
                    axis: MoveAxis::EndOfLine,
                }
            }
        }
    }

    // The word motions classify chars through [`span_class`] so `big` (the WORD
    // keys `W`/`B`/`E`) collapses punctuation into the non-blank class, while the
    // small-word keys keep word/punct distinct. Class `0` is blank throughout.
    fn word_forward(&self, mut idx: usize, big: bool) -> usize {
        let last = self.last_char_idx();
        if idx >= last {
            return idx;
        }
        let start = self.span_class(idx, big);
        if start != 0 {
            while idx < last && self.span_class(idx, big) == start {
                idx = self.next_grapheme_idx(idx);
            }
        }
        while idx < last && self.span_class(idx, big) == 0 {
            idx = self.next_grapheme_idx(idx);
        }
        idx
    }

    fn word_backward(&self, mut idx: usize, big: bool) -> usize {
        if idx == 0 {
            return 0;
        }
        idx = self.prev_grapheme_idx(idx);
        while idx > 0 && self.span_class(idx, big) == 0 {
            idx = self.prev_grapheme_idx(idx);
        }
        if idx == 0 {
            return 0;
        }
        let cls = self.span_class(idx, big);
        while idx > 0 {
            let prev = self.prev_grapheme_idx(idx);
            if self.span_class(prev, big) != cls {
                break;
            }
            idx = prev;
        }
        idx
    }

    fn word_end(&self, mut idx: usize, big: bool) -> usize {
        let last = self.last_char_idx();
        if idx >= last {
            return idx;
        }
        idx = self.next_grapheme_idx(idx);
        while idx < last && self.span_class(idx, big) == 0 {
            idx = self.next_grapheme_idx(idx);
        }
        let cls = self.span_class(idx, big);
        while idx < last {
            let next = self.next_grapheme_idx(idx);
            if next > last || self.span_class(next, big) != cls {
                break;
            }
            idx = next;
        }
        idx
    }

    /// Resolve the absolute charwise byte range `[start, end)` for a text
    /// object. `ia` is `'i'` (inner) or `'a'` (a/around); `obj` is the object
    /// key. Returns `None` for an unknown object key or when no object exists
    /// at the cursor.
    pub(crate) fn text_object_range(
        &self,
        ia: char,
        kind: ObjectKind,
        count: usize,
    ) -> Option<(usize, usize, bool)> {
        // Charwise objects return `(lo, hi)`; tag them `false` here.
        let charwise = |r: Option<(usize, usize)>| r.map(|(lo, hi)| (lo, hi, false));
        match kind {
            ObjectKind::Word(big) => charwise(self.word_object(ia, count, big)),
            ObjectKind::Pair(open, close) => self.pair_object(ia, open, close),
            ObjectKind::Quote(q) => charwise(self.quote_object(ia, q)),
            ObjectKind::Sentence => charwise(self.sentence_object(ia, count)),
            ObjectKind::Paragraph => self.paragraph_object(ia, count), // linewise
            // Tree-sitter objects need to query the (mutable) syntax engine, so they
            // resolve through the `&mut self` path [`Editor::ts_text_object_range`],
            // called by the executor *before* this `&self` resolver. Never here.
            ObjectKind::TsCapture(_) => None,
        }
    }

    /// Resolve a tree-sitter text object's charwise byte range `[start, end)` from
    /// the syntax engine. `ia` is `'i'` (inner) / `'a'` (around); `base` is the
    /// `textobjects.scm` capture base (`"function"`, `"parameter"`, `"comment"`,
    /// `"class"`). Selects the `count`-th **innermost** `<base>.<inner|outer>` node
    /// containing the cursor, so `2if` targets the 2nd enclosing function. When an
    /// `.inner` capture matches nothing it falls back to `.outer` (upstream queries
    /// often omit `@x.inner`, e.g. rust's `@comment`). `None` when no such object
    /// surrounds the cursor (or there is no grammar / `textobjects.scm`) — the
    /// executor then keeps the visual selection / no-ops the operator, as for any
    /// absent object. Always charwise (`linewise = false`).
    pub(crate) fn ts_text_object_range(
        &mut self,
        ia: char,
        base: &str,
        count: usize,
    ) -> Option<(usize, usize, bool)> {
        let buf = self.current_buffer_id();
        let byte = self.cursor_char();
        let suffix = if ia == 'i' { "inner" } else { "outer" };
        let mut ranges = self.ts_text_objects_at(buf, &format!("{base}.{suffix}"), byte);
        if ranges.is_empty() && ia == 'i' {
            ranges = self.ts_text_objects_at(buf, &format!("{base}.outer"), byte);
        }
        let (lo, hi) = ranges.into_iter().nth(count.saturating_sub(1))?;
        Some((lo, hi, false))
    }

    /// Resolve a tree-sitter text object from an **explicit** capture name (a user
    /// registry entry — `btv.textobject.map`), e.g. `"loop.inner"` or
    /// `"function.around"`. Unlike [`ts_text_object_range`], the capture is used
    /// verbatim — no `i`/`a` → `.inner`/`.outer` suffixing and no inner→outer
    /// fallback — so the user's chosen convention (bemtvi's `.inner`/`.outer`, Helix's
    /// `.inside`/`.around`, or anything their query defines) is honored exactly. A
    /// leading `@` is stripped (both `"@loop.inner"` and `"loop.inner"` work). Picks
    /// the `count`-th innermost containing region; always charwise.
    ///
    /// [`ts_text_object_range`]: Self::ts_text_object_range
    fn ts_text_object_range_capture(
        &mut self,
        capture: &str,
        count: usize,
    ) -> Option<(usize, usize, bool)> {
        let capture = capture.strip_prefix('@').unwrap_or(capture);
        let buf = self.current_buffer_id();
        let byte = self.cursor_char();
        let (lo, hi) = self
            .ts_text_objects_at(buf, capture, byte)
            .into_iter()
            .nth(count.saturating_sub(1))?;
        Some((lo, hi, false))
    }

    /// Resolve the object named by the introducer `ia` (`'i'`/`'a'`) and the object
    /// key `key` to its byte range. The single dispatch both the executor and the
    /// multi-cursor path use. Resolution order:
    ///
    /// 1. A **user registry** entry for the full `{ia}{key}` sequence
    ///    (`btv.textobject.map`) — checked first, so it can bind new keys *and*
    ///    override a built-in (e.g. remap `if` to a Helix `@function.inside`).
    /// 2. The **built-in** object alphabet ([`ObjectKind::from_key`]): the four
    ///    tree-sitter objects (`f`/`a`/`c`/`t`) and the vim objects (word, pairs,
    ///    quotes, sentence, paragraph).
    /// 3. `None` for an unknown key — the executor then cancels the operator / keeps
    ///    the visual selection, exactly as for any object that matches nothing.
    pub(crate) fn resolve_text_object(
        &mut self,
        ia: char,
        key: char,
        count: usize,
    ) -> Option<(usize, usize, bool)> {
        if let Some(capture) = self.textobject_map.get(&format!("{ia}{key}")).cloned() {
            return self.ts_text_object_range_capture(&capture, count);
        }
        match ObjectKind::from_key(key) {
            Some(ObjectKind::TsCapture(base)) => self.ts_text_object_range(ia, base, count),
            Some(kind) => self.text_object_range(ia, kind, count),
            None => None,
        }
    }

    /// Register (or, with `capture = None`, unregister) a user tree-sitter text
    /// object: bind the full `i`/`a` + object-key sequence `lhs` (`"il"`, `"af"`, …)
    /// to the exact `textobjects.scm` capture to select. Set from Lua via
    /// `btv.textobject.map`; consulted by [`resolve_text_object`](Self::resolve_text_object).
    pub fn set_textobject_map(&mut self, lhs: &str, capture: Option<String>) {
        match capture {
            Some(c) => {
                self.textobject_map.insert(lhs.to_string(), c);
            }
            None => {
                self.textobject_map.remove(lhs);
            }
        }
    }

    /// The user text-object registry entries whose introducer is `ia` (`'i'`/`'a'`),
    /// as `(object_key, capture)` pairs sorted by key — for the which-key object menu
    /// ([`Editor::command_pending`]), so registered objects appear alongside the
    /// built-ins under the `i`/`a` introducer.
    pub(crate) fn textobject_map_entries(&self, ia: char) -> Vec<(char, String)> {
        let mut out: Vec<(char, String)> = self
            .textobject_map
            .iter()
            .filter_map(|(lhs, cap)| {
                let mut chars = lhs.chars();
                match (chars.next(), chars.next(), chars.next()) {
                    (Some(i), Some(k), None) if i == ia => Some((k, cap.clone())),
                    _ => None,
                }
            })
            .collect();
        out.sort_by_key(|(k, _)| *k);
        out
    }

    /// Apply the pending operator (or extend the visual selection) to a text
    /// object's range `[lo, hi)`. `linewise` objects (paragraph) select whole
    /// lines; charwise objects span an exact byte range.
    pub(crate) fn apply_text_object(&mut self, lo: usize, hi: usize, linewise: bool) {
        if self.mode.is_visual() {
            if linewise {
                self.mode = Mode::VisualLine;
                self.set_visual_span_lines(lo, hi);
            } else {
                // A linewise-visual charwise object (e.g. `viw`) drops to
                // charwise, like vim.
                if self.mode == Mode::VisualLine {
                    self.mode = Mode::Visual;
                }
                self.set_visual_span(lo, hi);
            }
            self.pending.stage = Stage::Start;
            return;
        }
        if let Some(op) = self.pending.operator.take() {
            if linewise {
                let first_line = self.buffer().byte_to_line(lo);
                self.apply_operator_to_range(op, lo, hi, true, first_line);
            } else {
                self.apply_operator_to_range(op, lo, hi, false, 0);
            }
        }
        self.reset_pending();
    }

    /// Set a linewise visual selection spanning the lines covered by the
    /// byte range `[lo, hi)` (with `hi` a line-start boundary just past the
    /// last line). Anchor on the first line, cursor on the last.
    fn set_visual_span_lines(&mut self, lo: usize, hi: usize) {
        let first = self.buffer().byte_to_line(lo);
        let last = self.buffer().byte_to_line(hi.saturating_sub(1));
        self.visual_anchor = Cursor {
            line: first,
            col: 0,
        };
        self.cursor = Cursor { line: last, col: 0 };
        self.clamp_cursor();
    }

    /// Set the visual selection to span `[lo, hi)`: anchor at the first char,
    /// live cursor on the last char (inclusive). Empty ranges park both at `lo`.
    pub(crate) fn set_visual_span(&mut self, lo: usize, hi: usize) {
        self.set_cursor_char(lo);
        self.visual_anchor = self.cursor;
        let end = if hi > lo {
            self.prev_grapheme_idx(hi)
        } else {
            lo
        };
        self.set_cursor_char(end);
    }

    /// Classify the char at `idx` for span-finding. `big = false` uses the
    /// three-way `char_class` (`0` blank, `1` word, `2` punct); `big = true`
    /// collapses to blank (`0`) vs non-blank (`1`) — vim's `WORD`.
    fn span_class(&self, idx: usize, big: bool) -> u8 {
        match char_class(self.char_at(idx)) {
            CharClass::Blank => 0,
            CharClass::Word => 1,
            CharClass::Punct => {
                if big {
                    1
                } else {
                    2
                }
            }
        }
    }

    /// `[start, end)` of the maximal run around `idx` of chars sharing its
    /// `span_class`. Buffer-wide; `end` never passes the trailing phantom `\n`.
    pub(crate) fn class_span(&self, idx: usize, big: bool) -> (usize, usize) {
        let last = self.last_char_idx();
        let idx = idx.min(last);
        let cls = self.span_class(idx, big);
        let mut start = idx;
        while start > 0 {
            let prev = self.prev_grapheme_idx(start);
            if self.span_class(prev, big) != cls {
                break;
            }
            start = prev;
        }
        let mut end = idx;
        while end < last && self.span_class(end, big) == cls {
            end = self.next_grapheme_idx(end);
        }
        (start, end)
    }

    /// `iw`/`aw` (`big = false`) and `iW`/`aW` (`big = true`). `iw` is the run
    /// under the cursor; `aw` adds the trailing whitespace (or, if there is
    /// none, the leading whitespace). `count` extends over successive spans.
    fn word_object(&self, ia: char, count: usize, big: bool) -> Option<(usize, usize)> {
        let last = self.last_char_idx();
        let cur = self.cursor_char();
        let (start, end0) = self.class_span(cur, big);

        if ia == 'i' {
            let mut end = end0;
            let mut remaining = count.saturating_sub(1);
            while remaining > 0 && end < last {
                let (_, e2) = self.class_span(end, big);
                if e2 == end {
                    break;
                }
                end = e2;
                remaining -= 1;
            }
            return Some((start, end));
        }

        // `aw`: span plus surrounding whitespace.
        let mut start = start;
        let mut end = end0;
        let mut took_trailing = false;
        let mut units = count;

        // Starting on whitespace: the first unit is the blank run plus the
        // following word.
        if self.span_class(cur, big) == 0 {
            if end < last {
                let (_, e2) = self.class_span(end, big);
                end = e2;
            }
            took_trailing = true; // leading whitespace already covered
            units = units.saturating_sub(1);
        }

        while units > 0 {
            // Trailing whitespace after the current word.
            if end < last && self.span_class(end, big) == 0 {
                let (_, e2) = self.class_span(end, big);
                end = e2;
                took_trailing = true;
            } else {
                took_trailing = false;
            }
            units -= 1;
            // Another word follows for a further count.
            if units > 0 && end < last {
                let (_, e2) = self.class_span(end, big);
                end = e2;
            }
        }

        // No trailing whitespace was consumed: take the leading whitespace.
        if !took_trailing && start > 0 {
            let prev = self.prev_grapheme_idx(start);
            if self.span_class(prev, big) == 0 {
                let (s2, _) = self.class_span(prev, big);
                start = s2;
            }
        }
        Some((start, end))
    }

    /// `i(`/`a(` and friends. Find the innermost `open`/`close` pair enclosing
    /// the cursor; `i` excludes the brackets, `a` includes them. The bool is
    /// the linewise flag (see the inner promotion below).
    fn pair_object(&self, ia: char, open: char, close: char) -> Option<(usize, usize, bool)> {
        let open_idx = self.find_unmatched_open(open, close, self.cursor_char())?;
        let close_idx = self.find_match_close(open, close, open_idx)?;

        if ia == 'i' {
            // Linewise promotion: when the inner content is whole lines — the
            // open bracket ends its line and the close bracket starts its line
            // (modulo surrounding whitespace) — `i{`/`i(`/… select the lines
            // *between* the brackets, linewise, leaving the bracket lines. vim
            // keeps the object charwise in visual mode, so skip it there.
            let open_line = self.buffer().byte_to_line(open_idx);
            let close_line = self.buffer().byte_to_line(close_idx);
            if !self.mode.is_visual()
                && close_line > open_line
                && self.rest_of_line_blank(open_idx)
                && self.start_of_line_blank(close_idx)
            {
                let lo = self.buffer().line_start(open_line + 1);
                let hi = self.buffer().line_start(close_line);
                return Some((lo, hi, true));
            }
            return Some((self.next_grapheme_idx(open_idx), close_idx, false));
        }
        Some((open_idx, self.next_grapheme_idx(close_idx), false))
    }

    /// True if everything after byte `idx` to the end of its line is blank.
    fn rest_of_line_blank(&self, idx: usize) -> bool {
        let line = self.buffer().byte_to_line(idx);
        let end = self.buffer().line_start(line) + self.buffer().line_len(line);
        let mut i = self.next_grapheme_idx(idx);
        while i < end {
            if !matches!(self.char_at(i), ' ' | '\t') {
                return false;
            }
            i = self.next_grapheme_idx(i);
        }
        true
    }

    /// True if everything before byte `idx` from its line start is blank.
    fn start_of_line_blank(&self, idx: usize) -> bool {
        let mut i = self.buffer().line_start(self.buffer().byte_to_line(idx));
        while i < idx {
            if !matches!(self.char_at(i), ' ' | '\t') {
                return false;
            }
            i = self.next_grapheme_idx(i);
        }
        true
    }

    /// Scan backward from `from` (inclusive) for the `open` bracket that
    /// encloses it, honoring nesting. A `close` on `from` itself is the closing
    /// half of the wanted pair, so it is not counted.
    pub(crate) fn find_unmatched_open(
        &self,
        open: char,
        close: char,
        from: usize,
    ) -> Option<usize> {
        let mut idx = from;
        let mut depth = 0i32;
        loop {
            let c = self.char_at(idx);
            if c == close && idx != from {
                depth += 1;
            } else if c == open {
                if depth == 0 {
                    return Some(idx);
                }
                depth -= 1;
            }
            if idx == 0 {
                return None;
            }
            idx = self.prev_grapheme_idx(idx);
        }
    }

    /// Scan forward from `open_idx` for its matching `close`, honoring nesting.
    pub(crate) fn find_match_close(
        &self,
        open: char,
        close: char,
        open_idx: usize,
    ) -> Option<usize> {
        let last = self.last_char_idx();
        let mut idx = open_idx;
        let mut depth = 0i32;
        loop {
            let c = self.char_at(idx);
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(idx);
                }
            }
            if idx >= last {
                return None;
            }
            idx = self.next_grapheme_idx(idx);
        }
    }

    /// `i"`/`a"` (and `'`, `` ` ``). Quote objects are confined to the cursor's
    /// line: quotes are paired left-to-right (1st–2nd, 3rd–4th, …) and the pair
    /// chosen is the one enclosing the cursor, else the first pair beginning at
    /// or after it. `i"` is the text between the quotes; `a"` includes the
    /// quotes plus the trailing whitespace (or, if none, the leading).
    ///
    /// A backslash escapes the following byte (vim's `quoteescape`), so `\"` is
    /// not a delimiter and `\\` is a literal backslash followed by a real quote.
    fn quote_object(&self, ia: char, quote: char) -> Option<(usize, usize)> {
        let line = self.cursor.line;
        let base = self.buffer().line_start(line);
        let s = self.buffer().line(line);
        let bytes = s.as_bytes();
        let q = quote as u8;

        // Quote positions on the line, paired in order. A `\` consumes the next
        // byte, so escaped quotes are skipped (and `\\` leaves the quote real).
        let mut quotes: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == q {
                quotes.push(i);
            }
            i += 1;
        }
        let cursor_rel = self.cursor_char() - base;

        // First left-to-right pair whose closing quote is at or after the
        // cursor: it either encloses the cursor or is the next one ahead.
        let (open, close) = quotes
            .chunks_exact(2)
            .map(|p| (p[0], p[1]))
            .find(|&(_, c)| cursor_rel <= c)
            // Dangling-quote fallback (odd quote count, e.g. `"trib"uto"`): the
            // cursor is past the last complete pair but flanked by quotes, so
            // pair the quote before it with the next one. This lets `i"`/`a"`
            // work on both sides of a shared quote. (`"a" "b"`-style gaps still
            // hit the normal path above and seek forward to the next string.)
            .or_else(|| {
                let l = *quotes.iter().rev().find(|&&p| p < cursor_rel)?;
                let r = *quotes.iter().find(|&&p| p >= cursor_rel)?;
                Some((l, r))
            })?;

        let (lo_rel, hi_rel) = match ia {
            'i' => (open + 1, close),
            _ => {
                // `a"`: include the quotes, then the trailing whitespace.
                let is_blank = |i: usize| bytes[i] == b' ' || bytes[i] == b'\t';
                let mut hi = close + 1;
                let trail_start = hi;
                while hi < bytes.len() && is_blank(hi) {
                    hi += 1;
                }
                if hi > trail_start {
                    (open, hi)
                } else {
                    // No trailing whitespace: take the leading whitespace.
                    let mut lo = open;
                    while lo > 0 && is_blank(lo - 1) {
                        lo -= 1;
                    }
                    (lo, hi)
                }
            }
        };
        Some((base + lo_rel, base + hi_rel))
    }

    /// `ip`/`ap` — a paragraph: a run of non-blank lines, or a run of blank
    /// lines, delimited by the other kind. `ap` adds the trailing blank lines
    /// (or, if none, the leading blank lines). Linewise; `count` extends over
    /// successive blocks.
    fn paragraph_object(&self, ia: char, count: usize) -> Option<(usize, usize, bool)> {
        let total = self.buffer().line_count();
        if total == 0 {
            return None;
        }
        let blank = |l: usize| self.buffer().line_len(l) == 0;
        let cur = self.cursor.line.min(total - 1);
        let on_blank = blank(cur);

        let mut first = cur;
        while first > 0 && blank(first - 1) == on_blank {
            first -= 1;
        }
        let mut last = cur;
        while last + 1 < total && blank(last + 1) == on_blank {
            last += 1;
        }

        // `count` extends over further blocks, alternating blank/non-blank.
        let mut kind = on_blank;
        for _ in 1..count {
            if last + 1 >= total {
                break;
            }
            kind = !kind;
            while last + 1 < total && blank(last + 1) == kind {
                last += 1;
            }
        }

        if ia == 'a' {
            // Include the trailing block of the opposite kind (blank lines after
            // a normal paragraph); if there is none, take the preceding one.
            let trailing = !blank(last);
            if last + 1 < total && blank(last + 1) == trailing {
                while last + 1 < total && blank(last + 1) == trailing {
                    last += 1;
                }
            } else {
                let leading = !blank(first);
                while first > 0 && blank(first - 1) == leading {
                    first -= 1;
                }
            }
        }

        let lo = self.buffer().line_start(first);
        let hi = self.buffer().line_start((last + 1).min(total));
        Some((lo, hi, true))
    }

    /// Byte bounds `[start, end)` of the paragraph (block of non-blank lines)
    /// at the cursor; just the current line if the cursor is on a blank line.
    fn current_paragraph_bytes(&self) -> (usize, usize) {
        let total = self.buffer().line_count();
        if total == 0 {
            return (0, 0);
        }
        let blank = |l: usize| self.buffer().line_len(l) == 0;
        let cur = self.cursor.line.min(total - 1);
        let (mut first, mut last) = (cur, cur);
        if !blank(cur) {
            while first > 0 && !blank(first - 1) {
                first -= 1;
            }
            while last + 1 < total && !blank(last + 1) {
                last += 1;
            }
        }
        let start = self.buffer().line_start(first);
        let end = self.buffer().line_start((last + 1).min(total));
        (start, end)
    }

    /// `is`/`as` — a sentence: text ending at `.`/`!`/`?` (optionally followed
    /// by closing `)]"'`) and a space/tab or end of line. Bounded by the
    /// surrounding paragraph. `is` is the sentence text; `as` adds the trailing
    /// whitespace (or, if none, leading). Charwise; `count` extends over
    /// successive sentences.
    fn sentence_object(&self, ia: char, count: usize) -> Option<(usize, usize)> {
        let (p_start, p_end) = self.current_paragraph_bytes();
        if p_start >= p_end {
            return None;
        }
        let is_ws = |c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r';
        let is_close = |c: char| matches!(c, ')' | ']' | '"' | '\'');

        // Each sentence as `(start, text_end, ws_end)`.
        let mut segs: Vec<(usize, usize, usize)> = Vec::new();
        let mut s = p_start;
        while s < p_end && is_ws(self.char_at(s)) {
            s = self.next_grapheme_idx(s);
        }
        let mut i = s;
        while i < p_end {
            let c = self.char_at(i);
            if c == '.' || c == '!' || c == '?' {
                let mut j = self.next_grapheme_idx(i);
                while j < p_end && is_close(self.char_at(j)) {
                    j = self.next_grapheme_idx(j);
                }
                if j >= p_end || is_ws(self.char_at(j)) {
                    let text_end = j;
                    let mut k = j;
                    while k < p_end && is_ws(self.char_at(k)) {
                        k = self.next_grapheme_idx(k);
                    }
                    segs.push((s, text_end, k));
                    s = k;
                    i = k;
                    continue;
                }
            }
            i = self.next_grapheme_idx(i);
        }
        // Trailing run with no terminator.
        if s < p_end {
            let mut text_end = p_end;
            while text_end > s && is_ws(self.char_at(self.prev_grapheme_idx(text_end))) {
                text_end = self.prev_grapheme_idx(text_end);
            }
            segs.push((s, text_end, p_end));
        }
        if segs.is_empty() {
            return None;
        }

        let cursor = self.cursor_char();
        let mut idx = 0;
        for (n, &(start, _, _)) in segs.iter().enumerate() {
            if start <= cursor {
                idx = n;
            }
        }
        let start = segs[idx].0;
        let end = (idx + count.max(1) - 1).min(segs.len() - 1);
        let (_, text_end, ws_end) = segs[end];

        if ia == 'i' {
            Some((start, text_end))
        } else if ws_end > text_end {
            Some((start, ws_end))
        } else {
            // No trailing whitespace: take the leading whitespace instead.
            let mut lo = start;
            while lo > p_start && is_ws(self.char_at(self.prev_grapheme_idx(lo))) {
                lo = self.prev_grapheme_idx(lo);
            }
            Some((lo, ws_end))
        }
    }
}
