//! Editor options (`:set ...`), the rust-native analogue of neovim's
//! `option.c`. Kept deliberately small for now — only the options nxvim
//! actually honors live here, and they grow alongside the features that read
//! them.

/// Global editor options that affect editing and search. Window-local rendering
/// options (the number gutter) live on [`WindowOptions`]; per-buffer ones on
/// [`BufferOptions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Ignore case when searching (`/`, `?`, `n`, `N`).
    pub ignorecase: bool,
    /// Override [`Options::ignorecase`] for a pattern that contains an uppercase
    /// character, making such a search case-sensitive. Only consulted when
    /// `ignorecase` is on (vim's `smartcase`).
    pub smartcase: bool,
    /// Wrap searches around the ends of the buffer (vim's `wrapscan`). When off,
    /// a forward search past the last match fails with `E385` rather than
    /// continuing from the top (and `E384` for backward).
    pub wrapscan: bool,
    /// Highlight all matches of the last search pattern. (Honored in a later
    /// phase; stored here so `:set` accepts it now.)
    pub hlsearch: bool,
    /// Preview the match incrementally while typing the search. (Honored in a
    /// later phase; stored here so `:set` accepts it now.)
    pub incsearch: bool,
    /// When to draw the tabline: `0` never, `1` only with more than one tab
    /// (vim's default), `2` always. Gates both the projected `Vec<TabView>` and
    /// the top-row reservation in [`crate::Editor`]'s relayout.
    pub showtabline: u8,
    /// The `'statusline'` format string (neovim's `%`-format mini-language).
    /// Empty means the built-in default look; a non-empty value is parsed and
    /// rendered by the statusline engine. Global-only for now (no per-window
    /// override). The one wired string-valued global option.
    pub statusline: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            // Search defaults match modern neovim: case-sensitive unless asked
            // otherwise, but wrapping, highlighting, and incremental preview on.
            ignorecase: false,
            smartcase: false,
            wrapscan: true,
            hlsearch: true,
            incsearch: true,
            // Show the tabline only when more than one tab is open (vim's default).
            showtabline: 1,
            // No custom statusline by default — the built-in look is used.
            statusline: String::new(),
        }
    }
}

/// Window-local options, the rust-native analogue of neovim's per-window scope.
/// Unlike [`Options`] (one global copy on the editor), a [`WindowOptions`] lives
/// on each window, so two windows onto the *same* buffer can show different
/// line-number gutters. A split inherits these from the window it splits off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowOptions {
    /// Show the absolute line number in the number column.
    pub number: bool,
    /// Show line numbers relative to the cursor line. Combined with
    /// [`WindowOptions::number`] this gives vim's "hybrid" gutter: the absolute
    /// number on the cursor line, relative numbers elsewhere.
    pub relativenumber: bool,
    /// Minimum number of columns to scroll horizontally when the cursor moves off
    /// a `nowrap` window's edge (vim's `sidescroll`). `0` recenters the cursor;
    /// `1` (the default) scrolls just enough to keep it at the edge.
    pub sidescroll: usize,
    /// Minimum number of columns to keep between the cursor and the left/right
    /// edge while horizontally scrolling (vim's `sidescrolloff`). `0` by default —
    /// the cursor may sit on the very edge.
    pub sidescrolloff: usize,
}

impl Default for WindowOptions {
    fn default() -> Self {
        // nxvim ships with the hybrid number column on: the cursor line shows its
        // document line number, every other line shows its distance from the
        // cursor.
        WindowOptions {
            number: true,
            relativenumber: true,
            // neovim's horizontal-scroll defaults: scroll a minimal step, no margin.
            sidescroll: 1,
            sidescrolloff: 0,
        }
    }
}

/// Buffer-local options, the rust-native analogue of neovim's per-buffer scope.
/// Unlike [`Options`] (one global copy on the editor), a [`BufferOptions`] lives
/// on each [`crate::Buffer`], so two buffers can indent differently. These are
/// the indentation options nxvim honors today; they grow alongside the features
/// that read them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BufferOptions {
    /// Number of screen cells a `\t` occupies, and the grid the cursor snaps to
    /// over tabs (vim's `tabstop`). Drives how the buffer renders and how
    /// horizontal motion crosses a tab. nxvim defaults to **4** (not vim's 8):
    /// it's the lone knob the other indent options follow.
    pub tabstop: usize,
    /// Indent width for shift commands and autoindent (vim's `shiftwidth`). `0`
    /// means "follow [`tabstop`]" (vim's documented sentinel), which is nxvim's
    /// default — so indent width tracks `tabstop` until set. Accepted by
    /// `:set`/`vim.bo` now; the shift operators (`>>`/`<<`) that consume it
    /// directly are a later phase, but it already feeds [`effective_softtabstop`]
    /// and the LSP indent width (`get_effective_tabstop`).
    ///
    /// [`tabstop`]: BufferOptions::tabstop
    /// [`effective_softtabstop`]: BufferOptions::effective_softtabstop
    pub shiftwidth: usize,
    /// Columns a `<Tab>` keypress moves while editing (vim's `softtabstop`),
    /// distinct from [`tabstop`] (the *display* width of a real tab). `0` turns
    /// the feature off (a Tab uses `tabstop`); `-1` means "follow
    /// [`shiftwidth`]", which is nxvim's default — so it chains
    /// `softtabstop → shiftwidth → tabstop`. Resolve it with
    /// [`effective_softtabstop`].
    ///
    /// [`tabstop`]: BufferOptions::tabstop
    /// [`shiftwidth`]: BufferOptions::shiftwidth
    /// [`effective_softtabstop`]: BufferOptions::effective_softtabstop
    pub softtabstop: isize,
    /// Insert spaces instead of a `\t` when <Tab> is pressed in insert mode
    /// (vim's `expandtab`). The inserted run reaches the next tab boundary
    /// ([`effective_softtabstop`]).
    ///
    /// [`effective_softtabstop`]: BufferOptions::effective_softtabstop
    pub expandtab: bool,
}

impl Default for BufferOptions {
    fn default() -> Self {
        // nxvim's modern defaults: a 4-cell tab, with shiftwidth and softtabstop
        // following it via their sentinels (0 = follow tabstop, -1 = follow
        // shiftwidth), so the single `tabstop` knob sets the whole indent width.
        BufferOptions {
            tabstop: 4,
            shiftwidth: 0,
            softtabstop: -1,
            expandtab: false,
        }
    }
}

impl BufferOptions {
    /// `tabstop`, floored at 1 so a degenerate `0` never divides by zero.
    pub fn effective_tabstop(&self) -> usize {
        self.tabstop.max(1)
    }

    /// Resolve `shiftwidth`'s "follow tabstop" sentinel: `0` → [`effective_tabstop`].
    ///
    /// [`effective_tabstop`]: BufferOptions::effective_tabstop
    pub fn effective_shiftwidth(&self) -> usize {
        if self.shiftwidth == 0 {
            self.effective_tabstop()
        } else {
            self.shiftwidth
        }
    }

    /// The width a `<Tab>` keypress advances by, resolving the
    /// `softtabstop → shiftwidth → tabstop` chain: `softtabstop < 0` follows
    /// [`effective_shiftwidth`]; `softtabstop == 0` (feature off) uses
    /// [`effective_tabstop`]; a positive `softtabstop` is used as-is.
    ///
    /// [`effective_shiftwidth`]: BufferOptions::effective_shiftwidth
    /// [`effective_tabstop`]: BufferOptions::effective_tabstop
    pub fn effective_softtabstop(&self) -> usize {
        match self.softtabstop.cmp(&0) {
            std::cmp::Ordering::Less => self.effective_shiftwidth(),
            std::cmp::Ordering::Equal => self.effective_tabstop(),
            std::cmp::Ordering::Greater => self.softtabstop as usize,
        }
    }
}

/// What a `:set` token does to a boolean option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    On,
    Off,
    Toggle,
    Query,
}

/// What a `:set` token does to a number-valued option (e.g. `tabstop=4`,
/// `tabstop?`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumOp {
    /// `name=value` — assign the parsed value. Signed so `softtabstop`'s `-1`
    /// "follow shiftwidth" sentinel parses; the editor validates per option.
    Set(i64),
    /// `name` / `name?` — echo the current value.
    Query,
}

/// What a `:set` token does to a string-valued option (e.g. `statusline=%f`,
/// `statusline?`, `statusline&`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StrOp {
    /// `name=value` — assign the (already unescaped) value.
    Set(String),
    /// `name` / `name?` — echo the current value.
    Query,
    /// `name&` — reset to the option's default (empty for `statusline`).
    Reset,
}

/// A single resolved `:set` token: which canonical option, and the operation —
/// boolean toggles ([`SetOp`]), numeric assignments ([`NumOp`]), or string
/// assignments ([`StrOp`]) — depending on the option's kind. Not `Copy` since
/// [`SetCmd::Str`] owns its value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetCmd {
    Bool { name: &'static str, op: SetOp },
    Num { name: &'static str, op: NumOp },
    Str { name: &'static str, op: StrOp },
}

/// Whether a canonical option carries a boolean, a number, or a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptKind {
    Bool,
    Num,
    Str,
}

/// Split a `:set` argument string into tokens on **unescaped** whitespace,
/// unescaping `\<char>` to `<char>` as it goes — vim's rule, so a value with
/// spaces (`statusline=%f\ %l`) stays one token with the spaces intact, and a
/// literal backslash is written `\\`. For an argument with no backslashes this
/// is exactly `split_whitespace` (each run of spaces/tabs separates tokens, with
/// no empty tokens), so existing `:set number rnu ts=4` parsing is unchanged.
pub(crate) fn split_set_args(args: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut cur = String::new();
    let mut had_char = false; // distinguishes "" (no token) from a real empty token
    let mut chars = args.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // A backslash escapes the next char (kept literally); a trailing
                // backslash is dropped, matching vim.
                if let Some(n) = chars.next() {
                    cur.push(n);
                    had_char = true;
                }
            }
            c if c.is_whitespace() => {
                if had_char {
                    tokens.push(std::mem::take(&mut cur));
                    had_char = false;
                }
            }
            c => {
                cur.push(c);
                had_char = true;
            }
        }
    }
    if had_char {
        tokens.push(cur);
    }
    tokens
}

/// Resolve a single `:set` token (e.g. `number`, `nonu`, `rnu!`, `invnumber`,
/// `number?`, `tabstop=4`, `ts?`) into the canonical option name and the
/// operation it requests. Returns `None` for an unknown option, a malformed
/// value, or a prefix/suffix used on the wrong kind (e.g. `notabstop`,
/// `noexpandtab=2`).
///
/// The canonical name is resolved *before* the `no`/`inv` prefixes are tried,
/// so a real option name that happens to start with `no` (none yet, but vim has
/// them) is never mis-parsed as a negation.
pub fn resolve_set(tok: &str) -> Option<SetCmd> {
    // `name=value`: valid for a number or string option.
    if let Some((name, value)) = tok.split_once('=') {
        let (name, kind) = canonical(name)?;
        return match kind {
            OptKind::Num => {
                let value = value.trim().parse().ok()?;
                Some(SetCmd::Num {
                    name,
                    op: NumOp::Set(value),
                })
            }
            // The value reaches here already unescaped by the tokenizer (`\ ` →
            // space), so a statusline's spaces are intact. Kept verbatim (not
            // trimmed): leading/trailing spaces in a statusline are meaningful.
            OptKind::Str => Some(SetCmd::Str {
                name,
                op: StrOp::Set(value.to_string()),
            }),
            OptKind::Bool => None,
        };
    }
    if let Some(name) = tok.strip_suffix('?') {
        let (name, kind) = canonical(name)?;
        return Some(match kind {
            OptKind::Bool => SetCmd::Bool {
                name,
                op: SetOp::Query,
            },
            OptKind::Num => SetCmd::Num {
                name,
                op: NumOp::Query,
            },
            OptKind::Str => SetCmd::Str {
                name,
                op: StrOp::Query,
            },
        });
    }
    // `name&`: reset to the option's default. Only string options support it
    // today (the bool/num reset paths aren't wired); others fall through to
    // `None` (E518), as before.
    if let Some(name) = tok.strip_suffix('&') {
        let (name, kind) = canonical(name)?;
        return (kind == OptKind::Str).then_some(SetCmd::Str {
            name,
            op: StrOp::Reset,
        });
    }
    if let Some(name) = tok.strip_suffix('!') {
        // `!` toggles, which only makes sense for a boolean.
        let (name, kind) = canonical(name)?;
        return (kind == OptKind::Bool).then_some(SetCmd::Bool {
            name,
            op: SetOp::Toggle,
        });
    }
    if let Some((name, kind)) = canonical(tok) {
        return Some(match kind {
            // A bare boolean turns on; a bare number or string queries (vim shows
            // its value).
            OptKind::Bool => SetCmd::Bool {
                name,
                op: SetOp::On,
            },
            OptKind::Num => SetCmd::Num {
                name,
                op: NumOp::Query,
            },
            OptKind::Str => SetCmd::Str {
                name,
                op: StrOp::Query,
            },
        });
    }
    if let Some(name) = tok.strip_prefix("no") {
        let (name, kind) = canonical(name)?;
        return (kind == OptKind::Bool).then_some(SetCmd::Bool {
            name,
            op: SetOp::Off,
        });
    }
    if let Some(name) = tok.strip_prefix("inv") {
        let (name, kind) = canonical(name)?;
        return (kind == OptKind::Bool).then_some(SetCmd::Bool {
            name,
            op: SetOp::Toggle,
        });
    }
    None
}

/// Map an option name or its standard abbreviation to its canonical spelling and
/// kind.
fn canonical(name: &str) -> Option<(&'static str, OptKind)> {
    use OptKind::{Bool, Num, Str};
    match name {
        "number" | "nu" => Some(("number", Bool)),
        "relativenumber" | "rnu" => Some(("relativenumber", Bool)),
        "ignorecase" | "ic" => Some(("ignorecase", Bool)),
        "smartcase" | "scs" => Some(("smartcase", Bool)),
        "wrapscan" | "ws" => Some(("wrapscan", Bool)),
        "hlsearch" | "hls" => Some(("hlsearch", Bool)),
        "incsearch" | "is" => Some(("incsearch", Bool)),
        "tabstop" | "ts" => Some(("tabstop", Num)),
        "shiftwidth" | "sw" => Some(("shiftwidth", Num)),
        "softtabstop" | "sts" => Some(("softtabstop", Num)),
        "expandtab" | "et" => Some(("expandtab", Bool)),
        "sidescroll" | "ss" => Some(("sidescroll", Num)),
        "sidescrolloff" | "siso" => Some(("sidescrolloff", Num)),
        "showtabline" | "stal" => Some(("showtabline", Num)),
        "statusline" | "stl" => Some(("statusline", Str)),
        _ => None,
    }
}
