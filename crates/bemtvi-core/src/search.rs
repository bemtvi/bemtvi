//! Search and `:substitute` patterns, compiled by one of two interchangeable
//! engines selected by the `'regexsyntax'` option:
//!
//! * **`pcre`** (the default) — standard ("perl-compatible") regular expressions,
//!   matched by the Rust `regex` crate. A **deliberate divergence from vim**: bare
//!   `+ ? ( ) | { } [ ] ^ $ .` are operators and a leading `\` escapes them to a
//!   literal, exactly as in Perl/PCRE/RE2; per-pattern case is the inline `(?i)` /
//!   `(?-i)` flag. Its replacement is canonical too — `$0`/`$1`/`${name}`/`$$`
//!   captures with a small backslash-escape set (`\r`/`\n` → newline, `\t` → tab,
//!   `\\` → backslash).
//!
//! * **`vim`** — the real vim "magic" dialect (`\(\)` groups, `\1`/`&` back-refs,
//!   `\<`/`\>` word boundaries, `\zs`/`\ze`, look-around, the non-greedy `\{-}`
//!   family, …), matched by the embedded [`bemtvi_regex`] engine — vim's own
//!   backtracking + NFA regexp. Its replacement speaks vim too: `&`/`\0` (whole
//!   match), `\1`–`\9` (groups), the `\u \U \l \L \e \E` case modifiers, and the
//!   `\r`/`\t`/`\\` escapes. Available only when the `vim-regex` crate feature is
//!   built in (host builds enable it; the C-free `wasm32-unknown-unknown` web
//!   client does not — there, selecting it fails loud).
//!
//! Either way matching is line-by-line (each editor line is its own haystack), so
//! `^`/`$` anchor to line edges and the rope's trailing-newline invariant is never
//! in play; multi-line (`\n`-spanning) patterns are not supported. The active
//! engine is chosen per-compile by the [`RegexEngine`] passed to
//! [`SearchRegex::compile`].

use regex::{Captures, Regex, RegexBuilder};

use crate::sandbox::SandboxError;

/// Which regex dialect/engine compiles and matches a search or `:substitute`
/// pattern, chosen by the `'regexsyntax'` option. (Distinct from vim's numeric
/// `'regexpengine'`, which picks *between* vim's own backtracking/NFA engines —
/// both still vim syntax; this picks the **dialect**.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RegexEngine {
    /// Rust `regex` crate — canonical/PCRE syntax (the historical bemtvi default).
    Pcre,
    /// The real vim regexp engine (`bemtvi-regex`) — vim's "magic" dialect.
    Vim,
}

/// A compiled search pattern over a single line of text, backed by whichever
/// [`RegexEngine`] compiled it. The four primitives ([`find_from`](Self::find_from),
/// [`find_all`](Self::find_all), [`substitute_line`](Self::substitute_line),
/// [`match_replacement`](Self::match_replacement)) present an engine-neutral face
/// to the editor's search/substitute/global machinery.
pub(crate) enum SearchRegex {
    /// A `regex`-crate pattern (canonical/PCRE syntax).
    Pcre(Regex),
    /// A vim-dialect pattern matched by [`bemtvi_regex`]. Only built under the
    /// `vim-regex` feature.
    #[cfg(feature = "vim-regex")]
    Vim(VimSearch),
}

impl SearchRegex {
    /// Compile `pattern` with the given `engine`. `ignorecase` seeds case-folding
    /// — for `Pcre` an inline `(?i)`/`(?-i)`, for `Vim` an inline `\c`/`\C`, in the
    /// pattern overrides it. Returns a vim-style error string on a pattern the
    /// engine rejects (`E383`).
    pub(crate) fn compile(
        pattern: &str,
        ignorecase: bool,
        engine: RegexEngine,
    ) -> Result<SearchRegex, String> {
        match engine {
            RegexEngine::Pcre => {
                let re = RegexBuilder::new(pattern)
                    .case_insensitive(ignorecase)
                    .build()
                    .map_err(|_| format!("E383: Invalid search string: {pattern}"))?;
                Ok(SearchRegex::Pcre(re))
            }
            RegexEngine::Vim => compile_vim(pattern, ignorecase),
        }
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
        match self {
            SearchRegex::Pcre(re) => re
                .find_iter(line)
                .map(|m| (m.start(), m.end()))
                .find(|(s, _)| *s >= from),
            #[cfg(feature = "vim-regex")]
            SearchRegex::Vim(v) => v.exec(line, from).map(|m| (m.start, m.end)),
        }
    }

    /// Every match in `line`, left to right, as `(start, end)` byte ranges.
    pub(crate) fn find_all(&self, line: &str) -> Vec<(usize, usize)> {
        match self {
            SearchRegex::Pcre(re) => re.find_iter(line).map(|m| (m.start(), m.end())).collect(),
            #[cfg(feature = "vim-regex")]
            SearchRegex::Vim(v) => {
                let mut out = Vec::new();
                let mut from = 0;
                while from <= line.len() {
                    let Some(m) = v.exec(line, from) else { break };
                    out.push((m.start, m.end));
                    from = advance(line, m.start, m.end);
                }
                out
            }
        }
    }

    /// Substitute matches in `line` per `rep`, appending to a rewritten line.
    ///
    /// A [`Repl::Template`] expands the literal replacement dialect (`$`-captures
    /// for `Pcre`, `&`/`\1` for `Vim`; see the module header). A [`Repl::Expr`]
    /// hands each match's group texts to a sandbox expression instead, so the
    /// replacement is *computed* per match.
    ///
    /// With `global` false only the first match is replaced. Returns the
    /// rewritten line and the number of matches replaced. The line is passed
    /// *without* its trailing newline, so `^`/`$` anchor to its real edges;
    /// `\r`/`\n` in a template introduce real newlines into the result (the
    /// caller splices them back in). Only the expression form can fail.
    pub(crate) fn substitute_line(
        &self,
        line: &str,
        rep: &mut Repl<'_>,
        global: bool,
    ) -> Result<(String, usize), SandboxError> {
        match self {
            SearchRegex::Pcre(re) => {
                let mut out = String::new();
                let mut last = 0;
                let mut count = 0;
                for caps in re.captures_iter(line) {
                    let m = caps.get(0).expect("group 0 always present");
                    out.push_str(&line[last..m.start()]);
                    match rep {
                        Repl::Template(t) => expand_replacement(t, &caps, &mut out),
                        Repl::Expr(f) => out.push_str(&f(&pcre_groups(&caps))?),
                    }
                    last = m.end();
                    count += 1;
                    if !global {
                        break;
                    }
                }
                out.push_str(&line[last..]);
                Ok((out, count))
            }
            // The vendored engine expands its own templates, but an expression
            // needs the match-by-match walk `find_all` uses, so it is driven here.
            #[cfg(feature = "vim-regex")]
            SearchRegex::Vim(v) => match rep {
                Repl::Template(t) => Ok(v.substitute_line(line, t, global)),
                Repl::Expr(f) => {
                    let mut out = String::new();
                    let mut last = 0;
                    let mut count = 0;
                    let mut from = 0;
                    while from <= line.len() {
                        let Some(m) = v.exec(line, from) else { break };
                        out.push_str(&line[last..m.start]);
                        out.push_str(&f(&vim_groups(line, &m))?);
                        last = m.end;
                        count += 1;
                        if !global {
                            break;
                        }
                        from = advance(line, m.start, m.end);
                    }
                    out.push_str(&line[last..]);
                    Ok((out, count))
                }
            },
        }
    }

    /// The next match in the non-overlapping sequence whose start is at byte
    /// offset `from` or later, as `(start, end, replacement)` where `replacement`
    /// is `rep` resolved against that match (same dialects as
    /// [`Self::substitute_line`]). `Ok(None)` past the last match. The
    /// single-match primitive the interactive `:s///c` confirm walk and the
    /// `inccommand` live preview step one match at a time with.
    pub(crate) fn match_replacement(
        &self,
        line: &str,
        from: usize,
        rep: &mut Repl<'_>,
    ) -> Result<Option<(usize, usize, String)>, SandboxError> {
        match self {
            SearchRegex::Pcre(re) => {
                let Some(caps) = re
                    .captures_iter(line)
                    .find(|c| c.get(0).expect("group 0 always present").start() >= from)
                else {
                    return Ok(None);
                };
                let m = caps.get(0).expect("group 0 always present");
                let mut out = String::new();
                match rep {
                    Repl::Template(t) => expand_replacement(t, &caps, &mut out),
                    Repl::Expr(f) => out.push_str(&f(&pcre_groups(&caps))?),
                }
                Ok(Some((m.start(), m.end(), out)))
            }
            #[cfg(feature = "vim-regex")]
            SearchRegex::Vim(v) => {
                let Some(m) = v.exec(line, from) else {
                    return Ok(None);
                };
                let mut out = String::new();
                match rep {
                    Repl::Template(t) => expand_vim_replacement(t, line, &m, &mut out),
                    Repl::Expr(f) => out.push_str(&f(&vim_groups(line, &m))?),
                }
                Ok(Some((m.start, m.end, out)))
            }
        }
    }
}

/// The per-match call a [`Repl::Expr`] makes: this match's group texts in,
/// the replacement text out, or the reason the sandbox could not produce one.
pub(crate) type ExprCall<'a> = dyn FnMut(&[Option<&str>]) -> Result<String, SandboxError> + 'a;

/// How a substitute builds each match's replacement text.
///
/// One abstraction rather than a parallel expression-only code path, so the
/// literal and computed forms stay in lockstep across all three substitute call
/// sites (bulk `:s`, the `inccommand` preview, and the `:s///c` confirm walk).
pub(crate) enum Repl<'a> {
    /// The literal replacement template, expanded against the match's captures.
    Template(&'a str),
    /// A compiled sandbox expression, called once per match with that match's
    /// group texts — `[0]` the whole match, `[1..]` the groups, `None` for a
    /// group that did not participate.
    Expr(&'a mut ExprCall<'a>),
}

/// A PCRE match's groups as the uniform slice [`Repl::Expr`] takes.
fn pcre_groups<'t>(caps: &Captures<'t>) -> Vec<Option<&'t str>> {
    (0..caps.len())
        .map(|i| caps.get(i).map(|m| m.as_str()))
        .collect()
}

/// The vendored vim engine's groups, in the same shape (`submatches[0]` is the
/// whole match there too).
#[cfg(feature = "vim-regex")]
fn vim_groups<'t>(line: &'t str, m: &bemtvi_regex::LineMatch) -> Vec<Option<&'t str>> {
    m.submatches
        .iter()
        .map(|g| g.map(|(s, e)| &line[s..e]))
        .collect()
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

// ---- the vim engine ---------------------------------------------------------

/// Compile `pattern` with the embedded vim regexp engine. Only the real
/// implementation exists under the `vim-regex` feature; without it, selecting the
/// vim engine fails loud (the no-silent-stubs rule) rather than pretending.
#[cfg(feature = "vim-regex")]
fn compile_vim(pattern: &str, ignorecase: bool) -> Result<SearchRegex, String> {
    let re = bemtvi_regex::VimRegex::compile(pattern)
        .map_err(|e| format!("E383: Invalid search string: {pattern} ({e})"))?;
    Ok(SearchRegex::Vim(VimSearch {
        re,
        ignore_case: ignorecase,
    }))
}

#[cfg(not(feature = "vim-regex"))]
fn compile_vim(_pattern: &str, _ignorecase: bool) -> Result<SearchRegex, String> {
    Err("E319: the vim regex engine is not available in this build \
         (set regexsyntax=pcre)"
        .to_string())
}

/// The next scan offset after a match `[start, end)` when walking a line's match
/// sequence. A non-empty match advances past its end; an empty match (zero-width
/// assertion, `x*` on a gap) would loop forever there, so step one whole char
/// past it (and past the end of the line, ending the walk).
#[cfg(feature = "vim-regex")]
fn advance(line: &str, start: usize, end: usize) -> usize {
    if end > start {
        end
    } else {
        line[end..]
            .chars()
            .next()
            .map_or(end + 1, |c| end + c.len_utf8())
    }
}

/// A compiled vim-dialect pattern plus the option-level case default to feed each
/// match (an inline `\c`/`\C` in the pattern overrides it inside the engine).
#[cfg(feature = "vim-regex")]
pub(crate) struct VimSearch {
    re: bemtvi_regex::VimRegex,
    ignore_case: bool,
}

#[cfg(feature = "vim-regex")]
impl VimSearch {
    /// The leftmost match at byte offset `from` or later, or `None`. A match-time
    /// engine error (an interrupt/timeout — never a malformed pattern, which is
    /// caught at compile) is treated as "no match here" so the redraw/highlight
    /// path can't panic on it.
    fn exec(&self, line: &str, from: usize) -> Option<bemtvi_regex::LineMatch> {
        self.re
            .exec_line(line, from, self.ignore_case)
            .ok()
            .flatten()
    }

    fn substitute_line(&self, line: &str, rep: &str, global: bool) -> (String, usize) {
        let mut out = String::new();
        let mut last = 0;
        let mut from = 0;
        let mut count = 0;
        while from <= line.len() {
            let Some(m) = self.exec(line, from) else {
                break;
            };
            out.push_str(&line[last..m.start]);
            expand_vim_replacement(rep, line, &m, &mut out);
            last = m.end;
            count += 1;
            if !global {
                break;
            }
            from = advance(line, m.start, m.end);
        }
        out.push_str(&line[last..]);
        (out, count)
    }
}

/// One-shot / span case transform applied to vim replacement text by
/// `\u \U \l \L \e \E`. `once` affects only the next char; `span` runs until
/// `\e`/`\E`.
#[cfg(feature = "vim-regex")]
#[derive(Default)]
struct Case {
    once: Option<bool>,
    span: Option<bool>,
}

#[cfg(feature = "vim-regex")]
impl Case {
    /// Append `text` to `out`, applying any pending case transform char by char.
    fn emit(&mut self, text: &str, out: &mut String) {
        for ch in text.chars() {
            match self.once.take().or(self.span) {
                Some(true) => out.extend(ch.to_uppercase()),
                Some(false) => out.extend(ch.to_lowercase()),
                None => out.push(ch),
            }
        }
    }
}

/// Expand a **vim** replacement string against a single match `m` over `line`,
/// appending to `out`. `&`/`\0` are the whole match, `\1`–`\9` the submatches,
/// `\u \U \l \L \e \E` the case modifiers, `\r`/`\n` → newline (bemtvi splices a
/// line break in either way, like the canonical engine), `\t` → tab, and
/// `\\`/`\&` literal backslash / ampersand.
#[cfg(feature = "vim-regex")]
fn expand_vim_replacement(rep: &str, line: &str, m: &bemtvi_regex::LineMatch, out: &mut String) {
    // The byte range of submatch `n` (group 0 is the whole match), as a slice of
    // the matched line; `None` for a group that did not participate.
    let group = |n: usize| -> Option<&str> {
        m.submatches
            .get(n)
            .copied()
            .flatten()
            .map(|(s, e)| &line[s..e])
    };

    let chars: Vec<char> = rep.chars().collect();
    let mut i = 0;
    let mut case = Case::default();
    while i < chars.len() {
        let c = chars[i];
        if c == '&' {
            case.emit(group(0).unwrap_or(""), out);
            i += 1;
        } else if c == '\\' {
            i += 1;
            let Some(&n) = chars.get(i) else {
                out.push('\\');
                break;
            };
            i += 1;
            match n {
                '0'..='9' => case.emit(group(n as usize - '0' as usize).unwrap_or(""), out),
                '&' => case.emit("&", out),
                '\\' => case.emit("\\", out),
                'r' | 'n' => out.push('\n'),
                't' => out.push('\t'),
                'u' => case.once = Some(true),
                'l' => case.once = Some(false),
                'U' => case.span = Some(true),
                'L' => case.span = Some(false),
                'e' | 'E' => case.span = None,
                // A backslash before any other char yields that char literally.
                other => case.emit(&other.to_string(), out),
            }
        } else {
            case.emit(&c.to_string(), out);
            i += 1;
        }
    }
}
