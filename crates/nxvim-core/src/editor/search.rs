//! Search: pattern compilation, `/`/`?`/`n`/`N`, `incsearch` preview, and the
//! `hlsearch` match spans projected into the View.

use super::*;
use crate::buffer::Buffer;
use crate::mode::Mode;
use crate::options::RegexSyntax;
use crate::search::{RegexEngine, SearchRegex};
use crate::unicode;

/// A search match as whole-buffer byte offsets, `(start, end)` (end exclusive).
type MatchRange = (usize, usize);

/// Per visible row, the screen-column spans of every search match on that row
/// (the `Search`/`hlsearch` highlight). Empty inner vec for rows with no match.
pub(crate) type SearchSpans = Vec<Vec<(usize, usize)>>;

/// Per visible row, the single span the live `incsearch` preview rests on (the
/// `IncSearch` highlight), or `None`.
pub(crate) type IncSearchSpans = Vec<Option<(usize, usize)>>;

/// Which direction a `/` (forward) or `?` (backward) search runs in. Stored with
/// the last search so `n` repeats it and `N` inverts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchDir {
    Forward,
    Backward,
}

impl SearchDir {
    fn opposite(self) -> SearchDir {
        match self {
            SearchDir::Forward => SearchDir::Backward,
            SearchDir::Backward => SearchDir::Forward,
        }
    }

    /// The command-line prompt character (`/` forward, `?` backward).
    pub(crate) fn prefix(self) -> char {
        match self {
            SearchDir::Forward => '/',
            SearchDir::Backward => '?',
        }
    }
}

/// A search offset — the `e`/`s`/`b`/line suffix vim allows after the pattern
/// (`/pat/e`, `/pat/s-2`, `/pat/+3`). It repositions the cursor relative to the
/// match and, used as an operator motion, sets the motion's inclusiveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchOffset {
    /// No offset: land on the match start (exclusive motion).
    None,
    /// `s`/`b` start offset: `n` characters from the match start (exclusive).
    Start(isize),
    /// `e` end offset: `n` characters from the match's last char (inclusive).
    End(isize),
    /// A bare `[+-]n` line offset: `n` lines from the match's line (linewise).
    Line(isize),
}

impl SearchOffset {
    /// How a search resolves as an operator motion: `e` includes the match end,
    /// a line offset goes linewise, everything else stops short of the match.
    fn motion_kind(self) -> MotionKind {
        match self {
            SearchOffset::End(_) => MotionKind::Inclusive,
            SearchOffset::Line(_) => MotionKind::Linewise,
            _ => MotionKind::Exclusive,
        }
    }
}

/// Split a submitted search line into its pattern and trailing offset on the
/// **last unescaped** separator `sep` (`/` for a forward search, `?` for
/// backward), per vim's `/pat/e`, `/pat/+2` syntax. A `\`-escaped separator stays
/// in the pattern; with no separator the whole line is the pattern.
fn split_search_offset(line: &str, sep: char) -> (String, SearchOffset) {
    let chars: Vec<char> = line.chars().collect();
    let mut at = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' {
            i += 2; // skip the escaped char
            continue;
        }
        if chars[i] == sep {
            at = Some(i);
        }
        i += 1;
    }
    match at {
        Some(p) => (
            chars[..p].iter().collect(),
            parse_offset(&chars[p + 1..].iter().collect::<String>()),
        ),
        None => (line.to_string(), SearchOffset::None),
    }
}

/// Parse the text after a search separator into a [`SearchOffset`]: `e`/`s`/`b`
/// (optionally `+n`/`-n`/`n`) are character offsets; a bare `[+-]n` is a line
/// offset; anything else is no offset.
fn parse_offset(s: &str) -> SearchOffset {
    let s = s.trim();
    let mut it = s.chars();
    match it.next() {
        Some('e') => SearchOffset::End(parse_signed(it.as_str()).unwrap_or(0)),
        Some('s') | Some('b') => SearchOffset::Start(parse_signed(it.as_str()).unwrap_or(0)),
        Some(c) if c == '+' || c == '-' || c.is_ascii_digit() => {
            parse_signed(s).map_or(SearchOffset::None, SearchOffset::Line)
        }
        _ => SearchOffset::None,
    }
}

/// Parse an optionally-signed magnitude. A lone `+`/`-` is `±1` (vim's `e+` means
/// `e+1`); an empty string is `None`.
fn parse_signed(s: &str) -> Option<isize> {
    let s = s.trim();
    let (sign, digits) = match s.strip_prefix('-') {
        Some(d) => (-1, d),
        None => (1, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() {
        return (s == "+" || s == "-").then_some(sign);
    }
    digits.parse::<isize>().ok().map(|n| sign * n)
}

impl Editor {
    /// Run a search submitted from the `/`,`?` command line. The line is split on
    /// its last unescaped separator into a pattern and a trailing offset
    /// (`/pat/e`). An empty pattern repeats the last search (keeping its pattern,
    /// the just-typed direction, and — unless this line carries its own separator
    /// — its offset); with no previous pattern that is `E35`. The count prefixed
    /// onto the opening `/`,`?` finds the Nth match. A pending operator (`d/`)
    /// applies over the match instead of moving.
    pub(crate) fn submit_search(&mut self, line: &str, dir: SearchDir) {
        let (core, off) = split_search_offset(line, dir.prefix());
        let had_sep = core.len() != line.len();
        let pattern = if core.is_empty() {
            match &self.last_search {
                Some((p, _, _)) => p.clone(),
                None => {
                    self.echo("E35: No previous regular expression");
                    return;
                }
            }
        } else {
            core.clone()
        };
        // A bare empty line repeats verbatim (offset included); any explicit
        // separator — even `//e` over an empty pattern — sets a fresh offset.
        let offset = if had_sep || !core.is_empty() {
            off
        } else {
            self.last_search
                .as_ref()
                .map_or(SearchOffset::None, |(_, _, o)| *o)
        };
        self.remember_search(&pattern);
        self.last_search = Some((pattern.clone(), dir, offset));
        let op = self.search_operator.take();
        let count = self.pending_search_count.max(1);
        self.run_search(&pattern, dir, offset, count, op);
    }

    /// Record a submitted pattern in the search history, skipping a consecutive
    /// duplicate (vim collapses repeats), then trim to the `'history'` cap.
    fn remember_search(&mut self, pattern: &str) {
        if self.search_history.last().map(String::as_str) != Some(pattern) {
            self.search_history.push(pattern.to_string());
            cap_ring(&mut self.search_history, self.options.history);
        }
    }

    /// Record an interactively-submitted `:` command in the ex history, skipping
    /// an empty line or a consecutive duplicate (vim collapses repeats), then trim to
    /// the `'history'` cap.
    pub(crate) fn remember_ex(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if !cmd.is_empty() && self.ex_history.last().map(String::as_str) != Some(cmd) {
            self.ex_history.push(cmd.to_string());
            cap_ring(&mut self.ex_history, self.options.history);
        }
    }

    /// Trim every history ring (ex, search, and each `nx.ui.input` namespace ring) to
    /// the newest `'history'` entries. Used when the option is lowered and after a
    /// shada merge seeds the rings; the per-push `remember_*` paths trim inline.
    pub(crate) fn cap_history(&mut self) {
        let cap = self.options.history;
        cap_ring(&mut self.ex_history, cap);
        cap_ring(&mut self.search_history, cap);
        for ring in self.prompt_history.values_mut() {
            cap_ring(ring, cap);
        }
    }

    /// `n` (same direction) / `N` (opposite) — repeat the last search `count`
    /// times, reusing its offset. `E35` when there is no last search.
    pub(crate) fn search_repeat(&mut self, same: bool, count: usize) {
        let Some((pattern, last_dir, offset)) = self.last_search.clone() else {
            self.echo("E35: No previous regular expression");
            return;
        };
        let dir = if same { last_dir } else { last_dir.opposite() };
        // In Helix a search leaves the whole match *selected*, so the head sits on
        // its last char. A repeat must start from the selection *edge* the search
        // is heading away from — the end for a forward repeat, the start for a
        // backward one — else a backward `N` from the head re-finds the very match
        // the cursor still sits inside.
        if self.mode.is_helix() {
            let r = self.selections().primary();
            let (lo, hi) = {
                let a = self.anchor_byte(r.anchor);
                let h = self.anchor_byte(r.head);
                (a.min(h), a.max(h))
            };
            self.cursor = self.cursor_at_byte(match dir {
                SearchDir::Forward => hi,
                SearchDir::Backward => lo,
            });
        }
        self.run_search(&pattern, dir, offset, count.max(1), None);
    }

    /// `*`/`#` (and `g*`/`g#`): search for the word under the cursor — forward for
    /// `*`, backward for `#`. `bounded` wraps it in `\b…\b` (the plain `*`/`#`,
    /// whole-word) versus a bare substring (`g*`/`g#`). `E348` with no word under
    /// the cursor.
    pub(crate) fn search_word_under_cursor(&mut self, dir: SearchDir, bounded: bool, count: usize) {
        let Some(word) = self.word_under_cursor() else {
            self.echo("E348: No string under cursor");
            return;
        };
        let pattern = if bounded {
            // The whole-word boundaries are spelled in the active engine's
            // dialect: vim's magic dialect has no `\b` (it reads as a literal
            // that matches nothing), its word boundaries are `\<` / `\>`.
            match self.search_engine() {
                RegexEngine::Vim => format!(r"\<{word}\>"),
                RegexEngine::Pcre => format!(r"\b{word}\b"),
            }
        } else {
            word
        };
        self.remember_search(&pattern);
        self.last_search = Some((pattern.clone(), dir, SearchOffset::None));
        self.run_search(&pattern, dir, SearchOffset::None, count.max(1), None);
    }

    /// The keyword (alphanumerics + `_`) under the cursor, or the next one on the
    /// line if the cursor sits on a non-word char; `None` if the line has none
    /// from the cursor on. Drives `*`/`#`.
    /// The keyword (`[A-Za-z0-9_]`) the cursor sits on, or the next one to its
    /// right on the same line — vim's `<cword>`. Backs `*`/`#` search and the
    /// `<C-r><C-w>` register-insert pseudo-register. `None` when no keyword
    /// follows the cursor on the line.
    pub(crate) fn word_under_cursor(&self) -> Option<String> {
        let line = self.buffer().line(self.cursor.line);
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let is_word = |c: char| c.is_alphanumeric() || c == '_';
        // First word char at or after the cursor column.
        let start = chars.iter().position(|(b, _)| *b >= self.cursor.col)?;
        let mut k = start;
        while k < chars.len() && !is_word(chars[k].1) {
            k += 1;
        }
        if k >= chars.len() {
            return None;
        }
        // If the cursor itself was on the word, take it from its start.
        let on_word = chars
            .get(start)
            .is_some_and(|(b, c)| *b == self.cursor.col && is_word(*c));
        let mut lo = k;
        if on_word {
            while lo > 0 && is_word(chars[lo - 1].1) {
                lo -= 1;
            }
        }
        let mut hi = k;
        while hi < chars.len() && is_word(chars[hi].1) {
            hi += 1;
        }
        Some(chars[lo..hi].iter().map(|(_, c)| *c).collect())
    }

    /// Whether this search should ignore case by *option*: `ignorecase`, unless
    /// `smartcase` is also on and the pattern carries an uppercase character (then
    /// it stays case-sensitive). This is the default the regex compiler starts
    /// from; an embedded `\c`/`\C` in the pattern overrides it.
    pub(crate) fn search_ignorecase(&self, pattern: &str) -> bool {
        // A Helix session defaults to its own smart-case, self-contained from the
        // global `'ignorecase'`/`'smartcase'` (which vim-mode search reads and this
        // never touches). Gated on the session flag, not the mode, so the live
        // incsearch preview — typed with the command line in `Mode::Command`, not a
        // Helix mode — stays consistent with the committed search.
        if self.helix && self.helix_smart_case {
            return !pattern.chars().any(|c| c.is_uppercase());
        }
        self.options.ignorecase
            && !(self.options.smartcase && pattern.chars().any(|c| c.is_uppercase()))
    }

    /// Which regex dialect `/` search and `:substitute` speak — the current
    /// buffer's `'regexsyntax'` override if it pinned one, else the global
    /// `'regexsyntax'`. `"vim"` → the native vim engine, anything else (the
    /// default `"pcre"`) → the canonical-regex engine. Both layers are validated
    /// to those two values, so an unexpected global string safely reads as `Pcre`.
    pub(crate) fn search_engine(&self) -> RegexEngine {
        match self.resolve_regexsyntax(self.buffer().options.regexsyntax) {
            "vim" => RegexEngine::Vim,
            _ => RegexEngine::Pcre,
        }
    }

    /// Resolve a buffer-local `'regexsyntax'` value to the effective dialect name
    /// (`"pcre"`/`"vim"`): an explicit local override wins, else the global
    /// `'regexsyntax'` (itself validated, so an unexpected string reads as
    /// `"pcre"`). The one place the global-local fallback lives — used by
    /// [`search_engine`](Self::search_engine), `:set regexsyntax?`, and the
    /// `vim.bo` mirror.
    pub fn resolve_regexsyntax(&self, local: RegexSyntax) -> &'static str {
        match local {
            RegexSyntax::Pcre => "pcre",
            RegexSyntax::Vim => "vim",
            RegexSyntax::Inherit if self.options.regexsyntax == "vim" => "vim",
            RegexSyntax::Inherit => "pcre",
        }
    }

    /// The effective `'regexsyntax'` dialect for the current buffer — its override
    /// resolved against the global — for `:set regexsyntax?`.
    pub(crate) fn effective_regexsyntax(&self) -> &'static str {
        self.resolve_regexsyntax(self.buffer().options.regexsyntax)
    }

    /// Compile `pattern` with this editor's case options and active regex engine.
    pub(crate) fn compile_search(&self, pattern: &str) -> Result<SearchRegex, String> {
        SearchRegex::compile(
            pattern,
            self.search_ignorecase(pattern),
            self.search_engine(),
        )
    }

    /// Like [`compile_search`](Self::compile_search), but reuses the last
    /// compiled regex when the `(pattern, ignorecase, engine)` triple is
    /// unchanged. Used on the redraw highlight path, where the `hlsearch` pattern
    /// is stable across many frames and recompiling it every repaint (per window)
    /// is the dominant cost. Returns a shared handle so the cache can keep its
    /// copy.
    fn compile_search_cached(&self, pattern: &str) -> Result<Rc<SearchRegex>, String> {
        let ignorecase = self.search_ignorecase(pattern);
        let engine = self.search_engine();
        if let Some((p, ic, eng, re)) = self.search_re_cache.borrow().as_ref() {
            if p == pattern && *ic == ignorecase && *eng == engine {
                return Ok(Rc::clone(re));
            }
        }
        let re = Rc::new(SearchRegex::compile(pattern, ignorecase, engine)?);
        *self.search_re_cache.borrow_mut() =
            Some((pattern.to_string(), ignorecase, engine, Rc::clone(&re)));
        Ok(re)
    }

    /// The next match of the compiled `re` in `dir` from byte offset `from`, as
    /// `(primary, wrapped)` whole-buffer `(start, end)` ranges. `primary` is the
    /// match in the search direction without wrapping; `wrapped` is the first
    /// match from the opposite end (used when `wrapscan` lets the search wrap).
    /// Forward starts one grapheme past `from` so a match *under* it isn't an
    /// immediate self-hit; backward looks left of it. Matching is line-by-line;
    /// side-effect free (the shared core of `run_search` and the incsearch
    /// preview).
    /// The wrap scan is computed **lazily**: both callers use it only when the
    /// primary match misses, and this runs per `n`-step / per incsearch
    /// keystroke — an unconditional second full scan from the buffer's far end
    /// would be pure waste on every hit.
    fn search_matches(
        &self,
        re: &SearchRegex,
        dir: SearchDir,
        from: usize,
    ) -> (Option<MatchRange>, Option<MatchRange>) {
        match dir {
            SearchDir::Forward => {
                let primary = self.match_forward_from(re, self.next_grapheme_idx(from));
                let wrapped = primary
                    .is_none()
                    .then(|| self.match_forward_from(re, 0))
                    .flatten();
                (primary, wrapped)
            }
            SearchDir::Backward => {
                let primary = self.match_backward_before(re, from);
                let wrapped = primary
                    .is_none()
                    .then(|| self.match_backward_before(re, self.buffer().len_bytes()))
                    .flatten();
                (primary, wrapped)
            }
        }
    }

    /// The first match of `re` whose start is at or after byte `start`, scanning
    /// lines downward to the end of the buffer, as a whole-buffer `(start, end)`
    /// range. Walks each line's non-overlapping match sequence (see
    /// `SearchRegex::find_from`), so a greedy pattern doesn't yield a match that
    /// overlaps the one the cursor already sits in. `None` if no match starts in
    /// `[start, end_of_buffer)`.
    fn match_forward_from(&self, re: &SearchRegex, start: usize) -> Option<MatchRange> {
        let buf = self.buffer();
        let line_count = buf.line_count();
        let mut line = buf.byte_to_line(start.min(self.last_char_idx()));
        let mut col = start.saturating_sub(buf.line_start(line));
        while line < line_count {
            let text = buf.line(line);
            if let Some((s, e)) = re.find_from(&text, col) {
                let base = buf.line_start(line);
                return Some((base + s, base + e));
            }
            line += 1;
            col = 0;
        }
        None
    }

    /// The last match of `re` that *starts* before byte `limit`, scanning lines
    /// upward to the top, as a whole-buffer `(start, end)` range. `None` if
    /// nothing matches before `limit`.
    fn match_backward_before(&self, re: &SearchRegex, limit: usize) -> Option<MatchRange> {
        let buf = self.buffer();
        let limit_line = buf.byte_to_line(limit.min(self.last_char_idx()));
        let limit_col = limit.saturating_sub(buf.line_start(limit_line));
        let mut line = limit_line as isize;
        while line >= 0 {
            let l = line as usize;
            let text = buf.line(l);
            // On the limit line a match must start strictly before the cursor;
            // earlier lines admit any match.
            let cap = if l == limit_line {
                limit_col
            } else {
                text.len() + 1
            };
            if let Some((s, e)) = re
                .find_all(&text)
                .into_iter()
                .take_while(|(s, _)| *s < cap)
                .last()
            {
                let base = buf.line_start(l);
                return Some((base + s, base + e));
            }
            line -= 1;
        }
        None
    }

    /// Find the `count`-th match of `pattern` (a standard regex) in `dir` from the
    /// cursor and act on it: move the cursor (repositioned by `offset`), or — when
    /// `op` is `Some` — apply that operator over the `[origin, match]` motion
    /// instead of moving. Sets the `/pattern` echo, or the BOTTOM/TOP notice when
    /// it wrapped. A miss is `E486` with `wrapscan` (or `E385`/`E384` without it),
    /// an uncompilable pattern `E383`; all leave the cursor unmoved.
    fn run_search(
        &mut self,
        pattern: &str,
        dir: SearchDir,
        offset: SearchOffset,
        count: usize,
        op: Option<char>,
    ) {
        if pattern.is_empty() {
            return;
        }
        // A committed search / `n` / `*` in Normal mode abandons a multi-cursor
        // session — navigating away collapses to the primary. In MULTICURSOR
        // placement mode the search instead *navigates to* a match so you can drop
        // a cursor there, so the placed cursors are kept. Helix **select** mode
        // (`v`) *accumulates* — a search adds each match as a new selection — so it
        // keeps the set too; its pre-search selections are captured now (before any
        // cursor move) to append onto below.
        let helix_add = op.is_none() && self.mode == Mode::HelixSelect;
        let pre_sel = helix_add.then(|| self.selections());
        if self.mode != Mode::MultiCursor && !helix_add {
            self.clear_secondary_cursors();
        }
        // A committed search turns on `hlsearch` highlighting (cleared by `:noh`).
        self.search_active = true;
        let re = match self.compile_search(pattern) {
            Ok(re) => re,
            Err(e) => {
                self.echo(e);
                return;
            }
        };
        // Walk `count` matches over a local cursor so a miss leaves the real one
        // put; the offset and any operator apply once, to the final match.
        let origin = self.cursor_char();
        let mut from = origin;
        let mut last = None;
        let mut wrapped = false;
        for _ in 0..count {
            let (primary, wrap) = self.search_matches(&re, dir, from);
            let (hit, this_wrap) = match primary {
                Some(r) => (r, false),
                None => match wrap.filter(|_| self.options.wrapscan) {
                    Some(r) => (r, true),
                    None => {
                        last = None;
                        break;
                    }
                },
            };
            wrapped = this_wrap;
            from = hit.0;
            last = Some(hit);
        }

        let Some((ms, me)) = last else {
            self.echo(if self.options.wrapscan {
                format!("E486: Pattern not found: {pattern}")
            } else {
                match dir {
                    SearchDir::Forward => {
                        format!("E385: search hit BOTTOM without match for: {pattern}")
                    }
                    SearchDir::Backward => {
                        format!("E384: search hit TOP without match for: {pattern}")
                    }
                }
            });
            return;
        };

        // A search is a jump: stash the pre-search cursor in the previous-context
        // mark (`` `` ``/`''`) before landing — but only a *movement* search, not
        // an operator's search-motion range (`d/pat`).
        if op.is_none() {
            self.record_jump_context();
        }
        self.place_with_offset(ms, me, offset);
        // In a Helix mode a document search acts on the whole match, not a point
        // cursor. In **select** mode (`v`) it *adds* the match as a new selection —
        // keeping every existing one — so a search / `n` accumulates a
        // multi-selection. In normal mode it *replaces* the selection with the match
        // (anchor at its start, head on its last grapheme). Only a plain movement
        // search re-selects; an operator's search-motion (`d/pat`) leaves the range
        // to `apply_operator` below.
        if op.is_none() && self.mode.is_helix() && me > ms {
            let match_range = Range {
                anchor: self.cursor_at_byte(ms),
                head: self.cursor_at_byte(self.prev_grapheme_idx(me)),
            };
            if let Some(mut sel) = pre_sel {
                sel.ranges.push(match_range);
                sel.primary = sel.ranges.len() - 1;
                self.set_selections(&sel);
            } else {
                self.visual_anchor = match_range.anchor;
                self.cursor = match_range.head;
            }
            self.clamp_cursor();
        }
        if let Some(op) = op {
            // The cursor now rests on the (offset-adjusted) match; the operator
            // spans from there back to where the search began.
            let m = MotionResult::horizontal(origin, offset.motion_kind());
            self.apply_operator(op, m);
        } else if wrapped {
            self.echo(match dir {
                SearchDir::Forward => "search hit BOTTOM, continuing at TOP",
                SearchDir::Backward => "search hit TOP, continuing at BOTTOM",
            });
        } else {
            self.message_error = false;
            self.message = format!("{}{}", dir.prefix(), pattern);
        }
    }

    /// Settle the cursor for a match spanning bytes `[ms, me)` under `offset`: on
    /// the match start (no offset / `s`, shifted by its char count), on the
    /// match's last char (`e`, likewise shifted), or `n` lines away at the first
    /// non-blank (a line offset).
    fn place_with_offset(&mut self, ms: usize, me: usize, offset: SearchOffset) {
        match offset {
            SearchOffset::None => self.move_to_match(ms),
            SearchOffset::Start(n) => {
                let t = self.shift_graphemes(ms, n);
                self.move_to_match(t);
            }
            SearchOffset::End(n) => {
                let base = if me > ms {
                    self.prev_grapheme_idx(me)
                } else {
                    ms
                };
                let t = self.shift_graphemes(base, n);
                self.move_to_match(t);
            }
            SearchOffset::Line(n) => {
                let last_line = self.last_line() as isize;
                let line =
                    (self.buffer().byte_to_line(ms) as isize + n).clamp(0, last_line) as usize;
                self.cursor.line = line;
                self.cursor.col = self.first_non_blank(line);
                self.clamp_cursor();
            }
        }
    }

    /// Byte offset `n` grapheme clusters from `base` (forward for `n >= 0`,
    /// backward otherwise), clamped to the buffer.
    fn shift_graphemes(&self, base: usize, n: isize) -> usize {
        if n >= 0 {
            self.advance_graphemes(base, n as usize, self.last_char_idx())
                .0
        } else {
            let mut b = base;
            for _ in 0..n.unsigned_abs() {
                b = self.prev_grapheme_idx(b);
            }
            b
        }
    }

    /// Settle the cursor on a search match at byte offset `byte`.
    fn move_to_match(&mut self, byte: usize) {
        self.set_cursor_char(byte);
        self.clamp_cursor();
    }

    /// Refresh the live `incsearch` preview from the typed command line: jump the
    /// cursor (and, via the caller's `ensure_visible`, the viewport) to the match
    /// the pattern would land on, always measured from the fixed search origin so
    /// the preview doesn't drift as the pattern is edited. A no-op when
    /// `incsearch` is off; an empty pattern or a miss just rests at the origin.
    /// Side-effect free beyond the cursor — no message, history, or `last_search`
    /// change (those happen only on the committed `<CR>`).
    pub(crate) fn update_incsearch_preview(&mut self, dir: SearchDir) {
        if !self.options.incsearch {
            return;
        }
        // A Helix search does not move the primary while typing: it *adds* the match
        // as a new selection (select mode) or replaces it only on commit (normal).
        // Hopping the cursor to the preview would drag the live selection along —
        // making it look like it is extending — so leave it put. The pattern's
        // matches still light up via the `Search` highlight channel, and the commit
        // path re-runs the search from `search_origin` regardless.
        if self.helix {
            return;
        }
        self.cursor = self.search_origin;
        // Preview the pattern only; a trailing `/offset` repositions the preview.
        let (core, offset) = split_search_offset(&self.cmdline, dir.prefix());
        if let Some((ms, me)) = self.preview_match(&core, dir) {
            self.place_with_offset(ms, me, offset);
        } else {
            self.clamp_cursor();
        }
    }

    /// The match range the incsearch preview should rest on for `pattern` from the
    /// search origin in `dir`, honoring `wrapscan`. `None` for an empty pattern, a
    /// pattern that doesn't compile, or one that matches nowhere (the cursor then
    /// stays at the origin).
    fn preview_match(&self, pattern: &str, dir: SearchDir) -> Option<MatchRange> {
        if pattern.is_empty() {
            return None;
        }
        let re = self.compile_search(pattern).ok()?;
        let from = self
            .buffer()
            .byte_at(self.search_origin.line, self.search_origin.col);
        let (primary, wrapped) = self.search_matches(&re, dir, from);
        primary.or(if self.options.wrapscan { wrapped } else { None })
    }

    /// Per visible row (`count` rows from buffer line `base`), the screen-column
    /// spans to paint for search: `(matches, current)`. `matches[row]` lists
    /// every occurrence of the active pattern on that row (the `Search` group);
    /// `current[row]` is the one occurrence the live incsearch preview rests on
    /// (the `IncSearch` group), `None` elsewhere. Both are empty/all-`None` when
    /// nothing should show: while typing an `incsearch` the live command line
    /// lights up, otherwise the last search does — but only while `hlsearch` is on
    /// and a search is active (cleared by `:noh`).
    pub(crate) fn search_highlights_in(
        &self,
        buf: &Buffer,
        cursor: Cursor,
        focused: bool,
        base: usize,
        count: usize,
    ) -> (SearchSpans, IncSearchSpans) {
        let mut matches = vec![Vec::new(); count];
        let mut current = vec![None; count];

        // Helix selection-regex prompt (`s`/`S`/`K`/`Alt-K`): preview the pattern's
        // matches *within* the captured selection ranges as it is typed — the
        // would-be new selections. Reuses the `Search` highlight channel (no cursor
        // hop, no client change); the ranges were snapshotted when the prompt opened.
        if focused
            && self.mode == Mode::Command
            && matches!(self.cmdline_kind, CmdlineKind::HelixRegex(_))
            && self.options.incsearch
            && !self.cmdline.is_empty()
            && !self.helix_regex_ranges.is_empty()
        {
            if let Ok(re) = self.compile_search_cached(&self.cmdline) {
                let line_count = buf.line_count();
                for (row, row_spans) in matches.iter_mut().enumerate() {
                    let buf_line = base + row;
                    if buf_line >= line_count {
                        break;
                    }
                    let ls = buf.line_start(buf_line);
                    let text = buf.line_cow(buf_line);
                    let ts = buf.options.effective_tabstop();
                    let mut vc = unicode::LineVirtcol::new(&text, ts);
                    for (s, e) in re.find_all(&text) {
                        let (abs_s, abs_e) = (ls + s, ls + e);
                        if e > s
                            && self
                                .helix_regex_ranges
                                .iter()
                                .any(|&(lo, hi)| abs_s >= lo && abs_e <= hi)
                        {
                            row_spans.push((vc.at(s), vc.at(e)));
                        }
                    }
                }
            }
            return (matches, current);
        }

        let search_dir = match self.cmdline_kind {
            CmdlineKind::Search(dir) => Some(dir),
            CmdlineKind::Ex
            | CmdlineKind::Prompt
            | CmdlineKind::Confirm
            | CmdlineKind::HelixRegex(_) => None,
        };
        // The live incsearch preview belongs to the focused window (the command
        // line is there); `hlsearch` is global and shows in every window.
        let incsearch = focused
            && self.mode == Mode::Command
            && search_dir.is_some()
            && self.options.incsearch
            && !self.cmdline.is_empty();
        // A `:` command line previews too: while `:[range]s/pat…` (or `:g`/`:v`)
        // is being typed, its pattern lights up over the lines it would act on,
        // so `:%s/test` shows what is about to be replaced. Same `'incsearch'`
        // switch and same focused-window rule as the `/` preview above.
        let ex_preview = focused
            && self.mode == Mode::Command
            && self.cmdline_kind == CmdlineKind::Ex
            && self.options.incsearch;
        let mut ex_range = None;
        let pattern = if incsearch {
            // Highlight the pattern only, not the `/pat/offset` suffix being typed.
            let sep = search_dir.map_or('/', SearchDir::prefix);
            Some(split_search_offset(&self.cmdline, sep).0)
        } else if let Some((p, lo, hi)) = ex_preview.then(|| self.ex_preview_pattern()).flatten() {
            ex_range = Some((lo, hi));
            Some(p)
        } else if ex_preview && self.subst_preview_active() {
            // The replacement half is open (`:s/pat/rep…`), so the richer diff
            // overlay (struck removed + inline added) owns every match — that is
            // exactly why `ex_preview_pattern` yielded `None` above. Don't let the
            // stale `hlsearch` of a prior `/search` paint over it: retarget to the
            // command being typed like vim's incsearch does, which here means no
            // plain Search highlight at all.
            None
        } else if self.options.hlsearch && self.search_active {
            self.last_search.as_ref().map(|(p, _, _)| p.clone())
        } else {
            None
        };
        let Some(pattern) = pattern.filter(|p| !p.is_empty()) else {
            return (matches, current);
        };
        // A pattern still mid-edit (incsearch) may not compile yet; show nothing.
        let Ok(re) = self.compile_search_cached(&pattern) else {
            return (matches, current);
        };
        let line_count = buf.line_count();
        // During a `:s///c` confirm walk the match being prompted wears the diff
        // overlay instead (struck removed + inline added), so drop the plain match
        // highlight from that one span to keep it clean.
        let confirm_cur = self.subst_confirm_current();

        for (row, row_spans) in matches.iter_mut().enumerate() {
            let buf_line = base + row;
            if buf_line >= line_count {
                break;
            }
            // The `:s`/`:g` preview only lights the lines its range covers.
            if ex_range.is_some_and(|(lo, hi)| buf_line < lo || buf_line > hi) {
                continue;
            }
            let text = buf.line_cow(buf_line);
            let ts = buf.options.effective_tabstop();
            let mut vc = unicode::LineVirtcol::new(&text, ts);
            for (s, e) in re.find_all(&text) {
                if confirm_cur == Some((buf_line, s)) {
                    continue;
                }
                let span = (vc.at(s), vc.at(e));
                row_spans.push(span);
                // The preview cursor sits on the start of its match, so an exact
                // column hit on the cursor's line marks the current match.
                if incsearch && buf_line == cursor.line && s == cursor.col {
                    current[row] = Some(span);
                }
            }
        }
        (matches, current)
    }
}
