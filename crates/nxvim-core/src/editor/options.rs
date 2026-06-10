//! The `:set` command and its bool/number option-application helpers.

use super::*;
use crate::options::{
    resolve_set, split_set_args, NumOp, RegexSyntax, SetCmd, SetOp, StrOp, WindowOptions,
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
        let slot = match name {
            "number" => &mut self.windows.cur_mut().options.number,
            "relativenumber" => &mut self.windows.cur_mut().options.relativenumber,
            "ignorecase" => &mut self.options.ignorecase,
            "smartcase" => &mut self.options.smartcase,
            "wrapscan" => &mut self.options.wrapscan,
            "hlsearch" => &mut self.options.hlsearch,
            "incsearch" => &mut self.options.incsearch,
            "expandtab" => &mut self.buffer_mut().options.expandtab,
            _ => return,
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
                if name == "showtabline" || name == "laststatus" || name == "mousetime" {
                    self.set_global_option_num(name, v);
                    return;
                }
                let min = match name {
                    "tabstop" => 1,
                    "shiftwidth" | "sidescroll" | "sidescrolloff" => 0,
                    "softtabstop" => -1,
                    _ => return,
                };
                if v < min {
                    self.echo(format!("E487: Argument must be positive: {name}={v}"));
                    return;
                }
                match name {
                    "sidescroll" => self.windows.cur_mut().options.sidescroll = v as usize,
                    "sidescrolloff" => self.windows.cur_mut().options.sidescrolloff = v as usize,
                    _ => {
                        let opts = &mut self.buffer_mut().options;
                        match name {
                            "tabstop" => opts.tabstop = v as usize,
                            "shiftwidth" => opts.shiftwidth = v as usize,
                            "softtabstop" => opts.softtabstop = v as isize,
                            _ => {}
                        }
                    }
                }
            }
            NumOp::Query => {
                let v: i64 = match name {
                    "sidescroll" => self.windows.cur().options.sidescroll as i64,
                    "sidescrolloff" => self.windows.cur().options.sidescrolloff as i64,
                    "showtabline" => self.options.showtabline as i64,
                    "laststatus" => self.options.laststatus as i64,
                    "mousetime" => self.options.mousetime as i64,
                    _ => {
                        let opts = &self.buffer().options;
                        match name {
                            "tabstop" => opts.tabstop as i64,
                            "shiftwidth" => opts.shiftwidth as i64,
                            "softtabstop" => opts.softtabstop as i64,
                            _ => return,
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
        // treesitter language override (the same seam as `vim.treesitter.start`/
        // `stop`), not a global string slot. This is the no-Lua way to force a
        // language onto a buffer the extension table misses — e.g. on the web
        // build, where there is no Lua at all.
        if name == "filetype" {
            let buf = self.current_buffer_id();
            match op {
                // `:set ft=` to empty is vim's "no filetype → no syntax": an
                // explicit off, like `vim.treesitter.stop`.
                StrOp::Set(value) if value.is_empty() => self.ts_stop(buf),
                StrOp::Set(value) => self.ts_start(buf, value),
                // `:set ft&` resets to the default, which in nxvim is the
                // extension-derived language (more useful than vim's literal "").
                StrOp::Reset => self.ts_reset(buf),
                // `:set ft?` echoes the *effective* filetype (override or extension).
                StrOp::Query => {
                    let ft = self.ts_language_for(buf).unwrap_or_default();
                    self.echo(format!("filetype={ft}"));
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
        match op {
            StrOp::Set(value) => self.set_global_option_str(name, &value),
            StrOp::Reset => self.set_global_option_str(name, ""),
            StrOp::Query => {
                let value = match name {
                    "statusline" => self.options.statusline.clone(),
                    "tabline" => self.options.tabline.clone(),
                    "guifont" => self.options.guifont.clone(),
                    "mouse" => self.options.mouse.clone(),
                    "mousemodel" => self.options.mousemodel.clone(),
                    "mousescroll" => self.options.mousescroll.clone(),
                    _ => return,
                };
                self.echo(format!("{name}={value}"));
            }
        }
    }

    /// The number-gutter width for a window with window-local `opts` showing a
    /// buffer with `line_count` lines: `0` when both number options are off, else
    /// at least 4 cells, widening to fit the largest line number plus one trailing
    /// space. Sized per window so each gutter fits its own buffer and options.
    pub(crate) fn number_width_for(&self, opts: WindowOptions, line_count: usize) -> usize {
        if !opts.number && !opts.relativenumber {
            return 0;
        }
        (digit_count(line_count) + 1).max(4)
    }
}
