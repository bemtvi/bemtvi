//! Ex-command dispatch (`execute_ex`), range/address parsing, `:substitute`, and
//! the file/window/tab ex-commands.

use super::*;
use crate::buffer::Buffer;
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
    let args = cmd[i..].trim();
    (name, bang, args)
}

/// Parse a `:sleep` argument: `{n}` = seconds, `{n}m` = milliseconds, empty =
/// 1 second (matching vim). Returns a vim-style `E475` error string for
/// non-integer input.
/// Parse a buffer-navigation count argument (`:bnext 2`). Empty / invalid / zero
/// all mean 1, matching vim's default repeat count.
fn parse_count_arg(args: &str) -> usize {
    parse_opt_count_arg(args).unwrap_or(1)
}

/// A positive numeric command argument, or `None` when absent / non-numeric. The
/// `Option`-preserving form of [`parse_count_arg`] — `:tabnext` (no count → next
/// tab) needs the absent case distinguished from `1` (no count → tab 1).
fn parse_opt_count_arg(args: &str) -> Option<usize> {
    args.trim().parse::<usize>().ok().filter(|n| *n > 0)
}

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
    fn alternate_file_name(&self) -> Option<String> {
        let id = self.alternate?;
        self.buffers
            .map
            .get(&id)?
            .buffer
            .path
            .as_ref()
            .map(|p| p.display().to_string())
            .filter(|s| !s.is_empty())
    }

    /// Expand the `%` (current file) and `#` (alternate file) tokens in a file
    /// argument, each optionally followed by a run of `:` filename modifiers
    /// (`:h` head, `:t` tail, `:r` root, `:e` extension). `\%` / `\#` are literal.
    /// A `:` not introducing a known modifier ends the run and stays literal.
    ///
    /// Returns the rewritten argument, or a vim-style error string when a token
    /// has no name to substitute, or when an env-dependent modifier (`:p` / `:~` /
    /// `:.`) is used — those need the working directory / `$HOME`, which the pure
    /// core deliberately can't read, so they fail loud rather than mis-expand.
    fn expand_file_arg(&self, arg: &str) -> Result<String, String> {
        let chars: Vec<char> = arg.chars().collect();
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
    fn expand_file_arg_or_echo(&mut self, arg: &str) -> Option<String> {
        match self.expand_file_arg(arg) {
            Ok(s) => Some(s),
            Err(e) => {
                self.echo(e);
                None
            }
        }
    }

    pub(crate) fn execute_ex(&mut self, raw: &str) {
        let cmd = raw.trim();
        if cmd.is_empty() {
            return;
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

        let (name, bang, args) = split_ex(rest);
        match name {
            "w" | "write" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_write(&a);
                }
            }
            "q" | "quit" => self.ex_quit(bang),
            "wq" | "x" | "xit" | "exit" => {
                // Write the current buffer, then `:q` it (close the window, or
                // quit on the last window). A failed write leaves the buffer
                // modified, so a last-window quit then reports it.
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_write(&a);
                    self.ex_quit(bang);
                }
            }
            "qa" | "qall" | "quita" | "quitall" => self.ex_quit_all(bang),
            "wa" | "wall" => self.ex_write_all(),
            "wqa" | "xa" | "xall" => {
                self.ex_write_all();
                self.ex_quit_all(bang);
            }
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
            "tabl" | "tablast" => self.goto_tab_next(Some(self.tabs.len())),
            "tab" => self.ex_tab(args),
            "dr" | "dro" | "drop" => {
                if let Some(a) = self.expand_file_arg_or_echo(args) {
                    self.ex_drop(&a, false);
                }
            }
            "res" | "resize" => self.ex_resize(SplitDir::Horizontal, args),
            "vert" | "vertical" | "ver" => self.ex_vertical(args),
            "ls" | "buffers" | "files" => self.ex_buffers(),
            "b" | "bu" | "buf" | "buffer" => {
                if let Some(id) = self.resolve_buffer(args) {
                    self.switch_buffer(id);
                }
            }
            "bn" | "bnext" => self.ex_bnext(parse_count_arg(args)),
            "bp" | "bN" | "bprev" | "bprevious" | "bNext" => self.ex_bprev(parse_count_arg(args)),
            "bf" | "bfirst" | "br" | "brewind" => self.ex_bfirst(),
            "bl" | "blast" => self.ex_blast(),
            "bd" | "bdel" | "bdelete" | "bw" | "bwipe" | "bwipeout" => self.ex_bdelete(args, bang),
            "lua" => self.lua_queue.push(args.to_string()),
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
            "panelopen" | "panelo" => {
                if !self.reopen_last_panel() {
                    self.echo("No panel to reopen");
                }
            }
            // `:setlocal`/`:setl` shares the handler: buffer-local options
            // (tabstop/shiftwidth/expandtab) live on the current buffer, which is
            // exactly what `:set` already targets for them.
            "set" | "se" | "setlocal" | "setl" => self.ex_set(args),
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
            // Unknown to the core: defer to the server, which resolves it
            // against Lua user commands (or reports the unknown-command error).
            _ => self.deferred_commands.push(rest.to_string()),
        }
    }

    /// `:echo` / `:echomsg` / `:echoerr` — evaluate the argument as a Vim
    /// expression and surface the result per `kind`. A `:echo` sets only the
    /// message line (not the history); `:echomsg`/`:echoerr` go through
    /// [`Editor::echo`], which records them. An evaluation error is always shown
    /// (and recorded) as the error it is.
    fn ex_echo(&mut self, args: &str, kind: EchoKind) {
        match expr::eval_echo(args) {
            Ok(text) => match kind {
                EchoKind::Transient => self.message = text,
                EchoKind::Message | EchoKind::Error => self.echo(text),
            },
            Err(e) => self.echo(e),
        }
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
                    n = n * 10 + i64::from(d - b'0');
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
                n = n * 10 + i64::from(d - b'0');
                *i += 1;
                digits = true;
            }
            if !digits {
                n = 1;
            }
            line += if sign == b'+' { n } else { -n };
            have_offset = true;
        }

        if !have_base && !have_offset {
            *i = start;
            return Ok(None);
        }
        Ok(Some(line.clamp(0, self.last_line() as i64) as usize))
    }

    /// `:[range]s/{pat}/{rep}/[flags] [count]` — substitute matches of `pat`
    /// with `rep` over the range (canonical regex, the `/`-search dialect).
    /// Flags: `g` (every match on a line), `i`/`I` (force ignore/match case),
    /// `n` (count only, no edit). Fails loud on a bad delimiter, an unknown
    /// flag, the not-yet-built `c` flag, an invalid pattern, or no match.
    fn ex_substitute(&mut self, range: ExRange, args: &str) {
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
        let re = match SearchRegex::compile(&pattern, ignorecase) {
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
            Some(c) => (range.hi, (range.hi + c - 1).min(self.last_line())),
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
                self.echo(format!(
                    "{matches} {} on {nlines} {}",
                    if matches == 1 { "match" } else { "matches" },
                    if nlines == 1 { "line" } else { "lines" },
                ));
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
            self.echo(format!(
                "{subs} {} on {nlines} {}",
                if subs == 1 {
                    "substitution"
                } else {
                    "substitutions"
                },
                if nlines == 1 { "line" } else { "lines" },
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
        let re = match SearchRegex::compile(&pattern, self.search_ignorecase(&pattern)) {
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

    /// `:[range]d[elete]` — delete the range's lines (default: the current line),
    /// landing the cursor on the line that takes their place (first non-blank).
    fn ex_delete(&mut self, range: ExRange) {
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

    /// `:[line]pu[t] [x]` — insert register `x` (default the unnamed register) as
    /// whole lines below the addressed line (default the current line), or above
    /// it with `:put!`. Always linewise: a charwise register's text is inserted as
    /// a line regardless of its own kind, matching vim's `:put`. An empty register
    /// reports `E353` rather than silently doing nothing.
    fn ex_put(&mut self, range: ExRange, args: &str, above: bool) {
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
                    // Set the prompt directly, not via `echo`: it's a transient
                    // question, kept off the `:messages` history (like vim).
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
            self.echo(format!(
                "{} {} on {} {}",
                sc.subs,
                if sc.subs == 1 {
                    "substitution"
                } else {
                    "substitutions"
                },
                sc.nlines,
                if sc.nlines == 1 { "line" } else { "lines" },
            ));
        }
    }

    fn ex_write(&mut self, args: &str) {
        let path = if args.is_empty() {
            None
        } else {
            Some(PathBuf::from(args))
        };
        match self.buffer_mut().write(path) {
            Ok((bytes, lines)) => {
                // The current state is now what's on disk — undoing/redoing back
                // to it should read as clean, and the saved node carries a save
                // number for `vim.fn.undotree()`.
                self.mark_undo_saved(self.cur_buffer());
                let name = self
                    .buffer()
                    .path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                self.echo(format!("\"{name}\" {lines}L, {bytes}B written"));
            }
            Err(e) => self.echo(e.to_string()),
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
        // Last tiled window of *this* tab, but other tabs are open: `:q` closes the
        // tab page (like `:tabclose`), not the editor — its buffers stay loaded, so
        // there's nothing to lose and no modified guard, same as a non-last window.
        if self.tabs.len() > 1 {
            self.close_tab();
            return;
        }
        self.ex_quit_all(bang);
    }

    /// `:qa` (and last-window `:q`) — quit the *editor*, but only if nothing
    /// would be lost. With `!`, exit unconditionally (discarding every buffer).
    /// Otherwise, if any buffer has unsaved changes, *don't* quit: switch the
    /// window to that buffer (the current one if it's the one modified, else the
    /// lowest-numbered modified buffer) and report `E37`, so the user sees what's
    /// blocking the quit. With no modified buffers, exit.
    fn ex_quit_all(&mut self, bang: bool) {
        if bang {
            self.should_quit = true;
            return;
        }
        if self.buffer().modified {
            // Already showing the offending buffer.
            self.echo("E37: No write since last change (add ! to override)");
            return;
        }
        match self.first_modified_buffer() {
            Some(id) => {
                // Surface the blocking buffer, then warn (the switch clears the
                // message, so set it afterwards).
                self.switch_buffer(id);
                self.echo(format!(
                    "E37: No write since last change for buffer {} (add ! to override)",
                    id.0
                ));
            }
            None => self.should_quit = true,
        }
    }

    /// The lowest-numbered buffer with unsaved changes, if any.
    fn first_modified_buffer(&self) -> Option<BufferId> {
        self.buffers
            .map
            .iter()
            .find(|(_, ob)| ob.buffer.modified)
            .map(|(id, _)| *id)
    }

    fn ex_edit(&mut self, args: &str, bang: bool) {
        if args.is_empty() {
            self.echo("E32: No file name");
            return;
        }
        let path = PathBuf::from(args);

        // `:e dir` opens the in-window file explorer (netrw): `enter_dir` reuses
        // the window when the current buffer is a throwaway/explorer and otherwise
        // opens a fresh listing buffer, keeping the current one in the list.
        if path.is_dir() {
            self.enter_dir(&path);
            return;
        }

        // Re-editing the current file reloads it in place (`:e` / `:e!`),
        // discarding unsaved changes — so the modified guard applies here.
        if self.buffer().path.as_deref() == Some(path.as_path()) {
            if self.buffer().modified && !bang {
                self.echo("E37: No write since last change (add ! to override)");
                return;
            }
            self.load_into_current(&path);
            return;
        }

        // The file is already open in another buffer: just switch to it. The
        // current buffer stays in the list (vim's `hidden` behavior), so there is
        // nothing to lose and no modified guard.
        if let Some(id) = self.find_buffer_by_path(&path) {
            self.switch_buffer(id);
            return;
        }

        // A new file. Reuse a throwaway `[No Name]` buffer if that's all we have
        // (so the first `:e` doesn't strand an empty buffer 1); otherwise open it
        // in a fresh buffer and switch, keeping the current one open.
        if self.current_is_throwaway() {
            self.load_into_current(&path);
        } else {
            match Buffer::from_file(&path) {
                Ok(buf) => {
                    let id = self.add_buffer(buf);
                    self.switch_buffer(id);
                }
                Err(e) => self.echo(e.to_string()),
            }
        }
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
        let options = self.windows.cur().options;
        let file = args.trim();
        let buf = if file.is_empty() {
            self.add_buffer(Buffer::empty())
        } else {
            let path = PathBuf::from(file);
            match self.find_buffer_by_path(&path) {
                Some(id) => id,
                None => match Buffer::from_file(&path) {
                    Ok(b) => self.add_buffer(b),
                    Err(e) => {
                        self.echo(e.to_string());
                        self.add_buffer(Buffer::empty())
                    }
                },
            }
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
            _ => self.deferred_commands.push(format!("tab {args}")),
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
            _ => self.deferred_commands.push(format!("vertical {args}")),
        }
    }

    /// `:enew` — open a new, empty `[No Name]` buffer in the window. Reuses a
    /// throwaway current buffer rather than stacking another empty one.
    fn ex_enew(&mut self) {
        if self.current_is_throwaway() {
            return;
        }
        let id = self.add_buffer(Buffer::empty());
        self.switch_buffer(id);
    }

    /// `:wall` — write every modified buffer that has a file name.
    fn ex_write_all(&mut self) {
        let ids: Vec<BufferId> = self.buffers.map.keys().copied().collect();
        let mut written = 0;
        for id in ids {
            let ob = self.buffers.get_mut(id);
            if ob.buffer.modified && ob.buffer.path.is_some() && ob.buffer.write(None).is_ok() {
                // The written state is now the saved node (carries a save number).
                self.mark_undo_saved(id);
                written += 1;
            }
        }
        self.echo(format!("{written} buffer(s) written"));
    }
}
