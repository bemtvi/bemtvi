//! Search patterns are standard ("perl-compatible") regular expressions, matched
//! by the Rust `regex` crate.
//!
//! This is a **deliberate divergence from vim**, whose `/` search speaks vim's
//! own "magic" dialect (bare `+` `(` `|` are literal; `\+` `\(` `\|` are the
//! operators). nxvim instead uses canonical regex syntax: `+ ? * ( ) | { } [ ]
//! ^ $ .` are operators by default and a leading `\` escapes them to a literal,
//! exactly as in Perl/PCRE/RE2. Per-pattern case is the inline `(?i)` / `(?-i)`
//! flag (not vim's `\c`/`\C`), layered over the `ignorecase`/`smartcase` options.
//!
//! Matching is line-by-line (each editor line is its own haystack), so `^`/`$`
//! anchor to line edges and the rope's trailing-newline invariant is never in
//! play. Multi-line (`\n`-spanning) patterns are not supported. The `regex` crate
//! is not full PCRE — no backreferences or look-around — but covers the everyday
//! pattern surface.
//!
//! `:substitute` rides this same engine. Its replacement string is canonical
//! too: capture refs are PCRE-style `$0`/`$1`/`${name}`/`$$` (not vim's `&` /
//! `\1`), with a small backslash-escape set layered on so a replacement can
//! insert control characters — `\r`/`\n` → newline (vim splits a line with
//! `\r`; we treat `\n` the same rather than vim's NUL), `\t` → tab, `\\` → a
//! literal backslash. This mirrors the search divergence above and is enforced
//! by [`SearchRegex::substitute_line`].

use regex::{Captures, Regex, RegexBuilder};

/// A compiled search pattern: a Rust `Regex` over a single line of text.
pub(crate) struct SearchRegex {
    re: Regex,
}

impl SearchRegex {
    /// Compile `pattern` as a standard regex. `ignorecase` seeds case-folding —
    /// an inline `(?i)`/`(?-i)` in the pattern overrides it. Returns a vim-style
    /// error string on a pattern the engine rejects.
    pub(crate) fn compile(pattern: &str, ignorecase: bool) -> Result<SearchRegex, String> {
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ignorecase)
            .build()
            .map_err(|_| format!("E383: Invalid search string: {pattern}"))?;
        Ok(SearchRegex { re })
    }

    /// The first match in the line's **non-overlapping** left-to-right sequence
    /// (the same one `find_all` and the highlighter walk) whose start is at byte
    /// offset `from` or later, as a `(start, end)` byte range. `^` still anchors
    /// to the line start (offset 0), never to `from`.
    ///
    /// This deliberately differs from a raw "leftmost match at-or-after `from`":
    /// a greedy pattern such as `.+ab` can also match *starting inside* an earlier
    /// match, and a raw scan from `from` would return that overlapping sub-match.
    /// Skipping to the next match in the non-overlapping sequence keeps `n`
    /// stepping between the matches the user actually sees highlighted, instead of
    /// crawling one grapheme deeper into the current one.
    pub(crate) fn find_from(&self, line: &str, from: usize) -> Option<(usize, usize)> {
        self.re
            .find_iter(line)
            .map(|m| (m.start(), m.end()))
            .find(|(s, _)| *s >= from)
    }

    /// Every match in `line`, left to right, as `(start, end)` byte ranges.
    pub(crate) fn find_all(&self, line: &str) -> Vec<(usize, usize)> {
        self.re
            .find_iter(line)
            .map(|m| (m.start(), m.end()))
            .collect()
    }

    /// Substitute matches in `line` with `rep` (canonical `$`-captures plus the
    /// backslash-escape set — see the module header). With `global` false only
    /// the first match is replaced. Returns the rewritten line and the number
    /// of matches replaced. The line is passed *without* its trailing newline,
    /// so `^`/`$` anchor to its real edges; `\r`/`\n` in `rep` introduce real
    /// newlines into the result (the caller splices them back in).
    pub(crate) fn substitute_line(&self, line: &str, rep: &str, global: bool) -> (String, usize) {
        let mut out = String::new();
        let mut last = 0;
        let mut count = 0;
        for caps in self.re.captures_iter(line) {
            let m = caps.get(0).expect("group 0 always present");
            out.push_str(&line[last..m.start()]);
            expand_replacement(rep, &caps, &mut out);
            last = m.end();
            count += 1;
            if !global {
                break;
            }
        }
        out.push_str(&line[last..]);
        (out, count)
    }

    /// The next match in the non-overlapping sequence whose start is at byte
    /// offset `from` or later, as `(start, end, replacement)` where `replacement`
    /// is `rep` expanded against that match's captures (same `$`-captures and
    /// backslash escapes as [`Self::substitute_line`]). `None` past the last
    /// match. The single-match primitive the interactive `:s///c` confirm walk
    /// uses to step one match at a time.
    pub(crate) fn match_replacement(
        &self,
        line: &str,
        from: usize,
        rep: &str,
    ) -> Option<(usize, usize, String)> {
        let caps = self
            .re
            .captures_iter(line)
            .find(|c| c.get(0).expect("group 0 always present").start() >= from)?;
        let m = caps.get(0).expect("group 0 always present");
        let mut out = String::new();
        expand_replacement(rep, &caps, &mut out);
        Some((m.start(), m.end(), out))
    }
}

/// Expand a substitute replacement against a match's captures, appending to
/// `out`. Capture refs are PCRE-style — `$0`/`$1` (numeric), `$name`/`${name}`
/// (the brace form disambiguates `${1}x` from `$1x`), `$$` for a literal `$`,
/// and an unknown group expands to nothing. Backslash escapes: `\r`/`\n` →
/// newline, `\t` → tab, `\\` → backslash; `\` before anything else yields that
/// char literally.
fn expand_replacement(rep: &str, caps: &Captures, out: &mut String) {
    let chars: Vec<char> = rep.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '$' => {
                i += 1;
                match chars.get(i) {
                    Some('$') => {
                        out.push('$');
                        i += 1;
                    }
                    Some('{') => {
                        i += 1;
                        let mut name = String::new();
                        while let Some(&c) = chars.get(i).filter(|&&c| c != '}') {
                            name.push(c);
                            i += 1;
                        }
                        i += usize::from(chars.get(i) == Some(&'}')); // consume `}`
                        push_group(caps, &name, out);
                    }
                    Some(&c) if c.is_ascii_alphanumeric() || c == '_' => {
                        let mut name = String::new();
                        while let Some(&c) = chars
                            .get(i)
                            .filter(|&&c| c.is_ascii_alphanumeric() || c == '_')
                        {
                            name.push(c);
                            i += 1;
                        }
                        push_group(caps, &name, out);
                    }
                    // A `$` not introducing a group is a literal `$`.
                    _ => out.push('$'),
                }
            }
            '\\' => {
                i += 1;
                match chars.get(i) {
                    Some('n' | 'r') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(&c) => out.push(c),
                    None => out.push('\\'),
                }
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
}

/// Append capture group `name` (numeric index or named) to `out`; a missing
/// group contributes nothing, matching the `regex` crate's `expand`.
fn push_group(caps: &Captures, name: &str, out: &mut String) {
    let group = match name.parse::<usize>() {
        Ok(idx) => caps.get(idx),
        Err(_) => caps.name(name),
    };
    if let Some(m) = group {
        out.push_str(m.as_str());
    }
}
