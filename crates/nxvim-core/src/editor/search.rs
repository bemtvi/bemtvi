//! Search: pattern compilation, `/`/`?`/`n`/`N`, `incsearch` preview, and the
//! `hlsearch` match spans projected into the View.

use super::*;
use crate::buffer::Buffer;
use crate::mode::Mode;
use crate::search::SearchRegex;
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
    /// duplicate (vim collapses repeats).
    fn remember_search(&mut self, pattern: &str) {
        if self.search_history.last().map(String::as_str) != Some(pattern) {
            self.search_history.push(pattern.to_string());
        }
    }

    /// Record an interactively-submitted `:` command in the ex history, skipping
    /// an empty line or a consecutive duplicate (vim collapses repeats).
    pub(crate) fn remember_ex(&mut self, cmd: &str) {
        let cmd = cmd.trim();
        if !cmd.is_empty() && self.ex_history.last().map(String::as_str) != Some(cmd) {
            self.ex_history.push(cmd.to_string());
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
            format!(r"\b{word}\b")
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
    fn word_under_cursor(&self) -> Option<String> {
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
        self.options.ignorecase
            && !(self.options.smartcase && pattern.chars().any(|c| c.is_uppercase()))
    }

    /// Compile `pattern` (a standard regex) with this editor's case options.
    fn compile_search(&self, pattern: &str) -> Result<SearchRegex, String> {
        SearchRegex::compile(pattern, self.search_ignorecase(pattern))
    }

    /// The next match of the compiled `re` in `dir` from byte offset `from`, as
    /// `(primary, wrapped)` whole-buffer `(start, end)` ranges. `primary` is the
    /// match in the search direction without wrapping; `wrapped` is the first
    /// match from the opposite end (used when `wrapscan` lets the search wrap).
    /// Forward starts one grapheme past `from` so a match *under* it isn't an
    /// immediate self-hit; backward looks left of it. Matching is line-by-line;
    /// side-effect free (the shared core of `run_search` and the incsearch
    /// preview).
    fn search_matches(
        &self,
        re: &SearchRegex,
        dir: SearchDir,
        from: usize,
    ) -> (Option<MatchRange>, Option<MatchRange>) {
        match dir {
            SearchDir::Forward => (
                self.match_forward_from(re, self.next_grapheme_idx(from)),
                self.match_forward_from(re, 0),
            ),
            SearchDir::Backward => (
                self.match_backward_before(re, from),
                self.match_backward_before(re, self.buffer().len_bytes()),
            ),
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

        let search_dir = match self.cmdline_kind {
            CmdlineKind::Search(dir) => Some(dir),
            CmdlineKind::Ex | CmdlineKind::Prompt | CmdlineKind::Confirm => None,
        };
        // The live incsearch preview belongs to the focused window (the command
        // line is there); `hlsearch` is global and shows in every window.
        let incsearch = focused
            && self.mode == Mode::Command
            && search_dir.is_some()
            && self.options.incsearch
            && !self.cmdline.is_empty();
        let pattern = if incsearch {
            // Highlight the pattern only, not the `/pat/offset` suffix being typed.
            let sep = search_dir.map_or('/', SearchDir::prefix);
            Some(split_search_offset(&self.cmdline, sep).0)
        } else if self.options.hlsearch && self.search_active {
            self.last_search.as_ref().map(|(p, _, _)| p.clone())
        } else {
            None
        };
        let Some(pattern) = pattern.filter(|p| !p.is_empty()) else {
            return (matches, current);
        };
        // A pattern still mid-edit (incsearch) may not compile yet; show nothing.
        let Ok(re) = self.compile_search(&pattern) else {
            return (matches, current);
        };
        let line_count = buf.line_count();

        for (row, row_spans) in matches.iter_mut().enumerate() {
            let buf_line = base + row;
            if buf_line >= line_count {
                break;
            }
            let text = buf.line(buf_line);
            let ts = buf.options.effective_tabstop();
            for (s, e) in re.find_all(&text) {
                let span = (
                    unicode::virtcol(&text, s, ts),
                    unicode::virtcol(&text, e, ts),
                );
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
