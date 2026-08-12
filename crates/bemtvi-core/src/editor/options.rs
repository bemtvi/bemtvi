//! The `:set` command and its bool/number option-application helpers.

use super::*;
use crate::encoding::Encoding;
use crate::options::{
    resolve_set, split_set_args, BufferOptions, NumOp, OptKind, OptScope, RegexSyntax, SetCmd,
    SetOp, SetScope, StrOp, WindowOptions,
};

/// The buffer-local options whose value the **read** (or the buffer's identity) decides,
/// so they carry no global tier: the encoding trio detected from the bytes, and the
/// `modifiable` marker the read-only scratch listings set at creation. `:set`/`:setlocal`
/// write them on the current buffer as always; `:setglobal` on one fails loud rather than
/// storing a value nothing would ever read. Mirrors the buffer-born list in
/// [`BufferOptions::inherit_settable`].
///
/// These are the [`BufferOptions`] *slots* among the tier-less names; the classification
/// itself lives once in [`crate::options::has_global_tier`], which the Lua surfaces read
/// too, so this is a `debug_assert`-checked view of it rather than a second list.
fn is_buffer_born(name: &str) -> bool {
    let born = matches!(
        name,
        "fileencoding" | "bomb" | "fileformat" | "endofline" | "modifiable"
    );
    debug_assert!(
        !born || !crate::options::has_global_tier(name),
        "{name} is buffer-born here but `has_global_tier` says it has a tier"
    );
    born
}

/// A numeric buffer-local option's read/write pair against a [`BufferOptions`], so the
/// same accessor reaches either tier. Values cross as `i64` because the slots differ in
/// type (`softtabstop` is an `isize`, for its `-1` "follow shiftwidth" sentinel).
type BufNumSlot = (fn(&BufferOptions) -> i64, fn(&mut BufferOptions, i64));

/// The window twin of [`BufNumSlot`] — a numeric window-local option's read/write pair
/// against a [`WindowOptions`], so one accessor reaches either tier.
type WinNumSlot = (fn(&WindowOptions) -> i64, fn(&mut WindowOptions, i64));

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
    /// Handle `:set {options}` and its scoped siblings `:setlocal` / `:setglobal`. Each
    /// whitespace-separated token is a boolean option with the usual `no`/`inv`
    /// prefixes and `!`/`?` suffixes (`:set number`, `:set nonu`, `:set rnu!`) or
    /// a number option with `=value` / `?` (`:set tabstop=4`, `:set ts?`).
    ///
    /// `scope` picks which tier of a **buffer-local** option the write lands on (vim's
    /// global-local model — [`SetScope`]): `:set` writes the global value new buffers are
    /// born from *and* the current buffer's own, `:setlocal` only the buffer's,
    /// `:setglobal` only the global. Global-scope options have one value and ignore it.
    pub(crate) fn ex_set(&mut self, args: &str, scope: SetScope) {
        for tok in split_set_args(args) {
            match resolve_set(&tok) {
                Some(SetCmd::Bool { name, op }) => self.apply_set_bool(name, op, scope),
                Some(SetCmd::Num { name, op }) => self.apply_set_num(name, op, scope),
                Some(SetCmd::Str { name, op }) => self.apply_set_str(name, op, scope),
                None => self.echo(format!("E518: Unknown option: {tok}")),
            }
        }
    }

    /// Reject a `:setglobal` of an option that has no global value to write, naming the
    /// option and why — never a silent store into a tier nothing reads. Returns whether
    /// the caller should stop.
    ///
    /// What lands here is exactly what [`has_global_tier`](crate::options::has_global_tier)
    /// answers `false` for: the **buffer-born** options ([`is_buffer_born`]), decided by
    /// the read or by the buffer's identity, plus the two nouns derived per buffer
    /// (`filetype`, `ts_highlight`). Everything else — every window option, and the
    /// map-backed `commentstring` / `foldexpr` / `foldmarker` — has a real tier.
    fn reject_scopeless_global(&mut self, name: &str, scope: SetScope, why: &str) -> bool {
        if scope != SetScope::Global {
            return false;
        }
        self.echo(format!("E5100: {name} has no global value ({why})"));
        true
    }

    /// Apply one buffer-local option write to the tiers this command word targets: the
    /// current buffer's own options, the global values a new buffer is born from, or both.
    /// `write` runs once per tier with the same value, so `:set` can never leave the two
    /// disagreeing. Used by the enumerated-string options, whose value is parsed (and
    /// rejected) by the caller before it gets here; the boolean and numeric paths inline
    /// the same two-tier write because they also resolve toggles and range checks.
    fn write_buf_opt_str(&mut self, scope: SetScope, write: impl Fn(&mut BufferOptions)) {
        if scope.writes_local() {
            write(&mut self.buffer_mut().options);
        }
        if scope.writes_global() {
            write(&mut self.buf_opts_global);
        }
    }

    /// Set the **global value** of a boolean buffer-local option — the `:setglobal` tier
    /// every newly created buffer is born from — from outside the editor (the Lua
    /// `vim.o` / `vim.go` / `vim.opt_global` bridge). Routed through the very machinery
    /// `:setglobal` uses, so the two surfaces share one home: the same name resolution,
    /// and the same loud rejection for an option whose value the read decides.
    pub fn set_buf_global_option_bool(&mut self, name: &str, value: bool) {
        let op = if value { SetOp::On } else { SetOp::Off };
        self.apply_set_bool(name, op, SetScope::Global);
    }

    /// The numeric twin of [`Editor::set_buf_global_option_bool`] (`tabstop`,
    /// `shiftwidth`, `softtabstop`, `foldnestmax`, `foldminlines`). Out-of-range values
    /// fail loud exactly as on the `:setglobal` path.
    pub fn set_buf_global_option_num(&mut self, name: &str, value: i64) {
        self.apply_set_num(name, NumOp::Set(value), SetScope::Global);
    }

    /// The string twin of [`Editor::set_buf_global_option_bool`] (`regexsyntax`,
    /// `foldmethod`). An invalid value fails loud (`E474`) as on the `:setglobal` path.
    pub fn set_buf_global_option_str(&mut self, name: &str, value: &str) {
        self.apply_set_str(name, StrOp::Set(value.to_string()), SetScope::Global);
    }

    /// The global values of the buffer-local options — the tier a new buffer is born
    /// from — for the server to mirror to Lua (`vim.go` / `vim.opt_global` reads).
    pub fn buf_opts_global(&self) -> &BufferOptions {
        &self.buf_opts_global
    }

    /// Apply one `'regexsyntax'` write to the tiers this command word targets. It is the
    /// one genuinely **global-local** option in the vim sense: a buffer either pins a
    /// dialect or holds [`RegexSyntax::Inherit`], meaning "follow the global value" — and
    /// that global value is the editor-wide [`Options::regexsyntax`](crate::Options),
    /// which every inheriting buffer already resolves through. So the tier here is that
    /// option, not a separate `buf_opts_global` slot (which would be a *second* global
    /// nothing resolves against, disagreeing with `vim.o.regexsyntax`).
    ///
    /// `label` is the validated `"pcre"` / `"vim"` spelling for the global write; `choice`
    /// the parsed value for the buffer's own slot.
    fn write_regexsyntax(&mut self, scope: SetScope, choice: RegexSyntax, label: &str) {
        if scope.writes_local() {
            self.buffer_mut().options.regexsyntax = choice;
        }
        if scope.writes_global() {
            self.set_global_option_str("regexsyntax", label);
        }
    }

    /// Apply one window-local option write to the tiers this command word targets: the
    /// focused window, the global values a source-less window is born from, or both. The
    /// window twin of [`Editor::write_buf_opt_str`].
    fn write_win_opt(&mut self, scope: SetScope, write: impl Fn(&mut WindowOptions)) {
        if scope.writes_local() {
            write(&mut self.windows.cur_mut().options);
        }
        if scope.writes_global() {
            write(&mut self.win_opts_global);
        }
    }

    /// The window options a `:set`-family **query** reads: the global values for
    /// `:setglobal x?`, the focused window's for everything else.
    fn win_opts_queried(&mut self, scope: SetScope) -> &mut WindowOptions {
        if scope == SetScope::Global {
            &mut self.win_opts_global
        } else {
            &mut self.windows.cur_mut().options
        }
    }

    /// The global values of the window-local options, for the server to mirror to Lua
    /// (`vim.go` / `vim.opt_global` reads) and to seed a source-less window from.
    pub fn win_opts_global(&self) -> &WindowOptions {
        &self.win_opts_global
    }

    /// Set the **global value** of a window-local option from outside the editor (the Lua
    /// `vim.go` / `vim.opt_global` bridge), the window twin of
    /// [`Editor::set_buf_global_option_bool`]. Routed through the `:setglobal` machinery,
    /// so both surfaces share one home.
    pub fn set_win_global_option_bool(&mut self, name: &str, value: bool) {
        let op = if value { SetOp::On } else { SetOp::Off };
        self.apply_set_bool(name, op, SetScope::Global);
    }

    /// The numeric twin of [`Editor::set_win_global_option_bool`].
    pub fn set_win_global_option_num(&mut self, name: &str, value: i64) {
        self.apply_set_num(name, NumOp::Set(value), SetScope::Global);
    }

    /// The string twin of [`Editor::set_win_global_option_bool`].
    pub fn set_win_global_option_str(&mut self, name: &str, value: &str) {
        self.apply_set_str(name, StrOp::Set(value.to_string()), SetScope::Global);
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
    fn apply_set_bool(&mut self, name: &str, op: SetOp, scope: SetScope) {
        // `ts_highlight` is the buffer-local *whether-treesitter-paints* noun —
        // orthogonal to `filetype` (the language). It lives in the per-buffer
        // enable map (`set_ts_highlight`), not a plain `options` bool slot, so it
        // can drop/restore the engine parse; handle it before the slot match.
        if name == "ts_highlight" {
            if self.reject_scopeless_global(name, scope, "it is a per-buffer engine state") {
                return;
            }
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
        // A boolean global is identified by the catalog's own kind+scope classification
        // (`Bool` + `Global`), not a hand-kept name list — so this can never drift from the
        // catalog / `Options::set_scalar`. A non-bool global (numeric/string) doesn't match
        // and falls through to `apply_set_num`/`apply_set_str`; a window/buffer bool falls
        // through to the slot match below.
        if let Some((canon, OptKind::Bool, OptScope::Global)) = crate::options::option_meta(name) {
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
        // The boolean **buffer-local** options: each lives in a `BufferOptions` slot, so
        // one accessor reaches it in either tier — the current buffer's own options, or
        // the global values a new buffer is born from.
        let buf_slot: Option<fn(&mut BufferOptions) -> &mut bool> = match name {
            "expandtab" => Some(|o| &mut o.expandtab),
            "autoindent" => Some(|o| &mut o.autoindent),
            "smartindent" => Some(|o| &mut o.smartindent),
            "autopairs" => Some(|o| &mut o.autopairs),
            "indentemptylines" => Some(|o| &mut o.indentemptylines),
            "bomb" => Some(|o| &mut o.bomb),
            "endofline" => Some(|o| &mut o.endofline),
            "fixendofline" => Some(|o| &mut o.fixendofline),
            "modifiable" => Some(|o| &mut o.modifiable),
            _ => None,
        };
        if let Some(slot) = buf_slot {
            if is_buffer_born(name)
                && self.reject_scopeless_global(name, scope, "the read decides it per buffer")
            {
                return;
            }
            // `:setglobal x?` reads the tier; every other query reads what this buffer
            // actually uses.
            if op == SetOp::Query {
                let on = if scope == SetScope::Global {
                    *slot(&mut self.buf_opts_global)
                } else {
                    *slot(&mut self.buffer_mut().options)
                };
                let label = if on {
                    name.to_string()
                } else {
                    format!("no{name}")
                };
                self.echo(label);
                return;
            }
            // A toggle flips whichever tier this command word reads, then BOTH tiers a
            // `:set` writes take that one value — so `:set invexpandtab` can't leave the
            // global and the buffer disagreeing about what it just toggled to.
            let current = if scope == SetScope::Global {
                *slot(&mut self.buf_opts_global)
            } else {
                *slot(&mut self.buffer_mut().options)
            };
            let value = match op {
                SetOp::On => true,
                SetOp::Off => false,
                SetOp::Toggle => !current,
                SetOp::Query => unreachable!("handled above"),
            };
            if scope.writes_local() {
                *slot(&mut self.buffer_mut().options) = value;
            }
            // A buffer-born option has no tier to write; under a plain `:set` that half is
            // simply absent (a `:setglobal` of one was rejected above).
            if scope.writes_global() && !is_buffer_born(name) {
                *slot(&mut self.buf_opts_global) = value;
            }
            return;
        }
        // The boolean **window-local** options, reachable in either tier through one
        // accessor exactly like the buffer booleans above. A new window still copies the
        // window it came from (vim, and what `Editor::split` does), so the tier is not a
        // per-split seed — it is what `:setglobal` reads/writes and what seeds a window
        // minted with no source to copy (a dock, the quickfix tab).
        let win_slot: Option<fn(&mut WindowOptions) -> &mut bool> = match name {
            "number" => Some(|o| &mut o.number),
            "relativenumber" => Some(|o| &mut o.relativenumber),
            "cursorline" => Some(|o| &mut o.cursorline),
            "foldenable" => Some(|o| &mut o.foldenable),
            "wrap" => Some(|o| &mut o.wrap),
            "breakindent" => Some(|o| &mut o.breakindent),
            _ => None,
        };
        let Some(win_slot) = win_slot else {
            // A name `resolve_set` accepted as a boolean but no arm above handles is a
            // wiring gap (an option in the `canonical` registry never wired to its
            // slot — the bug `:set imagepreview` was). Fail loud rather than silently
            // no-op, so the next such gap surfaces the moment it's `:set`.
            self.echo(format!("E518: Unknown option: {name}"));
            return;
        };
        if op == SetOp::Query {
            let on = *win_slot(self.win_opts_queried(scope));
            let label = if on {
                name.to_string()
            } else {
                format!("no{name}")
            };
            self.echo(label);
            return;
        }
        let current = *win_slot(self.win_opts_queried(scope));
        let value = match op {
            SetOp::On => true,
            SetOp::Off => false,
            SetOp::Toggle => !current,
            SetOp::Query => unreachable!("handled above"),
        };
        self.write_win_opt(scope, |o| *win_slot(o) = value);
    }

    /// Apply one resolved number `:set` operation. The indentation options
    /// (`tabstop` / `shiftwidth` / `softtabstop`) are buffer-local; the horizontal-
    /// scroll governors (`sidescroll` / `sidescrolloff`) are window-local (they live
    /// on the focused window). The assigned value is range-checked per option (vim's
    /// `E487`): `tabstop ≥ 1`, `shiftwidth ≥ 0`, `softtabstop ≥ -1`, the scroll
    /// options `≥ 0`.
    fn apply_set_num(&mut self, name: &str, op: NumOp, scope: SetScope) {
        // The numeric **buffer-local** options, reachable in either tier through one
        // accessor pair (see `apply_set_bool`). All five are settable — none is
        // read-derived — so a `:set tabstop=3` in a config reaches every file opened
        // afterwards.
        let buf_num: Option<BufNumSlot> = match name {
            "tabstop" => Some((|o| o.tabstop as i64, |o, v| o.tabstop = v as usize)),
            "shiftwidth" => Some((|o| o.shiftwidth as i64, |o, v| o.shiftwidth = v as usize)),
            "softtabstop" => Some((|o| o.softtabstop as i64, |o, v| o.softtabstop = v as isize)),
            "foldnestmax" => Some((|o| o.foldnestmax as i64, |o, v| o.foldnestmax = v as usize)),
            "foldminlines" => Some((
                |o| o.foldminlines as i64,
                |o, v| o.foldminlines = v as usize,
            )),
            _ => None,
        };
        if let Some((get, set)) = buf_num {
            match op {
                NumOp::Set(v) => {
                    let min = if name == "softtabstop" {
                        -1
                    } else if name == "shiftwidth" || name == "foldminlines" {
                        0
                    } else {
                        1
                    };
                    if v < min {
                        self.echo(format!("E487: Argument must be positive: {name}={v}"));
                        return;
                    }
                    if scope.writes_local() {
                        set(&mut self.buffer_mut().options, v);
                    }
                    if scope.writes_global() {
                        set(&mut self.buf_opts_global, v);
                    }
                    // The indent-fold structure depends on these — rebuild it. (A pure
                    // `:setglobal` changed no open buffer, so there is nothing to redo.)
                    if scope.writes_local() {
                        self.refresh_folds();
                    }
                }
                NumOp::Query => {
                    let v = if scope == SetScope::Global {
                        get(&self.buf_opts_global)
                    } else {
                        get(&self.buffer().options)
                    };
                    self.echo(format!("{name}={v}"));
                }
            }
            return;
        }
        // The numeric **window-local** options, reachable in either tier through one
        // accessor pair, exactly like the buffer numerics above.
        let win_num: Option<WinNumSlot> = match name {
            "sidescroll" => Some((|o| o.sidescroll as i64, |o, v| o.sidescroll = v as usize)),
            "sidescrolloff" => Some((
                |o| o.sidescrolloff as i64,
                |o, v| o.sidescrolloff = v as usize,
            )),
            "scrolloff" => Some((|o| o.scrolloff as i64, |o, v| o.scrolloff = v as usize)),
            "numberwidth" => Some((|o| o.numberwidth as i64, |o, v| o.numberwidth = v as usize)),
            "foldcolumn" => Some((|o| o.foldcolumn as i64, |o, v| o.foldcolumn = v as usize)),
            "foldlevel" => Some((|o| o.foldlevel as i64, |o, v| o.foldlevel = v as usize)),
            _ => None,
        };
        if let Some((get, set)) = win_num {
            match op {
                NumOp::Set(v) => {
                    let min = if name == "numberwidth" { 1 } else { 0 };
                    if v < min {
                        self.echo(format!("E487: Argument must be positive: {name}={v}"));
                        return;
                    }
                    if scope.writes_global() {
                        set(&mut self.win_opts_global, v);
                    }
                    if scope.writes_local() {
                        // `foldlevel` re-derives which *computed* folds display closed;
                        // route the window's own value through the dedicated setter so
                        // the `:set` and `vim.wo` paths re-fold identically.
                        if name == "foldlevel" {
                            self.set_foldlevel(v as usize);
                        } else {
                            set(&mut self.windows.cur_mut().options, v);
                        }
                    }
                }
                NumOp::Query => {
                    let v = get(self.win_opts_queried(scope));
                    self.echo(format!("{name}={v}"));
                }
            }
            return;
        }
        match op {
            NumOp::Set(v) => {
                // The global numeric options route through the shared setter so the
                // `:set` and `vim.o` paths validate, echo, and relayout identically.
                // Identified by the catalog's `Num` + `Global` classification, not a
                // hand-kept name list, so this can't drift from the catalog.
                if matches!(
                    crate::options::option_meta(name),
                    Some((_, OptKind::Num, OptScope::Global))
                ) {
                    self.set_global_option_num(name, v);
                    return;
                }
                // A wiring gap (see `apply_set_bool`): a numeric option `resolve_set`
                // accepted but neither the global setter nor an arm above handles.
                // Fail loud, never a silent no-op.
                self.echo(format!("E518: Unknown option: {name}"));
            }
            NumOp::Query => {
                // Global numerics read through the shared scalar accessor (catalog-driven),
                // so the readout can't drift from `set_scalar`'s name→field map; window /
                // buffer locals (which `get_scalar` doesn't know) fall through below.
                if let Some(crate::options::OptionScalar::Num(v)) = self.options.get_scalar(name) {
                    self.echo(format!("{name}={v}"));
                    return;
                }
                // A wiring gap (see `apply_set_bool`): fail loud, not silent.
                self.echo(format!("E518: Unknown option: {name}"));
            }
        }
    }

    /// Apply one resolved string `:set` operation. The string options are the
    /// global `statusline` / `tabline` / `guifont` and the mouse strings
    /// (`mouse` / `mousemodel` / `mousescroll`); each routes through the shared
    /// [`Editor::set_global_option_str`] setter so the `:set` and `vim.o` paths
    /// share one home. `&` resets to the default (empty); `?` echoes the value.
    fn apply_set_str(&mut self, name: &str, op: StrOp, scope: SetScope) {
        // `filetype` is buffer-local and special: it drives the per-buffer
        // treesitter language override (the same seam as `btv.bo.filetype`), not a
        // global string slot. This is the no-Lua way to force a
        // language onto a buffer the extension table misses — e.g. on the web
        // build, where there is no Lua at all.
        if name == "filetype" {
            if self.reject_scopeless_global(name, scope, "it is derived per buffer") {
                return;
            }
            let buf = self.current_buffer_id();
            match op {
                // `filetype` is the *language* noun: set it (`""` = no filetype),
                // reset it to the extension default, or query it. Whether
                // treesitter actually paints is the orthogonal `ts_highlight` noun
                // (see `apply_set_bool`), not this.
                StrOp::Set(value) => self.set_filetype(buf, &value),
                // `:set ft&` resets to the default, which in bemtvi is the
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
                StrOp::Set(value) => {
                    if scope.writes_local() {
                        self.set_commentstring(buf, &value);
                    }
                    if scope.writes_global() {
                        self.set_commentstring_global(&value);
                    }
                }
                StrOp::Reset => {
                    if scope.writes_local() {
                        self.set_commentstring(buf, "");
                    }
                    if scope.writes_global() {
                        self.set_commentstring_global("");
                    }
                }
                // `:setglobal cms?` reads the global value (empty ⇒ none set, so every
                // buffer falls through to its filetype default); a plain `?` reads what
                // `gc` actually wraps this buffer with.
                StrOp::Query if scope == SetScope::Global => {
                    let cs = self.commentstring_global().to_string();
                    self.echo(format!("commentstring={cs}"));
                }
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
                    Some(scl) => self.write_win_opt(scope, |o| o.signcolumn = scl),
                    None => {
                        self.echo(format!("E474: Invalid argument: signcolumn={value}"));
                    }
                },
                StrOp::Reset => self.write_win_opt(scope, |o| {
                    o.signcolumn = crate::options::SignColumn::Auto { min: 1, max: 1 }
                }),
                StrOp::Query => {
                    let scl = self.win_opts_queried(scope).signcolumn;
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
                    self.write_regexsyntax(scope, choice, &value);
                }
                StrOp::Reset => self.write_regexsyntax(scope, RegexSyntax::Inherit, "pcre"),
                // `:setglobal rxs?` reads the editor-wide dialect a buffer with no
                // override of its own follows — the same value `:set rxs?` resolves
                // through for the current buffer.
                StrOp::Query if scope == SetScope::Global => {
                    let rs = self.options.regexsyntax.clone();
                    self.echo(format!("regexsyntax={rs}"));
                }
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
            if self.reject_scopeless_global(name, scope, "the read decides it per buffer") {
                return;
            }
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
            if self.reject_scopeless_global(name, scope, "the read decides it per buffer") {
                return;
            }
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
                        self.write_buf_opt_str(scope, |o| o.foldmethod = fdm);
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
                    self.write_buf_opt_str(scope, |o| o.foldmethod = FoldMethod::Manual);
                    self.refresh_folds();
                }
                StrOp::Query => {
                    let fdm = if scope == SetScope::Global {
                        self.buf_opts_global.foldmethod
                    } else {
                        self.buffer().options.foldmethod
                    };
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
                StrOp::Set(value) => {
                    if scope.writes_local() {
                        self.set_foldexpr(&value);
                    }
                    if scope.writes_global() {
                        self.set_foldexpr_global(&value);
                    }
                }
                StrOp::Reset => {
                    if scope.writes_local() {
                        self.set_foldexpr("");
                    }
                    if scope.writes_global() {
                        self.set_foldexpr_global("");
                    }
                }
                StrOp::Query if scope == SetScope::Global => {
                    let fde = self.foldexpr_global().to_string();
                    self.echo(format!("foldexpr={fde}"));
                }
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
                    if scope.writes_local() {
                        self.set_foldmarker(parts[0], parts[1]);
                    }
                    if scope.writes_global() {
                        self.set_foldmarker_global(parts[0], parts[1]);
                    }
                }
                StrOp::Reset => {
                    if scope.writes_local() {
                        self.reset_foldmarker();
                    }
                    if scope.writes_global() {
                        self.reset_foldmarker_global();
                    }
                }
                StrOp::Query if scope == SetScope::Global => {
                    let (open, close) = self.foldmarker_global();
                    self.echo(format!("foldmarker={open},{close}"));
                }
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
        // never a silently-stored bad value (the `btv.o` compat path stores any string,
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
        // The verbatim window-local string options, living on the focused window's
        // options rather than a global string slot (like `wrap`):
        // - `showbreak` (`:set sbr=↪`) — the wrapped-line marker; `breakindentopt`
        //   (`:set briopt=sbr`) aligns it within the indent.
        // - `colorcolumn` — the raw comma-separated ruler-column list, stored
        //   verbatim (unmodeled `+N`/`-N` and junk entries are simply skipped when
        //   the projection resolves the columns — matching vim, which ignores bad
        //   entries rather than erroring).
        let win_str_slot: Option<fn(&mut WindowOptions) -> &mut String> = match name {
            "showbreak" => Some(|o| &mut o.showbreak),
            "breakindentopt" => Some(|o| &mut o.breakindentopt),
            "colorcolumn" => Some(|o| &mut o.colorcolumn),
            // Stored raw and parsed to a `WinHl` at projection time (like `colorcolumn`);
            // malformed pairs are dropped there, so there is nothing to reject here.
            "winhighlight" => Some(|o| &mut o.winhighlight),
            _ => None,
        };
        if let Some(slot) = win_str_slot {
            self.set_win_str(name, op, scope, slot);
            return;
        }
        // `fillchars` is window-local (like `showbreak`): the `key:char` list
        // choosing structural fill characters. bemtvi honors only `eob` (the
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
                    self.write_win_opt(scope, |o| o.fillchars = value.clone());
                }
                StrOp::Reset => self.write_win_opt(scope, |o| o.fillchars.clear()),
                StrOp::Query => {
                    let v = self.win_opts_queried(scope).fillchars.clone();
                    self.echo(format!("fillchars={v}"));
                }
            }
            return;
        }
        // `padding` is window-local (bemtvi's own; no vim equivalent): a CSS-style
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
                    self.write_win_opt(scope, |o| o.padding = pad);
                    // The focused window's content box grew or shrank — re-clamp its
                    // viewport. (A pure `:setglobal` touched no live window.)
                    if scope.writes_local() {
                        self.ensure_visible();
                    }
                }
                StrOp::Reset => {
                    self.write_win_opt(scope, |o| o.padding = crate::options::Padding::default());
                    if scope.writes_local() {
                        self.ensure_visible();
                    }
                }
                StrOp::Query => {
                    let v = self.win_opts_queried(scope).padding;
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
                // The plain string globals read through the shared scalar accessor
                // (catalog-driven, so it can't drift from `set_scalar`); the special-cased
                // strings (filetype, fileencodings, errorformat, …) already returned above.
                match self.options.get_scalar(name) {
                    Some(crate::options::OptionScalar::Str(value)) => {
                        self.echo(format!("{name}={value}"))
                    }
                    _ => unknown(self),
                }
            }
        }
    }

    /// Apply a `:set` op to a **verbatim** window-local string option: `set`
    /// stores the value unvalidated in the focused window's `slot`, `&` clears it
    /// (the empty-string default), `?` echoes `name=value`. Validated window-local
    /// strings (`fillchars`, `padding`) keep their own arms — they must fail loud
    /// on a bad value instead of storing it.
    fn set_win_str(
        &mut self,
        name: &str,
        op: StrOp,
        scope: SetScope,
        slot: fn(&mut WindowOptions) -> &mut String,
    ) {
        match op {
            StrOp::Set(value) => self.write_win_opt(scope, |o| *slot(o) = value.clone()),
            StrOp::Reset => self.write_win_opt(scope, |o| slot(o).clear()),
            StrOp::Query => {
                let v = slot(self.win_opts_queried(scope)).clone();
                self.echo(format!("{name}={v}"));
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
