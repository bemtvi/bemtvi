//! The quickfix / location list model and the `errorformat` engine.
//!
//! This is a faithful port of vim's `quickfix.c` error-parsing core: an
//! `'errorformat'` string is split into comma-separated *parts*, each part is
//! converted into a vim regex by [`efm_part_to_regpat`] (the port of vim's
//! `efm_to_regpat` / `efmpat_to_regpat` / `scanf_fmt_to_regpat`), and each output
//! line is matched against the parts in turn, pulling fields out of the regex
//! submatches (the port of `qf_parse_line` / `qf_parse_match` / `qf_parse_fmt_*`).
//! The multi-line prefixes (`%A %C %Z %E %W %I %N`), the exclude/append flags
//! (`%-` / `%+`), the `%>` continuation, and the `%D` / `%X` directory stack are
//! all honored.
//!
//! The data types ([`QfEntry`], [`QfList`]) are plain and always compiled; the
//! engine itself rides on [`nxvim_regex`] (vim's vendored `regexp.c`) and so lives
//! behind the `vim-regex` feature — the same engine `:s` / `/` use under
//! `regexsyntax=vim`. A build without it (a pure-Rust core) keeps the list types
//! but fails loud on any parse, never silently dropping lines.

use super::*;
use crate::buffer::Buffer;
use std::path::Path;

/// How a populate request combines with the existing list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QfAction {
    /// Replace the list with the new items (vim's `' '` action / a fresh `:cexpr`).
    New,
    /// Append the new items to the current list (vim's `'a'`).
    Add,
    /// Replace the current list's items in place (vim's `'r'`). With the single
    /// list of Phase 1 this is identical to [`QfAction::New`]; it diverges once the
    /// list stack lands (Phase 4).
    Replace,
}

/// One parsed quickfix/location entry — vim's `qfline_T`, minus the
/// list-threading pointers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QfEntry {
    /// The error's file, resolved against the `%D`/`%X` directory stack. `None`
    /// for a non-error line (plain output text) or an entry addressed only by
    /// buffer number.
    pub filename: Option<String>,
    /// Buffer number from `%b` (`0` if none).
    pub bufnr: i32,
    /// Module name accumulated from `%o` (empty if none).
    pub module: String,
    /// 1-based line number (`0` = none / not jumpable).
    pub lnum: usize,
    /// 1-based end line number from `%e` (`0` = none).
    pub end_lnum: usize,
    /// 1-based column (`0` = none). Byte column unless [`QfEntry::vcol`].
    pub col: usize,
    /// 1-based end column from `%k` (`0` = none).
    pub end_col: usize,
    /// The column is a screen (virtual) column — set by `%v` / `%p`.
    pub vcol: bool,
    /// Error number from `%n` (`-1` = none).
    pub nr: i32,
    /// Search pattern from `%s` (empty if none), already wrapped as `^\V…\$`.
    pub pattern: String,
    /// The message text.
    pub text: String,
    /// Error type char: `'E'`/`'W'`/`'I'`/`'N'` (or a `%t` value), `0` if none.
    pub typ: u8,
    /// A real, jumpable error (vs. a copied non-matching output line).
    pub valid: bool,
}

/// A quickfix or location list: the entries plus a title and the current index.
#[derive(Debug, Clone, Default)]
pub struct QfList {
    /// The parsed entries, in output order.
    pub items: Vec<QfEntry>,
    /// The list title (`:copen` header / `getqflist({title})`). Empty if unset.
    pub title: String,
    /// The 1-based index of the "current" entry for `:cc`/`:cnext` (`0` = none).
    /// Tracked now so Phase 2 navigation has a home; unused until then.
    pub idx: usize,
}

impl QfList {
    /// Apply `items` under `action`, updating the title when one is given.
    fn apply(&mut self, items: Vec<QfEntry>, action: QfAction, title: Option<String>) {
        match action {
            QfAction::Add => self.items.extend(items),
            // `New` and `Replace` both swap the whole item vector while there is a
            // single list; they diverge only once the list stack exists (Phase 4).
            QfAction::New | QfAction::Replace => self.items = items,
        }
        if let Some(title) = title {
            self.title = title;
        }
        // A fresh/replaced list resets the cursor to the first entry; an append
        // leaves it. (`0` when the list is empty.)
        if action != QfAction::Add {
            self.idx = usize::from(!self.items.is_empty());
        }
    }
}

impl Editor {
    /// The current quickfix list (read-only) — the projection source for the
    /// `nx._qflist` Lua mirror and, later, the `:copen` window.
    pub fn qf_list(&self) -> &QfList {
        &self.quickfix
    }

    /// Set the list from already-structured `items` (vim's
    /// `setqflist(list)` non-parsing form).
    pub fn qf_set_items(&mut self, items: Vec<QfEntry>, action: QfAction, title: Option<String>) {
        self.quickfix.apply(items, action, title);
        self.qf_refresh_window();
    }

    /// Parse `lines` against `efm` and set the list (vim's
    /// `setqflist([], a, {lines, efm})` and the `:cexpr` family). Returns the
    /// number of entries added, or an `E37x` error string for an invalid
    /// `'errorformat'`. Behind `vim-regex`; without it, parsing fails loud.
    #[cfg(feature = "vim-regex")]
    pub fn qf_set_from_lines(
        &mut self,
        lines: &[String],
        efm: &str,
        action: QfAction,
        title: Option<String>,
    ) -> Result<usize, String> {
        let format = Errorformat::compile(efm)?;
        let items = format.parse(lines);
        let n = items.len();
        self.quickfix.apply(items, action, title);
        self.qf_refresh_window();
        Ok(n)
    }

    #[cfg(not(feature = "vim-regex"))]
    pub fn qf_set_from_lines(
        &mut self,
        _lines: &[String],
        _efm: &str,
        _action: QfAction,
        _title: Option<String>,
    ) -> Result<usize, String> {
        Err("E: 'errorformat' parsing requires the vim-regex engine (not built)".to_string())
    }

    /// Populate the list from buffer `bufnr`'s lines parsed against the editor's
    /// `'errorformat'` (`:cbuffer` / `:cgetbuffer`). The window-open / jump-to-first
    /// coupling vim's `:cbuffer` adds lands with the quickfix window (Phase 2);
    /// here it only fills the list.
    pub fn qf_from_buffer(&mut self, bufnr: BufferId, action: QfAction) {
        let Some(ob) = self.buffers.map.get(&bufnr) else {
            self.echo(format!("E92: Buffer {} not found", bufnr.0));
            return;
        };
        let lines = ob.buffer.lines();
        let efm = self.options.errorformat.clone();
        let title = format!(":cbuffer {}", bufnr.0);
        match self.qf_set_from_lines(&lines, &efm, action, Some(title)) {
            Ok(n) => self.echo(format!("(quickfix) {n} entries")),
            Err(e) => self.echo(e),
        }
    }

    /// `:cbuffer`/`:cgetbuffer`/`:caddbuffer [bufnr]` — populate the list from a
    /// buffer (current if no argument).
    pub(crate) fn ex_cbuffer(&mut self, args: &str, action: QfAction) {
        let bufnr = if args.trim().is_empty() {
            self.current_buffer_id()
        } else {
            match self.resolve_buffer(args) {
                Some(id) => id,
                None => return, // resolve_buffer already echoed the error
            }
        };
        self.qf_from_buffer(bufnr, action);
    }
}

// ---------------------------------------------------------------------------
// The quickfix window + navigation (Phase 2).

impl Editor {
    /// True when the focused buffer is the quickfix display buffer — its keys are
    /// read-only (routed through [`Editor::handle_quickfix`]).
    pub(crate) fn is_quickfix_buffer(&self) -> bool {
        self.qf_bufnr.is_some() && self.qf_bufnr == Some(self.current_buffer_id())
    }

    /// The window currently showing the quickfix list, if any.
    fn qf_window_id(&self) -> Option<WindowId> {
        let qf = self.qf_bufnr?;
        self.window_ids()
            .into_iter()
            .find(|&w| self.windows.get(w).buffer == qf)
    }

    /// (Re)render the quickfix display buffer from the current list. No-op until
    /// the buffer exists (the window has been opened at least once).
    pub(crate) fn qf_refresh_window(&mut self) {
        let Some(buf) = self.qf_bufnr else { return };
        if !self.buffers.map.contains_key(&buf) {
            self.qf_bufnr = None;
            return;
        }
        let text = self.qf_render_text();
        self.load_str_into(buf, Some("[Quickfix List]".to_string()), &text);
    }

    /// The quickfix buffer's text: one `file|lnum col N| message` line per entry.
    fn qf_render_text(&self) -> String {
        self.quickfix
            .items
            .iter()
            .map(qf_render_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `:copen [height]` — open (or focus) the quickfix window: a full-width split
    /// at the bottom of the screen (vim's `botright`), `height` rows tall (`10` by
    /// default, vim's default; clamped to the available rows). The display buffer
    /// is created on first use.
    pub(crate) fn ex_copen(&mut self, args: &str) {
        let needs_buf = match self.qf_bufnr {
            Some(id) => !self.buffers.map.contains_key(&id),
            None => true,
        };
        if needs_buf {
            let id = self.add_buffer(Buffer::empty());
            self.qf_bufnr = Some(id);
        }
        self.qf_refresh_window();
        if let Some(w) = self.qf_window_id() {
            self.set_current_window(w);
            return;
        }
        let height = args
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&h| h > 0)
            .unwrap_or(10);
        self.qf_prev_win = Some(self.windows.current);
        let qf = self.qf_bufnr.expect("qf buffer created above");
        self.open_bottom_window(qf, height);
    }

    /// `:cclose` — close the quickfix window if open (leaving focus on a code
    /// window).
    pub(crate) fn ex_cclose(&mut self) {
        let Some(w) = self.qf_window_id() else { return };
        let prev = self.windows.current;
        if self.windows.current != w {
            self.set_current_window(w);
        }
        self.close_window();
        if prev != w && self.window_ids().contains(&prev) {
            self.set_current_window(prev);
        }
    }

    /// `:cwindow` — open the quickfix window iff the list is non-empty, else close
    /// it.
    pub(crate) fn ex_cwindow(&mut self, args: &str) {
        if self.quickfix.items.is_empty() {
            self.ex_cclose();
        } else {
            self.ex_copen(args);
        }
    }

    /// `:cc [nr]` — jump to entry `nr` (1-based; current when omitted).
    pub(crate) fn ex_cc(&mut self, nr: Option<usize>) {
        if self.quickfix.items.is_empty() {
            self.echo("E42: No Errors".to_string());
            return;
        }
        let idx = match nr {
            Some(n) => n.saturating_sub(1),
            None => self.quickfix.idx.saturating_sub(1),
        };
        self.qf_jump_to_index(idx.min(self.quickfix.items.len() - 1));
    }

    /// `:cnext` / `:cprev` — step `count` *valid* entries forward / backward and
    /// jump there. `E553` past either end.
    pub(crate) fn ex_cstep(&mut self, forward: bool, count: usize) {
        if !self.quickfix.items.iter().any(|e| e.valid) {
            self.echo("E42: No Errors".to_string());
            return;
        }
        let len = self.quickfix.items.len() as isize;
        let step: isize = if forward { 1 } else { -1 };
        let mut pos = self.quickfix.idx as isize - 1; // 0-based current (-1 if unset)
        let mut remaining = count.max(1);
        while remaining > 0 {
            pos += step;
            if pos < 0 || pos >= len {
                self.echo("E553: No more items".to_string());
                return;
            }
            if self.quickfix.items[pos as usize].valid {
                remaining -= 1;
            }
        }
        self.qf_jump_to_index(pos as usize);
    }

    /// `:cfirst` / `:clast` — jump to the first / last valid entry.
    pub(crate) fn ex_cfirst(&mut self) {
        match self.quickfix.items.iter().position(|e| e.valid) {
            Some(i) => self.qf_jump_to_index(i),
            None => self.echo("E42: No Errors".to_string()),
        }
    }

    pub(crate) fn ex_clast(&mut self) {
        match self.quickfix.items.iter().rposition(|e| e.valid) {
            Some(i) => self.qf_jump_to_index(i),
            None => self.echo("E42: No Errors".to_string()),
        }
    }

    /// Jump to entry `idx` (0-based): mark it current, focus a code window per
    /// `'switchbuf'`, and land the cursor at the entry's `file:line:col`.
    pub(crate) fn qf_jump_to_index(&mut self, idx: usize) {
        let Some(entry) = self.quickfix.items.get(idx).cloned() else {
            self.echo("E42: No Errors".to_string());
            return;
        };
        self.quickfix.idx = idx + 1;
        let Some(filename) = entry.filename.clone() else {
            // A non-error line (no file): echo its text, like vim's E42-free no-op.
            self.echo(entry.text.clone());
            return;
        };
        self.qf_focus_target_window();
        let line0 = entry.lnum.saturating_sub(1);
        let col0 = entry.col.saturating_sub(1);
        self.jump_to(Path::new(&filename), line0, col0);
    }

    /// Move focus to the window a quickfix jump should land in, honoring
    /// `'switchbuf'`. From the quickfix window, step to the source code window
    /// (the one `:copen` was invoked from, else any non-quickfix window, else a
    /// fresh split). Then a `split`/`vsplit` switchbuf value opens a new window for
    /// the jump. (`newtab`/`usetab` are not yet acted on.)
    fn qf_focus_target_window(&mut self) {
        if self.is_quickfix_buffer() {
            let qf_win = self.qf_window_id();
            let live = self.window_ids();
            let target = self
                .qf_prev_win
                .filter(|w| live.contains(w) && Some(*w) != qf_win)
                .or_else(|| live.into_iter().find(|w| Some(*w) != qf_win));
            match target {
                Some(w) => self.set_current_window(w),
                None => self.split(SplitDir::Horizontal),
            }
        }
        let swb = self.options.switchbuf.clone();
        if swb.split(',').any(|s| s == "vsplit") {
            self.split(SplitDir::Vertical);
        } else if swb.split(',').any(|s| s == "split") {
            self.split(SplitDir::Horizontal);
        }
    }
}

/// Render one quickfix entry as a `:copen` line: `file|lnum col N| message`
/// (vim's default format). A non-error line drops the empty location: `|| text`.
fn qf_render_line(e: &QfEntry) -> String {
    let fname = e.filename.as_deref().unwrap_or("");
    let mut loc = String::new();
    if e.lnum > 0 {
        loc.push_str(&e.lnum.to_string());
        if e.col > 0 {
            loc.push_str(" col ");
            loc.push_str(&e.col.to_string());
        }
    }
    let text = e.text.replace('\n', " ");
    format!("{fname}|{loc}| {text}")
}

// ---------------------------------------------------------------------------
// The errorformat engine (vim regexp engine required).

#[cfg(feature = "vim-regex")]
pub(crate) use engine::Errorformat;

#[cfg(feature = "vim-regex")]
mod engine {
    use super::QfEntry;
    use nxvim_regex::{Engine, PatternKind, VimRegex};
    use std::path::Path;

    // The 'errorformat' conversion characters, in vim's `fmt_pat[]` order. The
    // index of each is its capture-group ordinal source and its `qf_parse_fmt`
    // slot. Keep in sync with `PATTERNS` below.
    const CONV: [u8; 14] = [
        b'f', b'b', b'n', b'l', b'e', b'c', b'k', b't', b'm', b'r', b'p', b'v', b's', b'o',
    ];
    // The regex fragment each conversion expands to (vim's `fmt_pat[].pattern`).
    // `%f` is special-cased in `efm_part_to_regpat` and never reads its slot here.
    const PATTERNS: [&str; 14] = [
        ".\\+",     // f (only used when %f is at the end)
        "\\d\\+",   // b
        "\\d\\+",   // n
        "\\d\\+",   // l
        "\\d\\+",   // e
        "\\d\\+",   // c
        "\\d\\+",   // k
        ".",        // t
        ".\\+",     // m
        ".*",       // r
        "[-\t .]*", // p
        "\\d\\+",   // v
        ".\\+",     // s
        ".\\+",     // o
    ];
    // Named field indices into `CONV`/`PATTERNS`/`EfmPattern::addr`.
    const I_F: usize = 0;
    const I_M: usize = 8; // FMT_PATTERN_M
    const I_R: usize = 9; // FMT_PATTERN_R

    fn fmt_index(c: u8) -> Option<usize> {
        CONV.iter().position(|&p| p == c)
    }

    /// One compiled `'errorformat'` part.
    struct EfmPattern {
        prog: VimRegex,
        /// The leading prefix char (`A E W I N C Z G O P Q D X`), `0` if none.
        prefix: u8,
        /// The `%+` / `%-` flag, `0` if none.
        flags: u8,
        /// `%>`: continue matching the *next* line from this pattern.
        conthere: bool,
        /// Field index → 1-based capture-group number (`0` = field absent), in the
        /// `CONV` order. Mirrors vim's `efm_T.addr`.
        addr: [usize; 14],
    }

    /// The full compiled `'errorformat'`: an ordered list of parts.
    pub(crate) struct Errorformat {
        parts: Vec<EfmPattern>,
    }

    impl Errorformat {
        /// Compile an `'errorformat'` string (comma-separated parts). Returns the
        /// `E37x` message vim would emit for a malformed part.
        pub(crate) fn compile(efm: &str) -> Result<Self, String> {
            let bytes = efm.as_bytes();
            let mut parts = Vec::new();
            let mut i = 0;
            while i < bytes.len() {
                let len = part_len(&bytes[i..]);
                if len > 0 {
                    parts.push(EfmPattern::compile(&bytes[i..i + len])?);
                }
                // Skip the comma and any following blanks (vim's
                // `skip_to_option_part`).
                i += len;
                while i < bytes.len() && (bytes[i] == b',' || bytes[i] == b' ') {
                    i += 1;
                }
            }
            if parts.is_empty() {
                return Err("E378: 'errorformat' contains no pattern".to_string());
            }
            Ok(Errorformat { parts })
        }

        /// Parse `lines` into quickfix entries.
        pub(crate) fn parse(&self, lines: &[String]) -> Vec<QfEntry> {
            let mut p = Parser::new(&self.parts);
            for line in lines {
                p.parse_line(line);
            }
            p.entries
        }
    }

    /// Length of one `'errorformat'` part — up to the next unescaped comma (vim's
    /// `efm_option_part_len`).
    fn part_len(efm: &[u8]) -> usize {
        let mut len = 0;
        while len < efm.len() && efm[len] != b',' {
            if efm[len] == b'\\' && len + 1 < efm.len() {
                len += 1;
            }
            len += 1;
        }
        len
    }

    impl EfmPattern {
        fn compile(part: &[u8]) -> Result<Self, String> {
            let (regpat, addr, prefix, flags, conthere) = efm_part_to_regpat(part)?;
            let prog = VimRegex::compile_with(&regpat, PatternKind::String, Engine::Auto)
                .map_err(|e| format!("E383: errorformat regex compile failed: {e}"))?;
            Ok(EfmPattern {
                prog,
                prefix,
                flags,
                conthere,
                addr,
            })
        }
    }

    /// Port of vim's `efm_to_regpat`: convert one `'errorformat'` part to a vim
    /// regex pattern, returning the pattern plus the parsed prefix/flags/addr.
    fn efm_part_to_regpat(part: &[u8]) -> Result<(String, [usize; 14], u8, u8, bool), String> {
        let n = part.len();
        let mut out: Vec<u8> = Vec::with_capacity(n * 4 + 16);
        out.push(b'^');
        let mut addr = [0usize; 14];
        let mut prefix = 0u8;
        let mut flags = 0u8;
        let mut conthere = false;
        let mut round = 0usize;

        let mut i = 0;
        while i < n {
            let c = part[i];
            if c != b'%' {
                // Copy a normal character, escaping regex atoms — and treating a
                // backslash as "take the next char literally" (vim's behavior).
                if c == b'\\' && i + 1 < n {
                    i += 1;
                    out.push(part[i]);
                } else {
                    if matches!(c, b'.' | b'*' | b'^' | b'$' | b'~' | b'[') {
                        out.push(b'\\');
                    }
                    out.push(c);
                }
                i += 1;
                continue;
            }

            // A '%' item.
            i += 1;
            if i >= n {
                return Err("E377: Invalid % in format string".to_string());
            }
            let cv = part[i];
            if let Some(idx) = fmt_index(cv) {
                efmpat_to_regpat(part, i, idx, prefix, &mut addr, &mut round, &mut out)?;
            } else if cv == b'*' {
                i += 1;
                if i >= n {
                    return Err("E375: Unsupported % in format string".to_string());
                }
                i = scanf_fmt_to_regpat(part, i, &mut out)?;
            } else if matches!(cv, b'%' | b'\\' | b'.' | b'^' | b'$' | b'~' | b'[') {
                out.push(cv); // regex magic characters, passed through
            } else if cv == b'#' {
                out.push(b'*');
            } else if cv == b'>' {
                conthere = true;
            } else if i == 1 {
                // A prefix — only valid at the very start of the part.
                i = efm_analyze_prefix(part, i, &mut prefix, &mut flags)?;
            } else {
                return Err(format!("E377: Invalid %%{} in format string", cv as char));
            }
            i += 1;
        }
        out.push(b'$');
        let pat = String::from_utf8(out)
            .map_err(|_| "E383: errorformat produced non-UTF-8 pattern".to_string())?;
        Ok((pat, addr, prefix, flags, conthere))
    }

    /// Port of `efmpat_to_regpat`: expand one field conversion (`part[at]` is the
    /// conversion char, `idx` its `CONV` index) into a `\(…\)` capture group.
    fn efmpat_to_regpat(
        part: &[u8],
        at: usize,
        idx: usize,
        prefix: u8,
        addr: &mut [usize; 14],
        round: &mut usize,
        out: &mut Vec<u8>,
    ) -> Result<(), String> {
        let cv = part[at];
        if addr[idx] != 0 {
            return Err(format!("E372: Too many %%{} in format string", cv as char));
        }
        let dxopq = matches!(prefix, b'D' | b'X' | b'O' | b'P' | b'Q');
        let opq = matches!(prefix, b'O' | b'P' | b'Q');
        if (idx != 0 && idx < I_R && dxopq) || (idx == I_R && !opq) {
            return Err(format!(
                "E373: Unexpected %%{} in format string",
                cv as char
            ));
        }
        *round += 1;
        addr[idx] = *round;
        out.push(b'\\');
        out.push(b'(');
        if cv == b'f' && at + 1 < part.len() {
            // A filename followed by more pattern: greedily-minimal up to the next
            // literal (`.\{-1,}`), or `\f\+` when the next item is `\`/`%`.
            let nxt = part[at + 1];
            if nxt != b'\\' && nxt != b'%' {
                out.extend_from_slice(b".\\{-1,}");
            } else {
                out.extend_from_slice(b"\\f\\+");
            }
        } else {
            out.extend_from_slice(PATTERNS[idx].as_bytes());
        }
        out.push(b'\\');
        out.push(b')');
        Ok(())
    }

    /// Port of `scanf_fmt_to_regpat` for `%*…`: `part[at]` is the char after `*`.
    /// Returns the index of the last consumed byte.
    fn scanf_fmt_to_regpat(part: &[u8], at: usize, out: &mut Vec<u8>) -> Result<usize, String> {
        let n = part.len();
        let mut i = at;
        let c = part[i];
        if c == b'[' {
            out.push(b'['); // %*[^a-z0-9] etc.
            if i + 1 < n && part[i + 1] == b'^' {
                i += 1;
                out.push(part[i]); // '^'
            }
            if i + 1 < n {
                i += 1;
                out.push(part[i]); // could be ']'
                loop {
                    if i + 1 >= n {
                        return Err("E374: Missing ] in format string".to_string());
                    }
                    i += 1;
                    let ch = part[i];
                    out.push(ch);
                    if ch == b']' {
                        break;
                    }
                }
            }
            out.extend_from_slice(b"\\+");
        } else if c == b'\\' {
            out.push(b'\\'); // %*\D, %*\s etc.
            if i + 1 < n {
                i += 1;
                out.push(part[i]);
            }
            out.extend_from_slice(b"\\+");
        } else {
            return Err(format!(
                "E375: Unsupported %%*{} in format string",
                c as char
            ));
        }
        Ok(i)
    }

    /// Port of `efm_analyze_prefix`: read an optional `+`/`-` flag and the prefix
    /// letter starting at `part[at]`. Returns the index of the prefix letter.
    fn efm_analyze_prefix(
        part: &[u8],
        at: usize,
        prefix: &mut u8,
        flags: &mut u8,
    ) -> Result<usize, String> {
        let n = part.len();
        let mut i = at;
        if i < n && matches!(part[i], b'+' | b'-') {
            *flags = part[i];
            i += 1;
        }
        if i < n
            && matches!(
                part[i],
                b'D' | b'X'
                    | b'A'
                    | b'E'
                    | b'W'
                    | b'I'
                    | b'N'
                    | b'C'
                    | b'Z'
                    | b'G'
                    | b'O'
                    | b'P'
                    | b'Q'
            )
        {
            *prefix = part[i];
            Ok(i)
        } else {
            let bad = if i < n { part[i] as char } else { '?' };
            Err(format!("E376: Invalid %%{bad} in format string prefix"))
        }
    }

    // -----------------------------------------------------------------------
    // Line parsing (port of qf_parse_line / qf_parse_match / qf_parse_fmt_*).

    /// Scratch fields filled while parsing one line (vim's `qffields_T`).
    #[derive(Default)]
    struct Fields {
        namebuf: String,
        bnr: i32,
        module: String,
        errmsg: String,
        lnum: usize,
        end_lnum: usize,
        col: usize,
        end_col: usize,
        use_viscol: bool,
        pattern: String,
        enr: i32,
        typ: u8,
        valid: bool,
        /// Byte offset into the line where `%r` started (the "rest"), if matched.
        tail: Option<usize>,
    }

    impl Fields {
        fn reset(&mut self, keep_errmsg: bool) {
            self.namebuf.clear();
            self.bnr = 0;
            self.module.clear();
            self.pattern.clear();
            if !keep_errmsg {
                self.errmsg.clear();
            }
            self.lnum = 0;
            self.end_lnum = 0;
            self.col = 0;
            self.end_col = 0;
            self.use_viscol = false;
            self.enr = -1;
            self.typ = 0;
            self.tail = None;
        }
    }

    struct Parser<'a> {
        parts: &'a [EfmPattern],
        entries: Vec<QfEntry>,
        multiline: bool,
        multiignore: bool,
        multiscan: bool,
        directory: Option<String>,
        dir_stack: Vec<String>,
        currfile: Option<String>,
        file_stack: Vec<String>,
        /// Index of the `%>` pattern to resume from on the next line.
        fmt_start: Option<usize>,
    }

    impl<'a> Parser<'a> {
        fn new(parts: &'a [EfmPattern]) -> Self {
            Parser {
                parts,
                entries: Vec::new(),
                multiline: false,
                multiignore: false,
                multiscan: false,
                directory: None,
                dir_stack: Vec::new(),
                currfile: None,
                file_stack: Vec::new(),
                fmt_start: None,
            }
        }

        fn parse_line(&mut self, line: &str) {
            let mut fields = Fields::default();
            // A line may be re-scanned from a `%r`/`%O%P%Q` tail (vim's
            // `goto restofline`); bound the loop by the line shrinking each pass.
            let mut cur = line.to_string();
            loop {
                match self.parse_one(&cur, &mut fields) {
                    LineStatus::AddEntry => {
                        self.add_entry(&fields);
                        return;
                    }
                    LineStatus::Ignore => return,
                    LineStatus::Rescan(rest) => {
                        if rest.len() >= cur.len() {
                            return; // no progress — drop the line
                        }
                        cur = rest;
                    }
                }
            }
        }

        /// One pass of `qf_parse_line` over `line`.
        fn parse_one(&mut self, line: &str, fields: &mut Fields) -> LineStatus {
            // `%>` resume point, else the first pattern.
            let start = self.fmt_start.take().unwrap_or(0);
            fields.valid = true;

            let mut matched: Option<usize> = None;
            for fi in start..self.parts.len() {
                if self.parse_get_fields(line, &self.parts[fi], fields) {
                    matched = Some(fi);
                    break;
                }
            }
            self.multiscan = false;

            let Some(fi) = matched else {
                // No pattern matched: a plain output line. It still becomes an
                // (invalid) entry so `:copen` shows it.
                self.line_nomatch(line, fields);
                self.multiline = false;
                self.multiignore = false;
                return LineStatus::AddEntry;
            };

            let prefix = self.parts[fi].prefix;
            if prefix == b'D' || prefix == b'X' {
                if let Err(()) = self.parse_dir_pfx(prefix, fields) {
                    return LineStatus::Ignore;
                }
                self.line_nomatch(line, fields);
                return LineStatus::AddEntry;
            }

            if self.parts[fi].conthere {
                self.fmt_start = Some(fi);
            }

            if matches!(prefix, b'A' | b'E' | b'W' | b'I' | b'N') {
                self.multiline = true;
                self.multiignore = false;
            } else if matches!(prefix, b'C' | b'Z') {
                self.parse_multiline_pfx(prefix, fields);
                return LineStatus::Ignore;
            } else if matches!(prefix, b'O' | b'P' | b'Q') {
                if let Some(rest) = self.parse_file_pfx(prefix, fields, line) {
                    return LineStatus::Rescan(rest);
                }
            }

            if self.parts[fi].flags == b'-' {
                if self.multiline {
                    self.multiignore = true;
                }
                return LineStatus::Ignore;
            }
            LineStatus::AddEntry
        }

        /// Run one pattern against `line`, filling `fields` (vim's
        /// `qf_parse_get_fields` + `qf_parse_match`). Returns whether it matched.
        fn parse_get_fields(&self, line: &str, fmt: &EfmPattern, fields: &mut Fields) -> bool {
            if self.multiscan && !matches!(fmt.prefix, b'O' | b'P' | b'Q') {
                return false;
            }
            fields.reset(self.multiscan);

            let m = match fmt.prog.exec_line(line, 0, true) {
                Ok(Some(m)) => m,
                // A no-match or even an engine error means "this pattern doesn't
                // apply"; fall through to the next pattern (fail-soft per line).
                _ => return false,
            };

            // (C/Z) continuation only when already in a multi-line message.
            if matches!(fmt.prefix, b'C' | b'Z') && !self.multiline {
                return false;
            }
            fields.typ = if matches!(fmt.prefix, b'E' | b'W' | b'I' | b'N') {
                fmt.prefix
            } else {
                0
            };

            let sub = |g: usize| m.submatches.get(g).copied().flatten();
            for (i, &g) in fmt.addr.iter().enumerate() {
                if i == I_F {
                    if g > 0 {
                        let Some((s, e)) = sub(g) else { return false };
                        // Filename: literal slice (env expansion is deferred).
                        fields.namebuf = line[s..e].to_string();
                    }
                    continue;
                }
                if i == I_M {
                    if fmt.flags == b'+' && !self.multiscan {
                        fields.errmsg = line.to_string();
                    } else if g > 0 {
                        let Some((s, e)) = sub(g) else { return false };
                        fields.errmsg = line[s..e].to_string();
                    }
                    continue;
                }
                if i == I_R {
                    if g > 0 {
                        let Some((s, _)) = sub(g) else { return false };
                        fields.tail = Some(s);
                    }
                    continue;
                }
                if g == 0 {
                    continue;
                }
                let Some((s, e)) = sub(g) else { return false };
                let text = &line[s..e];
                if !parse_field(CONV[i], text, fields) {
                    return false;
                }
            }
            true
        }

        fn line_nomatch(&mut self, line: &str, fields: &mut Fields) {
            fields.namebuf.clear();
            fields.lnum = 0;
            fields.valid = false;
            fields.errmsg = line.to_string();
        }

        /// `%D` (enter) / `%X` (leave) directory stack maintenance.
        fn parse_dir_pfx(&mut self, idx: u8, fields: &Fields) -> Result<(), ()> {
            if idx == b'D' {
                if fields.namebuf.is_empty() {
                    return Err(()); // E379: missing directory name
                }
                self.directory = Some(push_dir(&fields.namebuf, &mut self.dir_stack));
            } else {
                self.directory = pop_dir(&mut self.dir_stack);
            }
            Ok(())
        }

        /// `%O`/`%P`/`%Q` global-file prefixes. Returns the line tail to re-scan
        /// when there's trailing content (vim's `QF_MULTISCAN`).
        fn parse_file_pfx(&mut self, idx: u8, fields: &mut Fields, line: &str) -> Option<String> {
            // The named file's existence isn't checked (no fs in the core); treat
            // every `%O`/`%P`/`%Q` name as present.
            if idx == b'P' && !fields.namebuf.is_empty() {
                self.currfile = Some(push_dir(&fields.namebuf, &mut self.file_stack));
            } else if idx == b'Q' {
                self.currfile = pop_dir(&mut self.file_stack);
            }
            fields.namebuf.clear();
            if let Some(off) = fields.tail {
                let rest = line[off..].trim_start().to_string();
                if !rest.is_empty() {
                    self.multiscan = true;
                    return Some(rest);
                }
            }
            None
        }

        /// `%C`/`%Z` continuation: fold this line's data into the previous entry.
        fn parse_multiline_pfx(&mut self, idx: u8, fields: &Fields) {
            if !self.multiignore {
                // Resolve before the mutable borrow of `entries` below.
                let resolved = self.resolve_fname(fields);
                if let Some(prev) = self.entries.last_mut() {
                    if !fields.errmsg.is_empty() {
                        prev.text.push('\n');
                        prev.text.push_str(&fields.errmsg);
                    }
                    if prev.nr == -1 {
                        prev.nr = fields.enr;
                    }
                    if fields.typ.is_ascii_graphic() && prev.typ == 0 {
                        prev.typ = fields.typ;
                    }
                    if prev.lnum == 0 {
                        prev.lnum = fields.lnum;
                    }
                    if prev.end_lnum == 0 {
                        prev.end_lnum = fields.end_lnum;
                    }
                    if prev.col == 0 {
                        prev.col = fields.col;
                        prev.vcol = fields.use_viscol;
                    }
                    if prev.end_col == 0 {
                        prev.end_col = fields.end_col;
                    }
                    if prev.filename.is_none() {
                        prev.filename = resolved;
                    }
                }
            }
            if idx == b'Z' {
                self.multiline = false;
                self.multiignore = false;
            }
        }

        /// Resolve the entry's filename against the directory/file stacks, mirroring
        /// `qf_add_entry`'s filename selection + `qf_get_fnum`.
        fn resolve_fname(&self, fields: &Fields) -> Option<String> {
            let raw = if !fields.namebuf.is_empty() || self.directory.is_some() {
                fields.namebuf.as_str()
            } else if fields.valid {
                self.currfile.as_deref().unwrap_or("")
            } else {
                ""
            };
            if raw.is_empty() {
                return None;
            }
            match &self.directory {
                Some(dir) if !Path::new(raw).is_absolute() => Some(format!("{dir}/{raw}")),
                _ => Some(raw.to_string()),
            }
        }

        fn add_entry(&mut self, fields: &Fields) {
            let filename = self.resolve_fname(fields);
            self.entries.push(QfEntry {
                filename,
                bufnr: fields.bnr,
                module: fields.module.clone(),
                lnum: fields.lnum,
                end_lnum: fields.end_lnum,
                col: fields.col,
                end_col: fields.end_col,
                vcol: fields.use_viscol,
                nr: fields.enr,
                pattern: fields.pattern.clone(),
                text: fields.errmsg.clone(),
                typ: fields.typ,
                valid: fields.valid,
            });
        }
    }

    enum LineStatus {
        AddEntry,
        Ignore,
        Rescan(String),
    }

    /// Extract a single numeric/char/pattern field from its matched `text` (vim's
    /// `qf_parse_fmt_*`). Returns `false` to reject the whole match.
    fn parse_field(conv: u8, text: &str, fields: &mut Fields) -> bool {
        match conv {
            b'b' => fields.bnr = atoi(text),
            b'n' => fields.enr = atoi(text),
            b'l' => fields.lnum = atoi(text) as usize,
            b'e' => fields.end_lnum = atoi(text) as usize,
            b'c' => fields.col = atoi(text) as usize,
            b'k' => fields.end_col = atoi(text) as usize,
            b't' => fields.typ = text.bytes().next().unwrap_or(0),
            b'v' => {
                fields.col = atoi(text) as usize;
                fields.use_viscol = true;
            }
            b'p' => {
                // The pointer line's screen column: count chars, expanding tabs to
                // the next multiple of 8.
                let mut col = 0usize;
                for b in text.bytes() {
                    col += 1;
                    if b == b'\t' {
                        col += 7;
                        col -= col % 8;
                    }
                }
                fields.col = col + 1;
                fields.use_viscol = true;
            }
            b's' => {
                // A literal search pattern: `^\V…\$`.
                let mut p = String::from("^\\V");
                p.push_str(text);
                p.push_str("\\$");
                fields.pattern = p;
            }
            b'o' => fields.module.push_str(text),
            _ => {}
        }
        true
    }

    /// Leading-integer parse (vim's `atol`): stops at the first non-digit.
    fn atoi(s: &str) -> i32 {
        let s = s.trim_start();
        let (neg, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let mut n: i64 = 0;
        for b in digits.bytes() {
            if !b.is_ascii_digit() {
                break;
            }
            n = n * 10 + i64::from(b - b'0');
            if n > i64::from(i32::MAX) {
                n = i64::from(i32::MAX);
                break;
            }
        }
        let n = if neg { -n } else { n };
        n as i32
    }

    /// Push `dir` onto the directory/file stack, resolving a relative entry under
    /// the current top (a simplification of vim's `qf_push_dir`). Returns the new
    /// top.
    fn push_dir(dir: &str, stack: &mut Vec<String>) -> String {
        let resolved = match stack.last() {
            Some(top) if !Path::new(dir).is_absolute() => format!("{top}/{dir}"),
            _ => dir.to_string(),
        };
        stack.push(resolved.clone());
        resolved
    }

    /// Pop the top of the stack, returning the new top (vim's `qf_pop_dir`).
    fn pop_dir(stack: &mut Vec<String>) -> Option<String> {
        stack.pop();
        stack.last().cloned()
    }
}
