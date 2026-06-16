//! Editor options (`:set ...`), the rust-native analogue of neovim's
//! `option.c`. Kept deliberately small for now — only the options nxvim
//! actually honors live here, and they grow alongside the features that read
//! them.

use crate::encoding::Encoding;

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
    /// When to draw window status lines: `0` never, `1` only with two or more
    /// windows, `2` always (vim's default), `3` a single **global** status line
    /// at the bottom (shared by every window, shown for the current one). Modes
    /// `0/1/2` toggle the per-window status row; `3` additionally docks one global
    /// row in [`crate::Editor`]'s relayout, like the tabline at the top.
    pub laststatus: u8,
    /// The `'statusline'` format string (neovim's `%`-format mini-language).
    /// Empty means the built-in default look; a non-empty value is parsed and
    /// rendered by the statusline engine. Global-only for now (no per-window
    /// override).
    pub statusline: String,
    /// The `'tabline'` format string (the same `%`-format mini-language the
    /// statusline uses, plus the `%T`/`%X` tab click-region items). Empty means
    /// the built-in tab cells (`Vec<TabView>`); a non-empty value is parsed and
    /// rendered by the same engine into a single styled row. The mouse options
    /// (`mouse`/`mousemodel`/`mousescroll`/`mousetime`) follow this block. A whole-line
    /// `%!v:lua.…()` is the usual form. Global by nature (there is one tabline).
    pub tabline: String,
    /// The `'guifont'` value (`"Fira Code:h14"`, neovim/neovide syntax). The core
    /// doesn't use it — fonts are a pixel concern the server has no notion of — but
    /// it's stored here so `:set guifont=…` / `vim.o.guifont` are accepted and the
    /// value is relayed to a GUI client (which parses and applies it). Empty means
    /// the client's own default font.
    pub guifont: String,
    /// Which modes mouse input is acted on (`'mouse'`): a set of mode chars —
    /// `n`ormal, `v`isual, `i`nsert, `c`mdline, `a`ll, plus `r`/`h` (unused yet).
    /// A gesture is honored only if the current mode's char (or `a`) is present;
    /// otherwise it is a silent no-op (vim-faithful). Default `"nvi"` — note
    /// **not** `"a"`, so cmdline-mode mouse is off out of the box.
    pub mouse: String,
    /// Right-click semantics (`'mousemodel'`): `popup`/`popup_setpos` pop a menu
    /// (the selection-extend gesture is then `<S-LeftMouse>`), `extend` makes the
    /// right button extend the selection. Default `"popup_setpos"`. Honored from
    /// the phase that wires right-click; stored here so `:set` accepts it now.
    pub mousemodel: String,
    /// Wheel step (`'mousescroll'`): `"ver:{lines},hor:{cols}"`, a `0` count
    /// disabling that direction. Default `"ver:3,hor:6"`. Honored from the wheel
    /// phase; stored here so `:set` accepts it now.
    pub mousescroll: String,
    /// Max milliseconds between two presses for the second to count as a
    /// multi-click (`'mousetime'`). Default `500`. Honored from the multi-click
    /// phase; stored here so `:set` accepts it now.
    pub mousetime: usize,
    /// Which regex dialect `/` search and `:substitute` speak (nxvim's
    /// `'regexsyntax'`, not a standard vim option): `"pcre"` (the default) for
    /// canonical/PCRE regular expressions, or `"vim"` for vim's "magic" dialect
    /// matched by the embedded vim regexp engine. Read by
    /// [`crate::Editor::search_engine`]; the only accepted values are `"pcre"`
    /// and `"vim"` (validated in `apply_set_str`).
    pub regexsyntax: String,
    /// The ordered list of encodings to try when reading a file (`'fileencodings'`),
    /// a comma-separated string in vim's spelling. The special first entry
    /// `"ucs-bom"` means "sniff a byte-order mark"; the rest are encoding labels
    /// tried in order, with a never-failing fallback (`latin1`) last. The detected
    /// encoding becomes the buffer's [`BufferOptions::fileencoding`]. Read by the
    /// open path (wired in a later phase); stored here so `:set fencs=…` / `vim.o`
    /// accept it now. Default `"ucs-bom,utf-8,latin1"`.
    pub fileencodings: String,
    /// Re-read a file from disk when it changed outside nxvim and the buffer has
    /// no unsaved edits (`'autoread'`). On (neovim's default), `:checktime`
    /// silently reloads such a buffer; off, it warns (W11) and leaves the buffer
    /// for the user to `:edit!`. A *modified* buffer is never autoreloaded — that
    /// is the W12 conflict — regardless of this flag.
    pub autoread: bool,
    /// Open an image file (`.png`, `.jpg`, …) as a rendered **preview** rather than
    /// as its raw bytes (nxvim's `'imagepreview'`, not a standard vim option). Off
    /// by default. When on, [`crate::editor::is_image_path`] files load through
    /// [`crate::Buffer::from_image_file`] — an inert, empty buffer bound to the path
    /// whose bytes are never read as text — and the window projects an
    /// [`crate::view::ImageView`] the client renders as a picture. When off, an
    /// image file opens as ordinary (binary) text, exactly as before.
    pub imagepreview: bool,
    /// Animate viewport scrolls (`<C-d>`/`<C-u>`/`<C-f>`/`<C-b>`, the wheel, and
    /// off-screen jumps) as a slide instead of a teleport (nxvim's `'scrollanim'`,
    /// not a standard vim option — neoscroll.nvim's behavior built in). On by
    /// default. When off, [`crate::Editor::finalize_scroll_gesture`] emits no
    /// `scroll` descriptor, so every client snaps straight to the destination.
    pub scrollanim: bool,
    /// The longest a scroll animation may last, in milliseconds (nxvim's
    /// `'scrollanimduration'`). The per-scroll duration scales with the travel
    /// distance and is clamped to this ceiling; `0` disables animation entirely
    /// (equivalent to `noscrollanim`). Default `160`.
    pub scrollanimduration: usize,
    /// The maximum number of scrolled-off lines a `:terminal` keeps in scrollback
    /// (neovim's `'scrollback'`; default `10000`). `0` keeps none. Read when a
    /// terminal opens; changing it affects terminals opened afterward.
    pub scrollback: usize,
    /// The scanf-style `'errorformat'` used to parse `:make`/`:grep`/`:cbuffer`
    /// output into quickfix entries (see [`crate::editor::quickfix`]). A
    /// comma-separated list of format parts; default [`DFLT_EFM`]. Global-only for
    /// now (vim's per-buffer `'errorformat'` lands with `:lmake`/buffer compilers).
    pub errorformat: String,
    /// How a quickfix jump (`<CR>` / `:cc` / `:cnext`) chooses the window to land
    /// in (`'switchbuf'`): a comma list of `useopen` (reuse a window already on the
    /// target buffer), `usetab`, `split`, `vsplit`, `newtab`, `uselast`. Empty
    /// (vim's default) reuses the window the quickfix list was opened from. Honored
    /// by [`crate::editor::quickfix`]; `usetab` is not yet acted on.
    pub switchbuf: String,
    /// The program `:make` runs (`'makeprg'`; default `make`). A `$*` in the value
    /// is replaced by `:make`'s arguments (else they are appended). Run through the
    /// shell, its combined output parsed against [`Options::errorformat`].
    pub makeprg: String,
    /// The program `:grep` runs (`'grepprg'`; default `grep -n $* /dev/null`). `$*`
    /// is replaced by `:grep`'s arguments. Output is parsed against
    /// [`Options::grepformat`].
    pub grepprg: String,
    /// The scanf-style format parsing `:grep` output into quickfix entries
    /// (`'grepformat'`; default [`DFLT_GREPFORMAT`]). Same grammar as
    /// [`Options::errorformat`].
    pub grepformat: String,
}

/// The default `'errorformat'` — vim's compiled-in non-Windows `DFLT_EFM`
/// (`option_vars.h`), recognizing gcc/clang, the `make[N]: Entering directory`
/// stack, the `In file included from` chains, and the quickfix-window save form.
pub const DFLT_EFM: &str = "%*[^\"]\"%f\"%*\\D%l: %m,\"%f\"%*\\D%l: %m,%-Gg%\\?make[%*\\d]: *** [%f:%l:%m,%-Gg%\\?make: *** [%f:%l:%m,%-G%f:%l: (Each undeclared identifier is reported only once,%-G%f:%l: for each function it appears in.),%-GIn file included from %f:%l:%c:,%-GIn file included from %f:%l:%c\\,,%-GIn file included from %f:%l:%c,%-GIn file included from %f:%l,%-G%*[ ]from %f:%l:%c,%-G%*[ ]from %f:%l:,%-G%*[ ]from %f:%l\\,,%-G%*[ ]from %f:%l,%f:%l:%c:%m,%f(%l):%m,%f:%l:%m,\"%f\"\\, line %l%*\\D%c%*[^ ] %m,%D%*\\a[%*\\d]: Entering directory %*[`']%f',%X%*\\a[%*\\d]: Leaving directory %*[`']%f',%D%*\\a: Entering directory %*[`']%f',%X%*\\a: Leaving directory %*[`']%f',%DMaking %*\\a in %f,%f|%l| %m";

/// The default `'grepformat'` — neovim's compiled-in value, recognizing
/// `file:line:col:msg` (ripgrep / `grep -n` with column), `file:line:msg`, and the
/// bare `file line msg` forms.
pub const DFLT_GREPFORMAT: &str = "%f:%l:%c:%m,%f:%l:%m,%f:%l%m,%f %l%m";

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
            // Every window carries its own status line (vim's default).
            laststatus: 2,
            // No custom statusline by default — the built-in look is used.
            statusline: String::new(),
            // No custom tabline by default — the built-in tab cells are used.
            tabline: String::new(),
            // No custom GUI font by default — the client uses its own.
            guifont: String::new(),
            // Mouse defaults match neovim exactly. `mouse` is `"nvi"` (not `"a"`):
            // cmdline-mode mouse is off by default. `mousemodel` is `popup_setpos`,
            // so right-click pops a menu and `<S-LeftMouse>` is the extend gesture.
            mouse: "nvi".to_string(),
            mousemodel: "popup_setpos".to_string(),
            mousescroll: "ver:3,hor:6".to_string(),
            mousetime: 500,
            // Canonical/PCRE regex for `/` and `:s` out of the box; opt into vim's
            // magic dialect with `:set regexsyntax=vim`.
            regexsyntax: "pcre".to_string(),
            // BOM sniff first, then strict UTF-8, then latin1 (windows-1252) as
            // the always-succeeds fallback — a sane subset of neovim's
            // `ucs-bom,utf-8,default,latin1`.
            fileencodings: crate::encoding::DEFAULT_FILEENCODINGS.to_string(),
            // Reload externally-changed, unmodified buffers on `:checktime`
            // (neovim's default — vim's is off).
            autoread: true,
            // Show image files as text, not pictures, until a config opts in.
            imagepreview: false,
            // Slide the viewport on scroll commands (neoscroll-style), capped at
            // 160ms — the per-scroll duration scales with distance up to this.
            scrollanim: true,
            scrollanimduration: 160,
            // Keep 10000 scrolled-off terminal lines, matching neovim's default.
            scrollback: 10_000,
            // The compiled-in gcc/make-aware errorformat.
            errorformat: DFLT_EFM.to_string(),
            // Empty: a quickfix jump reuses the window the list was opened from.
            switchbuf: String::new(),
            // The compiled-in `:make` / `:grep` programs and grep parser.
            makeprg: "make".to_string(),
            grepprg: "grep -n $* /dev/null".to_string(),
            grepformat: DFLT_GREPFORMAT.to_string(),
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

/// Dock-local options — the **dock** scope, alongside the buffer, window, and
/// global scopes. One [`DockOptions`] per side lives on the editor (indexed by
/// `DockSide::idx`), so each permanent dock can override chrome that doesn't fit
/// the buffer or window scope. The dock's *size* is not here — it stays in the
/// load-bearing `dock_sizes` array — but the `nx.dock.opt` surface presents it
/// alongside these so a dock reads as one options bag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DockOptions {
    /// Per-dock `'showtabline'` override: `None` follows the global
    /// [`Options::showtabline`]; `Some(n)` forces this dock's own tabline gate
    /// (`0` never, `1` only with >1 tab, `2` always). Lets, e.g., an explorer dock
    /// always show its tabline while other regions follow the global default.
    pub showtabline: Option<u8>,
    /// A fixed label shown at the start of this dock's tabline strip, independent
    /// of the buffer names (e.g. `EXPLORER`, `TERMINAL`). Empty ⇒ no title; a
    /// non-empty title also makes the strip appear even with a single tab (unless
    /// `showtabline` is `0`).
    pub title: String,
    /// VSCode-style **auto-hide**: when `true`, this dock collapses itself the moment
    /// focus crosses out of it (it becomes [hidden](crate::editor::Editor), its
    /// content preserved), and re-appears on the next `nx.dock.toggle`/`focus`/`show`.
    /// Default `false` (a dock stays put when focus leaves).
    pub auto_hide: bool,
}

/// A buffer-local `'regexsyntax'` value: an explicit dialect for this buffer, or
/// [`Inherit`](RegexSyntax::Inherit) to follow the global [`Options::regexsyntax`].
/// This is what makes `regexsyntax` a *global-local* option — a buffer can pin its
/// own dialect (`:setlocal regexsyntax=vim` / `vim.bo.regexsyntax`) while every
/// other buffer keeps following the global default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RegexSyntax {
    /// Follow the global `Options::regexsyntax`.
    #[default]
    Inherit,
    /// Canonical/PCRE regex for this buffer.
    Pcre,
    /// Vim's magic dialect for this buffer.
    Vim,
}

/// Buffer-local options, the rust-native analogue of neovim's per-buffer scope.
/// Unlike [`Options`] (one global copy on the editor), a [`BufferOptions`] lives
/// on each [`crate::Buffer`], so two buffers can indent differently. These are
/// the indentation options nxvim honors today, plus the buffer-local
/// `regexsyntax` override; they grow alongside the features that read them.
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
    /// This buffer's `'regexsyntax'` override for `/` search and `:substitute`, or
    /// [`Inherit`](RegexSyntax::Inherit) (the default) to follow the global
    /// [`Options::regexsyntax`]. Resolved by [`crate::Editor::search_engine`].
    pub regexsyntax: RegexSyntax,
    /// The charset the buffer's bytes are in *on disk* (`'fileencoding'`). The
    /// rope is always UTF-8; this names the form the read/write seam transcodes
    /// to/from. Default [`Encoding::UTF8`]. (The seam itself is wired in a later
    /// phase; this stores the per-buffer value `:set fenc=…` / `vim.bo.fileencoding`
    /// set.)
    pub fileencoding: Encoding,
    /// Whether to prepend a byte-order mark when writing (`'bomb'`). Set on read
    /// when a BOM was detected, and honored on write. Default `false`.
    pub bomb: bool,
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
            // No buffer-local override out of the box: follow the global option.
            regexsyntax: RegexSyntax::Inherit,
            // UTF-8 on disk by default; no BOM. Read detection (a later phase)
            // overrides both per buffer.
            fileencoding: Encoding::UTF8,
            bomb: false,
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
        "autoread" | "ar" => Some(("autoread", Bool)),
        "imagepreview" | "imgp" => Some(("imagepreview", Bool)),
        "tabstop" | "ts" => Some(("tabstop", Num)),
        "shiftwidth" | "sw" => Some(("shiftwidth", Num)),
        "softtabstop" | "sts" => Some(("softtabstop", Num)),
        "expandtab" | "et" => Some(("expandtab", Bool)),
        "sidescroll" | "ss" => Some(("sidescroll", Num)),
        "sidescrolloff" | "siso" => Some(("sidescrolloff", Num)),
        "showtabline" | "stal" => Some(("showtabline", Num)),
        "laststatus" | "ls" => Some(("laststatus", Num)),
        "statusline" | "stl" => Some(("statusline", Str)),
        "tabline" | "tal" => Some(("tabline", Str)),
        "guifont" | "gfn" => Some(("guifont", Str)),
        "mouse" => Some(("mouse", Str)),
        "mousemodel" | "mousem" => Some(("mousemodel", Str)),
        "mousescroll" => Some(("mousescroll", Str)),
        "mousetime" | "mouset" => Some(("mousetime", Num)),
        "regexsyntax" | "rxs" => Some(("regexsyntax", Str)),
        // Buffer-local: the on-disk charset (handled specially in `apply_set_str`).
        "fileencoding" | "fenc" => Some(("fileencoding", Str)),
        // Global: the read-detection list (handled specially in `apply_set_str`).
        "fileencodings" | "fencs" => Some(("fileencodings", Str)),
        // Buffer-local: whether to write a BOM (a plain bool slot).
        "bomb" => Some(("bomb", Bool)),
        "scrollanim" | "sca" => Some(("scrollanim", Bool)),
        "scrollanimduration" | "scad" => Some(("scrollanimduration", Num)),
        "scrollback" | "scbk" => Some(("scrollback", Num)),
        "errorformat" | "efm" => Some(("errorformat", Str)),
        "switchbuf" | "swb" => Some(("switchbuf", Str)),
        "makeprg" | "mp" => Some(("makeprg", Str)),
        "grepprg" | "gp" => Some(("grepprg", Str)),
        "grepformat" | "gfm" => Some(("grepformat", Str)),
        // Buffer-local: drives the treesitter language override rather than a
        // global string slot (handled specially in `apply_set_str`).
        "filetype" | "ft" => Some(("filetype", Str)),
        // Buffer-local: whether treesitter paints this buffer — the *whether* noun,
        // orthogonal to `filetype` (the language). Handled specially in
        // `apply_set_bool` (the per-buffer enable map, not an `options` slot).
        "ts_highlight" => Some(("ts_highlight", Bool)),
        _ => None,
    }
}
