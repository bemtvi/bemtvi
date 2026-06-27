//! The `:set` command and its bool/number option-application helpers.

use super::*;
use crate::encoding::Encoding;
use crate::options::{
    resolve_set, split_set_args, NumOp, OptScope, RegexSyntax, SetCmd, SetOp, StrOp, WindowOptions,
};

/// Number of decimal digits in `n` (at least 1, so `0` is one digit).
fn digit_count(n: usize) -> usize {
    let mut n = n;
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

impl Editor {
    /// Handle `:set {options}` (and `:setlocal`, which is identical here — the
    /// buffer-local options apply to the current buffer either way). Each
    /// whitespace-separated token is a boolean option with the usual `no`/`inv`
    /// prefixes and `!`/`?` suffixes (`:set number`, `:set nonu`, `:set rnu!`) or
    /// a number option with `=value` / `?` (`:set tabstop=4`, `:set ts?`).
    pub(crate) fn ex_set(&mut self, args: &str) {
        for tok in split_set_args(args) {
            match resolve_set(&tok) {
                Some(SetCmd::Bool { name, op }) => self.apply_set_bool(name, op),
                Some(SetCmd::Num { name, op }) => self.apply_set_num(name, op),
                Some(SetCmd::Str { name, op }) => self.apply_set_str(name, op),
                None => self.echo(format!("E518: Unknown option: {tok}")),
            }
        }
    }

    /// Handle `:setf[iletype] {ft}` — force the current buffer's filetype, i.e.
    /// its treesitter language. Equivalent to `:set filetype={ft}`; an empty
    /// argument is an error (vim's `:setf` requires one), unlike `:set ft=`.
    pub(crate) fn ex_setfiletype(&mut self, args: &str) {
        let ft = args.trim();
        if ft.is_empty() {
            self.echo("E471: Argument required".to_string());
            return;
        }
        let buf = self.current_buffer_id();
        self.ts_start(buf, ft.to_string());
    }

    /// Apply one resolved boolean `:set` operation. `number` / `relativenumber`
    /// are window-local (they live on the focused window); `expandtab` is
    /// buffer-local (on the current buffer); the rest are global search options on
    /// the editor.
    fn apply_set_bool(&mut self, name: &str, op: SetOp) {
        // `ts_highlight` is the buffer-local *whether-treesitter-paints* noun —
        // orthogonal to `filetype` (the language). It lives in the per-buffer
        // enable map (`set_ts_highlight`), not a plain `options` bool slot, so it
        // can drop/restore the engine parse; handle it before the slot match.
        if name == "ts_highlight" {
            let buf = self.current_buffer_id();
            match op {
                SetOp::On => self.set_ts_highlight(buf, true),
                SetOp::Off => self.set_ts_highlight(buf, false),
                SetOp::Toggle => {
                    let on = self.ts_highlight_enabled(buf);
                    self.set_ts_highlight(buf, !on);
                }
                SetOp::Query => {
                    let label = if self.ts_highlight_enabled(buf) {
                        "ts_highlight"
                    } else {
                        "nots_highlight"
                    };
                    self.echo(label.to_string());
                }
            }
            return;
        }
        // Global boolean options route through the shared setter (which writes the global
        // *base* layer and recomputes the workspace overlay on top), so a `:set` of a
        // workspace-overridden option updates the base without clobbering the override — the
        // `:set` and `vim.o` paths share one home, exactly like the numeric/string globals.
        if let Some((canon, _, OptScope::Global)) = crate::options::option_meta(name) {
            // `option_meta` is the whole catalog; only the boolean globals belong here (the
            // numeric/string ones are handled by `apply_set_num`/`apply_set_str`). Match the
            // wired boolean globals so a non-bool global falls through to the slot E518 path.
            if matches!(
                canon,
                "ignorecase"
                    | "smartcase"
                    | "wrapscan"
                    | "hlsearch"
                    | "incsearch"
                    | "autoread"
                    | "imagepreview"
                    | "timeout"
                    | "scrollanim"
                    | "qfdock"
                    | "bdclosetab"
                    | "relative_splits"
                    | "relative_docks"
                    | "equalalways"
            ) {
                let cur = matches!(
                    self.options.get_scalar(canon),
                    Some(crate::options::OptionScalar::Bool(true))
                );
                match op {
                    SetOp::On => self.set_global_option_bool(canon, true),
                    SetOp::Off => self.set_global_option_bool(canon, false),
                    SetOp::Toggle => self.set_global_option_bool(canon, !cur),
                    SetOp::Query => {
                        let label = if cur {
                            canon.to_string()
                        } else {
                            format!("no{canon}")
                        };
                        self.echo(label);
                    }
                }
                return;
            }
        }
        let slot = match name {
            "number" => &mut self.windows.cur_mut().options.number,
            "relativenumber" => &mut self.windows.cur_mut().options.relativenumber,
            "cursorline" => &mut self.windows.cur_mut().options.cursorline,
            "foldenable" => &mut self.windows.cur_mut().options.foldenable,
            "wrap" => &mut self.windows.cur_mut().options.wrap,
            "breakindent" => &mut self.windows.cur_mut().options.breakindent,
            "expandtab" => &mut self.buffer_mut().options.expandtab,
            "autoindent" => &mut self.buffer_mut().options.autoindent,
            "smartindent" => &mut self.buffer_mut().options.smartindent,
            "autopairs" => &mut self.buffer_mut().options.autopairs,
            "bomb" => &mut self.buffer_mut().options.bomb,
            "modifiable" => &mut self.buffer_mut().options.modifiable,
            // A name `resolve_set` accepted as a boolean but no arm above handles is a
            // wiring gap (an option in the `canonical` registry never wired to its
            // slot — the bug `:set imagepreview` was). Fail loud rather than silently
            // no-op, so the next such gap surfaces the moment it's `:set`.
            _ => {
                self.echo(format!("E518: Unknown option: {name}"));
                return;
            }
        };
        match op {
            SetOp::On => *slot = true,
            SetOp::Off => *slot = false,
            SetOp::Toggle => *slot = !*slot,
            SetOp::Query => {
                let label = if *slot {
                    name.to_string()
                } else {
                    format!("no{name}")
                };
                self.echo(label);
            }
        }
    }

    /// Apply one resolved number `:set` operation. The indentation options
    /// (`tabstop` / `shiftwidth` / `softtabstop`) are buffer-local; the horizontal-
    /// scroll governors (`sidescroll` / `sidescrolloff`) are window-local (they live
    /// on the focused window). The assigned value is range-checked per option (vim's
    /// `E487`): `tabstop ≥ 1`, `shiftwidth ≥ 0`, `softtabstop ≥ -1`, the scroll
    /// options `≥ 0`.
    fn apply_set_num(&mut self, name: &str, op: NumOp) {
        match op {
            NumOp::Set(v) => {
                // `showtabline` / `laststatus` are the global numeric options;
                // route them through the shared setter so the `:set` and `vim.o`
                // paths validate, echo, and relayout identically.
                if name == "showtabline"
                    || name == "laststatus"
                    || name == "mousetime"
                    || name == "timeoutlen"
                    || name == "scrollanimduration"
                    || name == "scrollback"
                    || name == "history"
                {
                    self.set_global_option_num(name, v);
                    return;
                }
                let min = match name {
                    "tabstop" | "numberwidth" | "foldnestmax" => 1,
                    "shiftwidth" | "sidescroll" | "sidescrolloff" | "foldcolumn" | "foldlevel"
                    | "foldminlines" => 0,
                    "softtabstop" => -1,
                    // A wiring gap (see `apply_set_bool`): a numeric option `resolve_set`
                    // accepted but no arm handles. Fail loud, never a silent no-op.
                    _ => {
                        self.echo(format!("E518: Unknown option: {name}"));
                        return;
                    }
                };
                if v < min {
                    self.echo(format!("E487: Argument must be positive: {name}={v}"));
                    return;
                }
                match name {
                    "sidescroll" => self.windows.cur_mut().options.sidescroll = v as usize,
                    "sidescrolloff" => self.windows.cur_mut().options.sidescrolloff = v as usize,
                    "numberwidth" => self.windows.cur_mut().options.numberwidth = v as usize,
                    "foldcolumn" => self.windows.cur_mut().options.foldcolumn = v as usize,
                    // `foldlevel` re-derives which *computed* folds display closed; route
                    // it through the dedicated setter so the `:set` and `vim.wo` paths
                    // re-fold identically.
                    "foldlevel" => self.set_foldlevel(v as usize),
                    _ => {
                        let opts = &mut self.buffer_mut().options;
                        match name {
                            "tabstop" => opts.tabstop = v as usize,
                            "shiftwidth" => opts.shiftwidth = v as usize,
                            "softtabstop" => opts.softtabstop = v as isize,
                            // Structural knobs for computed folds; a recompute below
                            // picks up the new value.
                            "foldnestmax" => opts.foldnestmax = v as usize,
                            "foldminlines" => opts.foldminlines = v as usize,
                            _ => {}
                        }
                        // The indent-fold structure depends on these — rebuild it.
                        self.refresh_folds();
                    }
                }
            }
            NumOp::Query => {
                let v: i64 = match name {
                    "sidescroll" => self.windows.cur().options.sidescroll as i64,
                    "sidescrolloff" => self.windows.cur().options.sidescrolloff as i64,
                    "numberwidth" => self.windows.cur().options.numberwidth as i64,
                    "foldcolumn" => self.windows.cur().options.foldcolumn as i64,
                    "foldlevel" => self.windows.cur().options.foldlevel as i64,
                    "showtabline" => self.options.showtabline as i64,
                    "laststatus" => self.options.laststatus as i64,
                    "mousetime" => self.options.mousetime as i64,
                    "timeoutlen" => self.options.timeoutlen as i64,
                    "scrollanimduration" => self.options.scrollanimduration as i64,
                    "scrollback" => self.options.scrollback as i64,
                    "history" => self.options.history as i64,
                    _ => {
                        let opts = &self.buffer().options;
                        match name {
                            "tabstop" => opts.tabstop as i64,
                            "shiftwidth" => opts.shiftwidth as i64,
                            "softtabstop" => opts.softtabstop as i64,
                            "foldnestmax" => opts.foldnestmax as i64,
                            "foldminlines" => opts.foldminlines as i64,
                            // A wiring gap (see `apply_set_bool`): fail loud, not silent.
                            _ => {
                                self.echo(format!("E518: Unknown option: {name}"));
                                return;
                            }
                        }
                    }
                };
                self.echo(format!("{name}={v}"));
            }
        }
    }

    /// Apply one resolved string `:set` operation. The string options are the
    /// global `statusline` / `tabline` / `guifont` and the mouse strings
    /// (`mouse` / `mousemodel` / `mousescroll`); each routes through the shared
    /// [`Editor::set_global_option_str`] setter so the `:set` and `vim.o` paths
    /// share one home. `&` resets to the default (empty); `?` echoes the value.
    fn apply_set_str(&mut self, name: &str, op: StrOp) {
        // `filetype` is buffer-local and special: it drives the per-buffer
        // treesitter language override (the same seam as `nx.bo.filetype`), not a
        // global string slot. This is the no-Lua way to force a
        // language onto a buffer the extension table misses — e.g. on the web
        // build, where there is no Lua at all.
        if name == "filetype" {
            let buf = self.current_buffer_id();
            match op {
                // `filetype` is the *language* noun: set it (`""` = no filetype),
                // reset it to the extension default, or query it. Whether
                // treesitter actually paints is the orthogonal `ts_highlight` noun
                // (see `apply_set_bool`), not this.
                StrOp::Set(value) => self.set_filetype(buf, &value),
                // `:set ft&` resets to the default, which in nxvim is the
                // extension-derived filetype (more useful than vim's literal "").
                StrOp::Reset => self.reset_filetype(buf),
                // `:set ft?` echoes the *effective* filetype (override or extension).
                StrOp::Query => {
                    let ft = self.buffer_filetype(buf).unwrap_or_default();
                    self.echo(format!("filetype={ft}"));
                }
            }
            return;
        }
        // `commentstring` is buffer-local: the comment template `gc`/`gcc` wrap
        // lines with. Stored as a per-buffer override (empty clears it, falling
        // back to the filetype default); `?` echoes the *effective* value (override
        // or filetype default), `&` clears the override.
        if name == "commentstring" {
            let buf = self.current_buffer_id();
            match op {
                StrOp::Set(value) => self.set_commentstring(buf, &value),
                StrOp::Reset => self.set_commentstring(buf, ""),
                StrOp::Query => {
                    let cs = self.effective_commentstring(buf);
                    self.echo(format!("commentstring={cs}"));
                }
            }
            return;
        }
        // `signcolumn` is the (first) window-local enumerated string: `no`,
        // `auto`/`auto:min-max`, `yes`/`yes:n`/`yes:min-max`. A bad value (or the
        // not-yet-supported `number`) fails loud (E474) rather than silently
        // mis-setting the gutter. `&` resets to the `auto` default.
        if name == "signcolumn" {
            match op {
                StrOp::Set(value) => match crate::options::SignColumn::parse(&value) {
                    Some(scl) => self.windows.cur_mut().options.signcolumn = scl,
                    None => {
                        self.echo(format!("E474: Invalid argument: signcolumn={value}"));
                    }
                },
                StrOp::Reset => {
                    self.windows.cur_mut().options.signcolumn =
                        crate::options::SignColumn::Auto { min: 1, max: 1 }
                }
                StrOp::Query => {
                    let scl = self.windows.cur().options.signcolumn;
                    self.echo(format!("signcolumn={scl}"));
                }
            }
            return;
        }
        // `regexsyntax` is an enumerated string: only `"pcre"`/`"vim"` are valid,
        // and a bad value must fail loud (E474) rather than silently sticking the
        // buffer on the wrong dialect. `:set`/`:setlocal` set the *buffer-local*
        // override (like `tabstop`); `&` resets it to follow the global. (The
        // global itself is set via `vim.o.regexsyntax`.)
        if name == "regexsyntax" {
            match op {
                StrOp::Set(value) => {
                    let choice = match value.as_str() {
                        "pcre" => RegexSyntax::Pcre,
                        "vim" => RegexSyntax::Vim,
                        _ => {
                            self.echo(format!("E474: Invalid argument: regexsyntax={value}"));
                            return;
                        }
                    };
                    self.buffer_mut().options.regexsyntax = choice;
                }
                StrOp::Reset => self.buffer_mut().options.regexsyntax = RegexSyntax::Inherit,
                StrOp::Query => self.echo(format!("regexsyntax={}", self.effective_regexsyntax())),
            }
            return;
        }
        // `fileencoding` is buffer-local and enumerated: only a real encoding
        // label (or empty, meaning UTF-8) is valid, and a bad value must fail
        // loud (E474) rather than silently sticking the buffer on the wrong
        // charset. Changing it implies the next write re-encodes, so it marks the
        // buffer modified (vim does the same). `&` resets to UTF-8.
        if name == "fileencoding" {
            match op {
                StrOp::Set(value) => {
                    let enc = if value.is_empty() {
                        Encoding::UTF8
                    } else {
                        match Encoding::from_label(&value) {
                            Some(e) => e,
                            None => {
                                self.echo(format!("E474: Invalid argument: fileencoding={value}"));
                                return;
                            }
                        }
                    };
                    let changed = self.buffer().options.fileencoding != enc;
                    self.buffer_mut().options.fileencoding = enc;
                    if changed {
                        self.buffer_mut().modified = true;
                    }
                }
                StrOp::Reset => self.buffer_mut().options.fileencoding = Encoding::UTF8,
                StrOp::Query => {
                    let enc = self.buffer().options.fileencoding;
                    self.echo(format!("fileencoding={enc}"));
                }
            }
            return;
        }
        // `fileformat` is buffer-local and enumerated (unix/dos/mac); a bad value fails
        // loud (E474). Changing it implies the next write re-converts the line endings, so
        // it marks the buffer modified (vim does the same). `&` resets to unix.
        if name == "fileformat" {
            use crate::options::FileFormat;
            match op {
                StrOp::Set(value) => {
                    let ff = match FileFormat::from_label(&value) {
                        Some(f) => f,
                        None => {
                            self.echo(format!("E474: Invalid argument: fileformat={value}"));
                            return;
                        }
                    };
                    let changed = self.buffer().options.fileformat != ff;
                    self.buffer_mut().options.fileformat = ff;
                    if changed {
                        self.buffer_mut().modified = true;
                    }
                }
                StrOp::Reset => self.buffer_mut().options.fileformat = FileFormat::Unix,
                StrOp::Query => {
                    let ff = self.buffer().options.fileformat;
                    self.echo(format!("fileformat={ff}"));
                }
            }
            return;
        }
        // `foldmethod` is buffer-local and enumerated. `manual`/`indent` apply;
        // every other vim name fails loud (E474 for a non-vim value, an explicit
        // "not supported yet" for a real-but-unimplemented method like `expr`) —
        // never a silent no-op that leaves folding looking broken. Changing it
        // rebuilds the fold structure for the new source. `&` resets to `manual`.
        if name == "foldmethod" {
            use crate::options::{FoldMethod, FoldMethodErr};
            match op {
                StrOp::Set(value) => match FoldMethod::from_label(&value) {
                    Ok(fdm) => {
                        self.buffer_mut().options.foldmethod = fdm;
                        self.refresh_folds();
                    }
                    Err(FoldMethodErr::Unknown) => {
                        self.echo(format!("E474: Invalid argument: foldmethod={value}"));
                    }
                    Err(FoldMethodErr::Unimplemented) => {
                        self.echo(format!("foldmethod={value} is not supported yet"));
                    }
                },
                StrOp::Reset => {
                    self.buffer_mut().options.foldmethod = FoldMethod::Manual;
                    self.refresh_folds();
                }
                StrOp::Query => {
                    let fdm = self.buffer().options.foldmethod;
                    self.echo(format!("foldmethod={fdm}"));
                }
            }
            return;
        }
        // `foldexpr` is the buffer-local expression `foldmethod=expr` folds by,
        // stored beside `commentstring` (a per-buffer string, not a `Copy`
        // `BufferOptions` slot). `set_foldexpr` warns loud for a non-tree-sitter
        // expr (Phase 5) and rebuilds the structure. `&` clears it.
        if name == "foldexpr" {
            match op {
                StrOp::Set(value) => self.set_foldexpr(&value),
                StrOp::Reset => self.set_foldexpr(""),
                StrOp::Query => {
                    let fde = self.foldexpr().to_string();
                    self.echo(format!("foldexpr={fde}"));
                }
            }
            return;
        }
        // `foldmarker` is the buffer-local `start,end` delimiter pair `foldmethod=marker`
        // folds by, stored beside `foldexpr` (a per-buffer string pair, not a `Copy`
        // `BufferOptions` slot). The value must be exactly two distinct, non-empty
        // comma-separated markers — anything else fails loud (E474) rather than leave
        // an unusable marker set. `&` resets to vim's default `{{{`/`}}}`.
        if name == "foldmarker" {
            match op {
                StrOp::Set(value) => {
                    let parts: Vec<&str> = value.split(',').collect();
                    if parts.len() != 2
                        || parts[0].is_empty()
                        || parts[1].is_empty()
                        || parts[0] == parts[1]
                    {
                        self.echo(format!("E474: Invalid argument: foldmarker={value}"));
                        return;
                    }
                    self.set_foldmarker(parts[0], parts[1]);
                }
                StrOp::Reset => self.reset_foldmarker(),
                StrOp::Query => {
                    let (open, close) = self.effective_foldmarker();
                    self.echo(format!("foldmarker={open},{close}"));
                }
            }
            return;
        }
        // `fileencodings` is the global read-detection list: every comma-separated
        // entry must be `ucs-bom` or a known encoding label, else fail loud rather
        // than leave an unusable list. The store/echo go through the shared global
        // setter so `:set` and `vim.o` agree.
        if name == "fileencodings" {
            match op {
                StrOp::Set(value) => {
                    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
                        if !crate::encoding::is_fileencodings_entry(entry) {
                            self.echo(format!("E474: Invalid argument: fileencodings={value}"));
                            return;
                        }
                    }
                    self.set_global_option_str("fileencodings", &value);
                }
                // Reset to the built-in default (kept in sync with `Options::default`).
                // Route through the setter so the base layer / workspace overlay stay
                // consistent (a bare `self.options` write would be reverted by recompute).
                StrOp::Reset => {
                    self.set_global_option_str("fileencodings", "ucs-bom,utf-8,latin1");
                }
                StrOp::Query => self.echo(format!("fileencodings={}", self.options.fileencodings)),
            }
            return;
        }
        // `errorformat` resets to the compiled-in default (not the empty string,
        // which would leave the quickfix parser with no pattern — E378).
        if name == "errorformat" {
            match op {
                StrOp::Set(value) => {
                    self.set_global_option_str("errorformat", &value);
                }
                StrOp::Reset => {
                    let dflt = crate::options::DFLT_EFM.to_string();
                    self.set_global_option_str("errorformat", &dflt);
                }
                StrOp::Query => self.echo(format!("errorformat={}", self.options.errorformat)),
            }
            return;
        }
        // `persisthistory` is validated on the strict `:set` path — a typo is E474,
        // never a silently-stored bad value (the `nx.o` compat path stores any string,
        // and the parser ignores unknown tokens).
        if name == "persisthistory" {
            match op {
                StrOp::Set(value) => {
                    if crate::options::valid_persisthistory(&value) {
                        self.set_global_option_str("persisthistory", &value);
                    } else {
                        self.echo(format!("E474: Invalid argument: persisthistory={value}"));
                    }
                }
                StrOp::Reset => {
                    self.set_global_option_str("persisthistory", "workspace,global");
                }
                StrOp::Query => {
                    self.echo(format!("persisthistory={}", self.options.persisthistory))
                }
            }
            return;
        }
        // `showbreak` / `breakindentopt` are window-local (like `wrap`), so they live
        // on the focused window's options rather than a global string slot.
        // `:set sbr=↪` sets the marker; `:set briopt=sbr` aligns it within the indent.
        if name == "showbreak" {
            match op {
                StrOp::Set(value) => self.windows.cur_mut().options.showbreak = value,
                StrOp::Reset => self.windows.cur_mut().options.showbreak.clear(),
                StrOp::Query => {
                    let v = self.windows.cur().options.showbreak.clone();
                    self.echo(format!("showbreak={v}"));
                }
            }
            return;
        }
        if name == "breakindentopt" {
            match op {
                StrOp::Set(value) => self.windows.cur_mut().options.breakindentopt = value,
                StrOp::Reset => self.windows.cur_mut().options.breakindentopt.clear(),
                StrOp::Query => {
                    let v = self.windows.cur().options.breakindentopt.clone();
                    self.echo(format!("breakindentopt={v}"));
                }
            }
            return;
        }
        // `fillchars` is window-local (like `showbreak`): the `key:char` list
        // choosing structural fill characters. nxvim honors only `eob` (the
        // end-of-buffer `~` filler) today, but the whole value is validated so a
        // bad entry fails loud (E474) rather than silently sticking the window on a
        // junk value. `&` resets to the default look (empty ⇒ `eob:~`).
        if name == "fillchars" {
            match op {
                StrOp::Set(value) => {
                    if crate::options::parse_fillchars(&value).is_none() {
                        self.echo(format!("E474: Invalid argument: fillchars={value}"));
                        return;
                    }
                    self.windows.cur_mut().options.fillchars = value;
                }
                StrOp::Reset => self.windows.cur_mut().options.fillchars.clear(),
                StrOp::Query => {
                    let v = self.windows.cur().options.fillchars.clone();
                    self.echo(format!("fillchars={v}"));
                }
            }
            return;
        }
        // `padding` is window-local (nxvim's own; no vim equivalent): a CSS-style
        // shorthand for the per-side blank margin around this window's content box.
        // The whole value is validated so a bad token fails loud (E474) rather than
        // silently sticking the window on junk. `&` resets to no margin. A change
        // re-clamps the viewport (the text area grew/shrank).
        if name == "padding" {
            match op {
                StrOp::Set(value) => {
                    let Some(pad) = crate::options::parse_padding(&value) else {
                        self.echo(format!("E474: Invalid argument: padding={value}"));
                        return;
                    };
                    self.windows.cur_mut().options.padding = pad;
                    self.ensure_visible();
                }
                StrOp::Reset => {
                    self.windows.cur_mut().options.padding = crate::options::Padding::default();
                    self.ensure_visible();
                }
                StrOp::Query => {
                    let v = self.windows.cur().options.padding;
                    self.echo(format!("padding={v}"));
                }
            }
            return;
        }
        // A wiring gap (see `apply_set_bool`): a string option `resolve_set` accepted
        // but no arm / the global setter handles. Fail loud, never a silent no-op.
        let unknown = |ed: &mut Self| ed.echo(format!("E518: Unknown option: {name}"));
        match op {
            StrOp::Set(value) => {
                if !self.set_global_option_str(name, &value) {
                    unknown(self);
                }
            }
            // Most string options reset to the empty string; the `:make`/`:grep`
            // programs and the grep parser reset to their compiled-in defaults (an
            // empty value would make `:make` spawn nothing / leave the parser with
            // no pattern), mirroring the `errorformat` reset above.
            StrOp::Reset => {
                let value = match name {
                    "makeprg" => "make",
                    "grepprg" => "grep -n $* /dev/null",
                    "grepformat" => crate::options::DFLT_GREPFORMAT,
                    _ => "",
                };
                if !self.set_global_option_str(name, value) {
                    unknown(self);
                }
            }
            StrOp::Query => {
                let value = match name {
                    "statusline" => self.options.statusline.clone(),
                    "tabline" => self.options.tabline.clone(),
                    "guifont" => self.options.guifont.clone(),
                    "mouse" => self.options.mouse.clone(),
                    "mousemodel" => self.options.mousemodel.clone(),
                    "mousescroll" => self.options.mousescroll.clone(),
                    "switchbuf" => self.options.switchbuf.clone(),
                    "makeprg" => self.options.makeprg.clone(),
                    "grepprg" => self.options.grepprg.clone(),
                    "grepformat" => self.options.grepformat.clone(),
                    _ => {
                        unknown(self);
                        return;
                    }
                };
                self.echo(format!("{name}={value}"));
            }
        }
    }

    /// The number-gutter width for a window with window-local `opts` showing a
    /// buffer with `line_count` lines: `0` when both number options are off, else
    /// at least 4 cells, widening to fit the largest line number plus one trailing
    /// space. Sized per window so each gutter fits its own buffer and options.
    pub(crate) fn number_width_for(&self, opts: &WindowOptions, line_count: usize) -> usize {
        if !opts.number && !opts.relativenumber {
            return 0;
        }
        (digit_count(line_count) + 1).max(opts.numberwidth.max(1))
    }
}
