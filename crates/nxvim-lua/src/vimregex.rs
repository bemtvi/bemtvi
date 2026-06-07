//! A vim-regex-compatible `substitute()` for the `vim.fn.substitute` Lua surface.
//!
//! Plugins call `substitute({str}, {pat}, {sub}, {flags})` expecting **vim's**
//! regex "magic" dialect and **vim's** replacement syntax — and rely on the exact
//! result (e.g. `lspconfig.util.strip_archive_subpath` strips a `zipfile://…::…`
//! prefix with `'zipfile://\(.\{-}\)::[^\\].*$'` → `'\1'`). This is a **different
//! dialect from nxvim's `/` search** (which is canonical regex — see
//! `nxvim-core/src/search.rs`); the divergence is deliberate, and lives here in the
//! `vim.fn.*` compatibility layer rather than leaking into the editor's own search.
//!
//! We translate the vim pattern into an RE2 (`regex` crate) pattern and expand the
//! vim replacement against the captures of each match. Per the project's
//! no-silent-stubs rule, constructs RE2 cannot faithfully represent — `\zs`/`\ze`,
//! look-around (`\@=` …), in-pattern backreferences (`\1`) — **fail loud** with a
//! named error rather than silently producing the wrong string.
//!
//! Supported: the four magic levels (`\v` very magic, `\m` magic — the default,
//! `\M` nomagic, `\V` very nomagic) including mid-pattern switches; `\(\)` groups
//! and `\%(\)` non-capturing groups; `\|` alternation; `\+ \? \= \{n,m}`
//! quantifiers and the non-greedy `\{-}` family; the `\s \S \d \D \w \W \a \l \u
//! \x …` character classes and `[...]` collections (including POSIX `[:class:]`);
//! `\< \>` word boundaries (as `\b`); and `\c`/`\C` case overrides. The replacement
//! honours `&`/`\0` (whole match), `\1`-`\9` (groups), the `\u \U \l \L \e \E` case
//! modifiers, `\r` (newline), `\n` (NUL), `\t` (tab), and `\\`/`\&` literals.

use regex::{Captures, RegexBuilder};

/// The magic level in force while translating a vim pattern. The default is
/// [`Level::Magic`]; `\v`/`\m`/`\M`/`\V` switch it mid-pattern.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// `\v` — every punctuation char is a regex operator (closest to RE2).
    Very,
    /// `\m` — vim's default: `. * [ ] ^ $` are operators; `( ) | + ? { }` need a
    /// backslash to be operators.
    Magic,
    /// `\M` — only `^ $` are operators; `. * [` are literal.
    No,
    /// `\V` — only `\` is special; everything else is literal.
    VeryNo,
}

/// `vim.fn.substitute(input, pat, sub, flags)`: replace matches of the vim pattern
/// `pat` in `input` with the vim replacement `sub`. `flags` honours `g` (every
/// match, not just the first), `i` (ignore case) and `I` (force case-sensitive).
/// Returns the substituted string, or a named error on an invalid / unsupported
/// pattern (fail loud, never a fake identity result).
pub fn substitute(input: &str, pat: &str, sub: &str, flags: &str) -> Result<String, String> {
    let global = flags.contains('g');
    // `i` forces ignore-case and `I` forces match-case; an inline `\c`/`\C` in the
    // pattern overrides them (vim's precedence: the pattern wins).
    let mut ignorecase = flags.contains('i') && !flags.contains('I');
    let translated = translate_pattern(pat, &mut ignorecase)?;
    let re = RegexBuilder::new(&translated)
        .case_insensitive(ignorecase)
        .build()
        .map_err(|e| format!("vim.fn.substitute: invalid pattern {pat:?}: {e}"))?;

    let mut out = String::new();
    let mut last = 0;
    for caps in re.captures_iter(input) {
        let m = caps.get(0).expect("group 0 always present");
        out.push_str(&input[last..m.start()]);
        expand_replacement(sub, &caps, &mut out);
        last = m.end();
        if !global {
            break;
        }
    }
    out.push_str(&input[last..]);
    Ok(out)
}

/// Compile a vim pattern into an RE2 [`regex::Regex`], the engine behind the
/// `vim.regex(pat)` Lua object (its `:match_str`). Translates vim's magic dialect
/// the same way [`substitute`] does and folds an inline `\c`/`\C` into the
/// case-sensitivity. Fails loud (named error) on an invalid/unsupported pattern.
pub fn compile(pat: &str) -> Result<regex::Regex, String> {
    let mut ignorecase = false;
    let translated = translate_pattern(pat, &mut ignorecase)?;
    RegexBuilder::new(&translated)
        .case_insensitive(ignorecase)
        .build()
        .map_err(|e| format!("vim.regex: invalid pattern {pat:?}: {e}"))
}

/// Translate a vim pattern to an RE2 pattern, tracking the active magic level and
/// folding `\c`/`\C` into `ignorecase`.
fn translate_pattern(pat: &str, ignorecase: &mut bool) -> Result<String, String> {
    let chars: Vec<char> = pat.chars().collect();
    let mut i = 0;
    let mut mode = Level::Magic;
    let mut out = String::new();
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            i += 1;
            let Some(&n) = chars.get(i) else {
                // A trailing backslash matches a literal backslash in vim.
                out.push_str("\\\\");
                break;
            };
            i += 1;
            translate_escaped(n, &mut mode, ignorecase, &chars, &mut i, &mut out)?;
        } else if c == '[' && matches!(mode, Level::Very | Level::Magic) {
            copy_bracket(&chars, &mut i, &mut out)?;
        } else if c == '{' && mode == Level::Very {
            i += 1;
            translate_brace(&chars, &mut i, &mut out);
        } else {
            translate_unescaped(c, mode, &mut out)?;
            i += 1;
        }
    }
    Ok(out)
}

/// Handle a backslash-escaped item `\<n>`. `i` points just past `n`; brace /
/// `\%(` helpers advance it further.
fn translate_escaped(
    n: char,
    mode: &mut Level,
    ignorecase: &mut bool,
    chars: &[char],
    i: &mut usize,
    out: &mut String,
) -> Result<(), String> {
    match n {
        // Magic-level switches.
        'v' => *mode = Level::Very,
        'm' => *mode = Level::Magic,
        'M' => *mode = Level::No,
        'V' => *mode = Level::VeryNo,
        // Case overrides (the pattern wins over the flag).
        'c' => *ignorecase = true,
        'C' => *ignorecase = false,
        // In very magic these are *literal* (the operator is the bare char);
        // in every other level the backslash makes them operators.
        '(' | ')' | '|' | '+' => {
            if *mode == Level::Very {
                push_literal(out, n);
            } else if n == '(' {
                out.push('(');
            } else {
                out.push(n);
            }
        }
        '?' | '=' => {
            if *mode == Level::Very {
                push_literal(out, n);
            } else {
                out.push('?');
            }
        }
        '{' => {
            if *mode == Level::Very {
                push_literal(out, '{');
            } else {
                translate_brace(chars, i, out);
            }
        }
        '}' => push_literal(out, '}'),
        '<' | '>' => out.push_str("\\b"),
        '%' => translate_percent(chars, i, out)?,
        // Character classes (vim → RE2 equivalents).
        's' => out.push_str("[ \\t]"),
        'S' => out.push_str("[^ \\t]"),
        'd' => out.push_str("[0-9]"),
        'D' => out.push_str("[^0-9]"),
        'w' => out.push_str("[0-9A-Za-z_]"),
        'W' => out.push_str("[^0-9A-Za-z_]"),
        'a' => out.push_str("[A-Za-z]"),
        'A' => out.push_str("[^A-Za-z]"),
        'l' => out.push_str("[a-z]"),
        'L' => out.push_str("[^a-z]"),
        'u' => out.push_str("[A-Z]"),
        'U' => out.push_str("[^A-Z]"),
        'x' => out.push_str("[0-9A-Fa-f]"),
        'X' => out.push_str("[^0-9A-Fa-f]"),
        'o' => out.push_str("[0-7]"),
        'O' => out.push_str("[^0-7]"),
        'h' => out.push_str("[A-Za-z_]"),
        'H' => out.push_str("[^A-Za-z_]"),
        // Escape sequences.
        'n' => out.push_str("\\n"),
        't' => out.push_str("\\t"),
        'r' => out.push_str("\\r"),
        'e' => out.push_str("\\x1b"),
        // Escaped metacharacters → literal.
        '.' | '*' | '[' | ']' | '^' | '$' | '~' | '/' | '\\' => push_literal(out, n),
        // RE2 can't represent these — fail loud rather than mis-translate.
        'z' => return Err("vim.fn.substitute: \\zs/\\ze is unsupported".into()),
        '@' => return Err("vim.fn.substitute: look-around (\\@…) is unsupported".into()),
        '1'..='9' => {
            return Err(format!(
                "vim.fn.substitute: in-pattern backreference \\{n} is unsupported"
            ))
        }
        // A backslash before any other char matches that char literally.
        _ => push_literal(out, n),
    }
    Ok(())
}

/// Handle a bare (unescaped) char at magic level `mode`.
fn translate_unescaped(c: char, mode: Level, out: &mut String) -> Result<(), String> {
    match mode {
        Level::Very => match c {
            '(' | ')' | '|' | '+' | '?' | '.' | '*' | '^' | '$' => out.push(c),
            '<' | '>' => out.push_str("\\b"),
            '@' => return Err("vim.fn.substitute: look-around (\\@…) is unsupported".into()),
            _ => push_literal(out, c),
        },
        Level::Magic => match c {
            '.' | '*' | '^' | '$' => out.push(c),
            // Literal in magic — escape so RE2 doesn't read them as operators.
            '(' | ')' | '|' | '+' | '?' | '{' | '}' => {
                out.push('\\');
                out.push(c);
            }
            _ => push_literal(out, c),
        },
        Level::No => match c {
            '^' | '$' => out.push(c),
            _ => push_literal(out, c),
        },
        Level::VeryNo => push_literal(out, c),
    }
    Ok(())
}

/// Push `c` as a literal, escaping it when it is an RE2 metacharacter.
fn push_literal(out: &mut String, c: char) {
    if ".^$*+?()[]{}|\\".contains(c) {
        out.push('\\');
    }
    out.push(c);
}

/// Translate a vim quantifier brace whose contents start at `*i` (just past the
/// opening `{` / `\{`) and run to the closing `}` / `\}`. Handles the non-greedy
/// `-` prefix and the empty (`{}` → `*`) and open-ended forms.
fn translate_brace(chars: &[char], i: &mut usize, out: &mut String) {
    let mut spec = String::new();
    while *i < chars.len() {
        let c = chars[*i];
        if c == '}' {
            *i += 1;
            break;
        }
        if c == '\\' && chars.get(*i + 1) == Some(&'}') {
            *i += 2;
            break;
        }
        spec.push(c);
        *i += 1;
    }
    let non_greedy = spec.starts_with('-');
    let body = spec.trim_start_matches('-');
    if body.is_empty() {
        // `\{}` / `\{-}` — zero or more (greedy / non-greedy).
        out.push('*');
    } else {
        // RE2 needs an explicit lower bound: `\{,m}` → `{0,m}`.
        let body = if let Some(stripped) = body.strip_prefix(',') {
            format!("0,{stripped}")
        } else {
            body.to_string()
        };
        out.push('{');
        out.push_str(&body);
        out.push('}');
    }
    if non_greedy {
        out.push('?');
    }
}

/// Handle a `\%…` item. Only the non-capturing group `\%(` is representable in
/// RE2 (`(?:`); other `\%` forms (`\%[`, `\%d…`, `\%^`, …) fail loud.
fn translate_percent(chars: &[char], i: &mut usize, out: &mut String) -> Result<(), String> {
    if chars.get(*i) == Some(&'(') {
        out.push_str("(?:");
        *i += 1;
        Ok(())
    } else {
        Err("vim.fn.substitute: \\%… patterns are unsupported".into())
    }
}

/// Copy a `[...]` collection from `*i` (at the `[`) to its matching `]`, verbatim
/// (vim and RE2 bracket syntax agree closely, POSIX `[:class:]` included), so the
/// magic rules outside the class don't mangle its contents.
fn copy_bracket(chars: &[char], i: &mut usize, out: &mut String) -> Result<(), String> {
    out.push('[');
    *i += 1;
    if chars.get(*i) == Some(&'^') {
        out.push('^');
        *i += 1;
    }
    // A `]` as the first member is a literal `]`, not the terminator.
    if chars.get(*i) == Some(&']') {
        out.push_str("\\]");
        *i += 1;
    }
    while *i < chars.len() {
        let c = chars[*i];
        if c == ']' {
            out.push(']');
            *i += 1;
            return Ok(());
        }
        // POSIX class `[:name:]` — copy as a unit (its `]` isn't the terminator).
        if c == '[' && chars.get(*i + 1) == Some(&':') {
            out.push('[');
            out.push(':');
            *i += 2;
            while *i < chars.len() {
                if chars[*i] == ':' && chars.get(*i + 1) == Some(&']') {
                    out.push_str(":]");
                    *i += 2;
                    break;
                }
                out.push(chars[*i]);
                *i += 1;
            }
            continue;
        }
        if c == '\\' {
            if let Some(&next) = chars.get(*i + 1) {
                out.push('\\');
                out.push(next);
                *i += 2;
                continue;
            }
        }
        out.push(c);
        *i += 1;
    }
    Err("vim.fn.substitute: unterminated [] collection".into())
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

/// Expand a vim replacement string against a match's captures, appending to `out`.
fn expand_replacement(sub: &str, caps: &Captures, out: &mut String) {
    let chars: Vec<char> = sub.chars().collect();
    let mut i = 0;
    let mut case = Case::default();
    while i < chars.len() {
        let c = chars[i];
        if c == '&' {
            // The whole match (magic default).
            let whole = caps.get(0).map_or("", |m| m.as_str());
            case.emit(whole, out);
            i += 1;
        } else if c == '\\' {
            i += 1;
            let Some(&n) = chars.get(i) else {
                out.push('\\');
                break;
            };
            i += 1;
            match n {
                '0'..='9' => {
                    let idx = n as usize - '0' as usize;
                    let group = caps.get(idx).map_or("", |m| m.as_str());
                    case.emit(group, out);
                }
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
}
