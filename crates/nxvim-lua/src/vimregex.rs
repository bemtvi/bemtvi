//! A vim-regex `substitute()` for the `vim.fn.substitute` Lua surface, backed by
//! the real vim regexp engine ([`nxvim_regex`] — vim's own backtracking + NFA
//! regexp, vendored as C).
//!
//! Plugins call `substitute({str}, {pat}, {sub}, {flags})` expecting **vim's**
//! regex "magic" dialect and **vim's** replacement syntax — and rely on the exact
//! result (e.g. `lspconfig.util.strip_archive_subpath` strips a `zipfile://…::…`
//! prefix with `'zipfile://\(.\{-}\)::[^\\].*$'` → `'\1'`). This is a **different
//! dialect from nxvim's default `/` search** (canonical regex — see
//! `nxvim-core/src/search.rs`, whose `'regexsyntax'` option can also switch *it* to
//! this same engine); the divergence is deliberate, and lives here in the
//! `vim.fn.*` compatibility layer.
//!
//! Because the engine *is* vim's, constructs the old RE2 translator could not
//! represent — `\zs`/`\ze`, look-around (`\@=` …), in-pattern back-references
//! (`\1`) — now Just Work. The pattern is matched with [`PatternKind::String`]
//! semantics (`\n` is a literal newline, not a line break), the dialect
//! `vim.fn.substitute` documents. We expand the vim replacement against each
//! match's submatches.
//!
//! The replacement honours `&`/`\0` (whole match), `\1`-`\9` (groups), the
//! `\u \U \l \L \e \E` case modifiers, `\r` (newline), `\n` (NUL), `\t` (tab), and
//! `\\`/`\&` literals.

use nxvim_regex::{Engine, LineMatch, PatternKind, VimRegex};

/// `vim.fn.substitute(input, pat, sub, flags)`: replace matches of the vim pattern
/// `pat` in `input` with the vim replacement `sub`. `flags` honours `g` (every
/// match, not just the first), `i` (ignore case) and `I` (force case-sensitive).
/// Returns the substituted string, or a named error on an invalid pattern (fail
/// loud, never a fake identity result).
pub fn substitute(input: &str, pat: &str, sub: &str, flags: &str) -> Result<String, String> {
    let global = flags.contains('g');
    // `i` forces ignore-case and `I` forces match-case; an inline `\c`/`\C` in the
    // pattern overrides them (vim's precedence: the pattern wins, handled inside
    // the engine).
    let ignorecase = flags.contains('i') && !flags.contains('I');
    let re = compile(pat).map_err(|e| e.replace("vim.regex:", "vim.fn.substitute:"))?;

    // Pre-collect the replacement once: it's constant across every match, so
    // re-walking it per match would be wasted work and allocation.
    let sub_chars: Vec<char> = sub.chars().collect();

    let mut out = String::new();
    let mut last = 0;
    let mut from = 0;
    while from <= input.len() {
        let m = match re.exec_line(input, from, ignorecase) {
            Ok(Some(m)) => m,
            Ok(None) => break,
            Err(e) => return Err(format!("vim.fn.substitute: match failed: {e}")),
        };
        // The C engine reports byte offsets; with `\zs`/`\ze`/look-around a match
        // start can precede the previous match end or land off a char boundary.
        // Slice defensively so a pathological pattern is a loud Lua error, not a panic.
        out.push_str(safe_slice(input, last, m.start)?);
        expand_replacement(&sub_chars, input, &m, &mut out)?;
        last = m.end;
        if !global {
            break;
        }
        from = advance(input, m.start, m.end);
    }
    out.push_str(safe_slice(input, last, input.len())?);
    Ok(out)
}

/// Slice `input[start..end]`, returning a named error rather than panicking if the
/// engine handed back a backward range or a non-char-boundary offset.
fn safe_slice(input: &str, start: usize, end: usize) -> Result<&str, String> {
    input.get(start..end).ok_or_else(|| {
        format!("vim.fn.substitute: match byte range {start}..{end} is invalid (not a char boundary or reversed)")
    })
}

/// Compile a vim pattern into a [`VimRegex`], the engine behind the `vim.regex(pat)`
/// Lua object (its `:match_str`) and [`substitute`]. Matched with
/// [`PatternKind::String`] semantics (a string haystack, `\n` literal). Fails loud
/// (named error) on an invalid pattern.
pub fn compile(pat: &str) -> Result<VimRegex, String> {
    VimRegex::compile_with(pat, PatternKind::String, Engine::Auto)
        .map_err(|e| format!("vim.regex: invalid pattern {pat:?}: {e}"))
}

/// The next scan offset after a match `[start, end)` when walking the match
/// sequence. A non-empty match advances past its end; an empty (zero-width) match
/// would loop forever there, so step one whole char past it.
fn advance(input: &str, start: usize, end: usize) -> usize {
    if end > start {
        end
    } else {
        input[end..]
            .chars()
            .next()
            .map_or(end + 1, |c| end + c.len_utf8())
    }
}

/// One-shot / span case transform applied to replacement text by `\u \U \l \L \e
/// \E`. `once` affects only the next char; `span` runs until `\e`/`\E`.
#[derive(Default)]
struct Case {
    once: Option<bool>,
    span: Option<bool>,
}

impl Case {
    /// Append `text` to `out`, applying any pending case transform char by char.
    fn emit(&mut self, text: &str, out: &mut String) {
        for ch in text.chars() {
            let upper = self.once.take().or(self.span);
            match upper {
                Some(true) => out.extend(ch.to_uppercase()),
                Some(false) => out.extend(ch.to_lowercase()),
                None => out.push(ch),
            }
        }
    }
}

/// Expand a vim replacement (pre-split into `chars`) against a match `m` over
/// `input`, appending to `out`. Fails loud if a submatch's byte range is invalid
/// (the engine can report off-boundary offsets for `\zs`/`\ze`/look-around).
fn expand_replacement(
    chars: &[char],
    input: &str,
    m: &LineMatch,
    out: &mut String,
) -> Result<(), String> {
    // The byte range of submatch `n` (group 0 is the whole match), as a slice of
    // the input; `Ok(None)` for a group that did not participate.
    let group = |n: usize| -> Result<Option<&str>, String> {
        match m.submatches.get(n).copied().flatten() {
            Some((s, e)) => safe_slice(input, s, e).map(Some),
            None => Ok(None),
        }
    };

    let mut i = 0;
    let mut case = Case::default();
    while i < chars.len() {
        let c = chars[i];
        if c == '&' {
            // The whole match (magic default).
            case.emit(group(0)?.unwrap_or(""), out);
            i += 1;
        } else if c == '\\' {
            i += 1;
            let Some(&n) = chars.get(i) else {
                out.push('\\');
                break;
            };
            i += 1;
            match n {
                '0'..='9' => case.emit(group(n as usize - '0' as usize)?.unwrap_or(""), out),
                '&' => case.emit("&", out),
                '\\' => case.emit("\\", out),
                // vim's replacement specials: `\r` is a newline, `\n` is a NUL.
                'n' => out.push('\0'),
                'r' => out.push('\n'),
                't' => out.push('\t'),
                'u' => case.once = Some(true),
                'l' => case.once = Some(false),
                'U' => case.span = Some(true),
                'L' => case.span = Some(false),
                'e' | 'E' => case.span = None,
                // A backslash before any other char yields that char.
                other => case.emit(&other.to_string(), out),
            }
        } else {
            case.emit(&c.to_string(), out);
            i += 1;
        }
    }
    Ok(())
}
