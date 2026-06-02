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

use regex::{Regex, RegexBuilder};

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
}
