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
    /// Wait for a mapped key sequence to complete (`'timeout'`). On (vim's
    /// default), an ambiguous mapped prefix — one that is a live prefix of a
    /// longer mapping — is resolved after [`Options::timeoutlen`] of no further
    /// input (the client's idle flush). Off (`:set notimeout`), such a prefix is
    /// held **forever** until the next key disambiguates it — the behavior that
    /// makes a which-key popup stay up indefinitely while you decide. Read by the
    /// idle-flush path: [`crate::editor`]'s server gates the flush on it, and every
    /// client skips arming its idle timer when it is off.
    pub timeout: bool,
    /// How long (ms) to wait for a mapped sequence to complete before the idle
    /// flush resolves it (`'timeoutlen'`; default `1000`). Only consulted when
    /// [`Options::timeout`] is on. `0` flushes as soon as input pauses. The value
    /// is relayed to the client, which runs the actual wall-clock timer.
    pub timeoutlen: usize,
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
    /// The maximum number of entries kept in each history ring — the `:` ex history,
    /// the `/` search history, and each `nx.ui.input` namespace ring (neovim's
    /// `'history'`; default `10000`). `0` disables history (nothing recalled). The
    /// newest entries are kept; a value above the store's persistence ceiling (10000)
    /// is held in memory but only the newest 10000 survive a restart.
    pub history: usize,
    /// Where command-line / search history persists across sessions (nxvim's
    /// `'persisthistory'`; default `"workspace,global"`). A comma list of `workspace`
    /// (the per-namespace store) and/or `global` (the shared store), or the lone token
    /// `none` to persist no history. The server reads it to route the shada flush/load.
    pub persisthistory: String,
    /// The scanf-style `'errorformat'` used to parse `:make`/`:grep`/`:cbuffer`
    /// output into quickfix entries (see [`crate::editor::quickfix`]). A
    /// comma-separated list of format parts; default [`DFLT_EFM`]. Global-only for
    /// now (vim's per-buffer `'errorformat'` lands with `:lmake`/buffer compilers).
    pub errorformat: String,
    /// How a jump (a quickfix `<CR>`/`:cc`/`:cnext`, an LSP go-to, a picker / mark
    /// jump) chooses the window to land in (`'switchbuf'`): a comma list of
    /// `useopen` (reuse a window in the *current* tab already on the target buffer),
    /// `usetab` (reuse such a window in *any* tab, switching to it), `split`,
    /// `vsplit`. nxvim defaults to `usetab` (vim's default is empty, reusing the
    /// current window). Honored by [`Editor::jump_to`](crate::editor) and
    /// [`crate::editor::quickfix`].
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
    /// Where the **quickfix** and **named** list displays open (`'qfdock'`;
    /// nxvim-native, default `true`). When set, `:copen` and a named-list show host
    /// the `filetype=qf` display as a tab in the **bottom dock**, entries jumping into
    /// the main editing layer — the nxvim way, where several lists sit side by side as
    /// dock tabs. When unset, they open the classic way: a bottom **split** of the
    /// current window. A window-scoped **location list** (`:lopen`) is *not* governed
    /// by this — it always keeps vim's behavior (a bottom split owned by its window).
    /// Honored by [`crate::editor::quickfix`].
    pub qfdock: bool,
    /// Whether `:bdelete` of a tab's *last* buffer closes the tab page (`'bdclosetab'`;
    /// nxvim-native, default `true`). When set, deleting the only buffer a tab shows —
    /// with other tabs open — closes that tab rather than loading a sibling buffer into
    /// its window (vim's behavior). A tab whose windows still show *other* buffers is
    /// never closed. With it unset, `:bd` behaves the classic way. Honored by
    /// [`crate::editor::buffers`].
    pub bdclosetab: bool,
    /// Whether a saved session stores split sizes as proportional PERCENTAGES rather
    /// than absolute cells (`'relativesplits'`; nxvim-native, default `true`). A
    /// 30/70 vsplit then restores 30/70 at any terminal width. Read at session
    /// capture by [`crate::editor::persist`] — a property of the native session
    /// persistence, not any one plugin (any wrapper that opts into capture via
    /// `nx.shada.save_layout` honors it).
    pub relative_splits: bool,
    /// Whether a saved session stores a dock's size as a PERCENTAGE of the screen
    /// rather than absolute cells (`'relativedocks'`; nxvim-native, default
    /// `false`, so docks keep their cell size across terminal resizes). Read at
    /// session capture by [`crate::editor::persist`]; the native counterpart of
    /// [`Options::relative_splits`] for edge docks.
    pub relative_docks: bool,
    /// Whether opening or closing a window automatically re-equalizes every window
    /// to even sizes (`'equalalways'`; default `true`, as in vim). With it on, a
    /// `:split` / `:vsplit` and a window close both run the same leaf-count-weighted
    /// pass `<C-w>=` uses, so the layout stays balanced. Unset it to keep the
    /// classic carve-from-one-neighbor sizing and only rebalance on an explicit
    /// `<C-w>=`. Honored by [`crate::editor::windows`].
    pub equalalways: bool,
    /// Whether a saved workspace session persists **unnamed** (`[No Name]`) buffers —
    /// the ordinary, pathless buffers you typed into but never wrote to a file —
    /// together with their contents, restoring them on the next launch
    /// (`'workspacepersistunnamed'`; nxvim-native, default `true`). Only *modified*
    /// unnamed buffers are saved; a pristine startup `[No Name]` is not. Read at session
    /// capture by [`crate::editor::persist`]. Plugin-owned surfaces (terminals, file
    /// trees, `nx.view` widgets) are non-ordinary buffers and are never captured this way.
    pub workspace_persist_unnamed: bool,
}

/// A scalar value for one **global** option, the value type the per-workspace option
/// overlay ([`WorkspaceOptions`]) stores. Mirrors the three option kinds; the variant
/// must match the option's [`OptKind`] (`set_workspace_option` validates this), so the
/// overlay can be re-applied onto [`Options`] verbatim via [`Options::set_scalar`].
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum OptionScalar {
    Bool(bool),
    Num(i64),
    Str(String),
}

/// The per-workspace **option overlay**: a sparse map of canonical global-option name →
/// the workspace's overriding value. It sits *above* the process-global values
/// ([`crate::Editor`]'s `global_base`): the effective [`Options`] every read sees is
/// `global_base` with this overlay applied on top, so a workspace value wins over the
/// global one. Empty outside a `--workspace` session (or before any `nx.wso` write).
/// Persisted in the workspace shada (a `BTreeMap` so the serialized order is stable).
pub type WorkspaceOptions = std::collections::BTreeMap<String, OptionScalar>;

impl Options {
    /// Write one **global** option from a [`OptionScalar`], the single name→field map
    /// shared by the global setters ([`crate::Editor::set_global_option_bool`] et al.)
    /// and the workspace-overlay re-apply ([`crate::Editor`]'s `recompute_effective_options`).
    /// Returns whether `name` is a wired global option *of the scalar's kind* — a
    /// mismatched kind (e.g. a `Str` for `ignorecase`) writes nothing and returns `false`,
    /// so the string setter keeps its "was this a wired string global" result. No
    /// validation (range/enum) happens here; callers validate before writing.
    pub fn set_scalar(&mut self, name: &str, value: &OptionScalar) -> bool {
        use OptionScalar::{Bool, Num, Str};
        match (name, value) {
            ("ignorecase", Bool(b)) => self.ignorecase = *b,
            ("smartcase", Bool(b)) => self.smartcase = *b,
            ("wrapscan", Bool(b)) => self.wrapscan = *b,
            ("hlsearch", Bool(b)) => self.hlsearch = *b,
            ("incsearch", Bool(b)) => self.incsearch = *b,
            ("autoread", Bool(b)) => self.autoread = *b,
            ("imagepreview", Bool(b)) => self.imagepreview = *b,
            ("timeout", Bool(b)) => self.timeout = *b,
            ("scrollanim", Bool(b)) => self.scrollanim = *b,
            ("qfdock", Bool(b)) => self.qfdock = *b,
            ("bdclosetab", Bool(b)) => self.bdclosetab = *b,
            ("relativesplits", Bool(b)) => self.relative_splits = *b,
            ("relativedocks", Bool(b)) => self.relative_docks = *b,
            ("equalalways", Bool(b)) => self.equalalways = *b,
            ("workspacepersistunnamed", Bool(b)) => self.workspace_persist_unnamed = *b,
            ("showtabline", Num(n)) => self.showtabline = *n as u8,
            ("laststatus", Num(n)) => self.laststatus = *n as u8,
            ("mousetime", Num(n)) => self.mousetime = *n as usize,
            ("timeoutlen", Num(n)) => self.timeoutlen = *n as usize,
            ("scrollanimduration", Num(n)) => self.scrollanimduration = *n as usize,
            ("scrollback", Num(n)) => self.scrollback = *n as usize,
            ("history", Num(n)) => self.history = *n as usize,
            ("persisthistory", Str(s)) => self.persisthistory = s.clone(),
            ("statusline", Str(s)) => self.statusline = s.clone(),
            ("tabline", Str(s)) => self.tabline = s.clone(),
            ("guifont", Str(s)) => self.guifont = s.clone(),
            ("mouse", Str(s)) => self.mouse = s.clone(),
            ("mousemodel", Str(s)) => self.mousemodel = s.clone(),
            ("mousescroll", Str(s)) => self.mousescroll = s.clone(),
            ("regexsyntax", Str(s)) => self.regexsyntax = s.clone(),
            ("fileencodings", Str(s)) => self.fileencodings = s.clone(),
            ("errorformat", Str(s)) => self.errorformat = s.clone(),
            ("switchbuf", Str(s)) => self.switchbuf = s.clone(),
            ("makeprg", Str(s)) => self.makeprg = s.clone(),
            ("grepprg", Str(s)) => self.grepprg = s.clone(),
            ("grepformat", Str(s)) => self.grepformat = s.clone(),
            _ => return false,
        }
        true
    }

    /// Read one **global** option as an [`OptionScalar`] by canonical name — the read
    /// counterpart of [`Options::set_scalar`], used by the `:set` toggle/query path and
    /// available for inspecting the effective value. `None` for a non-global / unknown name.
    pub fn get_scalar(&self, name: &str) -> Option<OptionScalar> {
        use OptionScalar::{Bool, Num, Str};
        Some(match name {
            "ignorecase" => Bool(self.ignorecase),
            "smartcase" => Bool(self.smartcase),
            "wrapscan" => Bool(self.wrapscan),
            "hlsearch" => Bool(self.hlsearch),
            "incsearch" => Bool(self.incsearch),
            "autoread" => Bool(self.autoread),
            "imagepreview" => Bool(self.imagepreview),
            "timeout" => Bool(self.timeout),
            "scrollanim" => Bool(self.scrollanim),
            "qfdock" => Bool(self.qfdock),
            "bdclosetab" => Bool(self.bdclosetab),
            "relativesplits" => Bool(self.relative_splits),
            "relativedocks" => Bool(self.relative_docks),
            "equalalways" => Bool(self.equalalways),
            "workspacepersistunnamed" => Bool(self.workspace_persist_unnamed),
            "showtabline" => Num(self.showtabline as i64),
            "laststatus" => Num(self.laststatus as i64),
            "mousetime" => Num(self.mousetime as i64),
            "timeoutlen" => Num(self.timeoutlen as i64),
            "scrollanimduration" => Num(self.scrollanimduration as i64),
            "scrollback" => Num(self.scrollback as i64),
            "history" => Num(self.history as i64),
            "persisthistory" => Str(self.persisthistory.clone()),
            "statusline" => Str(self.statusline.clone()),
            "tabline" => Str(self.tabline.clone()),
            "guifont" => Str(self.guifont.clone()),
            "mouse" => Str(self.mouse.clone()),
            "mousemodel" => Str(self.mousemodel.clone()),
            "mousescroll" => Str(self.mousescroll.clone()),
            "regexsyntax" => Str(self.regexsyntax.clone()),
            "fileencodings" => Str(self.fileencodings.clone()),
            "errorformat" => Str(self.errorformat.clone()),
            "switchbuf" => Str(self.switchbuf.clone()),
            "makeprg" => Str(self.makeprg.clone()),
            "grepprg" => Str(self.grepprg.clone()),
            "grepformat" => Str(self.grepformat.clone()),
            _ => return None,
        })
    }
}

/// Which single store command-line / search history persists to (and restores from).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryScope {
    /// The per-workspace (namespace) store.
    Workspace,
    /// The shared global store.
    Global,
    /// Don't persist history.
    None,
}

/// Resolve a `'persisthistory'` value to the **single** store history uses, given
/// whether a workspace is open. The value is a *priority list*: the first token that
/// is available wins — `workspace` only when `workspace_open`, `global` always, and
/// `none` stops with no persistence. So the default `"workspace,global"` saves to the
/// workspace store when one is open, else falls back to the global store. An empty /
/// all-unavailable list is [`HistoryScope::None`]. (`:set` validates the value with
/// [`valid_persisthistory`]; the lenient `nx.o` path skips unknown tokens here.)
pub fn effective_history_scope(value: &str, workspace_open: bool) -> HistoryScope {
    for tok in value.split(',') {
        match tok.trim() {
            "none" => return HistoryScope::None,
            "workspace" if workspace_open => return HistoryScope::Workspace,
            "global" => return HistoryScope::Global,
            // `workspace` with no workspace open (skip to the fallback), or an unknown
            // token (lenient): keep scanning the list.
            _ => {}
        }
    }
    HistoryScope::None
}

/// Whether a `'persisthistory'` value is well-formed: either the lone token `none`, or
/// a comma list drawn from `workspace` / `global` (each at most once is not enforced —
/// duplicates are harmless). Used by the strict `:set` path to reject a typo with E474;
/// `nx.o` stores any string (lenient policy) and [`parse_persisthistory`] ignores the
/// rest.
pub fn valid_persisthistory(value: &str) -> bool {
    let tokens: Vec<&str> = value.split(',').map(str::trim).collect();
    if tokens.contains(&"none") {
        // `none` is exclusive — it may not combine with other tokens.
        return tokens.len() == 1;
    }
    !tokens.is_empty() && tokens.iter().all(|t| *t == "workspace" || *t == "global")
}

/// Resolve `name` (or its standard abbreviation) to its canonical spelling, kind, and
/// scope from the [`OPTIONS`] catalog — the public lookup the workspace-option overlay
/// uses to validate that a name is a *global* option of the expected kind. `None` for an
/// unknown option.
pub fn option_meta(name: &str) -> Option<(&'static str, OptKind, OptScope)> {
    OPTIONS
        .iter()
        .find(|o| o.name == name || o.abbrev == Some(name))
        .map(|o| (o.name, o.kind, o.scope))
}

/// The default `'errorformat'` — vim's compiled-in non-Windows `DFLT_EFM`
/// (`option_vars.h`), recognizing gcc/clang, the `make[N]: Entering directory`
/// stack, the `In file included from` chains, and the quickfix-window save form.
pub const DFLT_EFM: &str ="%*[^\"]\"%f\"%*\\D%l: %m,\"%f\"%*\\D%l: %m,%-Gg%\\?make[%*\\d]: *** [%f:%l:%m,%-Gg%\\?make: *** [%f:%l:%m,%-G%f:%l: (Each undeclared identifier is reported only once,%-G%f:%l: for each function it appears in.),%-GIn file included from %f:%l:%c:,%-GIn file included from %f:%l:%c\\,,%-GIn file included from %f:%l:%c,%-GIn file included from %f:%l,%-G%*[ ]from %f:%l:%c,%-G%*[ ]from %f:%l:,%-G%*[ ]from %f:%l\\,,%-G%*[ ]from %f:%l,%f:%l:%c:%m,%f(%l):%m,%f:%l:%m,\"%f\"\\, line %l%*\\D%c%*[^ ] %m,%D%*\\a[%*\\d]: Entering directory %*[`']%f',%X%*\\a[%*\\d]: Leaving directory %*[`']%f',%D%*\\a: Entering directory %*[`']%f',%X%*\\a: Leaving directory %*[`']%f',%DMaking %*\\a in %f,%f|%l| %m";

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
            // Wait for a mapped sequence to finish (vim's default), for 1000ms —
            // `:set notimeout` holds an ambiguous prefix forever instead.
            timeout: true,
            timeoutlen: 1000,
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
            history: 10_000,
            persisthistory: "workspace,global".to_string(),
            // The compiled-in gcc/make-aware errorformat.
            errorformat: DFLT_EFM.to_string(),
            // nxvim default: a jump to a buffer already shown in another tab switches
            // to that tab (vim's default is empty — reuse the current window).
            switchbuf: "usetab".to_string(),
            // The compiled-in `:make` / `:grep` programs and grep parser.
            makeprg: "make".to_string(),
            grepprg: "grep -n $* /dev/null".to_string(),
            grepformat: DFLT_GREPFORMAT.to_string(),
            // The nxvim way: quickfix / loclist displays open as bottom-dock tabs.
            qfdock: true,
            // The nxvim way: `:bd` of a tab's last buffer closes the tab.
            bdclosetab: true,
            // A saved session scales splits with the terminal (proportional), but
            // keeps docks at their cell size unless asked otherwise.
            relative_splits: true,
            relative_docks: false,
            // vim's default: opening/closing a window keeps the layout balanced.
            equalalways: true,
            // Keep modified `[No Name]` buffers across a workspace session by default.
            workspace_persist_unnamed: true,
        }
    }
}

/// neovim's `'signcolumn'` policy: whether the sign column shows and how wide it
/// may grow. The width is counted in sign *columns* (each 2 display cells); see
/// [`SignColumn::floor_cells`] for the part core reserves and the server's
/// `sign_width_for` for the rendered width (which expands `Auto` to fit signs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignColumn {
    /// `no` — never show a sign column.
    No,
    /// `auto` / `auto:min-max` — show between `min` and `max` sign columns when a
    /// visible line has a sign, and collapse to nothing when none do.
    Auto { min: u16, max: u16 },
    /// `yes` / `yes:n` / `yes:min-max` — always reserve at least `min` columns,
    /// widening to `max` to fit the busiest visible line's signs.
    Yes { min: u16, max: u16 },
}

impl SignColumn {
    /// Parse a `'signcolumn'` value (`no`, `auto`, `auto:1-3`, `yes`, `yes:2`,
    /// `yes:1-3`). Returns `None` for `number` (deferred) and any malformed value,
    /// so `:set` / `vim.o` can report `E474` instead of silently accepting junk.
    pub fn parse(s: &str) -> Option<Self> {
        let range = |spec: &str| -> Option<(u16, u16)> {
            // `n` → n-n; `min-max` → that range. Both bounded to neovim's 1..=9.
            let (min, max) = match spec.split_once('-') {
                Some((a, b)) => (a.parse::<u16>().ok()?, b.parse::<u16>().ok()?),
                None => {
                    let n = spec.parse::<u16>().ok()?;
                    (n, n)
                }
            };
            ((1..=9).contains(&min) && min <= max && max <= 9).then_some((min, max))
        };
        match s {
            "no" => Some(SignColumn::No),
            "auto" => Some(SignColumn::Auto { min: 1, max: 1 }),
            "yes" => Some(SignColumn::Yes { min: 1, max: 1 }),
            _ => {
                if let Some(spec) = s.strip_prefix("auto:") {
                    let (min, max) = range(spec)?;
                    Some(SignColumn::Auto { min, max })
                } else if let Some(spec) = s.strip_prefix("yes:") {
                    let (min, max) = range(spec)?;
                    Some(SignColumn::Yes { min, max })
                } else {
                    None
                }
            }
        }
    }

    /// The number of display cells core must always reserve for this policy,
    /// regardless of how many signs exist (core is sign-agnostic). `Yes` reserves
    /// its minimum; `No`/`Auto` reserve nothing (an `Auto` column only appears when
    /// the server sees a sign, which core can't know — see the plan's seam note).
    pub fn floor_cells(self) -> usize {
        match self {
            SignColumn::Yes { min, .. } => min as usize * 2,
            SignColumn::No | SignColumn::Auto { .. } => 0,
        }
    }
}

impl std::fmt::Display for SignColumn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Round-trips through `parse`; `n-n` collapses to `n` like neovim echoes it.
        let span = |min: u16, max: u16| {
            if min == max {
                format!("{min}")
            } else {
                format!("{min}-{max}")
            }
        };
        match self {
            SignColumn::No => f.write_str("no"),
            SignColumn::Auto { min: 1, max: 1 } => f.write_str("auto"),
            SignColumn::Yes { min: 1, max: 1 } => f.write_str("yes"),
            SignColumn::Auto { min, max } => write!(f, "auto:{}", span(*min, *max)),
            SignColumn::Yes { min, max } => write!(f, "yes:{}", span(*min, *max)),
        }
    }
}

/// A per-side blank margin (in screen cells) left around a window's content box —
/// the gutter, text body, and status line all sit inside it, so the window reads
/// with breathing room from its rect edges. nxvim's own option (vim has no
/// equivalent); see [`WindowOptions::padding`]. All-zero by default (no margin),
/// so a window with default padding renders exactly as before.
///
/// Sides follow CSS order where a `:set padding=` string is parsed (see
/// [`parse_padding`]): `top right bottom left`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Padding {
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
    pub left: usize,
}

impl Padding {
    /// A uniform margin of `n` cells on every side.
    pub fn uniform(n: usize) -> Self {
        Padding {
            top: n,
            right: n,
            bottom: n,
            left: n,
        }
    }

    /// Total cells consumed horizontally (left + right).
    pub fn horizontal(&self) -> usize {
        self.left + self.right
    }

    /// Total cells consumed vertically (top + bottom).
    pub fn vertical(&self) -> usize {
        self.top + self.bottom
    }

    /// Whether any side is non-zero (the common fast path skips the inset when not).
    pub fn is_zero(&self) -> bool {
        *self == Padding::default()
    }
}

impl std::fmt::Display for Padding {
    /// The canonical `:set padding?` form. Collapses to the shortest equivalent
    /// spec: `2` when uniform, `1 2` when vertical/horizontal pairs match,
    /// otherwise the full `top right bottom left`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.top == self.right && self.right == self.bottom && self.bottom == self.left {
            write!(f, "{}", self.top)
        } else if self.top == self.bottom && self.left == self.right {
            write!(f, "{} {}", self.top, self.right)
        } else {
            write!(
                f,
                "{} {} {} {}",
                self.top, self.right, self.bottom, self.left
            )
        }
    }
}

/// Parse a `'padding'` value into a [`Padding`], or `None` if any token is not a
/// non-negative integer or the token count is unsupported — the caller reports
/// `E474`. Accepts CSS-style shorthands, tokens separated by whitespace and/or
/// commas:
///
/// - `"2"` → all four sides `2`
/// - `"1 2"` → vertical (top/bottom) `1`, horizontal (left/right) `2`
/// - `"1 2 3 4"` → `top right bottom left` (CSS order)
///
/// An empty value parses to the all-zero default (no margin).
pub fn parse_padding(s: &str) -> Option<Padding> {
    let nums: Vec<usize> = s
        .split([' ', ',', '\t'])
        .filter(|t| !t.is_empty())
        .map(|t| t.parse::<usize>().ok())
        .collect::<Option<_>>()?;
    match nums.as_slice() {
        [] => Some(Padding::default()),
        &[a] => Some(Padding::uniform(a)),
        &[v, h] => Some(Padding {
            top: v,
            right: h,
            bottom: v,
            left: h,
        }),
        &[top, right, bottom, left] => Some(Padding {
            top,
            right,
            bottom,
            left,
        }),
        _ => None,
    }
}

/// Window-local options, the rust-native analogue of neovim's per-window scope.
/// Unlike [`Options`] (one global copy on the editor), a [`WindowOptions`] lives
/// on each window, so two windows onto the *same* buffer can show different
/// line-number gutters. A split inherits these from the window it splits off.
// Not `Copy`: `showbreak` is a `String`. `WindowOptions` is cloned (cheaply — one
// short string) where a window's options are snapshotted; it is mutated in place on
// the live window otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowOptions {
    /// Show the absolute line number in the number column.
    pub number: bool,
    /// Show line numbers relative to the cursor line. Combined with
    /// [`WindowOptions::number`] this gives vim's "hybrid" gutter: the absolute
    /// number on the cursor line, relative numbers elsewhere.
    pub relativenumber: bool,
    /// `'cursorline'`: highlight the screen line the cursor sits on, using the
    /// `CursorLine` highlight group. Off by default (vim's default). The
    /// highlight is a per-window decoration the clients paint behind the cursor
    /// row — core only carries the flag.
    pub cursorline: bool,
    /// Minimum width of the number gutter (vim's `numberwidth`). The gutter is at
    /// least this wide, growing to fit the largest line number plus a trailing
    /// space. Only consulted when `number`/`relativenumber` is on.
    pub numberwidth: usize,
    /// The sign-column policy (vim's `signcolumn`): whether the diagnostics sign
    /// column shows and how wide it may grow.
    pub signcolumn: SignColumn,
    /// Minimum number of columns to scroll horizontally when the cursor moves off
    /// a `nowrap` window's edge (vim's `sidescroll`). `0` recenters the cursor;
    /// `1` (the default) scrolls just enough to keep it at the edge.
    pub sidescroll: usize,
    /// Minimum number of columns to keep between the cursor and the left/right
    /// edge while horizontally scrolling (vim's `sidescrolloff`). `0` by default —
    /// the cursor may sit on the very edge.
    pub sidescrolloff: usize,
    /// Soft-wrap long lines: a buffer line wider than the text area is laid out
    /// across several screen rows (continuation rows) rather than scrolled
    /// horizontally. `false` (nxvim's historical `nowrap`) keeps one screen row per
    /// line and pans with `leftcol`. When on, `leftcol` is forced to 0.
    pub wrap: bool,
    /// `'breakindent'`: indent each soft-wrap continuation row to match the start of
    /// the wrapped line, so the wrapped text reads as a hanging block under the
    /// line's own indent rather than starting at the window's left edge. Only takes
    /// effect with [`wrap`] on. The indent (plus any [`showbreak`]) consumes leading
    /// cells of the continuation row, reducing the text width it wraps into.
    pub breakindent: bool,
    /// `'showbreak'`: a string drawn at the start of every soft-wrap continuation row
    /// (e.g. `"↪ "`), before any [`breakindent`] padding. Empty by default (no
    /// marker). Only takes effect with [`wrap`] on.
    pub showbreak: String,
    /// `'breakindentopt'`: comma-separated tweaks to [`breakindent`]. nxvim honors the
    /// `sbr` flag (draw [`showbreak`] *within* the indent so the wrapped text still
    /// aligns under the line's indent, instead of vim's default additive prefix);
    /// other vim flags (`min:`, `shift:`, `list:`, `vcol`) are accepted but ignored.
    /// Empty by default.
    pub breakindentopt: String,
    /// `'fillchars'`: a comma-separated `key:char` list choosing the characters
    /// drawn in structural spots (vim's `fillchars`). nxvim honors only the `eob`
    /// key today — the filler char drawn on screen rows past the end of the buffer
    /// (vim's `~`); the other keys (`vert`, `fold`, `diff`, …) are validated so a
    /// vim config's `fillchars` sets cleanly, but have no rendering effect yet.
    /// Empty by default, which means `eob:~` (see [`WindowOptions::fillchars_eob`]).
    pub fillchars: String,
    /// `'padding'` (nxvim's own; no vim equivalent): a per-side blank margin in
    /// screen cells left around this window's content box — the number gutter, text
    /// body, and status line all inset by it, so the window reads with breathing
    /// room from its rect edges. All-zero by default (no margin). Set with
    /// `:set padding=…` (CSS-style shorthand, see [`parse_padding`]) or
    /// `vim.wo.padding`.
    pub padding: Padding,
    /// `'winhighlight'`: a per-window highlight-group remap (`"Normal:NormalSB,
    /// EndOfBuffer:Hidden"`) applied while rendering this window — every group on
    /// the left resolves to the group on its right here, leaving other windows
    /// untouched. Stored as the raw option string (parsed to a
    /// [`WinHl`](crate::WinHl) at projection time, like [`fillchars`] is parsed
    /// lazily); empty by default (no remap). A window in a dock with no window-local
    /// value of its own inherits the dock's [`DockOptions::winhighlight`].
    pub winhighlight: String,
    /// Per-window override of the global [`'scrollanim'`](Options::scrollanim): `None`
    /// inherits the global value (the common case), `Some(false)` forces this window's
    /// viewport scrolls to snap instead of slide (and `Some(true)` forces the slide
    /// even when the global is off). The side-by-side diff sets `Some(false)` on its
    /// panes so a synced scroll doesn't desync — only the focused pane can animate, so
    /// a mirrored pane jumping while the focused one slides reads as a glitch. Resolved
    /// where the gesture is built ([`crate::Editor::finalize_scroll_gesture`]).
    pub scrollanim: Option<bool>,
    /// `'foldenable'`: whether closed folds collapse on screen in this window. On
    /// by default (vim's default). A fold can be *created* and marked closed while
    /// this is off, but nothing collapses until it is on; `zn` clears it, `zN`/`zi`
    /// restore/toggle it. Read by the view projection (`crate::view`) to decide
    /// whether to fold lines away. Per-window, as in vim.
    pub foldenable: bool,
    /// `'foldcolumn'`: width (in cells) of the gutter column that shows fold
    /// markers (`-`/`│` for open folds, `+` for a closed one). `0` (the default)
    /// hides it. Per-window; the projection (`crate::view`) fills a per-row marker
    /// string this wide that the client paints to the left of the sign / number
    /// gutter.
    pub foldcolumn: usize,
    /// `'foldlevel'`: folds at a level **higher** than this display closed (when
    /// `'foldenable'`); folds at this level or shallower are open. `0` (vim's
    /// default) closes every fold of a computed source, `1` shows only the
    /// outermost level, and a large value opens all. Per-window. Drives the closed
    /// state of *computed* folds (`indent`/…); changing it re-derives which folds
    /// are open. Manual folds track their own `zo`/`zc` state and ignore it.
    pub foldlevel: usize,
}

impl WindowOptions {
    /// Whether `'breakindentopt'` contains the `sbr` flag — draw `'showbreak'` within
    /// the breakindent rather than added on top (see [`WindowOptions::breakindentopt`]).
    pub fn breakindent_sbr(&self) -> bool {
        self.breakindentopt.split(',').any(|f| f.trim() == "sbr")
    }

    /// The soft-wrap continuation-prefix config bundled for the wrap helpers (borrows
    /// `showbreak`). One place builds it from the window-local options.
    pub fn wrap_prefix(&self) -> crate::unicode::WrapPrefix<'_> {
        crate::unicode::WrapPrefix {
            breakindent: self.breakindent,
            showbreak: self.showbreak.as_str(),
            sbr: self.breakindent_sbr(),
        }
    }

    /// The `eob` filler char from this window's [`'fillchars'`](WindowOptions::fillchars):
    /// the character drawn on screen rows past the end of the buffer. Defaults to
    /// `~` (vim's default, and what an empty `fillchars` means). Setting `eob` to a
    /// space (`:set fillchars=eob:\ `) blanks the markers. The stored value is always
    /// pre-validated by [`parse_fillchars`] at set time, so this never sees junk.
    pub fn fillchars_eob(&self) -> char {
        parse_fillchars(&self.fillchars).unwrap_or(DEFAULT_EOB)
    }
}

/// The default `'fillchars'` `eob` character — vim's end-of-buffer filler `~`.
pub const DEFAULT_EOB: char = '~';

/// The recognized `'fillchars'` keys (vim's `fillchars` table). nxvim honors only
/// `eob` today (see [`WindowOptions::fillchars`]); the rest are validated so a vim
/// config sets cleanly, but have no rendering effect yet.
const FILLCHARS_KEYS: &[&str] = &[
    "eob",
    "vert",
    "fold",
    "foldopen",
    "foldclose",
    "foldsep",
    "diff",
    "msgsep",
    "horiz",
    "horizup",
    "horizdown",
    "vertleft",
    "vertright",
    "verthoriz",
    "stl",
    "stlnc",
];

/// Parse a `'fillchars'` value, returning the `eob` filler char ([`DEFAULT_EOB`]
/// when the value omits an `eob:` entry), or `None` if any entry is malformed or
/// names an unknown key — the caller reports `E474`. Every entry must be
/// `key:char` where `key` is a [recognized key](FILLCHARS_KEYS) and `char` is
/// exactly one character; only `eob` changes the result, the rest are validated
/// and ignored. An empty value parses to [`DEFAULT_EOB`] (vim's default look).
pub fn parse_fillchars(s: &str) -> Option<char> {
    let mut eob = DEFAULT_EOB;
    // Entries are split on `,` only — never trimmed: a fill *value* is allowed to be
    // a space (`eob:\ ` blanks the markers), and trimming would eat it. (vim's
    // `:set` tokenizer already stripped the surrounding whitespace; spaces inside
    // the value reach here only when escaped, and are meaningful.)
    for entry in s.split(',').filter(|e| !e.is_empty()) {
        let (key, val) = entry.split_once(':')?;
        if !FILLCHARS_KEYS.contains(&key) {
            return None;
        }
        let mut chars = val.chars();
        let c = chars.next()?;
        if chars.next().is_some() {
            // A fillchars value is exactly one display char (vim's rule).
            return None;
        }
        if key == "eob" {
            eob = c;
        }
    }
    Some(eob)
}

impl Default for WindowOptions {
    fn default() -> Self {
        // nxvim ships with the hybrid number column on: the cursor line shows its
        // document line number, every other line shows its distance from the
        // cursor.
        WindowOptions {
            number: true,
            relativenumber: true,
            // Off by default, matching vim — `:set cursorline` opts in.
            cursorline: false,
            // neovim's `numberwidth` default (4) and `signcolumn=auto`.
            numberwidth: 4,
            signcolumn: SignColumn::Auto { min: 1, max: 1 },
            // neovim's horizontal-scroll defaults: scroll a minimal step, no margin.
            sidescroll: 1,
            sidescrolloff: 0,
            // nxvim has historically been `nowrap`-only; wrap is opt-in (`:set wrap`)
            // so the existing horizontal-scroll behavior is the default.
            wrap: false,
            // Wrap polish, all off / empty by default (continuation rows start at the
            // left edge with no marker, matching vim's out-of-the-box look).
            breakindent: false,
            showbreak: String::new(),
            breakindentopt: String::new(),
            // Empty fillchars: the end-of-buffer filler is vim's default `~`.
            fillchars: String::new(),
            // No margin out of the box — a default window renders flush as before.
            padding: Padding::default(),
            // No window-local highlight remap by default.
            winhighlight: String::new(),
            // Inherit the global `'scrollanim'` unless a window opts out (the diff does).
            scrollanim: None,
            // Folding is enabled out of the box (vim's default); a closed fold
            // collapses immediately. `zn` turns it off without losing the folds.
            foldenable: true,
            // No fold-marker gutter by default (vim's default `foldcolumn=0`).
            foldcolumn: 0,
            // vim's default: every fold of a computed source starts closed.
            foldlevel: 0,
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
    /// Per-dock `'laststatus'` override: `None` follows the global
    /// [`Options::laststatus`]; `Some(n)` forces this dock's per-window status row
    /// (`0`/`3` never, `1` only with >1 window in the dock, `2` always). Lets, e.g.,
    /// a terminal or explorer dock hide its statusline while other regions keep
    /// theirs.
    pub laststatus: Option<u8>,
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
    /// `'winhighlight'` for every window in this dock: a highlight-group remap
    /// (`"Normal:NormalSB,EndOfBuffer:Hidden"`) so a dock can paint itself like a
    /// VSCode sidebar without touching the global theme. Stored as the raw option
    /// string (parsed to a [`WinHl`](crate::WinHl) at projection time); empty by
    /// default. A window with its own [`WindowOptions::winhighlight`] overrides this.
    pub winhighlight: String,
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

/// How a buffer's folds are defined (`'foldmethod'`). `manual` (the default) is
/// the only source that stores explicit ranges; the rest derive the fold
/// structure from buffer content. Phase 3 adds `indent` (folds from leading
/// indent); `expr`/`marker`/`syntax` are recognized names that error loud at
/// set-time until their phase lands (no silent no-op — see the folds plan).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FoldMethod {
    /// Folds created/edited by hand with `zf`/`:fold` and the `z` family.
    #[default]
    Manual,
    /// Folds derived from each line's leading indent (`indent / shiftwidth`).
    Indent,
    /// Folds defined by `'foldexpr'`. nxvim evaluates the canonical tree-sitter
    /// foldexpr natively (the headline source); a generic Lua foldexpr is Phase 5.
    Expr,
    /// Folds bounded by the literal `'foldmarker'` strings in the text (default
    /// `{{{`/`}}}`). A start marker opens a fold at its line; the matching end
    /// marker's line is the fold's last line. Nests by counting markers, or an
    /// explicit level with a number after the marker (`{{{2`).
    Marker,
}

impl FoldMethod {
    /// Parse a `'foldmethod'` value. `manual`/`indent`/`expr`/`marker` are
    /// supported; the other vim names (`syntax`/`diff`) are valid spellings but not
    /// yet implemented, so they parse to an [`Unimplemented`](FoldMethodErr) error
    /// the caller surfaces as a loud "not supported yet" — never a silent no-op —
    /// kept distinct from an [`Unknown`](FoldMethodErr) value (E474).
    pub fn from_label(label: &str) -> Result<FoldMethod, FoldMethodErr> {
        match label {
            "manual" => Ok(FoldMethod::Manual),
            "indent" => Ok(FoldMethod::Indent),
            "expr" => Ok(FoldMethod::Expr),
            "marker" => Ok(FoldMethod::Marker),
            "syntax" | "diff" => Err(FoldMethodErr::Unimplemented),
            _ => Err(FoldMethodErr::Unknown),
        }
    }
}

impl std::fmt::Display for FoldMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FoldMethod::Manual => "manual",
            FoldMethod::Indent => "indent",
            FoldMethod::Expr => "expr",
            FoldMethod::Marker => "marker",
        })
    }
}

/// Why a `'foldmethod'` value didn't apply (see [`FoldMethod::from_label`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FoldMethodErr {
    /// A value vim doesn't define at all (E474).
    Unknown,
    /// A real vim foldmethod nxvim hasn't implemented yet — fail loud naming it.
    Unimplemented,
}

/// The line-ending convention a buffer was read with and is written back with
/// (`'fileformat'`): `\n` (Unix), `\r\n` (Dos), or a lone `\r` (classic Mac). The rope
/// always stores `\n` internally (read normalizes to it; [`crate::Buffer::to_save_bytes`]
/// converts back on write), so this is the one place the on-disk line break is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileFormat {
    /// `\n` line endings (the internal form; the default for a new buffer).
    #[default]
    Unix,
    /// `\r\n` line endings.
    Dos,
    /// Lone `\r` line endings (classic Mac).
    Mac,
}

impl FileFormat {
    /// Parse a `'fileformat'` value (`"unix"`/`"dos"`/`"mac"`); `None` for anything else.
    pub fn from_label(label: &str) -> Option<FileFormat> {
        match label {
            "unix" => Some(FileFormat::Unix),
            "dos" => Some(FileFormat::Dos),
            "mac" => Some(FileFormat::Mac),
            _ => None,
        }
    }
}

impl std::fmt::Display for FileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FileFormat::Unix => "unix",
            FileFormat::Dos => "dos",
            FileFormat::Mac => "mac",
        })
    }
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
    /// Copy the previous line's indent onto a freshly-opened line (vim's
    /// `autoindent`). The grammar-free base autoindent: it fires when there is no
    /// treesitter verdict for the new line. Off by default, matching vim/neovim.
    pub autoindent: bool,
    /// Bracket-aware autoindent (vim's `smartindent`): a line opened after one
    /// whose last non-blank char is `{`, `(`, or `[` gains one
    /// [`shiftwidth`](BufferOptions::shiftwidth), and typing a closing bracket as
    /// the first non-blank char of a line re-indents it to its opener's level.
    /// Builds on the [`autoindent`](BufferOptions::autoindent) copy-previous base
    /// and, like it, is the no-treesitter fallback. Off by default.
    pub smartindent: bool,
    /// Auto-pair the bracket and quote delimiters `(`, `[`, `{`, `'`, `"`
    /// (nxvim's own, no vim equivalent): typing an opener inserts its closer and
    /// parks the cursor between them, typing the closer over an auto-inserted one
    /// steps past it, `<BS>` between an empty pair deletes both, and `<CR>`
    /// between an open/close pair lays the closer on its own dedented line. Off by
    /// default.
    pub autopairs: bool,
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
    /// The line-ending convention (`'fileformat'`): set from the bytes on read and
    /// honored on write (the rope always holds `\n`). Default [`FileFormat::Unix`].
    pub fileformat: FileFormat,
    /// How this buffer's folds are defined (`'foldmethod'`). Default
    /// [`FoldMethod::Manual`] (folds built by hand). [`FoldMethod::Indent`] derives
    /// the fold structure from leading indent; the editor recomputes it on edit and
    /// option change (see [`crate::editor::fold`]). Buffer-local, as in vim.
    pub foldmethod: FoldMethod,
    /// Deepest nesting a *computed* foldmethod produces (`'foldnestmax'`). A line's
    /// indent-derived level is capped at this so deeply-indented code doesn't fold
    /// into runaway nesting. Default 20 (vim's default). Ignored by `manual`.
    pub foldnestmax: usize,
    /// Minimum number of lines a fold must span to display *closed*
    /// (`'foldminlines'`). A computed fold of `≤ foldminlines` lines stays open even
    /// when its level says it should close. Default 1 (vim's default ⇒ a fold needs
    /// two or more lines to collapse). Ignored by `manual` (a hand-made `zf` over a
    /// single line is already rejected).
    pub foldminlines: usize,
    /// Whether the buffer's text may be changed (`'modifiable'`). Default `true`.
    /// When `false`, edits are refused with `E21` at the same chokepoints as a
    /// read-only [`crate::BufferKind`] (via [`crate::Editor::modifiable`]) — vim's
    /// `nomodifiable`. nxvim uses it for its built-in read-only **scratch listings**
    /// (`:messages`, `:registers`, `:LspInfo`, …): an ordinary buffer in a bottom
    /// window whose content is editor-generated, navigated like any buffer but not
    /// edited. Distinct from the `BufferKind` markers (which also gate read-only):
    /// those mark *what a buffer is*, this is a plain per-buffer toggle on an
    /// otherwise-ordinary buffer, settable with `:setlocal [no]modifiable`.
    pub modifiable: bool,
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
            // Indent/auto-pair conveniences are opt-in (vim/neovim default them
            // off); a config or filetype rule enables them.
            autoindent: false,
            smartindent: false,
            autopairs: false,
            // No buffer-local override out of the box: follow the global option.
            regexsyntax: RegexSyntax::Inherit,
            // UTF-8 on disk by default; no BOM. Read detection (a later phase)
            // overrides both per buffer.
            fileencoding: Encoding::UTF8,
            bomb: false,
            // \n line endings by default; read detection overrides per buffer.
            fileformat: FileFormat::Unix,
            // Folds are hand-made out of the box (vim's default); a config or
            // filetype rule opts a buffer into a computed source.
            foldmethod: FoldMethod::Manual,
            // vim's `foldnestmax`/`foldminlines` defaults.
            foldnestmax: 20,
            foldminlines: 1,
            // An ordinary buffer is editable; the read-only scratch listings flip
            // this to false at creation.
            modifiable: true,
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
pub enum OptKind {
    Bool,
    Num,
    Str,
}

impl OptKind {
    /// A stable lowercase tag (`"bool"` / `"number"` / `"string"`) for the catalog
    /// API — what `:set` completion shows and what `nx._options_catalog()` exposes.
    pub fn as_str(self) -> &'static str {
        match self {
            OptKind::Bool => "bool",
            OptKind::Num => "number",
            OptKind::Str => "string",
        }
    }
}

/// Which scope an option lives in: a global editor setting, a window-local, or a
/// buffer-local. Shown alongside the option's help in `:set` completion so the
/// reader knows whether `:setlocal` matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptScope {
    Global,
    Window,
    Buffer,
}

impl OptScope {
    pub fn as_str(self) -> &'static str {
        match self {
            OptScope::Global => "global",
            OptScope::Window => "window",
            OptScope::Buffer => "buffer",
        }
    }
}

/// Static documentation for one settable option — the single source of truth for
/// "what options exist", their abbreviations, value kinds, scopes, and one-line
/// help. [`canonical`] resolves names/abbreviations against this table, and
/// [`options_catalog`] hands it to `:set` command-line completion (so the option
/// list and its docs can never drift from what `:set` actually accepts).
#[derive(Debug, Clone, Copy)]
pub struct OptionInfo {
    /// The canonical spelling (e.g. `"number"`).
    pub name: &'static str,
    /// The standard short form (e.g. `"nu"`), or `None` when there is none.
    pub abbrev: Option<&'static str>,
    /// Whether the value is a boolean, a number, or a string.
    pub kind: OptKind,
    /// Where the option lives (global / window-local / buffer-local).
    pub scope: OptScope,
    /// A one-line description shown in the `:set` completion docs pane.
    pub doc: &'static str,
}

/// The documented catalog of every settable option, grouped by scope. This is the
/// authoritative list — [`canonical`] and `:set` completion both read it.
static OPTIONS: &[OptionInfo] = {
    use OptKind::{Bool, Num, Str};
    use OptScope::{Buffer, Global, Window};
    &[
        // ---- Window-local --------------------------------------------------------
        OptionInfo {
            name: "number",
            abbrev: Some("nu"),
            kind: Bool,
            scope: Window,
            doc: "Show the absolute line number in front of each line.",
        },
        OptionInfo {
            name: "relativenumber",
            abbrev: Some("rnu"),
            kind: Bool,
            scope: Window,
            doc: "Show line numbers relative to the cursor line.",
        },
        OptionInfo {
            name: "cursorline",
            abbrev: Some("cul"),
            kind: Bool,
            scope: Window,
            doc: "Highlight the screen line the cursor is on.",
        },
        OptionInfo {
            name: "wrap",
            abbrev: None,
            kind: Bool,
            scope: Window,
            doc: "Wrap long lines to the window width instead of scrolling off-screen.",
        },
        OptionInfo {
            name: "foldenable",
            abbrev: Some("fen"),
            kind: Bool,
            scope: Window,
            doc: "Display closed folds collapsed; off shows every line.",
        },
        OptionInfo {
            name: "foldcolumn",
            abbrev: Some("fdc"),
            kind: Num,
            scope: Window,
            doc: "Width of the gutter column that shows fold markers.",
        },
        OptionInfo {
            name: "foldlevel",
            abbrev: Some("fdl"),
            kind: Num,
            scope: Window,
            doc: "Folds deeper than this display closed; 0 closes all.",
        },
        OptionInfo {
            name: "numberwidth",
            abbrev: Some("nuw"),
            kind: Num,
            scope: Window,
            doc: "Minimum number of columns reserved for the line-number column.",
        },
        OptionInfo {
            name: "signcolumn",
            abbrev: Some("scl"),
            kind: Str,
            scope: Window,
            doc: "When to draw the sign column: auto, yes, no, or number.",
        },
        OptionInfo {
            name: "breakindent",
            abbrev: Some("bri"),
            kind: Bool,
            scope: Window,
            doc: "Indent wrapped lines to line up with the start of the line.",
        },
        OptionInfo {
            name: "showbreak",
            abbrev: Some("sbr"),
            kind: Str,
            scope: Window,
            doc: "String shown at the start of each wrapped (continuation) line.",
        },
        OptionInfo {
            name: "breakindentopt",
            abbrev: Some("briopt"),
            kind: Str,
            scope: Window,
            doc: "Tuning for 'breakindent' (e.g. shift:n, min:n, sbr).",
        },
        OptionInfo {
            name: "fillchars",
            abbrev: Some("fcs"),
            kind: Str,
            scope: Window,
            doc: "Characters used to fill status lines and the end-of-buffer area.",
        },
        OptionInfo {
            name: "padding",
            abbrev: Some("pad"),
            kind: Str,
            scope: Window,
            doc: "Per-window content margin, CSS shorthand (nxvim extension).",
        },
        OptionInfo {
            name: "sidescroll",
            abbrev: Some("ss"),
            kind: Num,
            scope: Window,
            doc: "Minimum number of columns to scroll horizontally at a time.",
        },
        OptionInfo {
            name: "sidescrolloff",
            abbrev: Some("siso"),
            kind: Num,
            scope: Window,
            doc: "Minimum columns to keep to the left and right of the cursor.",
        },
        // ---- Buffer-local --------------------------------------------------------
        OptionInfo {
            name: "tabstop",
            abbrev: Some("ts"),
            kind: Num,
            scope: Buffer,
            doc: "Number of spaces a <Tab> in the file is displayed as.",
        },
        OptionInfo {
            name: "shiftwidth",
            abbrev: Some("sw"),
            kind: Num,
            scope: Buffer,
            doc: "Number of spaces used for each step of (auto)indent.",
        },
        OptionInfo {
            name: "softtabstop",
            abbrev: Some("sts"),
            kind: Num,
            scope: Buffer,
            doc: "Spaces a <Tab> feels like while editing (-1 follows 'shiftwidth').",
        },
        OptionInfo {
            name: "expandtab",
            abbrev: Some("et"),
            kind: Bool,
            scope: Buffer,
            doc: "Insert spaces instead of a real <Tab> character.",
        },
        OptionInfo {
            name: "autoindent",
            abbrev: Some("ai"),
            kind: Bool,
            scope: Buffer,
            doc: "Copy the indent of the previous line onto a new line.",
        },
        OptionInfo {
            name: "smartindent",
            abbrev: Some("si"),
            kind: Bool,
            scope: Buffer,
            doc: "Bracket-aware autoindent: deeper after { ( [, dedent on closers.",
        },
        OptionInfo {
            name: "autopairs",
            abbrev: None,
            kind: Bool,
            scope: Buffer,
            doc: "Auto-close and pair-edit brackets and quotes: ( [ { ' \".",
        },
        OptionInfo {
            name: "regexsyntax",
            abbrev: Some("rxs"),
            kind: Str,
            scope: Buffer,
            doc: "Regex engine for search/substitute: auto, vim, or rust.",
        },
        OptionInfo {
            name: "fileencoding",
            abbrev: Some("fenc"),
            kind: Str,
            scope: Buffer,
            doc: "Character encoding written when the buffer is saved.",
        },
        OptionInfo {
            name: "bomb",
            abbrev: None,
            kind: Bool,
            scope: Buffer,
            doc: "Write a byte-order mark (BOM) at the start of the file.",
        },
        OptionInfo {
            name: "fileformat",
            abbrev: Some("ff"),
            kind: Str,
            scope: Buffer,
            doc: "Line-ending style written when saved: unix, dos, or mac.",
        },
        OptionInfo {
            name: "modifiable",
            abbrev: Some("ma"),
            kind: Bool,
            scope: Buffer,
            doc: "Allow the buffer's contents to be changed.",
        },
        OptionInfo {
            name: "foldmethod",
            abbrev: Some("fdm"),
            kind: Str,
            scope: Buffer,
            doc: "How folds are defined: manual, indent, expr, or marker.",
        },
        OptionInfo {
            name: "foldexpr",
            abbrev: Some("fde"),
            kind: Str,
            scope: Buffer,
            doc: "Expression folds are computed by (foldmethod=expr).",
        },
        OptionInfo {
            name: "foldmarker",
            abbrev: Some("fmr"),
            kind: Str,
            scope: Buffer,
            doc: "The start,end marker pair for foldmethod=marker.",
        },
        OptionInfo {
            name: "foldnestmax",
            abbrev: Some("fdn"),
            kind: Num,
            scope: Buffer,
            doc: "Maximum nesting depth for computed (indent) folds.",
        },
        OptionInfo {
            name: "foldminlines",
            abbrev: Some("fml"),
            kind: Num,
            scope: Buffer,
            doc: "Minimum line span for a fold to display closed.",
        },
        OptionInfo {
            name: "filetype",
            abbrev: Some("ft"),
            kind: Str,
            scope: Buffer,
            doc: "The buffer's filetype; drives syntax, indent, and plugins.",
        },
        OptionInfo {
            name: "commentstring",
            abbrev: Some("cms"),
            kind: Str,
            scope: Buffer,
            doc: "Template for a line comment, where %s is the comment text.",
        },
        OptionInfo {
            name: "ts_highlight",
            abbrev: None,
            kind: Bool,
            scope: Buffer,
            doc: "Enable tree-sitter highlighting for this buffer.",
        },
        // ---- Global --------------------------------------------------------------
        OptionInfo {
            name: "ignorecase",
            abbrev: Some("ic"),
            kind: Bool,
            scope: Global,
            doc: "Ignore case when searching.",
        },
        OptionInfo {
            name: "smartcase",
            abbrev: Some("scs"),
            kind: Bool,
            scope: Global,
            doc: "Override 'ignorecase' when the search pattern has uppercase.",
        },
        OptionInfo {
            name: "wrapscan",
            abbrev: Some("ws"),
            kind: Bool,
            scope: Global,
            doc: "Searches wrap around the end of the file.",
        },
        OptionInfo {
            name: "hlsearch",
            abbrev: Some("hls"),
            kind: Bool,
            scope: Global,
            doc: "Highlight all matches of the last search pattern.",
        },
        OptionInfo {
            name: "incsearch",
            abbrev: Some("is"),
            kind: Bool,
            scope: Global,
            doc: "Show where the search pattern matches as you type it.",
        },
        OptionInfo {
            name: "autoread",
            abbrev: Some("ar"),
            kind: Bool,
            scope: Global,
            doc: "Reread a file when it changes on disk and was not modified here.",
        },
        OptionInfo {
            name: "imagepreview",
            abbrev: Some("imgp"),
            kind: Bool,
            scope: Global,
            doc: "Render image files as images instead of raw bytes.",
        },
        OptionInfo {
            name: "showtabline",
            abbrev: Some("stal"),
            kind: Num,
            scope: Global,
            doc: "When to show the tab line: 0 never, 1 when >1 tab, 2 always.",
        },
        OptionInfo {
            name: "laststatus",
            abbrev: Some("ls"),
            kind: Num,
            scope: Global,
            doc: "When to show the status line: 0 never, 1 when >1, 2 always, 3 global.",
        },
        OptionInfo {
            name: "statusline",
            abbrev: Some("stl"),
            kind: Str,
            scope: Global,
            doc: "Format string controlling the status line's contents.",
        },
        OptionInfo {
            name: "tabline",
            abbrev: Some("tal"),
            kind: Str,
            scope: Global,
            doc: "Format string controlling the tab line's contents.",
        },
        OptionInfo {
            name: "guifont",
            abbrev: Some("gfn"),
            kind: Str,
            scope: Global,
            doc: "Font and size used by GUI clients (e.g. \"Iosevka:h14\").",
        },
        OptionInfo {
            name: "mouse",
            abbrev: None,
            kind: Str,
            scope: Global,
            doc: "Modes in which the mouse is enabled (e.g. a for all).",
        },
        OptionInfo {
            name: "mousemodel",
            abbrev: Some("mousem"),
            kind: Str,
            scope: Global,
            doc: "What the mouse buttons do: popup, popup_setpos, or extend.",
        },
        OptionInfo {
            name: "mousescroll",
            abbrev: None,
            kind: Str,
            scope: Global,
            doc: "Lines/columns scrolled per mouse-wheel step (ver:n,hor:n).",
        },
        OptionInfo {
            name: "mousetime",
            abbrev: Some("mouset"),
            kind: Num,
            scope: Global,
            doc: "Maximum time in ms between clicks for a multi-click.",
        },
        OptionInfo {
            name: "timeout",
            abbrev: Some("to"),
            kind: Bool,
            scope: Global,
            doc: "Wait timeoutlen for a mapped sequence to complete (off: forever).",
        },
        OptionInfo {
            name: "timeoutlen",
            abbrev: Some("tm"),
            kind: Num,
            scope: Global,
            doc: "Time in ms to wait for a mapped sequence to complete.",
        },
        OptionInfo {
            name: "fileencodings",
            abbrev: Some("fencs"),
            kind: Str,
            scope: Global,
            doc: "Encodings tried, in order, when reading a file.",
        },
        OptionInfo {
            name: "scrollanim",
            abbrev: Some("sca"),
            kind: Bool,
            scope: Global,
            doc: "Animate scrolling smoothly instead of jumping.",
        },
        OptionInfo {
            name: "scrollanimduration",
            abbrev: Some("scad"),
            kind: Num,
            scope: Global,
            doc: "Duration in ms of a smooth-scroll animation.",
        },
        OptionInfo {
            name: "scrollback",
            abbrev: Some("scbk"),
            kind: Num,
            scope: Global,
            doc: "Maximum number of lines kept in a terminal buffer's scrollback.",
        },
        OptionInfo {
            name: "history",
            abbrev: Some("hi"),
            kind: Num,
            scope: Global,
            doc: "Maximum number of entries kept in each command-line / search history ring.",
        },
        OptionInfo {
            name: "persisthistory",
            abbrev: Some("phisto"),
            kind: Str,
            scope: Global,
            doc: "Where command-line / search history persists: none, workspace, global (comma list).",
        },
        OptionInfo {
            name: "errorformat",
            abbrev: Some("efm"),
            kind: Str,
            scope: Global,
            doc: "Scanf-like patterns used to parse :make / quickfix output.",
        },
        OptionInfo {
            name: "switchbuf",
            abbrev: Some("swb"),
            kind: Str,
            scope: Global,
            doc: "How a jump picks its window (useopen, usetab, split, …).",
        },
        OptionInfo {
            name: "makeprg",
            abbrev: Some("mp"),
            kind: Str,
            scope: Global,
            doc: "Program run by :make.",
        },
        OptionInfo {
            name: "grepprg",
            abbrev: Some("gp"),
            kind: Str,
            scope: Global,
            doc: "Program run by :grep.",
        },
        OptionInfo {
            name: "grepformat",
            abbrev: Some("gfm"),
            kind: Str,
            scope: Global,
            doc: "Format of :grep output (like 'errorformat').",
        },
        OptionInfo {
            name: "qfdock",
            abbrev: Some("qfd"),
            kind: Bool,
            scope: Global,
            doc: "Open quickfix/loclist as bottom-dock tabs (nxvim) vs a split (vim).",
        },
        OptionInfo {
            name: "bdclosetab",
            abbrev: Some("bdct"),
            kind: Bool,
            scope: Global,
            doc: "Closing a tab's last buffer with :bd closes the tab (nxvim).",
        },
        OptionInfo {
            name: "relativesplits",
            abbrev: None,
            kind: Bool,
            scope: Global,
            doc: "Save session split sizes as proportional % (scale with the terminal).",
        },
        OptionInfo {
            name: "relativedocks",
            abbrev: None,
            kind: Bool,
            scope: Global,
            doc: "Save session dock sizes as % of the screen (vs absolute cells).",
        },
        OptionInfo {
            name: "equalalways",
            abbrev: Some("ea"),
            kind: Bool,
            scope: Global,
            doc: "Opening/closing a window re-equalizes all windows to even sizes.",
        },
        OptionInfo {
            name: "workspacepersistunnamed",
            abbrev: None,
            kind: Bool,
            scope: Global,
            doc: "Persist modified [No Name] buffers (with contents) in a workspace session.",
        },
    ]
};

/// The documented option catalog — the single source of truth for what `:set`
/// accepts. `:set` command-line completion reads this to offer option names with
/// their docs; [`canonical`] resolves against it.
pub fn options_catalog() -> &'static [OptionInfo] {
    OPTIONS
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
/// kind by scanning the documented [`OPTIONS`] catalog (the single source of truth,
/// so the names `:set` accepts can never drift from the names it completes). The
/// table has ~55 entries, so the linear scan is a microsecond per `:set` token.
fn canonical(name: &str) -> Option<(&'static str, OptKind)> {
    OPTIONS
        .iter()
        .find(|o| o.name == name || o.abbrev == Some(name))
        .map(|o| (o.name, o.kind))
}
