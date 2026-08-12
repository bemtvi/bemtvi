//! A faithful, dependency-free port of Lua 5.4's string-pattern matcher
//! (`lstrlib.c`), exposing the single question a tree-sitter `#lua-match?`
//! predicate asks: *does this Lua pattern match anywhere in this text?* —
//! equivalent to `string.find(text, pattern) ~= nil`.
//!
//! Lua patterns are **not** regular expressions: there is no alternation, `*`/`+`
//! are greedy and `-` is lazy, character classes are `%a %d %s %w %l %u %p %c %x`
//! (and their uppercase complements), sets are `[...]` / `[^...]`, plus the special
//! items `%bxy` (balanced match) and `%f[set]` (frontier). nvim-treesitter queries
//! lean on these heavily (`(#lua-match? @x "^#!")`, …), and the standard
//! tree-sitter binding only enforces the regex `#match?` predicate — so without
//! this, every `#lua-match?`-gated pattern leaks onto every node it could match.
//!
//! The algorithm mirrors the reference recursion (`do_match` + `classend` +
//! `singlematch` + `max/min_expand` + `matchbalance` + frontier), operating on
//! bytes as Lua does. Captures are tracked only as far as backreferences (`%1`..)
//! need them; the public entry point returns a bool, so capture *values* are
//! irrelevant. Malformed patterns (which cannot occur in the vendored grammar
//! queries) and pathologically deep recursion yield `false` rather than panicking —
//! the matcher never unwinds into the render path.

const L_ESC: u8 = b'%';

/// Bound on recursion depth, mirroring Lua's `MAXCCALLS`. A pattern deep enough to
/// hit this is degenerate; treating it as "no match" keeps the render path safe.
const MAX_DEPTH: usize = 200;

const CAP_UNFINISHED: isize = -1;
const CAP_POSITION: isize = -2;

/// Whether `pattern` (Lua pattern syntax) matches somewhere in `text`, honoring a
/// leading `^` anchor — `string.find(text, pattern) ~= nil`.
pub fn lua_match(text: &[u8], pattern: &[u8]) -> bool {
    let anchor = pattern.first() == Some(&b'^');
    let pat = if anchor { &pattern[1..] } else { pattern };
    let mut s = 0usize;
    loop {
        let mut ms = MatchState {
            src: text,
            pat,
            captures: Vec::new(),
            depth: MAX_DEPTH,
        };
        if ms.do_match(s, 0).is_some() {
            return true;
        }
        if anchor || s >= text.len() {
            return false;
        }
        s += 1;
    }
}

#[derive(Clone, Copy)]
struct Capture {
    init: usize,
    len: isize,
}

struct MatchState<'a> {
    src: &'a [u8],
    pat: &'a [u8],
    captures: Vec<Capture>,
    depth: usize,
}

impl MatchState<'_> {
    /// Index just past the single pattern item beginning at `p` (a literal, a `%x`
    /// class, or a `[...]` set). `None` on a malformed pattern.
    fn class_end(&self, mut p: usize) -> Option<usize> {
        let c = *self.pat.get(p)?;
        p += 1;
        if c == L_ESC {
            // `%x` — skip the escaped character.
            return if p < self.pat.len() {
                Some(p + 1)
            } else {
                None
            };
        }
        if c == b'[' {
            if self.pat.get(p) == Some(&b'^') {
                p += 1;
            }
            // A do-while: the first member is read unconditionally, so a leading
            // `]` is a literal set member (`[]]`).
            loop {
                let cc = *self.pat.get(p)?;
                p += 1;
                if cc == L_ESC && p < self.pat.len() {
                    p += 1; // skip an escaped member, e.g. `%]`
                }
                if self.pat.get(p) == Some(&b']') {
                    return Some(p + 1);
                }
            }
        }
        Some(p)
    }

    /// Whether `src[s]` matches the single pattern item spanning `[p, ep)`.
    fn single_match(&self, s: usize, p: usize, ep: usize) -> bool {
        let Some(&c) = self.src.get(s) else {
            return false;
        };
        match self.pat[p] {
            b'.' => true,
            L_ESC => match_class(c, self.pat[p + 1]),
            b'[' => self.match_bracket(c, p, ep - 1),
            pc => pc == c,
        }
    }

    /// Match `c` against the `[...]` set spanning `[p, ec]` (`ec` indexes the `]`).
    fn match_bracket(&self, c: u8, mut p: usize, ec: usize) -> bool {
        let mut sig = true;
        if self.pat.get(p + 1) == Some(&b'^') {
            sig = false;
            p += 1; // skip the `^`
        }
        p += 1;
        while p < ec {
            if self.pat[p] == L_ESC {
                p += 1;
                if match_class(c, self.pat[p]) {
                    return sig;
                }
                p += 1;
            } else if self.pat.get(p + 1) == Some(&b'-') && p + 2 < ec {
                // A range `a-z`.
                if self.pat[p] <= c && c <= self.pat[p + 2] {
                    return sig;
                }
                p += 3;
            } else {
                if self.pat[p] == c {
                    return sig;
                }
                p += 1;
            }
        }
        !sig
    }

    /// The core recursion: try to match `pat[p..]` at `src[s..]`, returning the
    /// source index just past a successful match. A loop emulates the reference
    /// matcher's `goto`-based tail recursion.
    fn do_match(&mut self, mut s: usize, mut p: usize) -> Option<usize> {
        if self.depth == 0 {
            return None; // pattern too complex — bail rather than overflow the stack
        }
        self.depth -= 1;
        let result = self.do_match_inner(&mut s, &mut p);
        self.depth += 1;
        result
    }

    fn do_match_inner(&mut self, s: &mut usize, p: &mut usize) -> Option<usize> {
        loop {
            if *p >= self.pat.len() {
                return Some(*s);
            }
            match self.pat[*p] {
                b'(' => {
                    return if self.pat.get(*p + 1) == Some(&b')') {
                        self.start_capture(*s, *p + 2, CAP_POSITION)
                    } else {
                        self.start_capture(*s, *p + 1, CAP_UNFINISHED)
                    };
                }
                b')' => return self.end_capture(*s, *p + 1),
                b'$' if *p + 1 == self.pat.len() => {
                    // A trailing `$` anchors to end-of-subject.
                    return (*s == self.src.len()).then_some(*s);
                }
                L_ESC => {
                    match self.pat.get(*p + 1).copied() {
                        Some(b'b') => return self.match_balance(*s, *p + 2),
                        Some(b'f') => {
                            // Frontier `%f[set]`: a zero-width boundary where the
                            // char before is NOT in the set and the char at `s` IS.
                            *p += 2;
                            if self.pat.get(*p) != Some(&b'[') {
                                return None; // malformed: missing '[' after %f
                            }
                            let ep = self.class_end(*p)?;
                            let prev = if *s == 0 { 0 } else { self.src[*s - 1] };
                            let curr = self.src.get(*s).copied().unwrap_or(0);
                            if !self.match_bracket(prev, *p, ep - 1)
                                && self.match_bracket(curr, *p, ep - 1)
                            {
                                *p = ep;
                                continue;
                            }
                            return None;
                        }
                        Some(d) if d.is_ascii_digit() => {
                            // Backreference `%1`..`%9`.
                            let ns = self.match_capture(*s, d)?;
                            *s = ns;
                            *p += 2;
                            continue;
                        }
                        _ => {} // an escaped literal class — fall through to default
                    }
                    return self.default_match(*s, *p);
                }
                _ => return self.default_match(*s, *p),
            }
        }
    }

    /// The "single item, then optional quantifier" arm (the reference `dflt:`),
    /// driving the `*` / `+` / `-` / `?` expansion. Each branch recurses into
    /// [`do_match`](Self::do_match) for the rest of the pattern, so it returns the
    /// final result directly rather than threading state back to the outer loop.
    fn default_match(&mut self, s: usize, p: usize) -> Option<usize> {
        let ep = self.class_end(p)?;
        let matched = self.single_match(s, p, ep);
        match self.pat.get(ep).copied() {
            Some(b'?') => {
                if matched {
                    if let Some(res) = self.do_match(s + 1, ep + 1) {
                        return Some(res);
                    }
                }
                self.do_match(s, ep + 1)
            }
            Some(b'+') => {
                if matched {
                    self.max_expand(s + 1, p, ep)
                } else {
                    None
                }
            }
            Some(b'*') => self.max_expand(s, p, ep),
            Some(b'-') => self.min_expand(s, p, ep),
            _ => {
                if !matched {
                    return None;
                }
                self.do_match(s + 1, ep)
            }
        }
    }

    /// Greedy expansion (`*` / `+`): match as many items as possible, then back off
    /// one at a time until the remainder of the pattern matches.
    fn max_expand(&mut self, s: usize, p: usize, ep: usize) -> Option<usize> {
        let mut i = 0usize;
        while self.single_match(s + i, p, ep) {
            i += 1;
        }
        loop {
            if let Some(res) = self.do_match(s + i, ep + 1) {
                return Some(res);
            }
            if i == 0 {
                return None;
            }
            i -= 1; // back off and try a shorter run
        }
    }

    /// Lazy expansion (`-`): match as few items as possible, extending only when the
    /// remainder fails.
    fn min_expand(&mut self, mut s: usize, p: usize, ep: usize) -> Option<usize> {
        loop {
            if let Some(res) = self.do_match(s, ep + 1) {
                return Some(res);
            }
            if self.single_match(s, p, ep) {
                s += 1;
            } else {
                return None;
            }
        }
    }

    fn start_capture(&mut self, s: usize, p: usize, what: isize) -> Option<usize> {
        self.captures.push(Capture { init: s, len: what });
        let res = self.do_match(s, p);
        if res.is_none() {
            self.captures.pop();
        }
        res
    }

    fn end_capture(&mut self, s: usize, p: usize) -> Option<usize> {
        // Close the most recently opened (still-unfinished) capture.
        let l = (0..self.captures.len())
            .rev()
            .find(|&i| self.captures[i].len == CAP_UNFINISHED)?;
        self.captures[l].len = (s - self.captures[l].init) as isize;
        let res = self.do_match(s, p);
        if res.is_none() {
            self.captures[l].len = CAP_UNFINISHED;
        }
        res
    }

    fn match_capture(&mut self, s: usize, d: u8) -> Option<usize> {
        let idx = (d - b'1') as usize;
        let cap = self.captures.get(idx).copied()?;
        if cap.len < 0 {
            return None;
        }
        let len = cap.len as usize;
        let cap_bytes = &self.src[cap.init..cap.init + len];
        if self.src.len() - s >= len && &self.src[s..s + len] == cap_bytes {
            Some(s + len)
        } else {
            None
        }
    }

    fn match_balance(&mut self, s: usize, p: usize) -> Option<usize> {
        let b = *self.pat.get(p)?;
        let e = *self.pat.get(p + 1)?;
        if self.src.get(s) != Some(&b) {
            return None;
        }
        let mut cont = 1;
        let mut i = s + 1;
        while i < self.src.len() {
            if self.src[i] == e {
                cont -= 1;
                if cont == 0 {
                    return self.do_match(i + 1, p + 2);
                }
            } else if self.src[i] == b {
                cont += 1;
            }
            i += 1;
        }
        None
    }
}

/// Match a byte against a `%`-class letter (`a d s w l u p c x g` and their
/// uppercase complements); a non-letter class is a literal (`%%`, `%.`).
fn match_class(c: u8, cl: u8) -> bool {
    let res = match cl.to_ascii_lowercase() {
        b'a' => c.is_ascii_alphabetic(),
        b'c' => c.is_ascii_control(),
        b'd' => c.is_ascii_digit(),
        b'g' => c.is_ascii_graphic(),
        b'l' => c.is_ascii_lowercase(),
        b'p' => c.is_ascii_punctuation(),
        // Lua's `%s` is C `isspace`: space, \t, \n, \v, \f, \r.
        b's' => c == b' ' || (b'\t'..=b'\r').contains(&c),
        b'u' => c.is_ascii_uppercase(),
        b'w' => c.is_ascii_alphanumeric(),
        b'x' => c.is_ascii_hexdigit(),
        _ => return cl == c, // not a class: a literal escaped char
    };
    // An uppercase class letter complements the test.
    if cl.is_ascii_uppercase() {
        !res
    } else {
        res
    }
}
