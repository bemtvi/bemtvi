//! Ex-command dispatch (`execute_ex`), range/address parsing, `:substitute`, and
//! the file/window/tab ex-commands.

use super::*;
use crate::buffer::Buffer;
use crate::extmark::{VirtChunk, VirtDecor, VirtTextPos, DEFAULT_PRIORITY, SUBST_PREVIEW_NS};
use crate::input::{Key, KeyCode};
use crate::search::SearchRegex;
use std::path::PathBuf;

/// Byte offset of the char boundary just past `byte` in `line`, or `line.len()`
/// at the end. Used to force the confirm walk past a zero-width match.
fn next_char_boundary(line: &str, byte: usize) -> usize {
    line[byte..]
        .chars()
        .next()
        .map_or(line.len(), |c| byte + c.len_utf8())
}

/// Which `:echo`-family command is running — they differ only in where the
/// evaluated text lands.
#[derive(Clone, Copy)]
enum EchoKind {
    /// `:echo` — message line only, *not* recorded in `:messages`.
    Transient,
    /// `:echomsg` — message line *and* the `:messages` history.
    Message,
    /// `:echoerr` — surfaced as an error (and recorded), like a failed command.
    Error,
}

/// A resolved, 0-based, inclusive line range parsed from the head of an
/// ex-command. `explicit` is false when no address was present and the range
/// defaulted to the current line (so a bare range can be told from none).
#[derive(Clone, Copy)]
struct ExRange {
    lo: usize,
    hi: usize,
    explicit: bool,
}

/// An in-flight `:s///c` confirm substitute (see [`Editor::subst_confirm`]). The
/// match-by-match walk over `[.., hi]`, paused on each match's `replace with …?`
/// prompt; the answer key ([`Editor::subst_confirm_key`]) drives it forward.
///
/// Positions track *live* (post-edit) rope coordinates: a `\r`-splitting
/// replacement pushes later lines down, so applying a match bumps `hi` and the
/// continuation line by the number of newlines it introduced. `cur` is the match
/// currently being prompted (`Some` while a prompt is showing).
pub(crate) struct SubstConfirm {
    re: SearchRegex,
    rep: String,
    /// Replace every match on a line (`g`), not just the first.
    global: bool,
    /// Last line to scan, in live coordinates (grows as `\r` splits add lines).
    hi: usize,
    /// Line currently being scanned, in live coordinates.
    line: usize,
    /// Next byte offset to search from within `line`.
    byte: usize,
    /// The match awaiting an answer: `(start, end, expanded replacement)`.
    cur: Option<(usize, usize, String)>,
    /// `y`/`a`/`l` to one match already happened on the current line.
    line_dirty: bool,
    subs: usize,
    nlines: usize,
    last_changed: Option<usize>,
    /// The single undo snapshot is pushed lazily, on the first applied match.
    pushed: bool,
}

/// Split a substitute body (everything after the leading delimiter) into
/// `(pattern, replacement, flags)` on unescaped `delim`. A `\` before the
/// delimiter makes it literal (dropped from the field); any other `\x` is kept
/// verbatim so the regex / replacement expander sees it. Everything past the
/// second delimiter — the flags and trailing count — is taken verbatim.
/// Expand an unescaped `~` in a substitute replacement to `prev` (the previous
/// replacement string); `\~` stays a literal tilde. All other backslash escapes
/// pass through verbatim for [`SearchRegex::substitute_line`]'s expansion pass to
/// handle. Errors (`Err(())`) on a bare `~` when there is no previous
/// replacement, so the caller can fail loud rather than insert nothing.
fn expand_tilde(rep: &str, prev: Option<&str>) -> Result<String, ()> {
    let mut out = String::new();
    let mut chars = rep.chars();
    while let Some(c) = chars.next() {
        match c {
            // Copy a backslash escape (e.g. `\~`, `\r`) through untouched.
            '\\' => {
                out.push('\\');
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            }
            '~' => match prev {
                Some(p) => out.push_str(p),
                None => return Err(()),
            },
            _ => out.push(c),
        }
    }
    Ok(out)
}

/// Expand a leading `~` / `~/…` in a file argument to the home directory (vim
/// filename expansion). Only the argument's leading tilde is special; `~` anywhere
/// else is a literal path character. `~user` (a passwd lookup, which the core has no
/// way to resolve) and a `~` with no resolvable home are left verbatim. Borrows when
/// there is nothing to expand.
///
/// `home_override` is the **daemon's** home in a remote session (seeded at connect):
/// the core runs on the client but the read lands on the daemon, so `~` must mean the
/// daemon's home there. `None` — the local case — reads `$HOME` from this process.
fn expand_leading_tilde<'a>(
    arg: &'a str,
    home_override: Option<&std::path::Path>,
) -> std::borrow::Cow<'a, str> {
    let Some(rest) = arg.strip_prefix('~') else {
        return std::borrow::Cow::Borrowed(arg);
    };
    // `~` alone or `~/…`; `~user` (a non-empty, non-`/` remainder) is not ours.
    if !(rest.is_empty() || rest.starts_with('/')) {
        return std::borrow::Cow::Borrowed(arg);
    }
    let home = match home_override {
        Some(h) => Some(h.to_string_lossy().into_owned()),
        None => std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned()),
    };
    match home {
        Some(home) => std::borrow::Cow::Owned(format!("{home}{rest}")),
        None => std::borrow::Cow::Borrowed(arg),
    }
}

/// Whether `body` (a substitute body, everything after the opening delimiter)
/// contains an **unescaped** `delim` — i.e. the replacement half has been opened
/// (`:s/pat/…`). A `\delim` is a literal delimiter and doesn't count; any other
/// `\x` skips its escaped char. Used to hand the match off from the plain pattern
/// preview to the richer replacement diff preview.
fn has_unescaped_delim(body: &str, delim: char) -> bool {
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            chars.next(); // skip the escaped char (incl. an escaped delimiter)
        } else if c == delim {
            return true;
        }
    }
    false
}

fn split_substitute(body: &str, delim: char) -> (String, String, String) {
    let mut parts: Vec<String> = vec![String::new()];
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        // Once pat and rep are captured, flags + count are taken verbatim.
        if parts.len() >= 3 {
            parts[2].push(c);
        } else if c == '\\' {
            let cur = parts.last_mut().expect("at least one part");
            match chars.next() {
                Some(n) if n == delim => cur.push(delim),
                Some(n) => {
                    cur.push('\\');
                    cur.push(n);
                }
                None => cur.push('\\'),
            }
        } else if c == delim {
            parts.push(String::new());
        } else {
            parts.last_mut().expect("at least one part").push(c);
        }
    }
    let pat = parts.first().cloned().unwrap_or_default();
    let rep = parts.get(1).cloned().unwrap_or_default();
    let flags = parts.get(2).cloned().unwrap_or_default();
    (pat, rep, flags)
}

/// Split a `:global` body (everything after the leading delimiter) into
/// `(pattern, command)` on the first unescaped `delim`. `\delim` is a literal
/// delimiter in the pattern; everything past the delimiter is the command, taken
/// verbatim (it may itself contain delimiters). No closing delimiter → an empty
/// command (the caller defaults it to `:print`).
fn split_global(body: &str, delim: char) -> (String, String) {
    let mut pat = String::new();
    let mut chars = body.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(n) if n == delim => pat.push(delim),
                Some(n) => {
                    pat.push('\\');
                    pat.push(n);
                }
                None => pat.push('\\'),
            },
            _ if c == delim => return (pat, chars.collect()),
            _ => pat.push(c),
        }
    }
    (pat, String::new())
}

/// Recognize `:norm[al][!]` at the start of an ex remainder, returning
/// `(bang, literal_arg)` — or `None` when it isn't the `:normal` command. The
/// name accepts vim's abbreviations (`norm`..`normal`); without a `!` the name
/// must be followed by a space or end of line, so `:normalize` / `:normx` stay
/// other commands. One separating space after the name (and optional bang) is
/// consumed; everything after it is the literal argument, untrimmed.
fn parse_normal_prefix(rest: &str) -> Option<(bool, &str)> {
    // Longest first, so the full name is consumed before a shorter abbreviation.
    let after_name = ["normal", "norma", "norm"]
        .into_iter()
        .find_map(|n| rest.strip_prefix(n))?;
    let (bang, after_bang) = match after_name.strip_prefix('!') {
        Some(b) => (true, b),
        None => (false, after_name),
    };
    if !bang {
        // The bang already separates the name from its argument; without it the
        // name must end here or at a space (else it's a longer command name).
        match after_bang.chars().next() {
            None | Some(' ') => {}
            Some(_) => return None,
        }
    }
    Some((bang, after_bang.strip_prefix(' ').unwrap_or(after_bang)))
}

/// Convert a `:normal` literal argument into keys — one keystroke per character.
/// Control bytes map to their named keys (`\r`/`\n`→Enter, `\x1b`→Esc, `\t`→Tab,
/// `\x08`/`\x7f`→Backspace) or, for the rest of the C0 range, `<C-letter>`
/// (`\x17`→`<C-w>`), matching vim's handling of a control byte embedded via
/// `:execute "normal! …"`. Every other character is itself.
fn normal_keys(arg: &str) -> Vec<Key> {
    arg.chars()
        .map(|c| match c {
            '\r' | '\n' => Key::new(KeyCode::Enter),
            '\x1b' => Key::new(KeyCode::Esc),
            '\t' => Key::new(KeyCode::Tab),
            '\x08' | '\x7f' => Key::new(KeyCode::Backspace),
            c if ('\u{1}'..='\u{1a}').contains(&c) => Key::ctrl((b'a' + (c as u8 - 1)) as char),
            c => Key::char(c),
        })
        .collect()
}

/// The directory part of `f` (vim's `:h` modifier): everything up to the last
/// `/`. No slash → `.` (the current directory); a single leading `/` → `/`.
fn fmod_head(f: &str) -> String {
    match f.rfind('/') {
        None => ".".to_string(),
        Some(0) => "/".to_string(),
        Some(p) => f[..p].to_string(),
    }
}

/// The last path component of `f` (vim's `:t` modifier).
fn fmod_tail(f: &str) -> String {
    match f.rfind('/') {
        Some(p) => f[p + 1..].to_string(),
        None => f.to_string(),
    }
}

/// `f` with the last extension of its tail component stripped (vim's `:r`
/// modifier). A leading dot is *not* an extension (`.bashrc:r` == `.bashrc`).
fn fmod_root(f: &str) -> String {
    let (dir, tail) = match f.rfind('/') {
        Some(p) => (&f[..=p], &f[p + 1..]),
        None => ("", f),
    };
    let cut = tail
        .char_indices()
        .rev()
        .find(|&(idx, c)| c == '.' && idx >= 1)
        .map(|(idx, _)| idx);
    match cut {
        Some(idx) => format!("{dir}{}", &tail[..idx]),
        None => f.to_string(),
    }
}

/// The extension of `f` (vim's `:e` modifier), or `""` when the tail has none. A
/// run of `k` consecutive `:e` widens it to the last `k` dot-separated components
/// (capped at the count present) — vim's quirk, mirrored from `vim.fn.fnamemodify`.
fn fmod_ext(f: &str, k: usize) -> String {
    let tail = fmod_tail(f);
    let dots: Vec<usize> = tail
        .char_indices()
        .filter(|&(idx, c)| c == '.' && idx >= 1)
        .map(|(idx, _)| idx)
        .collect();
    if dots.is_empty() {
        return String::new();
    }
    let idx = dots.len().saturating_sub(k);
    tail[dots[idx] + 1..].to_string()
}

/// Apply a validated run of filename modifiers (`h`/`t`/`r`/`e` letters, the `:`
/// already stripped) to `fname`, left to right. Consecutive `e`s collapse into a
/// single widened [`fmod_ext`] call.
fn apply_file_mods(fname: &str, mods: &[char]) -> String {
    let mut fname = fname.to_string();
    let mut i = 0;
    while i < mods.len() {
        match mods[i] {
            'h' => {
                fname = fmod_head(&fname);
                i += 1;
            }
            't' => {
                fname = fmod_tail(&fname);
                i += 1;
            }
            'r' => {
                fname = fmod_root(&fname);
                i += 1;
            }
            'e' => {
                let mut k = 0;
                while i < mods.len() && mods[i] == 'e' {
                    k += 1;
                    i += 1;
                }
                fname = fmod_ext(&fname, k);
            }
            // Only the four pure modifiers reach here; the parser rejects the rest.
            _ => i += 1,
        }
    }
    fname
}

/// Trim an ex-command argument string: drop all leading ASCII whitespace, and
/// drop trailing ASCII whitespace *unless the last whitespace char is escaped by
/// an odd number of preceding backslashes* (a `\ `-protected trailing space, which
/// vim keeps for the arg parser to unescape — e.g. `:set fillchars=eob:\ `). For an
/// argument with no escaped trailing space this is exactly `str::trim`.
fn trim_ex_args(s: &str) -> &str {
    let s = s.trim_start();
    let trimmed = s.trim_end();
    // `trim_end` may have eaten a backslash-escaped space; give back the single
    // whitespace char if the backslash run just before it is odd (so the last
    // backslash escapes the space rather than being a literal `\\`).
    if trimmed.len() < s.len() {
        let backslashes = trimmed.bytes().rev().take_while(|&b| b == b'\\').count();
        if backslashes % 2 == 1 {
            // Keep one trailing whitespace char (the escaped one).
            return &s[..trimmed.len() + 1];
        }
    }
    trimmed
}

/// Split an ex-command into `(name, bang, args)`.
fn split_ex(cmd: &str) -> (&str, bool, &str) {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
        i += 1;
    }
    let name = &cmd[..i];
    let mut bang = false;
    if i < bytes.len() && bytes[i] == b'!' {
        bang = true;
        i += 1;
    }
    // Leading whitespace is always insignificant; trailing whitespace is dropped
    // too, *except* a final whitespace run that is backslash-escaped (`:set
    // fillchars=eob:\ `, `:w file\ name`) — vim keeps that, and the per-command
    // arg parsers unescape it. Stripping it here would leave a dangling backslash.
    let args = trim_ex_args(&cmd[i..]);
    (name, bang, args)
}

/// Commands that see a `|` as **part of their argument** rather than as a command
/// separator — vim's `:help :bar` exception list, restricted to the commands nxvim
/// has. Their argument is itself code or a shell/keystroke string, so a bar inside
/// it belongs to *them*: `:g/x/s/a/b/|d` is one `:global` whose sub-command chains,
/// and `:normal A|` types a literal bar.
fn ex_takes_bar(name: &str) -> bool {
    matches!(
        name,
        "normal"
            | "norm"
            | "g"
            | "global"
            | "v"
            | "vglobal"
            | "au"
            | "autocmd"
            | "com"
            | "command"
            | "func"
            | "function"
            | "lua"
            | "luado"
            | "argdo"
            | "bufdo"
            | "windo"
            | "tabdo"
            | "cdo"
            | "ldo"
            | "make"
            | "h"
            | "help"
    )
}

/// Byte index where the command *name* starts — past a leading `:`, whitespace, and
/// any address range. Search addresses (`/pat/`, `?pat?`) are skipped as spans so a
/// bar inside a pattern can't be mistaken for the command name's start.
fn ex_name_start(cmd: &str) -> usize {
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b' ' | b'\t' | b':' | b'.' | b'$' | b'%' | b',' | b';' | b'+' | b'-' => i += 1,
            b'0'..=b'9' => i += 1,
            // `'a` — a mark address; the mark char is never a delimiter.
            b'\'' => i += 2,
            // A `/pat/` or `?pat?` address: skip to the closing (unescaped) delimiter.
            d @ (b'/' | b'?') => {
                i += 1;
                while i < bytes.len() && bytes[i] != d {
                    i += if bytes[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            _ => break,
        }
    }
    i.min(cmd.len())
}

/// How many leading **delimited** sections a command's argument opens — sections a
/// bar can legally appear inside, so [`split_ex_bar`] must scan past them.
/// `:s/pat/rep/flags` has two, `:vimgrep /pat/flags files…` has one, everything else
/// has none.
fn ex_pattern_sections(name: &str) -> usize {
    match name {
        "s" | "su" | "sub" | "subs" | "subst" | "substi" | "substit" | "substitu" | "substitut"
        | "substitute" => 2,
        "vim" | "vimg" | "vimgr" | "vimgre" | "vimgrep" | "vimgrepa" | "vimgrepad"
        | "vimgrepadd" | "lvim" | "lvimg" | "lvimgr" | "lvimgre" | "lvimgrep" | "lvimgrepa"
        | "lvimgrepad" | "lvimgrepadd" => 1,
        _ => 0,
    }
}

/// Byte index just past a command's leading delimited sections, given the index
/// where its argument starts and how many sections it opens
/// ([`ex_pattern_sections`]). Bars before this point belong to the pattern
/// ([`split_ex_bar`]); flags, file arguments, and any `|` separator follow it.
///
/// The delimiter is whatever non-alphanumeric char opens the argument (`:s!a!b!` is
/// as valid as `:s/a/b/`), and `\` escapes it. An argument that opens no section — a
/// bare `:s`, or an alphanumeric flag/count — has none, so the scan starts right at
/// the argument. An unterminated section runs to the end of the line, exactly as
/// [`split_substitute`] reads it.
///
/// Skipping the whole span (rather than reasoning about escapes inside it) is what
/// keeps this correct under either `'regexsyntax'` engine: PCRE alternates on a bare
/// `|`, vim's on `\|`, and neither one's bars reach the separator scan.
fn pattern_sections_end(cmd: &str, arg_start: usize, sections: usize) -> usize {
    let bytes = cmd.as_bytes();
    let mut i = arg_start;
    while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
        i += 1;
    }
    let Some(&delim) = bytes.get(i) else {
        return i;
    };
    if delim.is_ascii_alphanumeric() || delim == b'\\' || delim == b'"' || delim == b'|' {
        return i;
    }
    i += 1;
    // Each section ends at the next unescaped delimiter.
    for _ in 0..sections {
        while i < bytes.len() && bytes[i] != delim {
            i += if bytes[i] == b'\\' { 2 } else { 1 };
        }
        if i >= bytes.len() {
            return bytes.len();
        }
        i += 1;
    }
    i.min(bytes.len())
}

/// Split a command line at the first `|` that separates two commands, into
/// `(first command, rest of the line)`. `None` when the whole line is one command.
///
/// A bar is a separator unless it is backslash-escaped, the line's command takes the
/// bar as part of its argument ([`ex_takes_bar`]), or it sits inside a leading
/// delimited pattern section ([`ex_pattern_sections`]).
///
/// That last case is where nxvim departs from vim, and it matters: nxvim's regex
/// flavor is PCRE, so a **bare** `|` is alternation (`:s/two|three/X/`) and `\|` is a
/// literal bar — the reverse of vim, where `\|` alternates and a bare bar always
/// separates commands. Splitting on the bare bar would silently truncate the pattern
/// and run its tail as a command, so the scan skips past the delimited sections and
/// only looks for a separator after the final delimiter (`:s/a/b/|w` still chains).
fn split_ex_bar(cmd: &str) -> Option<(&str, &str)> {
    let bytes = cmd.as_bytes();
    let mut i = ex_name_start(cmd);
    // The command name, for the takes-a-bar check. `:!cmd` (filter through a shell)
    // has no alphabetic name and takes the rest of the line verbatim.
    if bytes.get(i) == Some(&b'!') {
        return None;
    }
    // `:={expr}` evaluates a Lua expression, so a `|` inside it is the operand it is
    // in Lua (5.4 bitwise-or), not a command separator — the whole rest is the expr,
    // exactly as `:lua` swallows its chunk.
    if bytes.get(i) == Some(&b'=') {
        return None;
    }
    let name_end = i + cmd[i..]
        .bytes()
        .take_while(|b| b.is_ascii_alphabetic())
        .count();
    if ex_takes_bar(&cmd[i..name_end]) {
        return None;
    }
    let sections = ex_pattern_sections(&cmd[i..name_end]);
    if sections > 0 {
        i = pattern_sections_end(cmd, name_end, sections);
    }
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'|' => return Some((&cmd[..i], &cmd[i + 1..])),
            _ => i += 1,
        }
    }
    None
}

/// Parse a buffer-navigation count argument (`:bnext 2`). Empty / invalid / zero
/// all mean 1, matching vim's default repeat count.
fn parse_count_arg(args: &str) -> usize {
    parse_opt_count_arg(args).unwrap_or(1)
}

/// Format vim's substitute summary line — `"{count} {sing|plur} on {lines}
/// line[s]"` — with the count/line singular-plural agreement the `:s` counting,
/// edit, and confirm passes all share (only the noun differs: `match`/`matches`
/// for the `n` flag, `substitution`/`substitutions` for an actual run).
fn fmt_subst_report(count: usize, sing: &str, plur: &str, lines: usize) -> String {
    format!(
        "{count} {} on {lines} {}",
        if count == 1 { sing } else { plur },
        if lines == 1 { "line" } else { "lines" },
    )
}

/// A positive numeric command argument, or `None` when absent / non-numeric. The
/// `Option`-preserving form of [`parse_count_arg`] — `:tabnext` (no count → next
/// tab) needs the absent case distinguished from `1` (no count → tab 1).
fn parse_opt_count_arg(args: &str) -> Option<usize> {
    args.trim().parse::<usize>().ok().filter(|n| *n > 0)
}

/// Parse a `:sleep` argument: `{n}` = seconds, `{n}m` = milliseconds, empty =
/// 1 second (matching vim). Returns a vim-style `E475` error string for
/// non-integer input.
fn parse_sleep(args: &str) -> Result<u64, String> {
    let a = args.trim();
    if a.is_empty() {
        return Ok(1000);
    }
    let invalid = || format!("E475: Invalid argument: {a}");
    match a.strip_suffix('m') {
        Some(ms) => ms.trim().parse::<u64>().map_err(|_| invalid()),
        None => a
            .parse::<u64>()
            .map(|secs| secs.saturating_mul(1000))
            .map_err(|_| invalid()),
    }
}

impl Editor {
    /// The current buffer's file name as typed (vim's `%`), or `None` when the
    /// buffer is unnamed.
    fn current_file_name(&self) -> Option<String> {
        self.buffer()
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .filter(|s| !s.is_empty())
    }

    /// The alternate buffer's file name (vim's `#`), or `None` when there is no
    /// alternate or it is unnamed.
    ///
    /// Reads the tracked *name* rather than the alternate buffer's live path: `#` is
    /// a file name in vim and outlives the buffer it named, so `:e #` still reloads a
    /// file whose buffer has been `:bdelete`d (the `:%bd|e#` idiom).
    pub fn alternate_file_name(&self) -> Option<String> {
        self.alternate_name
            .as_ref()
            .map(|p| p.display().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Expand the `%` (current file) and `#` (alternate file) tokens in a file
    /// argument, each optionally followed by a run of `:` filename modifiers
    /// (`:h` head, `:t` tail, `:r` root, `:e` extension). `\%` / `\#` are literal.
    /// A `:` not introducing a known modifier ends the run and stays literal.
    ///
    /// A leading `~` / `~/…` first expands to `$HOME` (vim filename expansion): only
    /// the argument's leading tilde is special (`~` elsewhere is literal), and `~user`
    /// — a passwd lookup — is left verbatim. Reading `$HOME` from the env here matches
    /// [`absolutize_normalize`](super::absolutize_normalize) reading the process cwd:
    /// the core resolves paths against the process it runs in (the daemon's, for a
    /// remote session), which is where the file read lands.
    ///
    /// Returns the rewritten argument, or a vim-style error string when a token
    /// has no name to substitute, or when an env-dependent *modifier* (`:p` / `:~` /
    /// `:.`) is used — those need the working directory, which the pure core
    /// deliberately can't reconstruct, so they fail loud rather than mis-expand.
    pub(crate) fn expand_file_arg(&self, arg: &str) -> Result<String, String> {
        let expanded = expand_leading_tilde(arg, self.remote_home.as_deref());
        let chars: Vec<char> = expanded.chars().collect();
        let mut out = String::new();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            // `\%` / `\#` insert a literal `%` / `#`.
            if c == '\\' && matches!(chars.get(i + 1), Some('%') | Some('#')) {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c != '%' && c != '#' {
                out.push(c);
                i += 1;
                continue;
            }
            let name = if c == '%' {
                self.current_file_name()
                    .ok_or_else(|| "E499: Empty file name for '%'".to_string())?
            } else {
                self.alternate_file_name().ok_or_else(|| {
                    "E194: No alternate file name to substitute for '#'".to_string()
                })?
            };
            i += 1;
            // Consume a run of `:x` modifiers immediately following the token.
            let mut mods = Vec::new();
            while chars.get(i) == Some(&':') {
                match chars.get(i + 1) {
                    Some(&m @ ('h' | 't' | 'r' | 'e')) => {
                        mods.push(m);
                        i += 2;
                    }
                    Some(&m @ ('p' | '~' | '.')) => {
                        return Err(format!(
                            "E499: ':{m}' filename modifier is not supported in \
                             command-line expansion (needs the working directory)"
                        ));
                    }
                    // A `:` that isn't a modifier ends the run; it stays literal.
                    _ => break,
                }
            }
            out.push_str(&apply_file_mods(&name, &mods));
        }
        Ok(out)
    }

    /// Expand `%`/`#` in a file argument for dispatch, echoing and returning `None`
    /// on a bad expansion so the caller aborts the command (vim's behavior).
    pub(crate) fn expand_file_arg_or_echo(&mut self, arg: &str) -> Option<String> {
        match self.expand_file_arg(arg) {
            Ok(s) => Some(s),
            Err(e) => {
                self.echo(e);
                None
            }
        }
    }

    /// Run one command line, which may chain several commands with `|` (vim's
    /// `:bar`) — `:%bd|e#|bd#`, `:w|q`, `:e file|normal G`.
    ///
    /// Splitting is escape- and command-aware: `\|` is a literal bar (so `:s/a\|b/c/`
    /// keeps its alternation), and a command that takes the bar as *part of its
    /// argument* ([`ex_takes_bar`]) swallows the rest of the line. As in vim, a
    /// segment that reports an error aborts the rest of the chain — `:e missing|%d`
    /// must not delete the buffer it failed to replace.
    pub(crate) fn execute_ex(&mut self, raw: &str) {
        if split_ex_bar(raw).is_none() {
            // The overwhelmingly common single-command case: no split, no state to
            // reset — run the line exactly as before.
            self.execute_ex_one(raw);
            return;
        }
        // Only a *fresh* error aborts the chain, so clear any stale flag left by an
        // earlier command line before watching it.
        self.message_error = false;
        let mut rest = raw;
        loop {
            let (seg, tail) = match split_ex_bar(rest) {
                Some((seg, tail)) => (seg, Some(tail)),
                None => (rest, None),
            };
            let deferred_before = self.deferred_commands.len();
            self.execute_ex_one(seg);
            let Some(tail) = tail else { return };
            if self.message_error {
                return;
            }
            // This segment is unknown to the core and will run *after* the tick. The
            // rest of the line has to wait for it rather than running now — otherwise
            // `:MyCmd|w` writes before `MyCmd` has done anything. Hand the tail over
            // to the same queue, right behind it.
            if self.deferred_commands.len() > deferred_before {
                self.deferred_commands
                    .push(DeferredCmd::Chain(tail.to_string()));
                return;
            }
            rest = tail;
        }
    }

    fn execute_ex_one(&mut self, raw: &str) {
        // Escape-aware trim: a backslash-escaped trailing space (`:set
        // fillchars=eob:\ `) must survive to the per-command arg parser, so a plain
        // `raw.trim()` here (which would eat it, leaving a dangling `\`) is wrong.
        let cmd = trim_ex_args(raw);
        if cmd.is_empty() {
            return;
        }

        // `:normal`'s argument is *literal*, so whitespace at the end of it is a
        // real keystroke — `\t` is `<Tab>` (== `<C-i>`, jump forward) and a space
        // is `l` — but the trim above has just eaten it. When the line did lose a
        // trailing run, re-parse it from `raw` and dispatch `:normal` with the
        // argument untrimmed. Only leading whitespace is skipped, which is always
        // insignificant, and a range prefix (`%`, `2,3`) can't carry a meaningful
        // trailing space either, so nothing significant is lost by the pre-parse.
        // Guarded on the length change so the common no-trailing-space line keeps
        // the single range parse below.
        let lead = raw.trim_start();
        if lead.len() > cmd.len() {
            if let Ok((range, rest)) = self.parse_ex_range(lead) {
                if let Some((bang, body)) = parse_normal_prefix(rest.trim_start()) {
                    self.ex_normal(range, bang, body);
                    return;
                }
            }
        }

        // Strip and resolve any leading range (`.`, `$`, `%`, `N`, `+N`, `lo,hi`)
        // before the command name. A range with no command moves the cursor to
        // the last address; a malformed range fails loud rather than guessing.
        let (range, rest) = match self.parse_ex_range(cmd) {
            Ok(parsed) => parsed,
            Err(e) => {
                self.echo(e);
                return;
            }
        };
        // The raw range text (`%`, `2,3`, …) preceding the command name. Line-range
        // commands use the resolved `range`; the buffer-range commands (`:bdelete`
        // & co.) re-parse this span as *buffer numbers* instead.
        let range_text = &cmd[..cmd.len() - rest.len()];
        let rest = rest.trim_start();
        if rest.is_empty() {
            if range.explicit {
                // A `:line` move is a jump: stash the previous-context mark first.
                self.record_jump_context();
                self.cursor.line = range.hi;
                self.cursor.col = self.first_non_blank(range.hi);
            }
            return;
        }

        // `:&` and `:&&` repeat the last substitute (a bare `&` resets its flags;
        // `&&` keeps them). These have no alphabetic name, so they're matched on
        // the raw remainder before `split_ex` would strip an empty name.
        if let Some(after) = rest.strip_prefix("&&") {
            self.repeat_substitute(range, after.trim(), true);
            return;
        }
        if let Some(after) = rest.strip_prefix('&') {
            self.repeat_substitute(range, after.trim(), false);
            return;
        }

        // `:={expr}` — neovim's shorthand for `:lua vim.print({expr})`: evaluate the
        // Lua expression and pretty-print the result. Like `:&`, it has no alphabetic
        // name, so it's matched on the raw remainder ahead of `split_ex`. nxvim's own
        // touch: it also pops the `:messages` panel so a multi-line value is fully
        // visible. (`:lua= {expr}` is the same shorthand with an explicit prefix,
        // handled in the `lua` arm below.)
        if let Some(expr) = rest.strip_prefix('=') {
            self.print_lua_expr(expr);
            return;
        }

        // `:[range]normal[!] {commands}` runs its argument as **literal**
        // keystrokes — `<CR>` is the four chars `<`,`C`,`R`,`>` (not Enter) and
        // whitespace is significant — so it is recognized here, ahead of
        // `split_ex`, which trims the argument and is blind to that literal shape.
        // `:execute "normal! …"` is how special keys are embedded, exactly as in
        // vim. An argument ending in whitespace has already been dispatched by the
        // pre-parse above; this arm handles the rest, where `rest` and the raw
        // remainder agree.
        if let Some((bang, body)) = parse_normal_prefix(rest) {
            self.ex_normal(range, bang, body);
            return;
        }

        let (name, bang, args) = split_ex(rest);
        match name {
            "w" | "write" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_write(&a, bang, None);
                }
            }
            "q" | "quit" => self.ex_quit(bang),
            "wq" | "x" | "xit" | "exit" => {
                // Write the current buffer, then `:q` it (close the window, or
                // quit on the last window). A failed write leaves the buffer
                // modified, so a last-window quit then reports it. `ex_write`
                // performs the quit itself (synchronously after the write, or
                // deferred to the daemon ack in off-tick save mode) so the two
                // stay coupled across both paths.
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_write(&a, bang, Some(bang));
                }
            }
            "qa" | "qall" | "quita" | "quitall" => self.ex_quit_all(bang),
            "wa" | "wall" => {
                self.ex_write_all(bang);
            }
            "wqa" | "xa" | "xall" => self.ex_write_quit_all(bang),
            "e" | "edit" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_edit(&a, bang);
                }
            }
            "ene" | "enew" => self.ex_enew(),
            "sp" | "spl" | "split" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_split(SplitDir::Horizontal, &a);
                }
            }
            "vs" | "vsp" | "vsplit" | "vspl" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_split(SplitDir::Vertical, &a);
                }
            }
            "new" => self.ex_new(SplitDir::Horizontal),
            "vne" | "vnew" => self.ex_new(SplitDir::Vertical),
            "clo" | "close" => self.close_window(),
            "hid" | "hide" => self.close_window(),
            "on" | "only" => self.only_window(),
            "ter" | "term" | "termi" | "termin" | "termina" | "terminal" => self.ex_terminal(args),
            "tabnew" | "tabe" | "tabed" | "tabedit" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_tabnew(&a);
                }
            }
            "tabc" | "tabclo" | "tabclose" => self.close_tab_cmd(args),
            "tabo" | "tabonly" => self.tab_only(),
            "tabm" | "tabmo" | "tabmove" => self.move_tab(args),
            "tabn" | "tabnext" => self.goto_tab_next(parse_opt_count_arg(args)),
            "tabp" | "tabN" | "tabprevious" | "tabNext" => {
                self.goto_tab_prev(parse_opt_count_arg(args))
            }
            "tabfir" | "tabfirst" | "tabr" | "tabrewind" => self.goto_tab_next(Some(1)),
            "tabl" | "tablast" => self.goto_tab_next(Some(self.focused_stack().tabs.len())),
            "tab" => self.ex_tab(args),
            "dr" | "dro" | "drop" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_drop(&a, false);
                }
            }
            "res" | "resize" => self.ex_resize(SplitDir::Horizontal, args),
            "vert" | "vertical" | "ver" => self.ex_vertical(args),
            // `:checkt[ime]` re-stats loaded buffers against disk and reloads
            // (autoread) or warns (W11/W12/E211) on an external change.
            "checkt" | "checkti" | "checktim" | "checktime" => self.checktime(args),
            "ls" | "buffers" | "files" => self.ex_buffers(),
            "lspanels" | "panels" => self.ex_lspanels(),
            "b" | "bu" | "buf" | "buffer" => {
                if let Some(id) = self.resolve_buffer(args) {
                    // Honor 'switchbuf' (the `:ls`-then-`:b` navigation): under the
                    // default `usetab` a buffer already shown in another tab is focused
                    // there, like the picker / a jump; an empty 'switchbuf' swaps it
                    // into the current window.
                    self.switch_to_buffer_switchbuf(id);
                }
            }
            "bn" | "bnext" => self.ex_bnext(parse_count_arg(args)),
            "bp" | "bN" | "bprev" | "bprevious" | "bNext" => self.ex_bprev(parse_count_arg(args)),
            "bf" | "bfirst" | "br" | "brewind" => self.ex_bfirst(),
            "bl" | "blast" => self.ex_blast(),
            "bd" | "bdel" | "bdelete" | "bw" | "bwipe" | "bwipeout" => {
                self.ex_bdelete(range.explicit, range_text, args, bang)
            }
            // Quickfix / location-list ingest from a buffer. `:cbuffer` replaces the
            // list, `:caddbuffer` appends; the `:cget*` variants are identical here.
            // Every `:c*` has its `:l*` twin acting on the focused window's location
            // list. The optional argument is a buffer number; default current.
            "cb" | "cbu" | "cbuf" | "cbuff" | "cbuffe" | "cbuffer" | "cgetb" | "cgetbu"
            | "cgetbuf" | "cgetbuff" | "cgetbuffe" | "cgetbuffer" => {
                self.ex_cbuffer(QfWhich::Quickfix, args, QfAction::New)
            }
            "cad" | "cadd" | "caddb" | "caddbu" | "caddbuf" | "caddbuff" | "caddbuffe"
            | "caddbuffer" => self.ex_cbuffer(QfWhich::Quickfix, args, QfAction::Add),
            "lb" | "lbu" | "lbuf" | "lbuff" | "lbuffe" | "lbuffer" | "lgetb" | "lgetbu"
            | "lgetbuf" | "lgetbuff" | "lgetbuffe" | "lgetbuffer" => {
                let which = self.loclist_which();
                self.ex_cbuffer(which, args, QfAction::New)
            }
            "lad" | "ladd" | "laddb" | "laddbu" | "laddbuf" | "laddbuff" | "laddbuffe"
            | "laddbuffer" => {
                let which = self.loclist_which();
                self.ex_cbuffer(which, args, QfAction::Add)
            }
            // `:cfile`/`:cgetfile`/`:caddfile {file}` (+ `:l*`): read a file off disk
            // and parse it against `'errorformat'`. `:cfile` opens + jumps to the
            // first error, `:cgetfile` only fills, `:caddfile` appends silently.
            "cf" | "cfi" | "cfil" | "cfile" => {
                self.ex_cfile(QfWhich::Quickfix, args, QfAction::New, true, true)
            }
            "cgetf" | "cgetfi" | "cgetfil" | "cgetfile" => {
                self.ex_cfile(QfWhich::Quickfix, args, QfAction::New, false, false)
            }
            "caddf" | "caddfi" | "caddfil" | "caddfile" => {
                self.ex_cfile(QfWhich::Quickfix, args, QfAction::Add, false, false)
            }
            "lf" | "lfi" | "lfil" | "lfile" => {
                let which = self.loclist_which();
                self.ex_cfile(which, args, QfAction::New, true, true)
            }
            "lgetf" | "lgetfi" | "lgetfil" | "lgetfile" => {
                let which = self.loclist_which();
                self.ex_cfile(which, args, QfAction::New, false, false)
            }
            "laddf" | "laddfi" | "laddfil" | "laddfile" => {
                let which = self.loclist_which();
                self.ex_cfile(which, args, QfAction::Add, false, false)
            }
            // Quickfix window + navigation (and the location-list twins).
            "cope" | "copen" => self.ex_qf_open(QfWhich::Quickfix, args),
            "ccl" | "cclo" | "cclos" | "cclose" => self.ex_qf_close(QfWhich::Quickfix),
            "cw" | "cwin" | "cwindow" => self.ex_qf_window(QfWhich::Quickfix, args),
            "cc" => self.ex_qf_cc(QfWhich::Quickfix, parse_opt_count_arg(args)),
            "cn" | "cne" | "cnex" | "cnext" => {
                self.ex_qf_step(QfWhich::Quickfix, true, parse_count_arg(args))
            }
            "cp" | "cpr" | "cprev" | "cprevious" | "cN" | "cNext" => {
                self.ex_qf_step(QfWhich::Quickfix, false, parse_count_arg(args))
            }
            "cfir" | "cfirst" | "cr" | "crewind" => self.ex_qf_first(QfWhich::Quickfix),
            "cla" | "clast" => self.ex_qf_last(QfWhich::Quickfix),
            "cnf" | "cnfi" | "cnfil" | "cnfile" => {
                self.ex_qf_step_file(QfWhich::Quickfix, true, parse_count_arg(args))
            }
            "cpf" | "cpfi" | "cpfil" | "cpfile" => {
                self.ex_qf_step_file(QfWhich::Quickfix, false, parse_count_arg(args))
            }
            "col" | "cold" | "colde" | "colder" => {
                self.ex_qf_history(QfWhich::Quickfix, false, parse_count_arg(args))
            }
            "cnew" | "cnewe" | "cnewer" => {
                self.ex_qf_history(QfWhich::Quickfix, true, parse_count_arg(args))
            }
            "lop" | "lope" | "lopen" => {
                let which = self.loclist_which();
                self.ex_qf_open(which, args);
            }
            "lcl" | "lclo" | "lclos" | "lclose" => {
                let which = self.loclist_which();
                self.ex_qf_close(which);
            }
            "lw" | "lwin" | "lwindow" => {
                let which = self.loclist_which();
                self.ex_qf_window(which, args);
            }
            "ll" => {
                let which = self.loclist_which();
                self.ex_qf_cc(which, parse_opt_count_arg(args));
            }
            "lne" | "lnex" | "lnext" => {
                let which = self.loclist_which();
                self.ex_qf_step(which, true, parse_count_arg(args));
            }
            "lp" | "lpr" | "lprev" | "lprevious" | "lN" | "lNext" => {
                let which = self.loclist_which();
                self.ex_qf_step(which, false, parse_count_arg(args));
            }
            "lfir" | "lfirst" | "lr" | "lrewind" => {
                let which = self.loclist_which();
                self.ex_qf_first(which);
            }
            "lla" | "llast" => {
                let which = self.loclist_which();
                self.ex_qf_last(which);
            }
            "lnf" | "lnfi" | "lnfil" | "lnfile" => {
                let which = self.loclist_which();
                self.ex_qf_step_file(which, true, parse_count_arg(args));
            }
            "lpf" | "lpfi" | "lpfil" | "lpfile" => {
                let which = self.loclist_which();
                self.ex_qf_step_file(which, false, parse_count_arg(args));
            }
            "lol" | "lold" | "lolde" | "lolder" => {
                let which = self.loclist_which();
                self.ex_qf_history(which, false, parse_count_arg(args));
            }
            "lnew" | "lnewe" | "lnewer" => {
                let which = self.loclist_which();
                self.ex_qf_history(which, true, parse_count_arg(args));
            }
            // `:vimgrep[!] /{pat}/[gj] {file}…` (no external process — searches with
            // the active `'regexsyntax'` engine). `:vimgrepadd` appends; `:lvimgrep`
            // populates the focused window's location list.
            "vim" | "vimg" | "vimgr" | "vimgre" | "vimgrep" | "vimgrepa" | "vimgrepad"
            | "vimgrepadd" => {
                let action = if name.starts_with("vimgrepa") {
                    QfAction::Add
                } else {
                    QfAction::New
                };
                self.ex_vimgrep(QfWhich::Quickfix, args, action);
            }
            "lvim" | "lvimg" | "lvimgr" | "lvimgre" | "lvimgrep" | "lvimgrepa" | "lvimgrepad"
            | "lvimgrepadd" => {
                let action = if name.starts_with("lvimgrepa") {
                    QfAction::Add
                } else {
                    QfAction::New
                };
                let which = self.loclist_which();
                self.ex_vimgrep(which, args, action);
            }
            // `:lua {chunk}` runs the chunk; `:lua= {expr}` is the `:=` shorthand
            // spelled out (pretty-print the expression, then open `:messages`).
            "lua" => match args.strip_prefix('=') {
                Some(expr) => self.print_lua_expr(expr),
                None => self.lua_queue.push(args.to_string()),
            },
            "sleep" | "sl" => match parse_sleep(args) {
                Ok(ms) => self.pending_sleep = Some(ms),
                Err(e) => self.echo(e),
            },
            "mes" | "messages" | "message" => self.ex_messages(),
            // `:ec[ho]` evaluates its argument as a Vim expression and shows the
            // result on the message line *without* recording it in `:messages`;
            // `:echom[sg]` records it (the history-keeping form); `:echoe[rr]`
            // shows it as an error (also recorded).
            "ec" | "ech" | "echo" => self.ex_echo(args, EchoKind::Transient),
            "echom" | "echoms" | "echomsg" => self.ex_echo(args, EchoKind::Message),
            "echoe" | "echoer" | "echoerr" => self.ex_echo(args, EchoKind::Error),
            "reg" | "registers" | "di" | "dis" | "display" => self.ex_registers(args),
            "marks" => self.ex_marks(args),
            "ju" | "jum" | "jump" | "jumps" => self.ex_jumps(args),
            "changes" => self.ex_changes(args),
            // `:setlocal`/`:setl` shares the handler: buffer-local options
            // (tabstop/shiftwidth/expandtab) live on the current buffer, which is
            // exactly what `:set` already targets for them.
            "set" | "se" | "setlocal" | "setl" => self.ex_set(args),
            // `:setf[iletype] {ft}` forces the buffer's filetype (and thus its
            // treesitter language), equivalent to `:set filetype={ft}`. The
            // no-Lua way to highlight a buffer the extension table misses.
            "setf" | "setfi" | "setfil" | "setfile" | "setfilet" | "setfilety" | "setfiletyp"
            | "setfiletype" => self.ex_setfiletype(args),
            // `:u[ndo]` undoes one change; `:undo {N}` jumps to the state with
            // sequence number N (anywhere in the tree). `:red[o]` redoes one.
            "u" | "un" | "und" | "undo" => match args.trim() {
                "" => self.undo(),
                n => match n.parse::<u64>() {
                    Ok(seq) => self.undo_to_seq(seq),
                    Err(_) => self.echo(format!("E488: Trailing characters: {n}")),
                },
            },
            "red" | "redo" => self.redo(),
            "noh" | "nohlsearch" => self.search_active = false,
            "s" | "su" | "sub" | "subs" | "subst" | "substi" | "substit" | "substitu"
            | "substitut" | "substitute" => self.ex_substitute(range, args),
            // `:[range]g[!]/pat/cmd` runs `cmd` on every line matching `pat`
            // (default range = whole file); `:g!` and `:v` invert to non-matching.
            "g" | "gl" | "glo" | "glob" | "globa" | "global" => self.ex_global(range, bang, args),
            "v" | "vg" | "vgl" | "vglo" | "vglob" | "vgloba" | "vglobal" => {
                self.ex_global(range, true, args)
            }
            "d" | "de" | "del" | "dele" | "delet" | "delete" => self.ex_delete(range),
            // `:[range]m[ove] {addr}` / `:[range]co[py] {addr}` (`:t` is `:copy`)
            // relocate / duplicate the range below the addressed line.
            "m" | "mo" | "mov" | "move" => self.ex_move(range, args),
            "t" | "co" | "cop" | "copy" => self.ex_copy(range, args),
            "p" | "pr" | "pri" | "prin" | "print" => self.ex_print(range),
            // `:[line]pu[t] [x]` inserts register `x` (default unnamed) as whole
            // lines below the addressed line — always linewise, regardless of the
            // register's own kind. `:put!` inserts above instead.
            "pu" | "put" => self.ex_put(range, args, bang),
            // `:hi clear` resets the registry to defaults (empty); other `:hi`
            // forms are no-ops — catppuccin defines groups via the API, not `:hi`.
            "hi" | "highlight" => {
                if args.trim() == "clear" {
                    self.highlights.clear();
                }
            }
            // `:help` / `:helpt[ags]` are intentionally NOT handled here: the help
            // system ships as the optional `nxvim-help` plugin. They defer to the
            // server (below), which runs the plugin's command when installed or, when
            // it isn't, points the user at the plugin instead of erroring.
            // `:wsh[ada]` / `:rsh[ada][!]` — the explicit shada flush / reload. The
            // store lives in the server (behind the `ShadaStore` seam), so these only
            // *enqueue* a request the server drains after the tick; they never no-op.
            "wsh" | "wsha" | "wshad" | "wshada" => self.ex_wshada(args),
            "rsh" | "rsha" | "rshad" | "rshada" => self.ex_rshada(bang, args),
            // Unknown to the core: defer to the server, which resolves it
            // against Lua user commands (or reports the unknown-command error).
            _ => self
                .deferred_commands
                .push(DeferredCmd::Server(rest.to_string())),
        }
    }

    /// `:={expr}` / `:lua= {expr}` — the shorthand for `:lua vim.print({expr})`.
    /// Queues the pretty-print as a Lua chunk (so `{expr}` is evaluated by the Lua
    /// runtime, not the Vim-expression evaluator) and arms the follow-up that opens
    /// the `:messages` panel once the value has been recorded. An empty expression
    /// (`:=` alone) is a no-op, matching `:lua` with no chunk.
    fn print_lua_expr(&mut self, expr: &str) {
        let expr = expr.trim();
        if expr.is_empty() {
            return;
        }
        self.lua_queue.push(format!("vim.print({expr})"));
        self.open_messages_after_lua = true;
    }

    /// `:echo` / `:echomsg` / `:echoerr` — evaluate the argument as a Vim
    /// expression and surface the result per `kind`. A `:echo` sets only the
    /// message line (not the history); `:echomsg`/`:echoerr` go through
    /// [`Editor::echo`], which records them. An evaluation error is always shown
    /// (and recorded) as the error it is.
    fn ex_echo(&mut self, args: &str, kind: EchoKind) {
        match expr::eval_echo(args) {
            Ok(text) => match kind {
                EchoKind::Transient => {
                    self.message_error = false;
                    self.message = text;
                }
                EchoKind::Message => self.echo(text),
                EchoKind::Error => self.echo_err(text),
            },
            Err(e) => self.echo(e),
        }
    }

    /// `:wshada` — flush this instance's shada store now. Core can't write the
    /// store (it lives behind the server's `ShadaStore` seam), so this enqueues a
    /// [`ShadaRequest::Write`] the server drains after the tick. A filename argument
    /// (neovim's `:wshada {file}`) is not supported — we always write *this*
    /// instance's store — and is rejected loudly rather than silently ignored.
    fn ex_wshada(&mut self, args: &str) {
        if !args.trim().is_empty() {
            self.echo(
                "E474: :wshada to a named file is not supported (writes this instance's store)",
            );
            return;
        }
        self.pending_shada.push(ShadaRequest::Write);
    }

    /// `:rshada[!]` — re-read the shada store(s) into the running session. Enqueues a
    /// [`ShadaRequest::Read`] (the server re-merges every readable store and applies
    /// it); the `!` overwrites conflicting live registers, otherwise empty slots are
    /// filled. A filename argument is rejected loudly, as for `:wshada`.
    fn ex_rshada(&mut self, bang: bool, args: &str) {
        if !args.trim().is_empty() {
            self.echo(
                "E474: :rshada from a named file is not supported (reads this instance's store)",
            );
            return;
        }
        self.pending_shada
            .push(ShadaRequest::Read { replace: bang });
    }

    /// Consume any leading range from `cmd`, resolving addresses against the
    /// cursor and buffer. Returns the resolved 0-based inclusive range
    /// (defaulting to the current line when no address is present) and the
    /// remaining command text. Fails loud on a backwards range, an unset mark,
    /// or a malformed address — never silently swaps or guesses.
    fn parse_ex_range<'a>(&self, cmd: &'a str) -> Result<(ExRange, &'a str), String> {
        let bytes = cmd.as_bytes();
        let cur = self.cursor.line;

        // `%` is the whole file (`1,$`) and stands alone.
        if bytes.first() == Some(&b'%') {
            let range = ExRange {
                lo: 0,
                hi: self.last_line(),
                explicit: true,
            };
            return Ok((range, &cmd[1..]));
        }

        let mut i = 0;
        let first = self.parse_ex_address(cmd, &mut i, cur)?;
        let has_sep = matches!(bytes.get(i), Some(b',') | Some(b';'));
        if first.is_none() && !has_sep {
            // No address at all — the range defaults to the current line.
            let range = ExRange {
                lo: cur,
                hi: cur,
                explicit: false,
            };
            return Ok((range, cmd));
        }

        let lo = first.unwrap_or(cur);
        let hi = if has_sep {
            // `;` moves the cursor to the first address before resolving the
            // second; `,` resolves the second against the original cursor.
            let base = if bytes[i] == b';' { lo } else { cur };
            i += 1;
            self.parse_ex_address(cmd, &mut i, base)?.unwrap_or(cur)
        } else {
            lo
        };

        if lo > hi {
            return Err("E493: Backwards range given".to_string());
        }
        let range = ExRange {
            lo,
            hi,
            explicit: true,
        };
        Ok((range, &cmd[i..]))
    }

    /// Parse one ex address at `*i`: an optional base (`.`, `$`, `N`, `'m`)
    /// followed by any number of `+N` / `-N` offsets (a bare `+`/`-` counts as
    /// 1). `base` is the line a relative address (one with no explicit base, or
    /// a `.`) is measured from. Advances `*i` past what it consumes and returns
    /// the resolved 0-based line clamped to the buffer, or `None` when no
    /// address token is present.
    fn parse_ex_address(
        &self,
        cmd: &str,
        i: &mut usize,
        base: usize,
    ) -> Result<Option<usize>, String> {
        let raw = self.parse_ex_address_raw(cmd, i, base)?;
        Ok(raw.map(|line| line.clamp(0, self.last_line() as i64) as usize))
    }

    /// The body of [`Self::parse_ex_address`], before the clamp to the buffer.
    /// Kept separate because a *destination* address (`:move` / `:copy`) has one
    /// extra legal value the range form doesn't: line `0`, "above the first
    /// line", which resolves to `-1` here and would otherwise clamp into line 1.
    fn parse_ex_address_raw(
        &self,
        cmd: &str,
        i: &mut usize,
        base: usize,
    ) -> Result<Option<i64>, String> {
        let bytes = cmd.as_bytes();
        let start = *i;
        let mut line = base as i64;
        let mut have_base = false;

        match bytes.get(*i) {
            Some(b'.') => {
                *i += 1;
                have_base = true;
            }
            Some(b'$') => {
                line = self.last_line() as i64;
                *i += 1;
                have_base = true;
            }
            // A `'{mark}` range address (`:'a,'bd`, the `:'<,'>` of a visual
            // selection). The next byte names the mark; resolve it to its line via
            // the same store the `` `{x} `` jump reads (buffer-local marks and the
            // automatic `'<`/`'>`/`'.`/… specials). An unset or unknown mark fails
            // loud (*E20*) rather than resolving to a bogus line.
            Some(b'\'') => {
                let name = bytes.get(*i + 1).map(|&b| b as char);
                match name.and_then(|n| self.mark_position(n)) {
                    Some(cursor) => {
                        line = cursor.line as i64;
                        *i += 2;
                        have_base = true;
                    }
                    None => return Err("E20: Mark not set".to_string()),
                }
            }
            Some(c) if c.is_ascii_digit() => {
                let mut n = 0i64;
                while let Some(d) = bytes.get(*i).filter(|b| b.is_ascii_digit()) {
                    // Saturate: an absurdly wide address must clamp to the last
                    // line below, not overflow (a debug-build panic).
                    n = n.saturating_mul(10).saturating_add(i64::from(d - b'0'));
                    *i += 1;
                }
                line = n - 1; // 1-based source line -> 0-based index
                have_base = true;
            }
            _ => {}
        }

        let mut have_offset = false;
        while let Some(&sign) = bytes.get(*i).filter(|b| matches!(b, b'+' | b'-')) {
            *i += 1;
            let mut n = 0i64;
            let mut digits = false;
            while let Some(d) = bytes.get(*i).filter(|b| b.is_ascii_digit()) {
                // Saturating, as for the base address above.
                n = n.saturating_mul(10).saturating_add(i64::from(d - b'0'));
                *i += 1;
                digits = true;
            }
            if !digits {
                n = 1;
            }
            line = if sign == b'+' {
                line.saturating_add(n)
            } else {
                line.saturating_sub(n)
            };
            have_offset = true;
        }

        if !have_base && !have_offset {
            *i = start;
            return Ok(None);
        }
        Ok(Some(line))
    }

    /// The pattern a *still-being-typed* `:` command line would match, with the
    /// line range it applies to — vim's `'incsearch'` preview extended to the
    /// pattern commands: `:[range]s/{pat}…` and `:[range]g[!]/{pat}/…` (plus
    /// `:v`). Returns `(pattern, lo, hi)` with `lo`/`hi` the resolved 0-based
    /// inclusive range (`:g`'s default is the whole file, `:s`'s the current
    /// line). An empty typed pattern (`:%s//`) falls back to the last search and
    /// a bare `:s` to the last substitute's pattern — what submitting would run.
    /// `None` when the line isn't one of those commands, has no usable pattern,
    /// or carries a malformed range.
    pub(crate) fn ex_preview_pattern(&self) -> Option<(String, usize, usize)> {
        let cmd = self.cmdline.trim_start_matches([':', ' ']);
        let (range, rest) = self.parse_ex_range(cmd).ok()?;
        // Split the name by hand rather than through `split_ex`: that trims the
        // argument, and a trailing space is part of the pattern being typed
        // (`:%s/foo ` must preview "foo ", not "foo").
        let rest = rest.trim_start();
        let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        let name = &rest[..name_len];
        let after = &rest[name_len..];
        let args = after.strip_prefix('!').unwrap_or(after).trim_start();

        let subst = matches!(
            name,
            "s" | "su"
                | "sub"
                | "subs"
                | "subst"
                | "substi"
                | "substit"
                | "substitu"
                | "substitut"
                | "substitute"
        );
        let global = matches!(
            name,
            "g" | "gl"
                | "glo"
                | "glob"
                | "globa"
                | "global"
                | "v"
                | "vg"
                | "vgl"
                | "vglo"
                | "vglob"
                | "vgloba"
                | "vglobal"
        );
        if !subst && !global {
            return None;
        }

        let pat = match args.chars().next() {
            // The delimiter is whatever non-alphanumeric char follows the name,
            // exactly as `:s` / `:g` read it (`\` and `"` are never delimiters).
            Some(d) if !d.is_alphanumeric() && d != '\\' && d != '"' => {
                let body = &args[d.len_utf8()..];
                if subst {
                    // Once the replacement half has been opened (`:s/pat/…`), the
                    // richer diff preview (`refresh_subst_preview`) owns the match —
                    // showing the removed text struck through and the replacement
                    // inline — so the plain yellow pattern highlight yields to it.
                    if has_unescaped_delim(body, d) {
                        return None;
                    }
                    split_substitute(body, d).0
                } else {
                    split_global(body, d).0
                }
            }
            // A bare `:s` (or `:s{flags}`) repeats the last substitute's pattern.
            _ if subst => self.last_substitute.as_ref()?.0.clone(),
            _ => return None,
        };
        let pattern = if pat.is_empty() {
            self.last_search.as_ref()?.0.clone()
        } else {
            pat
        };
        if pattern.is_empty() {
            return None;
        }

        let (lo, hi) = if range.explicit {
            (range.lo, range.hi)
        } else if global {
            (0, self.last_line())
        } else {
            (self.cursor.line, self.cursor.line)
        };
        Some((pattern, lo, hi))
    }

    /// Whether the command line is a substitute whose **replacement half is open**
    /// (`:s/pat/…`), i.e. the diff overlay ([`refresh_subst_preview`]) owns the
    /// matches. A structural check only — unlike [`subst_preview`] it never compiles
    /// the pattern, so it still holds for a mid-edit (not-yet-valid) pattern. The
    /// hlsearch projection consults it to stay silent while the diff overlay is live,
    /// rather than leaking a prior `/search`'s stale highlight underneath it.
    pub(crate) fn subst_preview_active(&self) -> bool {
        if self.mode != Mode::Command || self.cmdline_kind != CmdlineKind::Ex {
            return false;
        }
        let cmd = self.cmdline.trim_start_matches([':', ' ']);
        let Ok((_range, rest)) = self.parse_ex_range(cmd) else {
            return false;
        };
        let rest = rest.trim_start();
        let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        if !matches!(
            &rest[..name_len],
            "s" | "su"
                | "sub"
                | "subs"
                | "subst"
                | "substi"
                | "substit"
                | "substitu"
                | "substitut"
                | "substitute"
        ) {
            return false;
        }
        let after = &rest[name_len..];
        let args = after.strip_prefix('!').unwrap_or(after).trim_start();
        // An opening delimiter whose replacement half has actually opened (a second
        // unescaped delimiter typed) — the same gate `subst_preview` uses.
        match args.chars().next() {
            Some(delim) if !delim.is_alphanumeric() && delim != '\\' && delim != '"' => {
                has_unescaped_delim(&args[delim.len_utf8()..], delim)
            }
            _ => false,
        }
    }

    /// The resolved live `:s/pat/rep/flags` **replacement preview**, once a
    /// substitute command line has opened its replacement half (`:s/pat/…`).
    /// Returns the compiled pattern, the (tilde-expanded) replacement, whether the
    /// `g` flag is set, and the resolved 0-based inclusive line range — everything
    /// [`refresh_subst_preview`](Self::refresh_subst_preview) needs to lay the diff
    /// overlay. `None` when the line isn't such a substitute, when the pattern can't
    /// be resolved / compiled (still mid-edit), or when `'incsearch'` is off.
    fn subst_preview(&self) -> Option<(SearchRegex, String, bool, usize, usize)> {
        // Only a live, focused ex command line previews; the mode/kind guard also
        // makes the teardown callers a clean no-op once the line has closed.
        if !self.options.incsearch
            || self.mode != Mode::Command
            || self.cmdline_kind != CmdlineKind::Ex
        {
            return None;
        }
        let cmd = self.cmdline.trim_start_matches([':', ' ']);
        let (range, rest) = self.parse_ex_range(cmd).ok()?;
        let rest = rest.trim_start();
        let name_len = rest.bytes().take_while(u8::is_ascii_alphabetic).count();
        if !matches!(
            &rest[..name_len],
            "s" | "su"
                | "sub"
                | "subs"
                | "subst"
                | "substi"
                | "substit"
                | "substitu"
                | "substitut"
                | "substitute"
        ) {
            return None;
        }
        let after = &rest[name_len..];
        let args = after.strip_prefix('!').unwrap_or(after).trim_start();
        // An opening delimiter, and its replacement half actually opened.
        let delim = args
            .chars()
            .next()
            .filter(|&d| !d.is_alphanumeric() && d != '\\' && d != '"')?;
        let body = &args[delim.len_utf8()..];
        if !has_unescaped_delim(body, delim) {
            return None;
        }
        let (pat, rep, flags) = split_substitute(body, delim);
        // Empty pattern reuses the last search pattern — what submitting would run.
        let pattern = if pat.is_empty() {
            self.last_search.as_ref()?.0.clone()
        } else {
            pat
        };
        if pattern.is_empty() {
            return None;
        }
        // `~` recalls the previous replacement; a bare `~` with no history drops the
        // added side (deleted-only is still a useful preview) rather than erroring.
        let prev_rep = self.last_substitute.as_ref().map(|(_, r, _)| r.clone());
        let rep = expand_tilde(&rep, prev_rep.as_deref()).unwrap_or_default();
        // Only `g` (every match on a line) and `i`/`I` (force case) change what the
        // preview shows; a trailing space / count / other flag ends the flag run.
        let mut global = false;
        let mut icase: Option<bool> = None;
        for b in flags.bytes() {
            match b {
                b'g' => global = true,
                b'i' => icase = Some(true),
                b'I' => icase = Some(false),
                b' ' | b'\t' | b'0'..=b'9' => break,
                _ => {}
            }
        }
        let ignorecase = icase.unwrap_or_else(|| self.search_ignorecase(&pattern));
        let re = SearchRegex::compile(&pattern, ignorecase, self.search_engine()).ok()?;
        let (lo, hi) = if range.explicit {
            (range.lo, range.hi)
        } else {
            (self.cursor.line, self.cursor.line)
        };
        Some((re, rep, global, lo, hi))
    }

    /// Recompute the live `:s` replacement diff overlay ([`SUBST_PREVIEW_NS`]) from
    /// the command line: clear the previous marks, then — while a substitute command
    /// line has its replacement half open — lay a `NxSubstituteDelete` range over
    /// every match with an inline `NxSubstituteAdd` `virt_text` holding the
    /// replacement spliced in right after it. Called wherever the command line
    /// changes, and (as a pure teardown) when it closes. A no-op teardown when the
    /// line isn't such a substitute.
    pub(crate) fn refresh_subst_preview(&mut self) {
        self.clear_subst_preview();
        let Some((re, rep, global, lo, hi)) = self.subst_preview() else {
            return;
        };
        // Only visible rows are ever painted, so a mark cap keeps a `:%s` over a
        // giant file cheap while still covering any realistic viewport.
        const MAX_MARKS: usize = 1000;
        let hi = hi.min(self.buffer().line_count().saturating_sub(1));
        let mut marks = 0usize;
        'lines: for l in lo..=hi {
            let line = self.buffer().line(l);
            let line_start = self.buffer().line_start(l);
            let mut from = 0;
            while let Some((s, e, repl)) = re.match_replacement(&line, from, &rep) {
                self.set_subst_preview_marks(line_start + s, line_start + e, &repl);
                marks += 1 + usize::from(!repl.is_empty() && !repl.contains('\n'));
                if marks >= MAX_MARKS {
                    break 'lines;
                }
                if !global {
                    break;
                }
                // Step past this match; a zero-width match advances one char so the
                // scan can't spin in place.
                from = if e > s {
                    e
                } else {
                    match line[e..].chars().next() {
                        Some(c) => e + c.len_utf8(),
                        None => break,
                    }
                };
                if from > line.len() {
                    break;
                }
            }
        }
    }

    /// Lay one match's diff overlay into [`SUBST_PREVIEW_NS`]: a
    /// `NxSubstituteDelete` range over the matched bytes `[start, end)` (the
    /// removed side, struck through) and — when `repl` is non-empty and single
    /// line — an inline `NxSubstituteAdd` `virt_text` at `end` holding the
    /// replacement (the added side). A `\r`-splitting replacement can't be shown
    /// inline (it would split the row), so only its removed side is drawn. Shared
    /// by the typed-line preview ([`refresh_subst_preview`](Self::refresh_subst_preview))
    /// and the `:s///c` confirm walk's current match.
    fn set_subst_preview_marks(&mut self, start: usize, end: usize, repl: &str) {
        let bid = self.cur_buffer();
        let buf = &mut self.buffers.get_mut(bid).buffer;
        buf.extmarks.set(
            SUBST_PREVIEW_NS,
            None,
            start,
            Some(end),
            Some("NxSubstituteDelete".to_string()),
            DEFAULT_PRIORITY,
            None,
        );
        if !repl.is_empty() && !repl.contains('\n') {
            let decor = VirtDecor {
                virt_text: vec![VirtChunk {
                    text: repl.to_string(),
                    hl_group: Some("NxSubstituteAdd".to_string()),
                }],
                virt_text_pos: VirtTextPos::Inline,
                ..VirtDecor::default()
            };
            buf.extmarks.set(
                SUBST_PREVIEW_NS,
                None,
                end,
                None,
                None,
                DEFAULT_PRIORITY,
                Some(Box::new(decor)),
            );
        }
    }

    /// Clear the `:s` diff overlay ([`SUBST_PREVIEW_NS`]) from the current buffer.
    fn clear_subst_preview(&mut self) {
        let bid = self.cur_buffer();
        self.buffers
            .get_mut(bid)
            .buffer
            .extmarks
            .clear(SUBST_PREVIEW_NS, None);
    }

    /// The current `:s///c` match as `(line, byte_start)` while a confirm prompt is
    /// showing — for the renderer to drop the plain match highlight on it so the
    /// diff overlay (`set_subst_preview_marks`) reads cleanly. `None` when no
    /// confirm walk is paused on a match.
    pub(crate) fn subst_confirm_current(&self) -> Option<(usize, usize)> {
        let sc = self.subst_confirm.as_ref()?;
        sc.cur.as_ref().map(|(s, _, _)| (sc.line, *s))
    }

    /// `:[range]s/{pat}/{rep}/[flags] [count]` — substitute matches of `pat`
    /// with `rep` over the range (canonical regex, the `/`-search dialect).
    /// Flags: `g` (every match on a line), `i`/`I` (force ignore/match case),
    /// `n` (count only, no edit). Fails loud on a bad delimiter, an unknown
    /// flag, the not-yet-built `c` flag, an invalid pattern, or no match.
    fn ex_substitute(&mut self, range: ExRange, args: &str) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let spec = args.trim();
        match spec.chars().next() {
            // No delimiter: a bare `:s` (or `:s {flags} [count]`) repeats the last
            // substitute with its flags reset — only freshly given flags apply.
            // An alphanumeric first char is a flag/count, not a delimiter.
            None => self.repeat_substitute(range, "", false),
            Some(c) if c.is_alphanumeric() => self.repeat_substitute(range, spec, false),
            Some('\\') | Some('"') => {
                self.echo("E146: Regular expressions can't be delimited by letters");
            }
            Some(delim) => {
                let (pat, rep, flags) = split_substitute(&spec[delim.len_utf8()..], delim);
                self.run_substitute(range, pat, rep, &flags);
            }
        }
    }

    /// Repeat the last `:substitute` — bare `:s`, `:&` (`keep_flags` false), or
    /// `:&&` (`keep_flags` true). `extra` carries any freshly typed flags/count;
    /// `&&` keeps the previous flags (prepended), the others reset them.
    fn repeat_substitute(&mut self, range: ExRange, extra: &str, keep_flags: bool) {
        let Some((pat, rep, prev_flags)) = self.last_substitute.clone() else {
            self.echo("E33: No previous substitute regular expression");
            return;
        };
        let flag_spec = if keep_flags {
            format!("{prev_flags}{extra}")
        } else {
            extra.to_string()
        };
        self.run_substitute(range, pat, rep, &flag_spec);
    }

    /// The substitute engine shared by `:s/pat/rep/flags` and the repeat forms.
    /// `pat` empty reuses the last search/substitute pattern; a `~` in `rep`
    /// expands to the previous replacement. Records the (resolved) substitute for
    /// later repeats — even on a no-match, matching vim.
    fn run_substitute(&mut self, range: ExRange, pat: String, rep: String, flag_spec: &str) {
        // Parse flag letters, then an optional trailing count.
        let mut global = false;
        let mut nflag = false;
        let mut confirm = false;
        let mut icase: Option<bool> = None;
        let tail = flag_spec.trim();
        let tb = tail.as_bytes();
        let mut k = 0;
        while k < tb.len() {
            match tb[k] {
                b'g' => global = true,
                b'i' => icase = Some(true),
                b'I' => icase = Some(false),
                b'n' => nflag = true,
                b'c' => confirm = true,
                b' ' | b'\t' | b'0'..=b'9' => break,
                _ => {
                    self.echo(format!("E488: Trailing characters: {}", &tail[k..]));
                    return;
                }
            }
            k += 1;
        }
        let flag_letters = tail[..k].to_string();
        let rest = tail[k..].trim();
        let count = if rest.is_empty() {
            None
        } else {
            match rest.parse::<usize>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    self.echo(format!("E488: Trailing characters: {rest}"));
                    return;
                }
            }
        };

        // An empty pattern reuses the last search/substitute pattern.
        let pattern = if pat.is_empty() {
            match self.last_search.as_ref() {
                Some((p, _, _)) => p.clone(),
                None => {
                    self.echo("E35: No previous regular expression");
                    return;
                }
            }
        } else {
            pat
        };
        // A `~` in the replacement recalls the previous replacement — resolved
        // before this substitute overwrites the remembered state.
        let prev_rep = self.last_substitute.as_ref().map(|(_, r, _)| r.clone());
        let rep = match expand_tilde(&rep, prev_rep.as_deref()) {
            Ok(r) => r,
            Err(()) => {
                self.echo("E33: No previous substitute regular expression");
                return;
            }
        };
        let ignorecase = icase.unwrap_or_else(|| self.search_ignorecase(&pattern));
        let re = match SearchRegex::compile(&pattern, ignorecase, self.search_engine()) {
            Ok(re) => re,
            Err(e) => {
                self.echo(e);
                return;
            }
        };
        // Remember this substitute (resolved pattern, expanded replacement, flag
        // letters) for bare `:s` / `:&` / `:&&` and `~`. vim repeats even a
        // substitute that matched nothing, so record before the match pass.
        self.last_substitute = Some((pattern.clone(), rep.clone(), flag_letters));

        // A trailing count restricts the edit to `count` lines from the range's
        // last line (vim semantics), overriding the range's start.
        let (lo, hi) = match count {
            // `c >= 1` is guaranteed by the parse; saturate so a huge count means
            // "through the last line" instead of overflowing before the clamp.
            Some(c) => (
                range.hi,
                range.hi.saturating_add(c - 1).min(self.last_line()),
            ),
            None => (range.lo, range.hi),
        };

        // `n` reports the match count and edits nothing.
        if nflag {
            let (mut matches, mut nlines) = (0usize, 0usize);
            for l in lo..=hi {
                let m = re.find_all(&self.buffer().line(l)).len();
                if m > 0 {
                    matches += m;
                    nlines += 1;
                }
            }
            if matches == 0 {
                self.echo(format!("E486: Pattern not found: {pattern}"));
            } else {
                self.echo(fmt_subst_report(matches, "match", "matches", nlines));
                self.set_substitute_search(pattern);
            }
            return;
        }

        // `c` runs the same range interactively, prompting before each match.
        // `n` takes precedence above (vim: a counting pass never prompts).
        if confirm {
            self.begin_subst_confirm(re, rep, global, pattern, lo, hi);
            return;
        }

        // Edit pass. A `\r` in the replacement splits a line into several, so a
        // running `added` offset keeps later original lines pointing at the
        // right (shifted) index.
        let (mut subs, mut nlines) = (0usize, 0usize);
        let mut added = 0i64;
        let mut last_changed: Option<usize> = None;
        let mut pushed = false;
        for orig in lo..=hi {
            let idx = (orig as i64 + added) as usize;
            let old = self.buffer().line(idx);
            let (new_text, n) = re.substitute_line(&old, &rep, global);
            if n == 0 {
                continue;
            }
            if !pushed {
                self.push_undo();
                pushed = true;
            }
            let start = self.buffer().line_start(idx);
            self.buffer_mut().remove(start..start + old.len());
            self.buffer_mut().insert(start, &new_text);
            let extra = new_text.matches('\n').count();
            added += extra as i64;
            subs += n;
            nlines += 1;
            last_changed = Some(idx + extra);
        }

        let Some(last) = last_changed else {
            self.echo(format!("E486: Pattern not found: {pattern}"));
            return;
        };
        self.buffer_mut().normalize();
        self.cursor.line = last;
        self.cursor.col = self.first_non_blank(last);
        self.clamp_cursor();
        self.set_substitute_search(pattern);

        // vim stays silent for a single substitution on a single line.
        if subs != 1 || nlines != 1 {
            self.echo(fmt_subst_report(
                subs,
                "substitution",
                "substitutions",
                nlines,
            ));
        }
    }

    /// Record `pattern` as the last-used search pattern (so `n` and `hlsearch`
    /// pick it up after a `:s`, matching vim) and light up the highlight.
    fn set_substitute_search(&mut self, pattern: String) {
        self.last_search = Some((pattern, SearchDir::Forward, SearchOffset::None));
        self.search_active = true;
    }

    /// `:[range]g[!]/{pat}/{cmd}` — run `cmd` (an ex command) on every line of the
    /// range matching `pat`; `invert` (`:g!` / `:v`) targets the *non*-matching
    /// lines. The range defaults to the whole file. The whole `:g` is one undo
    /// step, and a nested `:g`/`:v` in `cmd` fails loud (`E147`).
    fn ex_global(&mut self, range: ExRange, invert: bool, args: &str) {
        if self.in_global {
            self.echo("E147: Cannot do :global recursive");
            return;
        }
        let spec = args.trim();
        let delim = match spec.chars().next() {
            Some(d) if !d.is_alphanumeric() && d != '\\' && d != '"' => d,
            Some(_) => {
                self.echo("E146: Regular expressions can't be delimited by letters");
                return;
            }
            None => {
                self.echo("E471: Argument required");
                return;
            }
        };
        let (pat, cmd) = split_global(&spec[delim.len_utf8()..], delim);

        // An empty pattern reuses the last search/substitute pattern (like `:s`).
        let pattern = if pat.is_empty() {
            match self.last_search.as_ref() {
                Some((p, _, _)) => p.clone(),
                None => {
                    self.echo("E35: No previous regular expression");
                    return;
                }
            }
        } else {
            pat
        };
        let re = match SearchRegex::compile(
            &pattern,
            self.search_ignorecase(&pattern),
            self.search_engine(),
        ) {
            Ok(re) => re,
            Err(e) => {
                self.echo(e);
                return;
            }
        };
        let (lo, hi) = if range.explicit {
            (range.lo, range.hi)
        } else {
            (0, self.last_line())
        };

        // First pass: mark the target lines. Doing this up front means edits in
        // the command pass can't disturb which lines were selected (vim's
        // mark-then-execute). `:g` keeps the matching lines, `:g!`/`:v` the rest.
        let targets: Vec<usize> = (lo..=hi)
            .filter(|&l| re.find_from(&self.buffer().line(l), 0).is_some() != invert)
            .collect();
        self.set_substitute_search(pattern.clone());
        if targets.is_empty() {
            // `:g` with no match is a not-found, reported like `:s` (fail-loud).
            // `:v` finding "no non-matching lines" isn't a miss, so it's silent.
            if !invert {
                self.echo(format!("E486: Pattern not found: {pattern}"));
            }
            return;
        }
        let cmd = if cmd.trim().is_empty() {
            "p".to_string() // vim's default command is `:print`
        } else {
            cmd
        };

        // One undo step for the whole `:g`: force a fresh snapshot, then suppress
        // the per-command snapshots while the batch runs; leave `snapshot_taken`
        // cleared so the next edit snapshots on its own.
        self.snapshot_taken = false;
        self.push_undo();
        self.snapshot_taken = true;
        self.in_global = true;
        // Second pass: run `cmd` on each target. A running `offset` keeps the
        // remaining (later) targets aligned as the command adds or removes lines.
        let mut offset: i64 = 0;
        for t in targets {
            let line = t as i64 + offset;
            if line < 0 || line > self.last_line() as i64 {
                continue;
            }
            let before = self.buffer().line_count() as i64;
            self.cursor.line = line as usize;
            self.cursor.col = 0;
            self.execute_ex(&cmd);
            offset += self.buffer().line_count() as i64 - before;
        }
        self.in_global = false;
        self.snapshot_taken = false;
        self.buffer_mut().normalize();
        self.clamp_cursor();
    }

    /// `:[range]normal[!] {keys}` — execute `keys` as if typed in Normal mode.
    /// `keys` is fed straight through [`Editor::input`] (the same recursive replay
    /// path `.`/dot-repeat uses), so the whole built-in command grammar — counts,
    /// operators, registers, insert mode — re-parses and runs synchronously.
    ///
    /// With a range, the keys run once per line, the cursor parked at column 0 of
    /// each, as a single undo step — mirroring [`Editor::ex_global`]'s command pass
    /// (a running `offset` keeps the remaining lines aligned across edits).
    ///
    /// NOTE on the bang: user keymaps live one layer up (the server's matcher), so
    /// neither `:normal` nor `:normal!` expands them here — both behave as vim's
    /// `:normal!` (built-in commands only). The flag is accepted for compatibility.
    fn ex_normal(&mut self, range: ExRange, _bang: bool, arg: &str) {
        let keys = normal_keys(arg);
        if keys.is_empty() {
            // vim's `:normal` requires an argument; an empty one is a no-op here.
            return;
        }
        // Bound nesting so a `:normal` whose keys run another `:normal` (only
        // reachable with an embedded control byte via `:execute`) can't overflow
        // the stack.
        const MAX_DEPTH: usize = 200;
        if self.normal_depth >= MAX_DEPTH {
            self.echo("E192: Recursive use of :normal too deep".to_string());
            return;
        }
        self.normal_depth += 1;

        if range.explicit {
            // One undo step for the whole range run (the `:global` grouping): force
            // a fresh snapshot, then suppress the per-command snapshots the fed
            // keys would otherwise take.
            self.snapshot_taken = false;
            self.push_undo();
            self.snapshot_taken = true;
            let mut offset: i64 = 0;
            for t in range.lo..=range.hi {
                let line = t as i64 + offset;
                if line < 0 || line > self.last_line() as i64 {
                    continue;
                }
                let before = self.buffer().line_count() as i64;
                self.cursor.line = line as usize;
                self.cursor.col = 0;
                for &key in &keys {
                    self.input(key);
                }
                offset += self.buffer().line_count() as i64 - before;
            }
            self.snapshot_taken = false;
            self.buffer_mut().normalize();
            self.clamp_cursor();
        } else {
            for &key in &keys {
                self.input(key);
            }
        }

        self.normal_depth -= 1;
    }

    /// `:[range]d[elete]` — delete the range's lines (default: the current line),
    /// landing the cursor on the line that takes their place (first non-blank).
    fn ex_delete(&mut self, range: ExRange) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        self.push_undo();
        let start = self.buffer().line_start(range.lo);
        // Up to the start of the line after the range — or the buffer end when the
        // range runs to the last line, so the deleted block's newlines go too.
        let end = if range.hi < self.last_line() {
            self.buffer().line_start(range.hi + 1)
        } else {
            self.buffer().len_bytes()
        };
        self.buffer_mut().remove(start..end);
        self.buffer_mut().normalize();
        self.cursor.line = range.lo.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
    }

    /// Parse the destination address of `:move` / `:copy` — the whole argument,
    /// which must be exactly one address (`3`, `$`, `.`, `'>+1`, `-2`, …) and
    /// nothing else. Returned 0-based and *signed*: unlike a range address, vim
    /// lets a destination be line `0`, "above the first line", which lands here
    /// as `-1`. Fails loud on a missing (*E14*) or trailing-junk (*E488*)
    /// argument rather than guessing a destination.
    fn parse_ex_dest(&self, args: &str) -> Result<i64, String> {
        let arg = args.trim();
        if arg.is_empty() {
            return Err("E14: Invalid address".to_string());
        }
        let mut i = 0;
        let dest = self.parse_ex_address_raw(arg, &mut i, self.cursor.line)?;
        if arg[i..].trim().is_empty() {
            // `0` is the only address below the first line; anything further
            // down (`:m -5` from line 1) just pins there, as vim does.
            dest.map(|line| line.clamp(-1, self.last_line() as i64))
                .ok_or_else(|| "E14: Invalid address".to_string())
        } else {
            Err("E488: Trailing characters".to_string())
        }
    }

    /// The range's lines as one linewise chunk (always newline-terminated, since
    /// the rope keeps a trailing `\n`), plus the byte span they occupy.
    fn linewise_span(&self, range: ExRange) -> (String, std::ops::Range<usize>) {
        let start = self.buffer().line_start(range.lo);
        // Up to the start of the line after the range — or the buffer end when
        // the range runs to the last line, so the block's final newline goes too.
        let end = if range.hi < self.last_line() {
            self.buffer().line_start(range.hi + 1)
        } else {
            self.buffer().len_bytes()
        };
        (self.buffer().text.slice(start..end).to_string(), start..end)
    }

    /// Splice `chunk` (a whole number of lines) in below the 0-based line `dest`,
    /// where `dest == -1` means "above the first line", and land the cursor on
    /// the last spliced line at its first non-blank — the shared tail of
    /// [`Self::ex_move`] and [`Self::ex_copy`]. Returns the line the chunk's
    /// first line landed on.
    fn splice_lines_below(&mut self, dest: i64, chunk: &str) -> usize {
        let at_line = (dest + 1).max(0) as usize;
        let at = self
            .buffer()
            .line_start(at_line.min(self.buffer().line_count()));
        self.buffer_mut().insert(at, chunk);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        self.cursor.line = (at_line + chunk.matches('\n').count()).saturating_sub(1);
        self.cursor.line = self.cursor.line.min(self.last_line());
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
        at_line
    }

    /// `:[range]m[ove] {addr}` — move the range's lines (default: the current
    /// line) to just below `addr`; `:m0` lifts them to the top of the buffer.
    /// The cursor lands on the last moved line. Moving a range into itself is
    /// *E134*, as in vim.
    fn ex_move(&mut self, range: ExRange, args: &str) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let dest = match self.parse_ex_dest(args) {
            Ok(dest) => dest,
            Err(e) => return self.echo_err(e),
        };
        // Landing anywhere from just-above the range through its last line would
        // be a no-op at best and a self-overlapping splice at worst.
        if dest >= range.lo as i64 - 1 && dest <= range.hi as i64 {
            return self.echo_err("E134: Cannot move a range of lines into itself");
        }
        self.push_undo();
        let (chunk, span) = self.linewise_span(range);
        // Marks *inside* the moved block have to ride it to its new home (vim's
        // `mark_adjust`), or the lift below would drop them as deleted lines. This
        // is what makes the `:m '>+1<CR>gv=gv` idiom repeat: `` '< ``/`` '> ``
        // follow the block, so `gv` reselects the lines that just moved. The `.`
        // last-change mark is excluded — the splice itself is the last change.
        let carried: Vec<(char, usize, usize)> = self
            .buffer()
            .marks
            .iter()
            .filter(|(&name, &(line, _))| name != '.' && (range.lo..=range.hi).contains(&line))
            .map(|(&name, &(line, col))| (name, line - range.lo, col))
            .collect();
        for &(name, _, _) in &carried {
            self.buffer_mut().marks.remove(&name);
        }
        self.buffer_mut().remove(span);
        self.buffer_mut().normalize();
        // Lines below the lifted block shifted up by its height, so a destination
        // past the block has to shift with them.
        let lifted = (range.hi - range.lo + 1) as i64;
        let dest = if dest > range.hi as i64 {
            dest - lifted
        } else {
            dest
        };
        let landed = self.splice_lines_below(dest, &chunk);
        for (name, offset, col) in carried {
            self.buffer_mut().marks.insert(name, (landed + offset, col));
        }
    }

    /// `:[range]co[py] {addr}` (also `:t`) — copy the range's lines (default: the
    /// current line) to just below `addr`, leaving the originals in place. The
    /// cursor lands on the last copy. Unlike `:move`, copying a range into
    /// itself is legal.
    fn ex_copy(&mut self, range: ExRange, args: &str) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let dest = match self.parse_ex_dest(args) {
            Ok(dest) => dest,
            Err(e) => return self.echo_err(e),
        };
        self.push_undo();
        let (chunk, _) = self.linewise_span(range);
        self.splice_lines_below(dest, &chunk);
    }

    /// `:[line]pu[t] [x]` — insert register `x` (default the unnamed register) as
    /// whole lines below the addressed line (default the current line), or above
    /// it with `:put!`. Always linewise: a charwise register's text is inserted as
    /// a line regardless of its own kind, matching vim's `:put`. An empty register
    /// reports `E353` rather than silently doing nothing.
    fn ex_put(&mut self, range: ExRange, args: &str, above: bool) {
        if !self.modifiable() {
            self.refuse_edit();
            return;
        }
        let arg = args.trim();
        let reg = arg.chars().next();
        let text = match self.register_text(reg) {
            Some((text, _)) if !text.is_empty() => text,
            _ => {
                self.echo(format!("E353: Nothing in register {}", reg.unwrap_or('"')));
                return;
            }
        };
        self.push_undo();
        // Force linewise: ensure the chunk is a whole number of lines so the
        // insert can't splice into an existing line.
        let mut chunk = text;
        if !chunk.ends_with('\n') {
            chunk.push('\n');
        }
        let line = range.hi.min(self.last_line());
        let at = if above {
            self.buffer().line_start(line)
        } else {
            self.buffer()
                .line_start((line + 1).min(self.buffer().line_count()))
        };
        self.buffer_mut().insert(at, &chunk);
        self.buffer_mut().normalize();
        self.buffer_mut().modified = true;
        // Cursor lands on the last inserted line, at its first non-blank (vim).
        let inserted = chunk.matches('\n').count();
        self.cursor.line = if above {
            line + inserted - 1
        } else {
            line + inserted
        };
        self.cursor.col = self.first_non_blank(self.cursor.line);
        self.clamp_cursor();
    }

    /// `:[range]p[rint]` — echo the range's lines (default: the current line). The
    /// message line shows the last; each is recorded in `:messages`. Also the
    /// default command for a bare `:g/pat/`.
    fn ex_print(&mut self, range: ExRange) {
        for l in range.lo..=range.hi.min(self.last_line()) {
            let line = self.buffer().line(l);
            self.echo(line);
        }
    }

    /// Open a `:s///c` confirm substitute over `[lo, hi]`: light up the pattern
    /// highlight (matches show while prompting), seed the walk, and prompt on the
    /// first match — or finish straight away if the range holds none. The
    /// substitute is *not* recorded for repeats here; the caller in
    /// [`Self::run_substitute`] already did that before this point.
    fn begin_subst_confirm(
        &mut self,
        re: SearchRegex,
        rep: String,
        global: bool,
        pattern: String,
        lo: usize,
        hi: usize,
    ) {
        self.set_substitute_search(pattern);
        self.subst_confirm = Some(SubstConfirm {
            re,
            rep,
            global,
            hi,
            line: lo,
            byte: 0,
            cur: None,
            line_dirty: false,
            subs: 0,
            nlines: 0,
            last_changed: None,
            pushed: false,
        });
        self.subst_confirm_seek();
    }

    /// Find the next match from the walk's current position and prompt for it,
    /// putting the cursor on the match and the `replace with …?` question on the
    /// message line. Skips lines with no match; finishes when the range is spent.
    fn subst_confirm_seek(&mut self) {
        loop {
            let (line, byte, hi) = {
                let sc = self.subst_confirm.as_ref().expect("confirm active");
                (sc.line, sc.byte, sc.hi)
            };
            if line > hi {
                self.subst_confirm_finish();
                return;
            }
            let text = self.buffer().line(line);
            let found = {
                let sc = self.subst_confirm.as_ref().expect("confirm active");
                sc.re.match_replacement(&text, byte, &sc.rep)
            };
            match found {
                Some((s, e, repl)) => {
                    self.subst_confirm.as_mut().expect("confirm active").cur =
                        Some((s, e, repl.clone()));
                    self.cursor.line = line;
                    self.cursor.col = s;
                    self.clamp_cursor();
                    self.ensure_visible();
                    // Show the diff on the match being prompted, exactly like the
                    // typed-line preview: strike the matched text and splice the
                    // replacement in after it. Repopulated each seek, so it always
                    // tracks the current match; the plain match highlight is dropped
                    // from this one span (see `subst_confirm_current`).
                    let line_start = self.buffer().line_start(line);
                    self.clear_subst_preview();
                    self.set_subst_preview_marks(line_start + s, line_start + e, &repl);
                    // Set the prompt directly, not via `echo`: it's a transient
                    // question, kept off the `:messages` history (like vim).
                    self.message_error = false;
                    self.message = format!("replace with {repl} (y/n/a/l/q/^E/^Y)?");
                    return;
                }
                None => {
                    // No (more) matches on this line — drop to the next.
                    let sc = self.subst_confirm.as_mut().expect("confirm active");
                    sc.line += 1;
                    sc.byte = 0;
                    sc.line_dirty = false;
                }
            }
        }
    }

    /// Answer the open confirm prompt. `y` substitutes and advances, `n` skips,
    /// `a` substitutes this match and every remaining one without prompting, `l`
    /// substitutes this one then stops, `q`/`<Esc>` stop without it. Any other
    /// key leaves the prompt up (vim beeps).
    pub(crate) fn subst_confirm_key(&mut self, key: Key) {
        let Some((s, e, repl)) = self.subst_confirm.as_ref().and_then(|sc| sc.cur.clone()) else {
            // No pending match (defensive) — close the walk out.
            self.subst_confirm_finish();
            return;
        };
        // `^E`/`^Y` scroll the window one line to peek around the match, leaving
        // the prompt up and the match unconsumed (vim). The pending match lives in
        // `cur`, not the cursor, so a peek that rides the cursor along is harmless —
        // and required here, because nxvim re-runs `ensure_visible` every redraw
        // and would otherwise snap the view straight back to the on-match cursor.
        // So use the full cursor-pulling [`scroll_line`], and clear `scroll_from`
        // so the early return doesn't leak a stale scroll gesture.
        if key.ctrl && matches!(key.code, KeyCode::Char('e') | KeyCode::Char('y')) {
            self.scroll_line(key.code == KeyCode::Char('e'));
            self.scroll_from = None;
            return;
        }
        let answer = match key.code {
            KeyCode::Esc => 'q',
            KeyCode::Char(c) if !key.ctrl => c,
            _ => return, // ignore; keep the prompt up
        };
        match answer {
            'y' => {
                self.subst_confirm_apply(s, e, &repl);
                self.subst_confirm_seek();
            }
            'n' => {
                self.subst_confirm_skip(e);
                self.subst_confirm_seek();
            }
            'l' => {
                self.subst_confirm_apply(s, e, &repl);
                self.subst_confirm_finish();
            }
            'a' => {
                // This match and all the rest, no further prompts.
                self.subst_confirm_apply(s, e, &repl);
                while let Some((s, e, repl)) = {
                    self.subst_confirm_seek();
                    self.subst_confirm.as_ref().and_then(|sc| sc.cur.clone())
                } {
                    self.subst_confirm_apply(s, e, &repl);
                }
            }
            'q' => self.subst_confirm_finish(),
            _ => {} // unknown answer: leave the prompt up
        }
    }

    /// Substitute the match at `[s, e)` on the walk's current line with `repl`,
    /// tallying it and setting the continuation point. A `\r`-splitting `repl`
    /// pushes later lines down, so `hi` and the continuation line shift by the
    /// number of newlines introduced.
    fn subst_confirm_apply(&mut self, s: usize, e: usize, repl: &str) {
        let (line, global) = {
            let sc = self.subst_confirm.as_ref().expect("confirm active");
            (sc.line, sc.global)
        };
        if !self.subst_confirm.as_ref().expect("confirm active").pushed {
            self.push_undo();
            self.subst_confirm.as_mut().expect("confirm active").pushed = true;
        }
        let start = self.buffer().line_start(line);
        self.buffer_mut().remove(start + s..start + e);
        self.buffer_mut().insert(start + s, repl);

        let extra = repl.matches('\n').count();
        // Where the rest of the original line now begins: just past `repl`, on a
        // later rope line when `repl` split it.
        let tail_byte = match repl.rfind('\n') {
            Some(nl) => repl.len() - (nl + 1),
            None => s + repl.len(),
        };
        let cont_line = line + extra;
        // An empty match (`s == e`) with an empty/zero-width replacement would
        // re-fire at the same spot forever; step one grapheme on so the walk
        // progresses (mirrors the regex crate's empty-match handling).
        let cont_byte = if global && e == s && extra == 0 {
            next_char_boundary(&self.buffer().line(cont_line), tail_byte)
        } else {
            tail_byte
        };

        let sc = self.subst_confirm.as_mut().expect("confirm active");
        sc.subs += 1;
        if !sc.line_dirty {
            sc.nlines += 1;
            sc.line_dirty = true;
        }
        sc.hi += extra;
        sc.last_changed = Some(cont_line);
        if global {
            sc.line = cont_line;
            sc.byte = cont_byte;
        } else {
            // One substitution per line without `g`: move to the next line.
            sc.line = cont_line + 1;
            sc.byte = 0;
            sc.line_dirty = false;
        }
    }

    /// Decline the match ending at `e` and advance the walk past it (to the next
    /// line without `g`, else just past this match on the same line).
    fn subst_confirm_skip(&mut self, e: usize) {
        let (line, byte, global) = {
            let sc = self.subst_confirm.as_ref().expect("confirm active");
            (sc.line, sc.byte, sc.global)
        };
        if global {
            // Past the match; force a step for an empty one so we make progress.
            let next = if e > byte {
                e
            } else {
                next_char_boundary(&self.buffer().line(line), byte)
            };
            self.subst_confirm.as_mut().expect("confirm active").byte = next;
        } else {
            let sc = self.subst_confirm.as_mut().expect("confirm active");
            sc.line += 1;
            sc.byte = 0;
            sc.line_dirty = false;
        }
    }

    /// Close out the confirm walk: normalize and land the cursor on the last
    /// changed line (vim's resting place), then report the count — silent for a
    /// lone single substitution, just like the non-confirm path.
    fn subst_confirm_finish(&mut self) {
        // Drop the current-match diff overlay before the walk ends.
        self.clear_subst_preview();
        let sc = self.subst_confirm.take().expect("confirm active");
        if let Some(last) = sc.last_changed {
            self.buffer_mut().normalize();
            self.cursor.line = last;
            self.cursor.col = self.first_non_blank(last);
            self.clamp_cursor();
        }
        // Clear the now-stale prompt; report a count only when there's more than
        // one substitution or more than one line (vim stays silent otherwise).
        self.message.clear();
        if sc.subs != 0 && (sc.subs != 1 || sc.nlines != 1) {
            self.echo(fmt_subst_report(
                sc.subs,
                "substitution",
                "substitutions",
                sc.nlines,
            ));
        }
    }

    /// `:w` (and the write half of `:wq` / `:x`, which pass `then_quit`). In a daemon
    /// session (off-tick save mode) this *snapshots and enqueues* the write instead of
    /// touching the filesystem — the server pushes the bytes over the wire and
    /// finalizes the saved-state on the ack — so a slow remote write never freezes the
    /// editor; otherwise it writes synchronously through `host_fs` exactly as before.
    fn ex_write(&mut self, args: &str, bang: bool, then_quit: Option<bool>) {
        let path = if args.is_empty() {
            None
        } else {
            Some(PathBuf::from(args))
        };
        // Off-tick save (daemon session): resolve the target (arg, else bound path),
        // snapshot, and enqueue — the disk-change guard and the actual write happen
        // off the editor tick. The guard can't run here anyway: it needs a remote
        // stat, which we've sworn off on the tick (a `HostWatch`-driven check is a
        // later slice); a deferred quit rides the pending save until its ack.
        if self.host_fs_offtick {
            match path.or_else(|| self.buffer().path.clone()) {
                Some(target) => {
                    self.enqueue_save(target, then_quit);
                }
                None => self.echo("E32: No file name"),
            }
            return;
        }
        // Refuse to overwrite a file that changed on disk since we read or last
        // saved it, unless forced with `:w!`. The guard only applies to a write to
        // the buffer's *own* file — `:w other-name` targets a different file the
        // buffer's disk snapshot says nothing about (that's E13 territory, not
        // handled here). `:w!` skips the check and clobbers, as in vim.
        let writes_own_file = match &path {
            None => true,
            Some(p) => self.buffer().path.as_deref() == Some(p.as_path()),
        };
        let fs = self.host_fs.clone();
        if !bang && writes_own_file && self.buffer().disk_changed(&*fs) {
            self.echo(
                "WARNING: The file has changed on disk since editing started (add ! to override)",
            );
            return;
        }
        let buffer = self.cur_buffer();
        match self.buffer_mut().write(path, &*fs) {
            Ok((bytes, lines)) => {
                // The current state is now what's on disk — undoing/redoing back
                // to it should read as clean, and the saved node carries a save
                // number for `vim.fn.undotree()`.
                self.mark_undo_saved(buffer);
                let written_path = self.buffer().path.clone();
                let name = written_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.echo(format!("\"{name}\" {lines}L, {bytes}B written"));
                // Record the write so the server fires `BufWritePre`/`BufWritePost`
                // (a path-less buffer can't reach a successful `write`).
                if let Some(p) = written_path {
                    self.record_write(buffer, p);
                }
            }
            Err(e) => self.echo(e.to_string()),
        }
        // The synchronous companion to a deferred quit: `:wq` / `:x` close the
        // window (or quit) right after the write, exactly as the old call site did.
        // (A failed write leaves the buffer modified, so a last-window quit reports
        // E37 — same as before.)
        if let Some(bang) = then_quit {
            self.ex_quit(bang);
        }
    }

    /// `:q` / `<C-w>q` — close the focused window. Closing a float, or an ordinary
    /// window while other ordinary windows remain, is exactly `<C-w>c`: the window
    /// goes away and its buffer stays in the store (other windows may still show
    /// it, or it's reachable via `:b`), so there is nothing to lose and no modified
    /// guard — closing a non-last window onto a modified buffer is fine. Only the
    /// **last ordinary** window is a real editor quit, which defers to
    /// [`Self::ex_quit_all`] (and its `E37` guard); a leftover float never keeps
    /// the editor alive (and gating on the *total* window count would wrongly try
    /// to close that last ordinary window, stranding focus on a deleted id).
    pub(crate) fn ex_quit(&mut self, bang: bool) {
        // `:q` on a focused float closes just the float and never quits the
        // editor. For a tiled window, only the *last tiled* one is a real quit —
        // floats don't keep the editor alive, nor do they count toward "more
        // than one window" (the last-tiled rule lives in `remove_window`).
        let on_float = self.windows.cur().float.is_some();
        if on_float || self.windows.tiled_count() > 1 {
            self.close_window();
            return;
        }
        // Last tiled window of *this* tab. What that means depends on the focused
        // layer (`self.windows` only ever holds the focused layer's tree):
        match self.focused_layer {
            // A dock: `:q` is a dock dismissal, never an editor quit — the main
            // layer's buffers are still open. Route through `close_tab`, whose
            // last-tab guard closes the whole dock; an earlier dock tab just closes.
            Layer::Dock(_) => self.close_tab(),
            // Main layer: another tab open means `:q` closes the tab page (like
            // `:tabclose`) — its buffers stay loaded, so there's nothing to lose and
            // no modified guard, same as a non-last window. The genuine last window
            // of the last tab is a real editor quit.
            Layer::Main => {
                if self.main_tabs.tabs.len() > 1 {
                    self.close_tab();
                } else {
                    self.ex_quit_all(bang);
                }
            }
        }
    }

    /// `:qa` (and last-window `:q`) — quit the *editor*, but only if nothing
    /// would be lost. With `!`, exit unconditionally (discarding every buffer).
    /// Otherwise, if any buffer has unsaved changes, *don't* quit: surface that
    /// buffer (the current one if it's the one modified, else the lowest-numbered
    /// modified buffer) **in its own layer** — crossing into its home dock when it
    /// lives in one, never dragging it into the main area — and report `E37`, so the
    /// user sees what's blocking the quit. With no modified buffers, exit.
    fn ex_quit_all(&mut self, bang: bool) {
        if bang {
            self.should_quit = true;
            return;
        }
        let cur = self.current_buffer_id();
        if self.buffer().modified && !self.quit_safe_unnamed(cur) {
            // Already showing the offending buffer.
            self.echo("E37: No write since last change (add ! to override)");
            return;
        }
        match self.first_blocking_modified_buffer() {
            Some(id) => {
                // Surface the blocking buffer *in its own layer* — buffers are
                // scoped per layer, so a dock's modified buffer must reappear in
                // that dock, never get yanked into the main area (which would hide
                // the real main buffer and retag the dock buffer's home layer).
                // Cross to its home layer first when it's still open, then show it.
                let home = self.buffers.get(id).layer;
                if home != self.focused_layer && self.layer_is_open(home) {
                    self.switch_layer(home);
                }
                // Warn after the switch — the layer/buffer change clears the message.
                self.switch_buffer(id);
                self.echo(format!(
                    "E37: No write since last change for buffer {} (add ! to override)",
                    id.0
                ));
            }
            None => self.should_quit = true,
        }
    }

    /// The lowest-numbered buffer whose unsaved changes would actually be **lost** on
    /// quit — modified, and not a [`quit_safe_unnamed`](Self::quit_safe_unnamed) buffer
    /// the workspace session is about to persist. `None` when nothing blocks the quit.
    fn first_blocking_modified_buffer(&self) -> Option<BufferId> {
        self.buffers
            .map
            .iter()
            .filter(|(_, ob)| ob.buffer.modified)
            .map(|(id, _)| *id)
            .find(|id| !self.quit_safe_unnamed(*id))
    }

    /// Whether buffer `id`'s unsaved changes will be **persisted** by the workspace
    /// session on exit, so `:qa` needn't block (`E37`) on it: a modified, ordinary,
    /// **unnamed** (`[No Name]`) buffer that is shown in a window the session captures —
    /// any main tab *or* any edge dock (see [`Self::buffer_in_persisted_window`]) — with
    /// `'workspacepersistunnamed'` on in a layout-capturing session. A *hidden* modified
    /// `[No Name]` isn't captured, so it still blocks — its content really would be lost.
    fn quit_safe_unnamed(&self, id: BufferId) -> bool {
        if !self.options.workspace_persist_unnamed || !self.session_captures_layout {
            return false;
        }
        let Some(ob) = self.buffers.map.get(&id) else {
            return false;
        };
        ob.buffer.path.is_none() && !ob.buffer.read_only() && self.buffer_in_persisted_window(id)
    }

    /// Whether `buf` is shown in a tiled window the workspace session will CAPTURE: any
    /// main tab, or any existing edge dock (visible or hidden). Mirrors exactly the trees
    /// [`Editor::export_session`] + `export_docks` walk, so [`Self::quit_safe_unnamed`]
    /// exempts precisely the unnamed buffers the session is about to persist. Floating
    /// windows are excluded (the session never captures floats).
    ///
    /// Deliberately does NOT use [`Editor::window_showing`]: that reads `self.windows`
    /// for the active main tab, which holds the *dock* tree when a dock has focus (the
    /// main tree is parked then), so it scans the wrong tree and misses a main-layer
    /// buffer — the exact case (`:qa` from a focused dock) this guards. [`Editor::tab_tree`]
    /// resolves the active main tab correctly regardless of which layer holds focus.
    fn buffer_in_persisted_window(&self, buf: BufferId) -> bool {
        let main_trees = self.tab_ids().into_iter().filter_map(|t| self.tab_tree(t));
        let dock_trees = super::DockSide::ALL
            .into_iter()
            .filter(|&s| self.dock_exists(s))
            .filter_map(|s| self.layer_tree(super::Layer::Dock(s)));
        for tree in main_trees.chain(dock_trees) {
            for win in tree.leaves() {
                if tree.try_get(win).map(|w| w.buffer) == Some(buf) {
                    return true;
                }
            }
        }
        false
    }

    /// `nx.open(path, { where })` — open `path` (a file or directory) in the editing
    /// area. With `where_main`, first cross back to the **Main** layer so an open
    /// fired from a dock keymap (a file tree's `<CR>`) lands in the main editor
    /// rather than inside the sidebar; otherwise it opens in the current window like
    /// `:edit`. Reuses the `:edit` open dispatch wholesale (the in-window explorer
    /// for a directory, the shared open kernel for a file, off-tick aware).
    pub fn open_path_in_layer(&mut self, path: &str, where_main: bool) {
        if path.is_empty() {
            self.echo("E32: No file name");
            return;
        }
        if where_main {
            self.ensure_main_layer();
        }
        self.ex_edit(path, false);
    }

    fn ex_edit(&mut self, args: &str, bang: bool) {
        if args.is_empty() {
            self.echo("E32: No file name");
            return;
        }
        let path = PathBuf::from(args);

        // `:e dir` opens the in-window file explorer (vim's netrw), which is a pure-Lua
        // plugin (`prelude/explorer.lua`). A directory flows through the ordinary
        // open-or-switch path below: `should_defer_open` (a `BufReadCmd` handler is
        // registered) enqueues it so the server fires `BufReadCmd` and the explorer claims
        // the read and fills the listing. The core has no directory-buffer notion, so
        // there is no directory-specific branch here.

        // Re-editing the current file reloads it in place (`:e` / `:e!`),
        // discarding unsaved changes — so the modified guard applies here.
        // cwd-aware: `:e <abs path of the current relative buffer>` reloads in
        // place too, rather than stranding a duplicate buffer.
        if self.current_buffer_is(&path) {
            if self.buffer().modified && !bang {
                self.echo("E37: No write since last change (add ! to override)");
                return;
            }
            // Off-tick (daemon session): defer the re-read to the server, which
            // fetches over the wire and replaces this buffer's content when it lands
            // (`load_str_into`). The old content shows until then — a slow remote
            // reload never freezes the editor.
            if self.host_fs_offtick {
                let buf = self.cur_buffer();
                self.enqueue_open(buf, path);
            } else {
                self.load_into_current(&path);
            }
            return;
        }

        // Otherwise open or switch into the current window through the shared open
        // kernel: switch to it if already open (the current buffer stays in the list —
        // vim's `hidden` — so no modified guard), reuse a throwaway `[No Name]` in place
        // (so the first `:e` doesn't strand an empty buffer 1), or load a fresh buffer and
        // switch. The kernel routes the load off-tick in a daemon session and synchronously
        // otherwise — the same `:tabnew` / go-to / explorer share.
        //
        // Reaching here means the target is *not* the current file (the reload
        // branch above returned), so this is a jump in vim's sense: record the
        // position we leave first, so `<C-o>` returns here after the switch. (The
        // shared `edit_in_current_window` kernel must not do this itself — the
        // located-navigation callers, e.g. `jump_to`, record their own jump.)
        self.record_jump_context();
        self.edit_in_current_window(&path);
    }

    /// `:split [file]` / `:vsplit [file]` — split the focused window, then (with a
    /// file argument) `:edit` it in the new window. With no argument both windows
    /// show the same buffer.
    fn ex_split(&mut self, dir: SplitDir, args: &str) {
        self.split(dir);
        let file = args.trim();
        if !file.is_empty() {
            self.ex_edit(file, false);
        }
    }

    /// `:new` / `:vnew` — split the focused window and open a fresh `[No Name]`
    /// buffer in the new window.
    fn ex_new(&mut self, dir: SplitDir) {
        self.split(dir);
        self.ex_enew();
    }

    /// `:tabnew` / `:tabedit [file]` — open a new tab page after the current one.
    /// With a file argument the new tab edits it; with none it opens a fresh
    /// `[No Name]` buffer (vim's behavior for both names). The new tab inherits the
    /// source window's local options (the number gutter), like a split does.
    ///
    /// The buffer is resolved *before* the tab is created — a fresh empty one, the
    /// named file reused if already open, or a new buffer loaded from disk — so the
    /// new tab never reuses (and clobbers) the current tab's buffer the way an
    /// in-place `:edit` of a throwaway buffer would.
    fn ex_tabnew(&mut self, args: &str) {
        let options = self.windows.cur().options.clone();
        let file = args.trim();
        let buf = if file.is_empty() {
            self.add_buffer(Buffer::empty())
        } else {
            // Share the open kernel — but a new tab must get its *own* buffer (never reuse
            // the current window's the way `:e` does), so this is `open_buffer`
            // (find-or-load), not `edit_in_current_window`. Off-tick aware via the kernel.
            // A failed synchronous load falls back to an empty buffer so the tab still opens.
            self.open_buffer(&PathBuf::from(file))
                .unwrap_or_else(|| self.add_buffer(Buffer::empty()))
        };
        self.new_tab(buf, options);
    }

    /// `:res[ize] {n}` — set the focused window's height; `:vertical resize {n}`
    /// routes here with `axis = Vertical` for its width. `{n}` is absolute; `+n`
    /// / `-n` are relative. A bare `:resize` maximizes along the axis (vim). The
    /// extent is text rows for height (the status line is excluded, as in vim) and
    /// columns for width.
    fn ex_resize(&mut self, axis: SplitDir, args: &str) {
        let arg = args.trim();
        if arg.is_empty() {
            self.maximize_window(axis);
            return;
        }
        let delta = if let Some(rest) = arg.strip_prefix('+') {
            rest.trim().parse::<isize>().ok()
        } else if let Some(rest) = arg.strip_prefix('-') {
            rest.trim().parse::<isize>().ok().map(|n| -n)
        } else {
            // Absolute: aim for `target` and resize by the difference from the
            // current extent.
            match arg.parse::<usize>() {
                Ok(target) => {
                    let rect = self.windows.cur().rect;
                    let current = match axis {
                        SplitDir::Horizontal => rect.height.saturating_sub(1),
                        SplitDir::Vertical => rect.width,
                    };
                    Some(target as isize - current as isize)
                }
                Err(_) => None,
            }
        };
        match delta {
            Some(d) => self.resize_window(axis, d),
            None => self.echo("E487: Argument must be a number"),
        }
    }

    /// `:tab {cmd}` — the `tab` modifier. Makes the next window-opening command
    /// target a **new tab page** (after the current one) instead of a split or
    /// the current window. Mirrors [`Editor::ex_vertical`]: the window-opening
    /// commands are re-routed here; `:tab drop` defers to [`Editor::ex_drop`];
    /// anything else falls through to the user-command resolver.
    ///
    /// `:tab split` is special — it clones the *current* buffer + view into the
    /// new tab (a split made into its own tab). `:tab edit`/`:tab new`/`:tab
    /// enew` open a named or fresh buffer there, exactly like `:tabedit`.
    fn ex_tab(&mut self, args: &str) {
        let (name, _bang, rest) = split_ex(args.trim());
        match name {
            "sp" | "spl" | "split" => self.tab_split(),
            "e" | "edit" | "new" | "ene" | "enew" => {
                if let Some(a) = self.expand_file_arg_or_echo(rest) {
                    self.ex_tabnew(&a);
                }
            }
            "dr" | "dro" | "drop" => {
                if let Some(a) = self.expand_file_arg_or_echo(rest) {
                    self.ex_drop(&a, true);
                }
            }
            "" => self.echo("E471: Argument required"),
            _ => self
                .deferred_commands
                .push(DeferredCmd::Server(format!("tab {args}"))),
        }
    }

    /// `:drop {file}` / `:tab drop {file}` — focus a window that already shows
    /// `{file}` (in any tab), else open it. With the file already on screen the
    /// `tab` / split distinction is moot: both forms just jump to that window.
    /// Otherwise `:drop` edits it in the current window (`:edit`) and `:tab drop`
    /// opens it in a new tab (`:tabedit`). An empty argument is `E471`.
    fn ex_drop(&mut self, args: &str, in_new_tab: bool) {
        let file = args.trim();
        if file.is_empty() {
            self.echo("E471: Argument required");
            return;
        }
        let path = PathBuf::from(file);
        if let Some(buf) = self.find_buffer_by_path(&path) {
            if let Some((tab_idx, win)) = self.window_showing(buf) {
                self.goto_tab_window(tab_idx, win);
                return;
            }
        }
        if in_new_tab {
            self.ex_tabnew(file);
        } else {
            self.ex_edit(file, false);
        }
    }

    /// `:vertical {cmd}` — the `vertical` modifier. Re-routes the split / resize
    /// commands to their vertical (width-dividing) form.
    fn ex_vertical(&mut self, args: &str) {
        let (name, _bang, rest) = split_ex(args.trim());
        match name {
            "res" | "resize" => self.ex_resize(SplitDir::Vertical, rest),
            "sp" | "spl" | "split" => {
                if let Some(a) = self.expand_file_arg_or_echo(rest) {
                    self.ex_split(SplitDir::Vertical, &a);
                }
            }
            "new" => self.ex_new(SplitDir::Vertical),
            "" => self.echo("E471: Argument required"),
            _ => self
                .deferred_commands
                .push(DeferredCmd::Server(format!("vertical {args}"))),
        }
    }

    /// `:enew` — open a new, empty `[No Name]` buffer in the window. Reuses a
    /// throwaway current buffer rather than stacking another empty one.
    fn ex_enew(&mut self) {
        if self.current_is_throwaway() {
            return;
        }
        // Starting to edit a new buffer is a jump: stash the position we leave so
        // `<C-o>` returns here (vim records the pre-jump mark in `do_ecmd`).
        self.record_jump_context();
        let id = self.add_buffer(Buffer::empty());
        self.switch_buffer(id);
    }

    /// `:wall` — write every modified buffer that has a file name. Returns the
    /// [`PendingSave::seq`]s enqueued in off-tick mode (empty otherwise), so `:wqa`
    /// can gate its quit on the whole batch; a bare `:wall` discards them.
    ///
    /// A buffer whose file changed on disk since we read it is *not* clobbered
    /// (unless forced with `:wall!`): the safe buffers are still written, and the
    /// conflicting ones skipped. If any was skipped we then switch to the *first*
    /// such buffer and warn — the same way `:q` surfaces the buffer blocking a
    /// quit — so the user lands on the file at risk rather than silently losing
    /// the outside edit.
    fn ex_write_all(&mut self, bang: bool) -> Vec<u64> {
        // Off-tick (daemon session): snapshot every modified file-backed buffer into a
        // `PendingSave` the server pushes over the wire — the multi-buffer companion to
        // the single-buffer `:w` (`ex_write`). The disk-change/conflict guard is skipped
        // here for the same reason `ex_write` skips it off-tick: it needs a remote stat
        // we've sworn off the editor tick (a `HostWatch`-driven check is a later slice).
        // Each save acks independently with its own `written` echo; per-buffer ordering
        // and failure handling are the server's (the buffers are distinct, so they all
        // dispatch concurrently). No summary echo — the per-ack echoes carry it.
        if self.host_fs_offtick {
            let ids: Vec<BufferId> = self.buffers.map.keys().copied().collect();
            let mut seqs = Vec::new();
            for id in ids {
                let buf = &self.buffers.get(id).buffer;
                if let (true, Some(path)) = (buf.modified, buf.path.clone()) {
                    // A buffer that can't be encoded to its `'fileencoding'` is skipped
                    // (the error is echoed inside `enqueue_save_of`); the rest still save.
                    if let Some(seq) = self.enqueue_save_of(id, path, None) {
                        seqs.push(seq);
                    }
                }
            }
            return seqs;
        }
        let ids: Vec<BufferId> = self.buffers.map.keys().copied().collect();
        let fs = self.host_fs.clone();
        let mut written = 0;
        let mut conflict = None;
        for id in ids {
            let ob = self.buffers.get_mut(id);
            if !(ob.buffer.modified && ob.buffer.path.is_some()) {
                continue;
            }
            if !bang && ob.buffer.disk_changed(&*fs) {
                // Don't clobber an outside edit; remember the first such buffer (ids
                // ascend, so this is the lowest-numbered conflict) to surface below.
                conflict.get_or_insert(id);
                continue;
            }
            if self.buffers.get_mut(id).buffer.write(None, &*fs).is_ok() {
                // The written state is now the saved node (carries a save number).
                self.mark_undo_saved(id);
                written += 1;
                // Fire `BufWritePre`/`BufWritePost` for each buffer that landed
                // (it's modified and file-backed, so it has a path).
                if let Some(path) = self.buffers.get(id).buffer.path.clone() {
                    self.record_write(id, path);
                }
            }
        }
        if let Some(id) = conflict {
            // Surface the first at-risk buffer and warn (the switch clears the
            // message, so set it afterwards); the safe buffers are already saved.
            self.switch_buffer(id);
            self.echo(
                "WARNING: The file has changed on disk since editing started (add ! to override)",
            );
        } else {
            self.echo(format!("{written} buffer(s) written"));
        }
        Vec::new()
    }

    /// `:wqa` / `:xa` — write every modified buffer, then quit the editor.
    ///
    /// Locally this is just `:wall` followed by `:qall` (the writes are synchronous, so
    /// the buffers are clean by the time the quit runs). In a daemon session the writes
    /// go *off-tick* over the wire, so the quit can't fire inline — the buffers are still
    /// `[+]` until their acks land. Instead core hands the server a [`PendingQuitAll`]
    /// naming the batch's writes; the server replays `:qa` only once every one has acked,
    /// and **cancels** the quit if any fails (the multi-buffer form of `:wq`'s deferred,
    /// ack-gated, failure-cancels quit). A `:wqa` with nothing to save still quits
    /// immediately — there's no write to wait on — exactly as `:qa` would.
    fn ex_write_quit_all(&mut self, bang: bool) {
        let seqs = self.ex_write_all(bang);
        if !self.host_fs_offtick {
            self.ex_quit_all(bang);
            return;
        }
        if seqs.is_empty() {
            // Off-tick, but no modified file-backed buffer was enqueued: nothing to wait
            // on, so quit now (a modified *no-name* buffer makes `:qa` report E37, as in
            // vim — the gate would never let us, since it watches no writes).
            self.ex_quit_all(bang);
        } else {
            self.pending_quit_all = Some(PendingQuitAll { bang, seqs });
        }
    }
}
