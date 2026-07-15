//! The embedded Lua VM and its Rust-facing API. [`LuaRuntime`] owns the `mlua`
//! state and the [`Shared`] effect buffer; its methods are the only way the
//! server talks to Lua — running chunks / callbacks, pushing the Rust→Lua state
//! mirrors (buffers, diagnostics, clients), and draining the queued effects. The
//! `vim.*` surface it drives is installed by [`crate::install`] and layered with
//! the `src/prelude/` Lua modules in [`LuaRuntime::new`].

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Table};
use serde::Serialize;

use crate::convert::{json_to_lua, lua_int, lua_to_rmpv};
use crate::host::seed_package_path;
use crate::install::fs_stat_table;
use crate::install::{install_runtime_api, install_vim};
use crate::ops::{
    BufOp, CallbackArgs, CompletePush, CompleteSetupReq, ConfirmReq, DecorPublish, DiagnosticData,
    DockOp, ExtmarkOp, FeedKeysOp, FsValue, GlobalOptionOp, HlSet, HttpServerRequest,
    InlayHintMirrorData, LayerOp, LoopOp, LspClientData, LspOp, NamedListOp, PanelOp,
    PickerOpenReq, PickerPush, QfSetOp, RawKeymap, RawRhs, RegisterSetOp, SemanticTokenData,
    SnippetAddReq, SnippetSetupReq, StatuslinePublishReq, StatuslineSetupReq, TabOp,
    TerminalOpenReq, TsOp, UiFloatReq, UiInputReq, UiSelectReq, ViewOp, WindowOp,
    WorkspaceOptionOp,
};

/// `skip_serializing_if` predicate: drop a `false` flag from the serialized
/// table so an unset attribute is *absent*, not `false` — matching how neovim's
/// API (`nvim_get_hl`) reports only the attributes that are set.
fn is_false(b: &bool) -> bool {
    !*b
}

/// `skip_serializing_if` predicate for the float's `win`: omit it when `0` (no
/// parent), the sentinel the server uses for a non-`relative="win"` float.
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Stringify one `'foldexpr'` per-line result for [`LuaRuntime::eval_foldexpr_lines`]:
/// a number becomes its integer text (`1`, `-1`, …), a string passes through
/// (`">1"`, `"="`, …), and any other type reads as `"0"` — the line isn't folded.
fn foldexpr_value_string(value: &mlua::Value) -> String {
    match value {
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => (*n as i64).to_string(),
        mlua::Value::String(s) => s.to_string_lossy(),
        _ => "0".to_string(),
    }
}

/// The outcome of resolving a command-line completion request
/// (`Runtime::run_cmdline_complete`): either inline catalog candidates for the
/// wildmenu, or a signal that the Lua source launched the file picker instead
/// (`:e <Tab>` and the other file-taking commands) — the server then dismisses
/// the command line and lets the queued picker take over.
#[derive(Clone, Debug)]
pub enum CmdlineComplete {
    /// `(label, insert, doc)` rows for the wildmenu (command names / `:set` options).
    Candidates(Vec<(String, String, String)>),
    /// The source opened the file picker; there is no wildmenu to render.
    PickerLaunched,
}

/// One window's row in the Rust→Lua window mirror, in layout order. The
/// number/relativenumber flags back `vim.wo`'s wired window-local options;
/// `float` carries a floating window's placement so `nvim_win_get_config` reads
/// it from Lua (`None` for a tiled window). Serialized into its Lua table by
/// `to_value`; the field names are the table keys, so they match the shape the
/// `nvim_win_*` getters expect.
#[derive(Clone, Debug, Default, Serialize)]
pub struct WindowMirror {
    pub id: u64,
    pub buffer: u64,
    /// 1-based cursor row, neovim convention.
    pub row: u64,
    /// 0-based cursor column.
    pub col: u64,
    pub width: u64,
    /// Text rows (the rect height minus the status line).
    pub height: u64,
    pub number: bool,
    pub relativenumber: bool,
    /// `'cursorline'` — whether the cursor's screen line is highlighted (so
    /// `vim.wo`/`vim.o` read it back).
    pub cursorline: bool,
    /// `'wrap'` — whether long lines soft-wrap (so `vim.wo`/`vim.o` read it back;
    /// also what makes a programmatic `vim.wo.wrap = true` reach the core).
    pub wrap: bool,
    /// `'scrollanim'` — whether this window's viewport scrolls animate, **resolved**
    /// (the window-local override or, when unset, the global default), so
    /// `vim.wo[win].scrollanim` reads back the value in effect.
    pub scrollanim: bool,
    /// `numberwidth` — the minimum number-gutter width (so `vim.wo`/`vim.o` read it
    /// back).
    pub numberwidth: u64,
    /// `signcolumn` in its string form (`no`/`auto`/`auto:1-3`/`yes`/`yes:2`),
    /// for `vim.wo`/`vim.o` read-back.
    pub signcolumn: String,
    /// `'fillchars'` as the raw `key:char` list string (empty ⇒ vim's default
    /// look), for `vim.wo`/`vim.o` read-back.
    pub fillchars: String,
    /// `'padding'` in its canonical shorthand string (`""` ⇒ no margin; `"2"`
    /// uniform; `"1 2"` vert/horiz; `"1 2 3 4"` top/right/bottom/left), for
    /// `vim.wo`/`vim.o` read-back.
    pub padding: String,
    /// First visible buffer line, 1-based (neovim's `winsaveview().topline`).
    pub topline: u64,
    /// First visible screen column (`winsaveview().leftcol`).
    pub leftcol: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub float: Option<FloatMirror>,
    /// This window's jumplist for `vim.fn.getjumplist`, oldest-first. Omitted from
    /// the table when empty (the common case), so an unused jumplist costs nothing
    /// across the mirror.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jumps: Vec<JumpMirror>,
    /// The jumplist navigation pointer (`getjumplist`'s `curidx`): a 0-based index
    /// into `jumps`, equal to `jumps.len()` when sitting at the present (not
    /// navigating with `<C-o>`/`<C-i>`).
    pub jump_idx: u64,
}

/// One jumplist entry's row in a [`WindowMirror`], pre-shaped into the dict
/// `vim.fn.getjumplist` returns: `bufnr`, `lnum` (1-based, the server adds the
/// `+1`), `col` (0-based byte), and `coladd` (always `0` — nxvim has no
/// `virtualedit`). Serialized by `to_value`, so the field names are the table keys.
#[derive(Clone, Debug, Serialize)]
pub struct JumpMirror {
    pub bufnr: u64,
    pub lnum: u64,
    pub col: u64,
    pub coladd: u64,
}

/// A floating window's placement for the [`WindowMirror`], pre-formatted into the
/// strings `nvim_win_get_config` returns (the server translates the core's
/// `FloatConfig` enums into these so nxvim-lua stays free of the core's types).
#[derive(Clone, Debug, Serialize)]
pub struct FloatMirror {
    /// `"editor"` / `"win"` / `"cursor"`.
    pub relative: String,
    /// The parent window for `relative == "win"`, else `0` (omitted).
    #[serde(skip_serializing_if = "is_zero")]
    pub win: u64,
    /// `"NW"` / `"NE"` / `"SW"` / `"SE"`.
    pub anchor: String,
    pub row: i64,
    pub col: i64,
    pub width: u64,
    pub height: u64,
    /// The high-level alignment word (`"top-right"` / `"center"` / …) when the float
    /// is aligned, omitted for the low-level `anchor`/`row`/`col` form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub align: Option<String>,
    pub zindex: u64,
    pub focusable: bool,
    /// `"none"` / `"single"` / `"rounded"` / `"double"` / `"solid"`.
    pub border: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// One tab page's row in the Rust→Lua tab mirror, in tabline order. Backs the
/// `vim.api.nvim_tabpage_*` reads (`list_wins`/`get_win`/`is_valid`/`get_number`)
/// the same way [`WindowMirror`] backs the window getters, so they resolve from
/// Lua without an RPC round-trip.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TabMirror {
    pub id: u64,
    /// The tab's window ids, in its in-tab layout order.
    pub windows: Vec<u64>,
    /// The buffer shown in each window, parallel to `windows`. Lets
    /// `vim.fn.tabpagebuflist` resolve an inactive tab's buffers (the global
    /// window mirror only carries the current tab's windows).
    pub buffers: Vec<u64>,
    /// The tab's focused window id (`nvim_tabpage_get_win`).
    pub current_window: u64,
}

/// One quickfix entry's row in the Rust→Lua `nx._qflist` mirror, pushed before
/// each chunk so `vim.fn.getqflist()` reads the live list. Fields mirror the dict
/// vim's `getqflist()` returns (minus the buffer-resolved extras Phase 1 omits).
#[derive(Clone, Debug, Default)]
pub struct QfMirror {
    pub filename: String,
    pub bufnr: i32,
    pub module: String,
    pub lnum: i64,
    pub end_lnum: i64,
    pub col: i64,
    pub end_col: i64,
    pub vcol: bool,
    pub nr: i32,
    pub pattern: String,
    pub text: String,
    /// Type char as a string (`"E"`/`"W"`/`"I"`/`"N"`), empty if none.
    pub typ: String,
    pub valid: bool,
}

/// One extmark's row in the Rust→Lua extmark mirror, pushed before each chunk so
/// `nvim_buf_get_extmarks` reads positions current with the buffer. `(row, col)`
/// are 0-based, the server having converted the byte anchors against the rope.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ExtmarkMirror {
    pub ns: u32,
    pub id: u64,
    pub row: u64,
    pub col: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_row: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_col: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hl_group: Option<String>,
    pub priority: u32,
    /// The mark's gutter sign glyph (`sign_text`), if any — round-tripped so a
    /// `get_extmarks(details=true)` AFTER the tick (when the server refreshes this
    /// mirror) still surfaces it, not just the same-chunk write-through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sign_hl_group: Option<String>,
    /// The `line_fill` text / hl (round-tripped like the signs, so a details read
    /// after the tick still surfaces the whole-line fill the mark carries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_fill_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_fill_hl: Option<String>,
}

/// One highlight group's row in the Rust→Lua highlight mirror (`nx._hl_defs`),
/// pushed when the core registry changes so `nvim_get_hl` reads live definitions.
/// Colors ride as the `0xRRGGBB` integers neovim's API reports; a `link` group
/// carries only `link` (its attrs are ignored, matching neovim), and the Lua side
/// follows the chain when asked for the resolved form (`{ link = false }`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct HlDefMirror {
    /// The namespace this group lives in (`0` is global). Keys the per-namespace
    /// mirror push, so it isn't part of the serialized row.
    #[serde(skip)]
    pub ns: u32,
    /// The group name keys the mirror table (`nx._hl_defs[name]`), so it isn't a
    /// field of the serialized entry.
    #[serde(skip)]
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fg: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bg: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sp: Option<u32>,
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub underline: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub undercurl: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub strikethrough: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub reverse: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// One buffer's row in the Rust→Lua buffer mirror (`nx._buffers[bufnr]`). `lines`
/// rides only on the ticks where the buffer changed (the server passes `None` to
/// reuse the table already in Lua, the bulk the per-chunk push otherwise skips);
/// `bufnr` is both the mirror key and a field the entry carries.
#[derive(Clone, Debug, Serialize)]
pub struct BufMirror {
    pub bufnr: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<String>>,
    pub name: String,
    /// Whether this buffer belongs to the **focused** window layer (the main area
    /// or whichever dock currently holds focus). Backs `nx.buf.list{ focused = true }`,
    /// the per-region buffer list — the buffer list is scoped per layer (see
    /// `OpenBuffer::layer` in core).
    pub focused: bool,
}

/// One buffer's wired buffer-local options (`nx._bo_mirror[bufnr]`) read by
/// `vim.bo` / `nvim_get_option_value`. `bufnr` keys the table, so it isn't a
/// field of the serialized entry.
#[derive(Clone, Debug, Serialize)]
pub struct BoMirror {
    #[serde(skip)]
    pub bufnr: u64,
    pub tabstop: usize,
    pub shiftwidth: usize,
    pub softtabstop: isize,
    pub expandtab: bool,
    /// The buffer's `'autoindent'` / `'smartindent'` flags (the grammar-free
    /// indent fallbacks) and `'autopairs'` flag (bracket/quote auto-pairing),
    /// mirrored so `vim.bo.autoindent` / `.smartindent` / `.autopairs` read the
    /// core's value regardless of who set it (`:set`, `vim.bo`).
    pub autoindent: bool,
    pub smartindent: bool,
    pub autopairs: bool,
    /// Whether `=` reindents blank lines (`'indentemptylines'`), mirrored so
    /// `nx.bo.indentemptylines` / `vim.bo.indentemptylines` read the core value.
    pub indentemptylines: bool,
    /// The buffer's *effective* `'regexsyntax'` dialect (`"pcre"`/`"vim"`) — its
    /// local override resolved against the global, so `vim.bo.regexsyntax` reads
    /// what `/`/`:s` actually use in this buffer.
    pub regexsyntax: String,
    /// The buffer's `'fileencoding'` (the on-disk charset, e.g. `"utf-8"` /
    /// `"latin1"`), mirrored so `vim.bo.fileencoding` reads the core's value
    /// regardless of who set it (`:set fenc`, `vim.bo`, read-detection).
    pub fileencoding: String,
    /// The buffer's `'bomb'` flag (whether a BOM is written), mirrored for
    /// `vim.bo.bomb`.
    pub bomb: bool,
    /// The buffer's `'fileformat'` (`"unix"`/`"dos"`/`"mac"`), mirrored so
    /// `nx.bo.fileformat` / `vim.bo.fileformat` read the core's value (set from the
    /// bytes on read or by `:set ff=`).
    pub fileformat: String,
    pub modified: bool,
    /// The buffer's filetype (the treesitter language noun) — explicit override
    /// or extension-derived, mirrored so `nx.bo.filetype` / `vim.bo.filetype` read
    /// the core's value regardless of who set it (`:set`, `nx.bo`, `:setf`). Empty
    /// when the buffer has no filetype.
    pub filetype: String,
    /// Whether treesitter highlighting is enabled for the buffer (the
    /// `ts_highlight` noun); mirrored so `nx.bo.ts_highlight` reads the core flag.
    pub ts_highlight: bool,
    /// The buffer's *effective* `'commentstring'` — its explicit override, or the
    /// filetype's built-in default when none — mirrored so `nx.bo.commentstring`
    /// reads what `gc`/`gcc` actually wrap lines with.
    pub commentstring: String,
    /// The buffer's `'modifiable'` flag, mirrored so `vim.bo.modifiable` reads it and
    /// `nx.buf.set_lines` can fail loud *before* queueing an edit a `nomodifiable`
    /// buffer would refuse.
    pub modifiable: bool,
}

/// One buffer change projected into neovim's `nvim_buf_attach` `on_bytes`
/// argument tuple, fired through [`LuaRuntime::fire_buf_bytes`] so an attached
/// `on_bytes` callback can edit its state and reparse incrementally. The
/// row/col fields are *relative* deltas exactly as neovim's `on_bytes` reports
/// them (see the server's `on_bytes_edit`, which builds these); the runtime
/// forwards them verbatim to the prelude's `nx._buf_bytes_changed`.
#[derive(Clone, Debug)]
pub struct BufBytesEdit {
    pub bufnr: u64,
    pub tick: u64,
    pub start_row: u64,
    pub start_col: u64,
    pub start_byte: u64,
    pub old_row: u64,
    pub old_col: u64,
    pub old_byte: u64,
    pub new_row: u64,
    pub new_col: u64,
    pub new_byte: u64,
}

/// The wired global options (`nx._go_mirror`) read by `vim.o`. Serialized as one
/// flat table, so the awkward positional signature the hand-rolled setter carried
/// becomes a single struct the caller fills by field name.
#[derive(Clone, Debug, Serialize)]
pub struct GoMirror {
    pub ignorecase: bool,
    pub smartcase: bool,
    pub wrapscan: bool,
    pub hlsearch: bool,
    pub incsearch: bool,
    pub showtabline: u8,
    pub laststatus: u8,
    pub statusline: String,
    pub tabline: String,
    pub guifont: String,
    /// The `'regexsyntax'` dialect (`"pcre"`/`"vim"`) backing `vim.o.regexsyntax`.
    pub regexsyntax: String,
    /// The `'fileencodings'` read-detection list (comma-separated) backing
    /// `vim.o.fileencodings`.
    pub fileencodings: String,
    /// `'autoread'` — whether `:checktime` silently reloads an externally-changed,
    /// unmodified buffer. Backs `vim.o.autoread`.
    pub autoread: bool,
    /// `'imagepreview'` — whether image files open as rendered previews rather than
    /// raw bytes. Backs `vim.o.imagepreview` / `nx.o.imagepreview`.
    pub imagepreview: bool,
    /// `'httphost'` — the interface `nx.http.mount` binds its listener on. Backs
    /// `vim.o.httphost` / `nx.o.httphost`.
    pub httphost: String,
    /// `'httpport'` — the port `nx.http.mount` binds its listener on (`0` picks a free
    /// one). Backs `vim.o.httpport` / `nx.o.httpport`.
    pub httpport: u16,
    /// `'scrollanim'` — whether viewport scrolls animate as a slide. Backs
    /// `vim.o.scrollanim`.
    pub scrollanim: bool,
    /// `'scrollanimduration'` — the scroll-animation duration ceiling in ms (`0`
    /// disables). Backs `vim.o.scrollanimduration`.
    pub scrollanimduration: u64,
    /// `'scrollback'` — the terminal scrollback cap in rows. Backs `vim.o.scrollback`.
    pub scrollback: u64,
    /// `'history'` — the per-ring history cap. Backs `vim.o.history` / `nx.o.history`.
    pub history: u64,
    /// `'persisthistory'` — where history persists (`none`/`workspace`/`global`, comma
    /// list). Backs `vim.o.persisthistory` / `nx.o.persisthistory`.
    pub persisthistory: String,
    /// `'timeout'` — whether a mapped sequence waits `timeoutlen` to resolve (on) or
    /// is held forever for the next key (off). Backs `vim.o.timeout` / `nx.o.timeout`.
    pub timeout: bool,
    /// `'timeoutlen'` — the mapped-sequence wait in ms. Backs `vim.o.timeoutlen`.
    pub timeoutlen: u64,
    /// The editor screen extent backing `vim.o.columns` / `vim.o.lines`, so a
    /// float-positioning plugin can center its windows.
    pub columns: u64,
    pub lines: u64,
    /// The quickfix `'errorformat'` / `'switchbuf'` and the `:make`/`:grep`
    /// programs + grep parser, so a config reading them through `vim.o` sees the
    /// live value (and a `vim.o.makeprg = …` write round-trips).
    pub errorformat: String,
    pub switchbuf: String,
    pub makeprg: String,
    pub grepprg: String,
    pub grepformat: String,
    /// `'qfdock'` — whether quickfix/location lists open as bottom-dock tabs (nxvim)
    /// vs a split (vim/telescope). Backs `vim.o.qfdock` / `nx.o.qfdock`.
    pub qfdock: bool,
    /// `'bdclosetab'` — whether `:bd` of a tab's last buffer closes the tab (nxvim)
    /// rather than loading a sibling. Backs `vim.o.bdclosetab` / `nx.o.bdclosetab`.
    pub bdclosetab: bool,
    /// `'relativesplits'` — whether a saved session stores split sizes as proportional
    /// percentages (default true). Backs `nx.o.relativesplits`. The serialized Lua mirror
    /// key must equal the canonical option name (squashed, no `_`), which the snake_case
    /// Rust field doesn't — so it's renamed for the `nx._go_mirror` table.
    #[serde(rename = "relativesplits")]
    pub relative_splits: bool,
    /// `'relativedocks'` — whether a saved session stores dock sizes as a percentage
    /// of the screen (default false). Backs `nx.o.relativedocks`.
    #[serde(rename = "relativedocks")]
    pub relative_docks: bool,
    /// `'equalalways'` — whether opening/closing a window re-equalizes all windows
    /// to even sizes (default true). Backs `vim.o.equalalways` / `nx.o.equalalways`.
    pub equalalways: bool,
    /// `'workspacepersistunnamed'` — whether a workspace session persists modified
    /// `[No Name]` buffers with their contents (default true). Backs
    /// `nx.o.workspacepersistunnamed`.
    #[serde(rename = "workspacepersistunnamed")]
    pub workspace_persist_unnamed: bool,
}

/// The pure-Lua `vim.*` prelude, split into focused modules under `src/prelude/`
/// and loaded in this order at VM init — the order is significant (a later module
/// reads `vim.*` an earlier one installed), so it mirrors the original single
/// file top to bottom: the core stdlib first, the runtime/registry services, then
/// the editor-facing surfaces. `(chunk name, source)`; the name shows up in Lua
/// tracebacks.
const PRELUDE_MODULES: &[(&str, &str)] = &[
    // Pure helpers first (nx.tbl / nx.list / nx.str / nx.iter + bit), then the VM
    // runtime services (schedule / notify / callbacks).
    ("nxvim:prelude/stdlib", include_str!("prelude/stdlib.lua")),
    ("nxvim:prelude/runtime", include_str!("prelude/runtime.lua")),
    // nx.promise / nx.async — Promises/A+ over the deferral primitives (nx.schedule
    // for microtasks, nx.timer for nx.promise.delay), both from runtime.lua just
    // above. It is the async FOUNDATION every later surface builds on (process /
    // picker / complete / nx.ui), so it loads early — right after the runtime
    // services it needs and before any of them.
    ("nxvim:prelude/promise", include_str!("prelude/promise.lua")),
    // The Rust↔Lua mirror state, shared resolvers, context lock, and the scalar
    // surfaces (variables / options / registers) the entity API reads.
    ("nxvim:prelude/state", include_str!("prelude/state.lua")),
    // The shared window-geometry normalizer (`nx._geom`): one place that validates
    // size specs / alignment words / margin and emits the wire shape every surface
    // (floats, `nx.view`, pickers, the panel) sends. Loaded before its consumers.
    (
        "nxvim:prelude/geometry",
        include_str!("prelude/geometry.lua"),
    ),
    // The namespace / highlight / extmark surface (`nx.ns` / `nx.hl` / the extmark
    // decoration layer) and the two current-handle getters the option/variable
    // scopes read. Reads the mirror state / resolvers from state.lua above.
    ("nxvim:prelude/api", include_str!("prelude/api.lua")),
    // Autocmds / augroups / user commands / ex-command drivers / vim.cmd.
    ("nxvim:prelude/autocmd", include_str!("prelude/autocmd.lua")),
    ("nxvim:prelude/keymap", include_str!("prelude/keymap.lua")),
    // nx.view: plugin-owned, dockable read-only content surfaces (the file-tree /
    // list widget). Loads after keymap (which seeds `nx.view.actions` + the `view`
    // bucket defaults) over the `nx.view._*` Rust bridges.
    ("nxvim:prelude/view", include_str!("prelude/view.lua")),
    // The async nx.ui.* primitives (input / select / confirm return promises; float
    // is fire-and-forget). The deferral primitives nx.schedule / nx.timer live in
    // runtime.lua; this module is the UI surface only.
    ("nxvim:prelude/ui", include_str!("prelude/ui.lua")),
    // nx.component / nx.view.component: the Vue-shaped component model (reactive state +
    // computed + pure render + lifecycle) with pluggable backends — the "view" backend
    // (focus-taking nx.view buffer) and the "float" backend (non-focus nx.ui.float). Loads
    // after view + ui so both surfaces it dispatches to exist; uses promise/async (above).
    (
        "nxvim:prelude/component",
        include_str!("prelude/component.lua"),
    ),
    // nx.run / nx.run_stream / nx.await_each: promise-only process API (replaces the
    // callback-shaped nx.spawn). After promise.lua (builds on nx.promise/async).
    ("nxvim:prelude/process", include_str!("prelude/process.lua")),
    // nx.fs: promise-always filesystem API over the nx._fs_* bridge. After
    // promise.lua (every op returns a promise).
    ("nxvim:prelude/fs", include_str!("prelude/fs.lua")),
    // nx.http: promise-always HTTP client (fetch-modeled) over the nx._http_fetch
    // bridge. After promise.lua (returns a promise) and stdlib.lua (uses nx.json for
    // JSON bodies / `res:json()`).
    ("nxvim:prelude/http", include_str!("prelude/http.lua")),
    // nx.http.mount: the plugin HTTP *server* surface — the inbound twin of the fetch
    // client just above, which it extends the `nx.http` table with (so it must load
    // after it). Promise for the one-shot mount, handler for the request stream.
    (
        "nxvim:prelude/httpmount",
        include_str!("prelude/httpmount.lua"),
    ),
    // nx.hash: the hashing surface (one-shot string digests + the incremental
    // nx.hash.new) over the nx._hash / nx._hash_new bridges. After fs.lua, which
    // defines the sibling nx.hash.file (the streamed file digest, an fs op).
    ("nxvim:prelude/hash", include_str!("prelude/hash.lua")),
    // nx.markdown: the CommonMark+GFM renderer surface (nx.markdown.render) over the
    // pure nx._markdown_render bridge. Self-contained — needs only the native bridge.
    (
        "nxvim:prelude/markdown",
        include_str!("prelude/markdown.lua"),
    ),
    // nx.utils: the general helper namespace (nx.utils.debounce, …) — may build on
    // the timer and promise surfaces loaded just above.
    ("nxvim:prelude/utils", include_str!("prelude/utils.lua")),
    // nx.test: the plugin test framework (describe/it/expect + an async-aware test
    // context). Inert until a spec registers tests and the `--test-plugin` runner
    // calls `nx.test._run()`; builds on nx.async/await/wait_for/on_next_tick (promise
    // + runtime, above) and the nx.buf/cursor/keymap reads (api/keymap, above).
    ("nxvim:prelude/test", include_str!("prelude/test.lua")),
    // nx.picker: the fuzzy finder (sources + open) over the float-list widget.
    ("nxvim:prelude/picker", include_str!("prelude/picker.lua")),
    // nx.complete: the native completion engine (Phase 4-A, buffer source).
    (
        "nxvim:prelude/complete",
        include_str!("prelude/complete.lua"),
    ),
    // nx.cmdline_complete: command-line completion (the widget's fifth orchestration
    // — `<Tab>` command-name suggestions with a docs pane).
    (
        "nxvim:prelude/cmdline_complete",
        include_str!("prelude/cmdline_complete.lua"),
    ),
    // nx.snippet: the native snippet engine (tabstop session + snippets source).
    ("nxvim:prelude/snippet", include_str!("prelude/snippet.lua")),
    // nx.statusline: the declarative segment registry (lualine shape). Loads after
    // autocmd (it registers invalidation autocmds) and picker/complete.
    (
        "nxvim:prelude/statusline",
        include_str!("prelude/statusline.lua"),
    ),
    // nx.decor: viewport-scoped decoration providers (the registry + off-tick
    // dispatch; needs nx.ns from api and nx.notify from runtime, both above).
    ("nxvim:prelude/decor", include_str!("prelude/decor.lua")),
    // The file-explorer's visual layer (directory-row highlighting), authored as an
    // `nx.decor` provider — so it loads AFTER decor.lua, whose `nx.decor.provider`
    // it registers against.
    (
        "nxvim:prelude/explorer",
        include_str!("prelude/explorer.lua"),
    ),
    (
        "nxvim:prelude/diagnostic",
        include_str!("prelude/diagnostic.lua"),
    ),
    // nx.lsp.buf.hover / signature_help — the position-family verbs whose replies
    // render through the content float (over the guarded nx._lsp_buf bridge).
    ("nxvim:prelude/lsp", include_str!("prelude/lsp.lua")),
    // vim.fn editor-query read builtins (line / col / expand / pos / …) that read
    // the state mirror; loads after the surfaces they build on.
    ("nxvim:prelude/vimfn", include_str!("prelude/vimfn.lua")),
    // Loads last: the `nx.*` namespace's remaining surface (events, commands,
    // async). The variable/option/dispatch/keymap nouns are authored as `nx.*`
    // directly in the chunks above, each aliasing `vim.*` onto itself.
    ("nxvim:prelude/nx", include_str!("prelude/nx.lua")),
    // Built-in `.editorconfig` support. Loads AFTER nx.lua: it builds on `nx.on` /
    // `nx.augroup` (events) plus `nx.fs` (async, above), `nx.bo` (buffer options,
    // state.lua) and the `vim.g` / `vim.b` variable toggle. On by default; switch
    // off with `vim.g.editorconfig = false` (global) or `vim.b.editorconfig = false`
    // (per buffer).
    (
        "nxvim:prelude/editorconfig",
        include_str!("prelude/editorconfig.lua"),
    ),
    // The package manager (`nx.plugins`) — declarative install + lazy/eager load
    // over `git` (`nx.run`) and `nx.fs`. Loads AFTER nx.lua: it builds on
    // `nx.command` / `nx.on` (defined there) plus nx.run / nx.fs / nx.promise /
    // nx.keymap / nx.notify (all above), and the `nx._add_rtp` Rust bridge.
    ("nxvim:prelude/plugins", include_str!("prelude/plugins.lua")),
    // The package manager's UI (`nx.plugins.ui` / `:Plugins`) — the lazy-style
    // dashboard and the first-run welcome checklist. Loads AFTER plugins.lua (it reads
    // the manager state and subscribes via `M.on_change`) and after component.lua /
    // view.lua / api.lua (nx.view.component / nx.hl), all above.
    (
        "nxvim:prelude/plugins_ui",
        include_str!("prelude/plugins_ui.lua"),
    ),
];

/// A generated stub declaring the two root globals nxvim injects from **Rust**
/// (`install_vim` / `install_runtime_api`), so the Lua language server has a
/// definition site for them. Without it, `nx` still resolves (the prelude does
/// `nx = nx or {}`), but `vim` is only ever read via `local vim = vim` and never
/// assigned at global scope — so LuaLS reports `undefined global 'vim'` for every
/// `vim.*` alias. Declaring `vim` here gives the prelude's `vim.keymap`/`vim.g`/…
/// field writes a global to attach to (verified against lua-language-server 3.18).
const GLOBALS_STUB: &str = "\
-- Generated by `nxvim --extract-lua-runtime` — DO NOT EDIT.
-- Declares the globals nxvim injects from Rust so the Lua language server has a
-- definition site for them; the prelude modules alongside decorate these.
vim = vim or {}
nx = nx or {}
";

/// Write every embedded prelude Lua module to `dir` as a real file, so the Lua
/// language server can index nxvim's `nx.*` / `vim.*` API from source (point its
/// `workspace.library` at `dir`). Each `nxvim:prelude/<name>` chunk is written to
/// `<dir>/prelude/<name>.lua`, preserving the sub-path in the chunk name; a generated
/// `<dir>/nxvim-globals.lua` ([`GLOBALS_STUB`]) declares the Rust-injected globals so
/// `vim.*` resolves too. `dir` (and any parent it names) is created if absent. Returns
/// the written file paths.
pub fn extract_prelude(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut written = Vec::with_capacity(PRELUDE_MODULES.len() + 1);
    for (name, src) in PRELUDE_MODULES {
        // Chunk names are `nxvim:prelude/<name>`; drop the `nxvim:` scheme prefix
        // and lay the rest out as directories under `dir` (so `prelude/` is a real
        // subdir), then add the `.lua` extension.
        let rel = name.strip_prefix("nxvim:").unwrap_or(name);
        let path = dir.join(rel).with_extension("lua");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, src)?;
        written.push(path);
    }
    // The globals stub sits at the extraction root (create_dir_all above only made the
    // `prelude/` subdir; `dir` itself may still be missing if the tree was empty).
    std::fs::create_dir_all(dir)?;
    let stub = dir.join("nxvim-globals.lua");
    std::fs::write(&stub, GLOBALS_STUB)?;
    written.push(stub);
    Ok(written)
}

/// One line of captured Lua message output plus whether it was emitted as an
/// *error* (`nx.err_write*`) — so the server paints it red rather than routing it
/// through the plain message path.
pub struct OutputLine {
    pub text: String,
    pub error: bool,
}

/// One `nx.ui.input` completion candidate resolved by a prompt source: `(label,
/// insert, doc, range)` where `range` is the optional `(start_char, len_char)`
/// adapter-specified replace span (DAP `CompletionItem.start`/`length`).
pub(crate) type PromptCompleteCands = Vec<(String, String, String, Option<(usize, usize)>)>;

/// Side effects produced by running Lua, drained by the server.
#[derive(Default)]
pub(crate) struct Shared {
    /// Ex-commands requested via `vim.cmd(...)`.
    pub(crate) commands: Vec<String>,
    /// Text emitted via `print(...)` / `vim.api.nvim_echo(...)` / the error writers
    /// (`nx.err_write*`), each carrying whether it was an error so the server can
    /// route it through the red `echo_err` path.
    pub(crate) output: Vec<OutputLine>,
    /// Highlight-group definitions from `nvim_set_hl`, applied to the core
    /// registry after the chunk drains (so the core stays the sole mutator).
    pub(crate) highlights: Vec<HlSet>,
    /// Dock requests from `nx.dock.*`, applied to the core after the chunk.
    pub(crate) dock_ops: Vec<DockOp>,
    /// Layer crosses from `nx.open` / `nx.layer.*`, applied to the core after the
    /// chunk.
    pub(crate) layer_ops: Vec<LayerOp>,
    /// `nx.view` content / mount / lifecycle requests, applied to the core after the
    /// chunk.
    pub(crate) view_ops: Vec<ViewOp>,
    /// `nx.panel` open / close requests, applied to the core after the chunk.
    pub(crate) panel_ops: Vec<PanelOp>,
    /// Terminal-open requests from `nx.terminal.open`, applied to the core
    /// (`Editor::open_terminal`) after the chunk.
    pub(crate) terminal_ops: Vec<TerminalOpenReq>,
    /// Server-start requests from `nx.lsp.start` (driven by `nx.lsp.enable`),
    /// drained by the server into its `LspManager` after the chunk.
    pub(crate) lsp_ops: Vec<LspOp>,
    /// Async-runtime requests from `nx.schedule` / `nx.timer` (`vim.schedule` /
    /// `vim.defer_fn`) / async `vim.system` (`nx.run`), drained by the server into
    /// its scheduled-work queue and event-loop actor after the chunk.
    pub(crate) loop_ops: Vec<LoopOp>,
    /// Buffer-local option writes (`vim.bo`), drained by the server into the live
    /// editor after the chunk. (Buffer text/lifecycle mutation is not part of the API.)
    pub(crate) buf_ops: Vec<BufOp>,
    /// Extmark mutations from `nvim_buf_set_extmark` / `_del_extmark` /
    /// `_clear_namespace`, drained by the server into the target buffer's
    /// [`ExtmarkStore`](nxvim_core::ExtmarkStore) after the chunk.
    pub(crate) extmark_ops: Vec<ExtmarkOp>,
    /// Window mutations from the `vim.api.nvim_win_*` / `nvim_open_win` /
    /// `nvim_set_current_win` API, drained by the server into the live editor
    /// after the chunk (Phase 5).
    pub(crate) window_ops: Vec<WindowOp>,
    /// Tab-page mutations from `vim.api.nvim_set_current_tabpage`, drained by the
    /// server into the live editor after the chunk (Phase 3). Reads resolve from
    /// the `nx._tabs` mirror, so only the switch needs an op.
    pub(crate) tab_ops: Vec<TabOp>,
    /// Global-option writes from `vim.o` for a wired search option, drained by
    /// the server into the editor's global options after the chunk.
    pub(crate) global_ops: Vec<GlobalOptionOp>,
    /// Per-workspace option overrides from `nx.wso`, drained by the server into the
    /// editor's workspace overlay after the chunk (`Some` = set, `None` = clear).
    pub(crate) workspace_option_ops: Vec<WorkspaceOptionOp>,
    /// Treesitter bridge requests from `vim.treesitter.start` / `stop`, drained
    /// by the server into the editor's per-buffer treesitter override.
    pub(crate) ts_ops: Vec<TsOp>,
    /// Register writes from `vim.fn.setreg`, drained by the server into the
    /// editor's register file after the chunk. Reads resolve from the
    /// `nx._registers` mirror, so only the write needs an op.
    pub(crate) reg_ops: Vec<RegisterSetOp>,
    /// Clipboard seeds from `nx.test.clipboard.seed` (plugin-test seam), drained by
    /// the server into the editor's clipboard provider — `(text, linewise)`.
    pub(crate) clipboard_seeds: Vec<(String, bool)>,
    /// `vim.fn.setqflist` requests, drained by the server into the editor's
    /// quickfix list after the chunk. Reads resolve from the `nx._qflist` mirror.
    pub(crate) qf_ops: Vec<QfSetOp>,
    /// Named-list lifecycle ops (`nx.qf.show` / `nx.qf.drop` for a `kind = "list"`
    /// dynamic list), drained *after* [`Self::qf_ops`] so a show observes the
    /// refresh queued just before it.
    pub(crate) named_list_ops: Vec<NamedListOp>,
    /// `vim.ui.input` prompt requests, drained by the server into the editor's
    /// command line (`Editor::open_prompt`) after the chunk (Phase 8).
    pub(crate) ui_inputs: Vec<UiInputReq>,
    /// Candidate lists resolved by an `nx.ui.input{ complete = fn }` prompt source,
    /// drained by the server into the prompt wildmenu
    /// (`Editor::open_prompt_complete_menu`). Each candidate is `(label, insert, doc,
    /// range)`, where `range` is the adapter-specified replace span `(start_char,
    /// len_char)` (the DAP `CompletionItem.start`/`length`, 0-based char offset + char
    /// count) or `None`. The source may resolve a promise first, so an entry can land a
    /// tick after the request was stamped.
    pub(crate) prompt_complete_results: Vec<PromptCompleteCands>,
    /// `nx.ui.select` requests, drained by the server into the editor's floating
    /// selectable-list widget (`Editor::open_menu`) after the chunk.
    pub(crate) ui_selects: Vec<UiSelectReq>,
    /// `nx.ui.float` requests, drained by the server into the editor's list-less
    /// content float (`Editor::open_content_float`) after the chunk.
    pub(crate) ui_floats: Vec<UiFloatReq>,
    /// `nx.picker.open` requests, drained by the server into the editor's fuzzy
    /// finder (`Editor::open_picker`) after the chunk.
    pub(crate) picker_opens: Vec<PickerOpenReq>,
    /// `nx.picker.resume()` request: reopen the last resumable picker from its frozen
    /// snapshot (`Editor::restore_picker_snapshot`). A bare flag — at most one resume
    /// per chunk — drained by the server after re-arming `nx._picker` Lua-side.
    pub(crate) picker_resume: bool,
    /// `nx.complete.setup{}` configurations, drained by the server into
    /// `Editor::configure_complete` (the native completion engine, Phase 4-A).
    pub(crate) complete_setups: Vec<CompleteSetupReq>,
    /// `nx.cmdline_complete.setup{}` configurations (the command-line completion
    /// engine — the float-list widget's fifth orchestration). Each carries the
    /// `docs` flag; the server drains it into `Editor::configure_cmdline_complete`.
    /// Empty until a config arrives, so command-line completion is off by default.
    pub(crate) cmdline_complete_setups: Vec<bool>,
    /// Pending `nx._cmdline_set_arg(path)` requests — the file picker confirm pasting
    /// the chosen path into the still-open command line's argument token. The server
    /// drains each into `Editor::cmdline_replace_arg`.
    pub(crate) cmdline_set_args: Vec<String>,
    /// Pending `nx.complete.trigger()` requests (a manual completion open). Each is
    /// payload-free; the server runs `Editor::complete_manual_trigger` once if any
    /// arrived since the last drain.
    pub(crate) complete_triggers: Vec<()>,
    /// Pending `nx.complete.accept{ behavior = … }` requests: each is the behavior
    /// string (`"insert"` / `"replace"`, or `""` to use the configured default). The
    /// server accepts the highlighted row under that behavior — the remappable
    /// alternate to the default confirm key.
    pub(crate) complete_accepts: Vec<String>,
    /// Streamed **async** completion candidates (`nx.complete` source `push`),
    /// drained by the server (generation-gated) into `Editor::menu_push` to append
    /// to the open completion popup. Empty for a buffer-only config. Phase 4-B.
    pub(crate) complete_pushes: Vec<CompletePush>,
    /// Generations whose async completion sources have *all* finished streaming
    /// (`done()` reduced to zero pending sources, Lua-side), drained by the server
    /// into `Editor::complete_finish` so a prefix that matched nothing closes the
    /// confirmed-empty popup. Phase 4-B.
    pub(crate) complete_finishes: Vec<u64>,
    /// Resolved lazy docs for a plugin completion row (`nx._complete_resolve_done`):
    /// `(resolve id, doc text)`. The server issued `nx._complete_resolve(id)` for the
    /// selected row; the source's `resolve` callback responded, and this carries the
    /// docs back to the server's resolve cache for the sidebar. Phase 4-E.
    pub(crate) complete_resolve_dones: Vec<(u64, String)>,
    /// `nx.statusline.setup{}` segment layouts, drained by the server into its
    /// active `SegmentLayout` (which takes precedence over `'statusline'`).
    pub(crate) statusline_setups: Vec<StatuslineSetupReq>,
    /// Published custom-segment cells (`nx._statusline_publish`), drained by the
    /// server into its per-`(win, name)` statusline-segment cell cache.
    pub(crate) statusline_publishes: Vec<StatuslinePublishReq>,
    /// Custom-segment names marked dirty by `nx.statusline.invalidate(name)` (and
    /// the autocmd callbacks a declared `events` list installs). The server folds
    /// these into its pending-re-render set and re-renders them — per window, with
    /// a fresh window mirror — after the current input settles (`run_pending`), so
    /// a segment invalidated from an autocmd that fired with a stale mirror still
    /// renders against the post-transition window/focus state.
    pub(crate) statusline_invalidates: Vec<String>,
    /// `nx.snippet.setup{}` jump-key configurations, drained by the server into
    /// `Editor::set_snippet_keys`.
    pub(crate) snippet_setups: Vec<SnippetSetupReq>,
    /// `nx.snippet.add(ft, …)` registrations, drained by the server into its
    /// per-filetype snippet store for the `snippets` completion source.
    pub(crate) snippet_adds: Vec<SnippetAddReq>,
    /// `nx.snippet.expand(body)` requests — a snippet body to expand at the cursor,
    /// drained by the server which parses and expands it via `Editor::expand_snippet`.
    pub(crate) snippet_expands: Vec<String>,
    /// Streamed picker candidates (`nx.picker` source `push`), drained by the
    /// server (generation-gated) into `Editor::menu_push` after the chunk / a
    /// streaming `on_stdout`.
    pub(crate) picker_pushes: Vec<PickerPush>,
    /// Generations whose source run has completed (`done()`), drained by the server
    /// into `Editor::menu_finish` so a query that matched nothing clears the now
    /// stale results (one that matched swaps them via `picker_pushes` instead).
    pub(crate) picker_finishes: Vec<u64>,
    /// Named picker actions a `picker`-bucket keymap fired (`nx._picker_action`),
    /// drained by the server into `Editor::apply_picker_action` — the rebindable
    /// picker keys (next / prev / confirm / cancel / preview scroll / query edit).
    pub(crate) picker_actions: Vec<String>,
    /// Named `select` actions a `select`-bucket keymap fired (`nx._select_action`),
    /// drained by the server into `Editor::apply_select_action` — the rebindable
    /// `nx.ui.select` keys (next / prev / first / last / confirm / cancel).
    pub(crate) select_actions: Vec<String>,
    /// Named `view` actions a view buffer-local keymap fired (`nx._view_action`),
    /// drained by the server into `Editor::apply_view_action` — the `nx.view`
    /// activation key (`confirm`).
    pub(crate) view_actions: Vec<String>,
    /// Named `qf` actions a `FileType qf` buffer-local keymap fired
    /// (`nx._qf_action`), drained by the server into `Editor::apply_qf_action` — the
    /// quickfix / loclist activation key (`jump`).
    pub(crate) qf_actions: Vec<String>,
    /// Named `cmdline` actions a `cmdline`-bucket (`'c'`) keymap fired
    /// (`nx._cmdline_action`), drained by the server into `Editor::apply_cmdline_action`
    /// — the rebindable command-line keys (cancel / submit / backspace / delete /
    /// cursor motion / history / `<C-r>` register arm).
    pub(crate) cmdline_actions: Vec<String>,
    /// Marks a `nx.decor` provider published for a window's viewport
    /// (`nx._decor_publish`), drained by the server (generation-gated) into the
    /// provider's namespace in the extmark layer. Empty for a no-provider config.
    /// Phase 3 of `nx.decor`.
    pub(crate) decor_publishes: Vec<DecorPublish>,
    /// Whether any `nx.decor` provider has been registered (`nx._decor_register`).
    /// The gate the server checks before dispatching a viewport-change signal: while
    /// no provider is set it skips the whole off-tick decor path (never slices the
    /// visible lines, never re-enters Lua). Phase 2 of `nx.decor`.
    pub(crate) decor_active: bool,
    /// Whether the workspace session should CAPTURE the window/tab layout into the
    /// (namespaced) shada. Default `false`; a workspace plugin flips it on via
    /// `nx.shada.save_layout(true)` once it knows this is the right namespace. The
    /// server reads it at flush ([`LuaRuntime::session_save_layout`]). How sizes are
    /// stored (proportional vs absolute) is the native `'relativesplits'` /
    /// `'relativedocks'` options, read straight off the editor at capture — not
    /// mirrored here, so layout sizing isn't coupled to this plugin opt-in.
    pub(crate) session_save_layout: bool,
    /// The shada namespace this launch is scoped to (the `--shada-namespace` value, or a
    /// `--workspace`-derived token), surfaced read-only to Lua via `nx.shada.namespace()`.
    /// Seeded by the server from [`ServerInit`](nxvim_server) before any config runs — a
    /// *daemon* session derives it from the daemon's cwd, which the binary can't know up
    /// front, so this is a runtime value, not an env var. `None` = the global store.
    pub(crate) shada_namespace: Option<String>,
    /// The absolute workspace root for a `--workspace` launch (the daemon's directory for a
    /// remote session), surfaced via `nx.workspace.dir()` / `nx.workspace.active()`. `None`
    /// outside a workspace launch. Seeded alongside [`shada_namespace`](Self::shada_namespace).
    pub(crate) workspace_dir: Option<String>,
    /// Per-plugin isolated shada data: `namespace -> (key -> value)`, where each
    /// value is the plugin's serialized blob (JSON, from `nx.shada.plugin`). The
    /// live home of an opted-in plugin's cross-session store: the `nx._shada_plugin_*`
    /// bridges read/write it, the server seeds it at shada load
    /// ([`LuaRuntime::plugin_shada_seed`]) and harvests it at flush
    /// ([`LuaRuntime::plugin_shada_export`]). A `BTreeMap` so `:keys` and the
    /// exported order are deterministic. Empty for a config with no opted-in plugin.
    pub(crate) plugin_shada:
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    /// Whether any `nx.on_key_pending` listener has been registered
    /// (`nx._key_pending_register`). The gate the server checks before computing /
    /// pushing the pending-key signal: while no listener is set it never walks the
    /// trie for continuations or re-enters Lua, so a no-which-key config pays nothing
    /// per keystroke.
    pub(crate) key_pending_active: bool,
    /// `vim.fn.confirm` button-dialog requests, drained by the server into the
    /// editor's command line (`Editor::open_confirm`) after the chunk.
    pub(crate) confirms: Vec<ConfirmReq>,
    /// `nvim_feedkeys` typeahead requests, drained by the server into its feed
    /// buffer and processed (through the mapping engine, or straight to the
    /// editor) after the chunk / off-tick settle.
    pub(crate) feedkeys: Vec<FeedKeysOp>,
}

/// Marshal a settled [`FsValue`] (the success payload of an off-tick `nx.fs` op)
/// into the Lua value the promise resolves with. The per-op shape that used to live
/// in the inline `nx._fs_*` bridges now lives here, run once on the result whether it
/// came from the native actor or the wasm daemon leg. `Bytes` (raw `nx.fs.read`) and
/// `Text` (decoded `read_text` / `realpath`) both become a Lua string; `Dir` becomes
/// the `{ { name=, type= }, … }` list a single `scandir` round-trip produced.
fn fs_value_to_lua(lua: &Lua, value: FsValue) -> mlua::Result<mlua::Value> {
    Ok(match value {
        FsValue::Nil => mlua::Value::Nil,
        FsValue::Bool(b) => mlua::Value::Boolean(b),
        FsValue::Bytes(b) => mlua::Value::String(lua.create_string(b)?),
        FsValue::Text(t) => mlua::Value::String(lua.create_string(t)?),
        FsValue::Stat(st) => mlua::Value::Table(fs_stat_table(lua, &st)?),
        FsValue::Dir(entries) => {
            let list = lua.create_table()?;
            for e in entries {
                let t = lua.create_table()?;
                t.set("name", e.name)?;
                t.set("type", e.kind.as_str())?;
                list.push(t)?;
            }
            mlua::Value::Table(list)
        }
    })
}

/// One option's documented metadata, as plain strings for
/// [`LuaRuntime::set_options_catalog`]. The server builds these from core's
/// `nxvim_core::options::options_catalog()` (the single source of truth) and hands
/// them in, keeping nxvim-lua decoupled from the editor-core types.
#[derive(Clone, Debug)]
pub struct OptionCatalogRow {
    /// Canonical spelling, e.g. `"number"`.
    pub name: String,
    /// Standard abbreviation, e.g. `Some("nu")`, or `None`.
    pub abbrev: Option<String>,
    /// `"bool"` / `"number"` / `"string"`.
    pub kind: String,
    /// `"global"` / `"window"` / `"buffer"`.
    pub scope: String,
    /// One-line help shown in the `:set` completion docs pane.
    pub doc: String,
}

/// An embedded Lua VM with nxvim's `vim` global installed.
///
/// `!Send` (Lua state is thread-local); it lives on the server's single thread.
pub struct LuaRuntime {
    lua: Lua,
    shared: Rc<RefCell<Shared>>,
    /// The directories Lua searches: their `lua/` feeds `package.path` (so
    /// `require` resolves plugin modules), and their roots hold `colors/`,
    /// `after/`, … for later phases. nxvim's analogue of neovim's runtimepath.
    ///
    /// Shared + mutable (`Rc<RefCell<…>>`) because the package manager
    /// (`nx.plugins`) adds a freshly-installed plugin's directory to it *at
    /// runtime* (`nx._add_rtp`), and the `nvim_get_runtime_file` closure reads the
    /// live list so the new plugin's `colors/` / `queries/` / `lsp/` resolve
    /// without a restart. The Rc is cloned into those FFI closures in
    /// [`install_runtime_api`].
    runtimepath: Rc<RefCell<Vec<PathBuf>>>,
}

/// Generate a `take_*` accessor that drains one [`Shared`] queue with
/// `mem::take`. Every queue is filled by the Lua FFI closures and emptied once
/// per turn by the server's effect drain; all seventeen have the identical body, so
/// it lives here once rather than being hand-copied (where it is easy to drain the
/// wrong field). Each invocation still carries its own doc comment.
macro_rules! take_queue {
    ($(#[$doc:meta])* $method:ident -> $ty:ty = $field:ident) => {
        $(#[$doc])*
        pub fn $method(&self) -> $ty {
            std::mem::take(&mut self.shared.borrow_mut().$field)
        }
    };
}

impl LuaRuntime {
    /// Build the VM and point `require` at `runtimepath`: each entry's `lua/`
    /// subdirectory is prepended to `package.path` as `<rt>/lua/?.lua` and
    /// `<rt>/lua/?/init.lua` (the layout neovim plugins ship), so a plugin
    /// dropped on the runtimepath is `require`-able by module name.
    pub fn new(runtimepath: Vec<PathBuf>) -> mlua::Result<Self> {
        // Load the full safe stdlib *plus* `debug`. Real plugins (catppuccin
        // among them) call `debug.getinfo` to locate their own install path, and
        // neovim exposes the full `debug` library to its trusted user config —
        // so nxvim does the same. mlua only permits `debug` via its unsafe
        // constructor (it also re-enables C-module loading, which a user config
        // is already trusted to do); the VM is otherwise the standard safe set.
        // nxvim runs vendored PUC Lua 5.4, which has no `ffi` library (that is a
        // LuaJIT extension), so the safe set + `debug` is the whole VM.
        let libs = StdLib::ALL_SAFE | StdLib::DEBUG;
        let lua = unsafe { Lua::unsafe_new_with(libs, LuaOptions::default()) };
        // `loadstring` was folded into `load` in 5.2; restore the 5.1 spelling so
        // plugin/colorscheme code that compiles chunks at runtime (e.g. a colorscheme
        // caching itself via `loadstring` + `string.dump`) keeps working.
        lua.load("loadstring = loadstring or load").exec()?;
        let shared = Rc::new(RefCell::new(Shared::default()));
        let runtimepath = Rc::new(RefCell::new(runtimepath));
        install_vim(&lua, &shared)?;
        install_runtime_api(&lua, &shared, &runtimepath)?;
        seed_package_path(&lua, &runtimepath.borrow())?;
        // The pure-Lua half of `vim.*` + the `nx.*` namespace, layered over the Rust
        // bridge above. Split across focused modules but loaded in source order —
        // each is its own chunk (its own `local` scope), so the order is what one big
        // chunk's was. Their chunk names carry into Lua tracebacks
        // (`nxvim:prelude/lsp:42`).
        for (name, src) in PRELUDE_MODULES {
            lua.load(*src).set_name(*name).exec()?;
        }
        Ok(LuaRuntime {
            lua,
            shared,
            runtimepath,
        })
    }

    /// The `vim` global table — the root every bridge method reaches through.
    fn vim(&self) -> mlua::Result<Table> {
        self.lua.globals().get("vim")
    }

    /// The `nx` global table — home of nxvim's private native bridge (`nx._*`).
    fn nx(&self) -> mlua::Result<Table> {
        self.lua.globals().get("nx")
    }

    /// The runtimepath this VM searches (read by the colorscheme/`require`
    /// machinery to locate `colors/<name>.lua` and friends). A snapshot clone —
    /// the live list is shared+mutable (`nx._add_rtp` appends to it), so callers
    /// that iterate get a stable view for the duration of their borrow.
    pub fn runtimepath(&self) -> Vec<PathBuf> {
        self.runtimepath.borrow().clone()
    }

    /// Append `dir` to the live runtimepath and rebuild `package.path` from it, so the
    /// directory's modules are `require`-able and its `colors/` / `queries/` / `lsp/`
    /// resolve through `nvim_get_runtime_file` — the typed Rust twin of the `nx._add_rtp`
    /// Lua bridge. The rebuild keeps runtimepath order (config dir first), so a plugin
    /// never shadows a user config module. Idempotent: a dir already on the path is a
    /// no-op (no duplicate `package.path` entries). Used to seed a remote session's
    /// runtimepath after the VM is built (the edit-host fetches the daemon's config +
    /// plugins, materializes them locally, then points the path at the copy).
    pub fn add_runtimepath(&self, dir: &Path) -> mlua::Result<()> {
        let mut paths = self.runtimepath.borrow_mut();
        if paths.iter().any(|p| p == dir) {
            return Ok(());
        }
        paths.push(dir.to_path_buf());
        seed_package_path(&self.lua, &paths)
    }

    /// Run a Lua chunk. Errors are returned for the server to surface.
    pub fn exec(&self, chunk: &str) -> mlua::Result<()> {
        self.lua.load(chunk).exec()
    }

    /// Run a Lua chunk read from a file, naming it `name` (use `@<path>`) so an
    /// error/traceback points at the source file — the entry for sourcing the
    /// package `plugin/` scripts at startup.
    pub fn exec_named(&self, chunk: &str, name: &str) -> mlua::Result<()> {
        self.lua.load(chunk).set_name(name).exec()
    }

    /// Evaluate a Lua chunk and convert its return value to an RPC [`rmpv::Value`]
    /// — the `nvim_exec_lua` entry point. The chunk is loaded as an expression
    /// when it is one, else as statements with an explicit `return` (mlua's
    /// `eval` tries both), so `vim.diagnostic.get(0)` and `return …` both work.
    /// Exposes synchronous getters to RPC and to the black-box tests; effects the
    /// chunk queued (ops, panel, commands) are drained by the caller afterward,
    /// exactly like a `:lua` chunk.
    pub fn eval_to_value(&self, chunk: &str) -> mlua::Result<rmpv::Value> {
        let value: mlua::Value = self.lua.load(chunk).eval()?;
        lua_to_rmpv(&value)
    }

    /// Evaluate a generic `'foldexpr'` for buffer lines `1..=line_count` — vim's
    /// per-line fold-expr model, with `v:lnum` (the 1-based line) bound before each
    /// call — and return one fold value string per line (`"0"`, `"1"`, `">1"`,
    /// `"<1"`, `"="`, …) for the core fold engine to resolve. nxvim-core can't run
    /// Lua, so the server drives this off the [`Editor`]'s `'foldexpr'` and pushes
    /// the result back via `set_foldexpr_values`.
    ///
    /// The expression is the `'foldexpr'` value with an optional `v:lua.` prefix
    /// stripped (the realistic nxvim spelling, e.g. `v:lua.MyMod.foldexpr()`),
    /// compiled once as `return <expr>` and called per line so the foldexpr re-reads
    /// `vim.v.lnum` each time. A numeric result stringifies to its integer; a string
    /// result passes through; anything else reads as `"0"` (the line isn't folded).
    /// A compile/eval error propagates so the server can surface it (no silent
    /// no-op).
    pub fn eval_foldexpr_lines(&self, expr: &str, line_count: usize) -> mlua::Result<Vec<String>> {
        let trimmed = expr.trim();
        let chunk = trimmed.strip_prefix("v:lua.").unwrap_or(trimmed);
        let func: mlua::Function = self.lua.load(format!("return {chunk}")).into_function()?;
        let vmirror: Table = self.nx()?.get("_v_mirror")?;
        let mut out = Vec::with_capacity(line_count);
        for lnum in 1..=line_count {
            vmirror.set("lnum", lua_int(lnum as i64))?;
            let value: mlua::Value = func.call(())?;
            out.push(foldexpr_value_string(&value));
        }
        Ok(out)
    }

    /// Mirror a buffer's diagnostics into `nx._diagnostics[bufnr]` as the plain
    /// data `vim.diagnostic.get` reads back (the Rust→Lua state mirror). Called on
    /// every `publishDiagnostics`; keyed by `bufnr`, so it never goes stale on a
    /// buffer switch (the getter resolves `0` → current via `nx._cur_buf`).
    pub fn set_diagnostics(&self, bufnr: u64, diags: &[DiagnosticData]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let set: mlua::Function = nx.get("_set_diagnostics")?;
        let list = self.lua.create_table()?;
        for (i, d) in diags.iter().enumerate() {
            let t = self.lua.create_table()?;
            t.set("lnum", d.lnum)?;
            t.set("col", d.col)?;
            t.set("end_lnum", d.end_lnum)?;
            t.set("end_col", d.end_col)?;
            t.set("severity", d.severity)?;
            t.set("message", d.message.clone())?;
            if let Some(src) = &d.source {
                t.set("source", src.clone())?;
            }
            list.set(i + 1, t)?;
        }
        set.call((bufnr, list))
    }

    /// Mirror a buffer's decoded semantic tokens into `nx._semantic_tokens[bufnr]`
    /// as the plain data `vim.lsp.semantic_tokens.get_at_pos` reads back (Phase 3,
    /// the diagnostics-mirror analogue). Called on every `semanticTokens/full`(/delta)
    /// reply; keyed by `bufnr`. Each entry's `modifiers` is both a list (legend
    /// order) and a set (`modifiers[name] == true`), matching neovim's shape.
    pub fn set_semantic_tokens(
        &self,
        bufnr: u64,
        tokens: &[SemanticTokenData],
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let set: mlua::Function = nx.get("_set_semantic_tokens")?;
        let list = self.lua.create_table()?;
        for (i, tok) in tokens.iter().enumerate() {
            let t = self.lua.create_table()?;
            t.set("line", tok.line)?;
            t.set("start_col", tok.start_col)?;
            t.set("end_col", tok.end_col)?;
            t.set("type", tok.token_type.clone())?;
            t.set("client_id", tok.client_id)?;
            let mods = self.lua.create_table()?;
            for (j, m) in tok.modifiers.iter().enumerate() {
                mods.set(j + 1, m.clone())?;
                mods.set(m.clone(), true)?;
            }
            t.set("modifiers", mods)?;
            list.set(i + 1, t)?;
        }
        set.call((bufnr, list))
    }

    /// Mirror a buffer's decoded inlay hints into `nx._inlay_hints[bufnr]` as the
    /// plain data `vim.lsp.inlay_hint.get` reads back (the semantic-tokens-mirror
    /// analogue). Called on every `textDocument/inlayHint` reply (and after a lazy
    /// hint resolves, or when hints are disabled — an empty list clears the mirror);
    /// keyed by `bufnr`. `col` is a 0-based byte column.
    pub fn set_inlay_hints(&self, bufnr: u64, hints: &[InlayHintMirrorData]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let set: mlua::Function = nx.get("_set_inlay_hints")?;
        let list = self.lua.create_table()?;
        for (i, h) in hints.iter().enumerate() {
            let t = self.lua.create_table()?;
            t.set("line", h.line)?;
            t.set("col", h.col)?;
            t.set("label", h.label.clone())?;
            t.set("kind", h.kind)?;
            t.set("client_id", h.client_id)?;
            list.set(i + 1, t)?;
        }
        set.call((bufnr, list))
    }

    /// Hand a server-resolved LSP location list to `nx.lsp._show_locations`, which
    /// opens `nx.picker` with the built-in "location" preview (design principle 4:
    /// goto-with-many-hits / references / symbols dogfood the picker, not a bespoke
    /// loclist). Each item is `(text, path, 1-based row, 1-based col)`; the picker's
    /// `confirm` jumps via `nx.picker.edit`. The caller drains the queued picker-open
    /// effect (`apply_lua_effects`), exactly as `fire_lsp_attach` does for `on_attach`.
    pub fn show_lsp_locations(&self, items: &[(String, String, u32, u32)]) -> mlua::Result<()> {
        let lsp: Table = self.nx()?.get("lsp")?;
        let show: mlua::Function = lsp.get("_show_locations")?;
        let list = self.lua.create_table()?;
        for (i, (text, path, row, col)) in items.iter().enumerate() {
            let t = self.lua.create_table()?;
            t.set("text", text.clone())?;
            t.set("path", path.clone())?;
            t.set("row", *row)?;
            t.set("col", *col)?;
            list.set(i + 1, t)?;
        }
        show.call(list)
    }

    /// Mirror one LSP client into `nx.lsp._clients[id]` (the Rust→Lua client
    /// registry) so `get_client_by_id` — and the `LspAttach` `on_attach` it feeds
    /// — can read `client.server_capabilities`. Pushed once per server when it
    /// finishes `initialize`. The provider flags become the camelCase
    /// `*Provider` keys neovim configs probe.
    pub fn set_lsp_client(&self, client: &LspClientData) -> mlua::Result<()> {
        let lsp: Table = self.nx()?.get("lsp")?;
        let set: mlua::Function = lsp.get("_set_client")?;
        let caps = self.lua.create_table()?;
        let c = &client.capabilities;
        caps.set("definitionProvider", c.definition)?;
        caps.set("declarationProvider", c.declaration)?;
        caps.set("typeDefinitionProvider", c.type_definition)?;
        caps.set("implementationProvider", c.implementation)?;
        caps.set("referencesProvider", c.references)?;
        caps.set("hoverProvider", c.hover)?;
        caps.set("signatureHelpProvider", c.signature_help)?;
        caps.set("completionProvider", c.completion)?;
        caps.set("documentFormattingProvider", c.document_formatting)?;
        caps.set("renameProvider", c.rename)?;
        caps.set("codeActionProvider", c.code_action)?;
        caps.set("semanticTokensProvider", c.semantic_tokens)?;
        caps.set("inlayHintProvider", c.inlay_hints)?;
        set.call((client.id, client.name.clone(), caps))
    }

    /// Forget an LSP client (`nx.lsp._clients[id] = nil`) when its server exits,
    /// so a stale `get_client_by_id` after a `LspDetach` returns `nil`.
    pub fn remove_lsp_client(&self, id: u64) -> mlua::Result<()> {
        let lsp: Table = self.nx()?.get("lsp")?;
        let remove: mlua::Function = lsp.get("_remove_client")?;
        remove.call(id)
    }

    /// Run the config's `on_init(client, result)` hook for client `id` (Phase 3),
    /// passing the raw `initialize` result as a Lua table. Called when the server
    /// finishes `initialize`, right after the client is mirrored — so the hook can
    /// read `result.capabilities` / `result.offsetEncoding` and tweak the client.
    pub fn run_lsp_on_init(&self, id: u64, result: &serde_json::Value) -> mlua::Result<()> {
        let lsp: Table = self.nx()?.get("lsp")?;
        let run: mlua::Function = lsp.get("_run_on_init")?;
        let result = json_to_lua(&self.lua, result)?;
        run.call((id, result))
    }

    /// Run the config's `on_exit(code, signal, client)` hook for client `id`
    /// (Phase 3), when its server exits. Called while the client is still in
    /// `nx.lsp._clients` (before [`Self::remove_lsp_client`]). `code`/`signal`
    /// are the child's exit status (`signal` is unix-only).
    pub fn run_lsp_on_exit(
        &self,
        id: u64,
        code: Option<i32>,
        signal: Option<i32>,
    ) -> mlua::Result<()> {
        let lsp: Table = self.nx()?.get("lsp")?;
        let run: mlua::Function = lsp.get("_run_on_exit")?;
        run.call((id, code, signal))
    }

    take_queue! {
        /// Take ex-commands queued by `vim.cmd` since the last drain.
        take_commands -> Vec<String> = commands
    }

    take_queue! {
        /// Take captured `print` / error-writer output since the last drain.
        take_output -> Vec<OutputLine> = output
    }

    take_queue! {
        /// Take the highlight-group definitions queued by `nvim_set_hl` since the
        /// last drain, for the server to apply to the core registry.
        take_highlights -> Vec<HlSet> = highlights
    }

    take_queue! {
        /// Take the dock requests queued by `nx.dock.*` since the last drain, for the
        /// server to apply to the core (which owns the dock state).
        take_dock_ops -> Vec<DockOp> = dock_ops
    }

    take_queue! {
        /// Take the layer crosses queued by `nx.open` / `nx.layer.*` since the last
        /// drain, for the server to apply to the core's layer machine.
        take_layer_ops -> Vec<LayerOp> = layer_ops
    }

    take_queue! {
        /// Take the `nx.view` requests queued since the last drain, for the server to
        /// apply to the core's view registry.
        take_view_ops -> Vec<ViewOp> = view_ops
    }

    take_queue! {
        /// Take the `nx.panel` open / close requests queued since the last drain, for the
        /// server to apply to the core's panel.
        take_panel_ops -> Vec<PanelOp> = panel_ops
    }

    take_queue! {
        /// Take the terminal-open requests queued by `nx.terminal.open` since the
        /// last drain, for the server to apply to the core (`Editor::open_terminal`).
        take_terminal_open_reqs -> Vec<TerminalOpenReq> = terminal_ops
    }

    take_queue! {
        /// Take the server-start requests queued by `nx.lsp.start` since the last
        /// drain, for the server to apply to its `LspManager`.
        take_lsp_ops -> Vec<LspOp> = lsp_ops
    }

    take_queue! {
        /// Take the async-runtime requests queued by `nx.schedule` / `nx.timer` /
        /// `vim.system` (`nx.run`) since the last drain, for the server to
        /// service directly (`Schedule`) or forward to the event-loop actor.
        take_loop_ops -> Vec<LoopOp> = loop_ops
    }

    take_queue! {
        /// Take the buffer-local option writes (`vim.bo`) queued since the last drain,
        /// for the server to apply to the live editor.
        take_buf_ops -> Vec<BufOp> = buf_ops
    }

    take_queue! {
        /// Take the extmark mutations queued by the `nvim_buf_set_extmark` family
        /// since the last drain, for the server to apply to the target buffers'
        /// [`ExtmarkStore`](nxvim_core::ExtmarkStore).
        take_extmark_ops -> Vec<ExtmarkOp> = extmark_ops
    }

    take_queue! {
        /// Take the window mutations queued by the `vim.api.nvim_win_*` family since
        /// the last drain, for the server to apply to the live editor (Phase 5).
        take_window_ops -> Vec<WindowOp> = window_ops
    }

    take_queue! {
        /// Take the tab-page mutations queued by `nvim_set_current_tabpage` since the
        /// last drain, for the server to apply to the live editor (Phase 3).
        take_tab_ops -> Vec<TabOp> = tab_ops
    }

    take_queue! {
        /// Take the global-option writes queued by `vim.o` since the last drain, for
        /// the server to apply to the editor's global options.
        take_global_ops -> Vec<GlobalOptionOp> = global_ops
    }

    take_queue! {
        /// Take the per-workspace option overrides queued by `nx.wso` since the last
        /// drain, for the server to apply to the editor's workspace overlay.
        take_workspace_option_ops -> Vec<WorkspaceOptionOp> = workspace_option_ops
    }

    take_queue! {
        /// Take the treesitter bridge requests queued by `vim.treesitter.start` /
        /// `stop` since the last drain, for the server to apply to the editor's
        /// per-buffer treesitter override.
        take_ts_ops -> Vec<TsOp> = ts_ops
    }

    take_queue! {
        /// Take the register writes queued by `vim.fn.setreg` since the last drain,
        /// for the server to apply to the editor's register file.
        take_reg_ops -> Vec<RegisterSetOp> = reg_ops
    }

    take_queue! {
        /// Take the clipboard seeds queued by `nx.test.clipboard.seed` since the last
        /// drain, for the server to write into the editor's clipboard provider.
        take_clipboard_seeds -> Vec<(String, bool)> = clipboard_seeds
    }

    take_queue! {
        /// Take the `setqflist` requests queued since the last drain, for the server
        /// to apply to the editor's quickfix list.
        take_qf_ops -> Vec<QfSetOp> = qf_ops
    }

    take_queue! {
        /// Take the named-list lifecycle ops (`nx.qf.show` / `nx.qf.drop`) queued
        /// since the last drain. Drained *after* [`Self::take_qf_ops`].
        take_named_list_ops -> Vec<NamedListOp> = named_list_ops
    }

    take_queue! {
        /// Take the `vim.ui.input` prompt requests queued since the last drain, for
        /// the server to open as command-line prompts (Phase 8).
        take_ui_inputs -> Vec<UiInputReq> = ui_inputs
    }

    take_queue! {
        /// Take the prompt-completion candidate lists resolved since the last drain,
        /// for the server to rebuild the `nx.ui.input` wildmenu.
        take_prompt_complete_results -> Vec<PromptCompleteCands> = prompt_complete_results
    }

    take_queue! {
        /// Take the `nx.ui.select` requests queued since the last drain, for the
        /// server to open as floating selectable-list menus.
        take_ui_selects -> Vec<UiSelectReq> = ui_selects
    }

    take_queue! {
        /// Take the `nx.ui.float` requests queued since the last drain, for the
        /// server to open as list-less content floats.
        take_ui_floats -> Vec<UiFloatReq> = ui_floats
    }

    take_queue! {
        /// Take the `nx.picker.open` requests queued since the last drain, for the
        /// server to open as fuzzy-finder widgets.
        take_picker_opens -> Vec<PickerOpenReq> = picker_opens
    }

    /// Take (and clear) the pending `nx.picker.resume()` flag — `true` once since the
    /// last drain. The server then replays the last picker's frozen snapshot.
    pub fn take_picker_resume(&self) -> bool {
        std::mem::take(&mut self.shared.borrow_mut().picker_resume)
    }

    take_queue! {
        /// Take the `nx.complete.setup{}` configurations queued since the last
        /// drain, for the server to apply to the native completion engine.
        take_complete_setups -> Vec<CompleteSetupReq> = complete_setups
    }

    take_queue! {
        /// Take the pending `nx.complete.trigger()` requests since the last drain;
        /// a non-empty result means the server runs a manual completion open.
        take_complete_triggers -> Vec<()> = complete_triggers
    }

    take_queue! {
        /// Take the pending `nx.complete.accept{}` requests since the last drain; each
        /// is a behavior string the server accepts the highlighted row under.
        take_complete_accepts -> Vec<String> = complete_accepts
    }

    take_queue! {
        /// Take the `nx.cmdline_complete.setup{}` configurations queued since the
        /// last drain, for the server to enable the command-line completion engine.
        take_cmdline_complete_setups -> Vec<bool> = cmdline_complete_setups
    }

    take_queue! {
        /// Take the `nx._cmdline_set_arg(path)` requests queued since the last drain —
        /// the file picker confirm pasting a chosen path into the open command line.
        take_cmdline_set_args -> Vec<String> = cmdline_set_args
    }

    take_queue! {
        /// Take the async completion candidates streamed since the last drain, for
        /// the server to append (generation-gated) to the open completion popup.
        take_complete_pushes -> Vec<CompletePush> = complete_pushes
    }

    take_queue! {
        /// Take the completion generations whose async sources have all finished
        /// since the last drain, for the server to close a confirmed-empty popup.
        take_complete_finishes -> Vec<u64> = complete_finishes
    }

    take_queue! {
        /// Take the marks `nx.decor` providers published since the last drain, for
        /// the server to apply (generation-gated) into each provider's namespace.
        take_decor_publishes -> Vec<DecorPublish> = decor_publishes
    }

    take_queue! {
        /// Take the resolved lazy-docs `(id, doc)` pairs queued since the last drain,
        /// for the server to fill its completion-docs resolve cache and repaint.
        take_complete_resolve_dones -> Vec<(u64, String)> = complete_resolve_dones
    }

    take_queue! {
        /// Take the `nx.statusline.setup{}` segment layouts queued since the last
        /// drain, for the server to install as the active status line.
        take_statusline_setups -> Vec<StatuslineSetupReq> = statusline_setups
    }

    take_queue! {
        /// Take the custom statusline-segment cell publishes queued since the last
        /// drain, for the server to fold into its per-`(win, name)` segment cache.
        take_statusline_publishes -> Vec<StatuslinePublishReq> = statusline_publishes
    }

    take_queue! {
        /// Take the custom-segment names invalidated since the last drain, for the
        /// server to re-render (per window) after the current input settles.
        take_statusline_invalidates -> Vec<String> = statusline_invalidates
    }

    take_queue! {
        /// Take the `nx.snippet.setup{}` jump-key configs queued since the last drain.
        take_snippet_setups -> Vec<SnippetSetupReq> = snippet_setups
    }

    take_queue! {
        /// Take the `nx.snippet.add(ft, …)` registrations queued since the last drain.
        take_snippet_adds -> Vec<SnippetAddReq> = snippet_adds
    }

    take_queue! {
        /// Take the `nx.snippet.expand(body)` requests queued since the last drain.
        take_snippet_expands -> Vec<String> = snippet_expands
    }

    take_queue! {
        /// Take the picker candidates streamed since the last drain, for the server
        /// to feed (generation-gated) into the open picker.
        take_picker_pushes -> Vec<PickerPush> = picker_pushes
    }

    take_queue! {
        /// Take the completed source generations since the last drain, for the
        /// server to settle the open picker (clear a now-empty query's stale rows).
        take_picker_finishes -> Vec<u64> = picker_finishes
    }

    take_queue! {
        /// Take the named picker actions fired since the last drain, for the server
        /// to apply to the open picker via `Editor::apply_picker_action`.
        take_picker_actions -> Vec<String> = picker_actions
    }

    take_queue! {
        /// Take the named select actions fired since the last drain, for the server
        /// to apply to the open `nx.ui.select` list via `Editor::apply_select_action`.
        take_select_actions -> Vec<String> = select_actions
    }

    take_queue! {
        /// Take the named `view` actions a view buffer-local keymap fired, for the
        /// server to apply to the focused `nx.view` via `Editor::apply_view_action`.
        take_view_actions -> Vec<String> = view_actions
    }

    take_queue! {
        /// Take the named `qf` actions a `FileType qf` buffer-local keymap fired, for
        /// the server to apply to the focused quickfix display via
        /// `Editor::apply_qf_action`.
        take_qf_actions -> Vec<String> = qf_actions
    }

    take_queue! {
        /// Take the named cmdline actions fired since the last drain, for the server
        /// to apply to the open command line via `Editor::apply_cmdline_action`.
        take_cmdline_actions -> Vec<String> = cmdline_actions
    }

    take_queue! {
        /// Take the `vim.fn.confirm` button-dialog requests queued since the last
        /// drain, for the server to open as command-line confirm prompts.
        take_confirms -> Vec<ConfirmReq> = confirms
    }

    take_queue! {
        /// Take the `nvim_feedkeys` typeahead requests queued since the last drain,
        /// for the server to parse and feed (through the mapping engine or straight to
        /// the editor).
        take_feedkeys -> Vec<FeedKeysOp> = feedkeys
    }

    /// Deliver a `vim.ui.input` result to its callback `id`: the typed line
    /// (`Some`) on `<CR>`, or `nil` (`None`) on cancel. Runs `nx._run_cb(id,
    /// false, text)` — a one-shot, so the callback registry entry is dropped after
    /// firing (Phase 8). Effects it queues drain through `apply_lua_effects`.
    pub fn run_ui_input(&self, id: u64, result: Option<String>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_cb")?;
        let arg = match result {
            Some(s) => mlua::Value::String(self.lua.create_string(&s)?),
            None => mlua::Value::Nil,
        };
        run.call::<()>((id, false, arg))
    }

    /// Deliver a `nx.ui.select` result to its callback `id`: the **1-based**
    /// index the user confirmed (`Some`), or `nil` (`None`) on cancel. Runs
    /// `nx._run_cb(id, false, idx)` — a one-shot, so the registry entry drops
    /// after firing. The Lua wrapper maps the index back to the original item
    /// before calling the user's `on_choice`. Effects it queues drain through
    /// `apply_lua_effects`.
    pub fn run_ui_select(&self, id: u64, choice: Option<usize>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_cb")?;
        let arg = match choice {
            // The core records a 0-based highlight index; Lua sees 1-based.
            // `lua_int` keeps the cast correct on wasm32 (where `Integer` is i32).
            Some(idx0) => mlua::Value::Integer(lua_int((idx0 + 1) as i64)),
            None => mlua::Value::Nil,
        };
        run.call::<()>((id, false, arg))
    }

    /// Run the active `nx.picker` source for generation `gen` with the prompt
    /// `query` (`nx._picker_run`). Cancels the previous run (`on_cancel`), resets
    /// the source's per-generation item array, and invokes its `items(ctx, push,
    /// done)`; the `push`es land back as [`PickerPush`](crate::ops::PickerPush)es.
    /// Called on open (`gen 0`, empty query) and on each dynamic query edit.
    pub fn run_picker_run(&self, gen: u64, query: &str) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_picker_run")?;
        run.call::<()>((lua_int(gen as i64), query.to_string()))
    }

    /// Re-render the custom statusline segment `name` for **every** window
    /// (`nx.statusline._rerender`). The Lua wrapper iterates the window mirror,
    /// runs the segment's `render(ctx)` against each window's `{ buf, win, focused }`
    /// context, and publishes the resolved cells per window via
    /// `nx._statusline_publish`. Driven by the server from `run_pending` (with a
    /// freshly pushed window mirror) when the segment is invalidated or the window
    /// layout changed; the resulting publishes land in `statusline_publishes`.
    pub fn run_statusline_rerender(&self, name: &str) -> mlua::Result<()> {
        let nx = self.nx()?;
        let statusline: Table = nx.get("statusline")?;
        let rerender: mlua::Function = statusline.get("_rerender")?;
        rerender.call::<()>(name.to_string())
    }

    /// Run the configured `nx.complete` **async** sources for generation `gen`
    /// against the snapshot `ctx` (`nx._complete_run`). The Lua wrapper debounces
    /// each source, invokes its `complete(ctx)` (the source emits via `ctx.push` and
    /// signals completion by returning / resolving its promise), and reaps a
    /// superseded run; the `push`es land back as
    /// [`CompletePush`](crate::ops::CompletePush)es and the reduced completion as a
    /// `complete_finishes` entry — both generation-stamped.
    /// The `ctx` snapshot (`{ prefix, buf, row, col }`) is passed as primitives (the
    /// server unpacks `CompleteCtx`, which `nxvim-lua` cannot see), never live editor
    /// state. `row` / `col` are 0-based, matching the core cursor; an LSP source
    /// (Phase 4-C) translates to the protocol's coordinates. Phase 4-B.
    pub fn run_complete_run(
        &self,
        gen: u64,
        prefix: &str,
        buf: u64,
        row: usize,
        col: usize,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_complete_run")?;
        let t = self.lua.create_table()?;
        t.set("prefix", self.lua.create_string(prefix)?)?;
        t.set("buf", lua_int(buf as i64))?;
        t.set("row", lua_int(row as i64))?;
        t.set("col", lua_int(col as i64))?;
        run.call::<()>((lua_int(gen as i64), t))
    }

    /// Resolve a command-line completion request synchronously (`nx._cmdline_complete_run`):
    /// the bundled `nx.cmdline_complete` source filters its curated command catalog
    /// (merged with `nx.user_command.get()`) for the command line `line` / cursor `col`
    /// and **returns** the candidate list directly — a `{ {label, insert, doc}, … }`
    /// array. Unlike the insert-completion sources (async / streamed, for slow rg / lsp
    /// scans), the catalog filter is a microsecond table scan, so it is a single
    /// round-trip on the input path; the server fuzzy-ranks + renders the result via
    /// `Editor::open_cmdline_menu`. `col` is a 0-based char offset into `line`.
    ///
    /// For a **file argument** (`:e <Tab>`, …) the source instead *launches the file
    /// picker* (a side effect, queued like any `nx.picker.open`) and returns a
    /// sentinel table (`{ __picker = true }`) — there are no inline candidates. The
    /// server dismisses the command line and lets the queued picker open take over
    /// ([`CmdlineComplete::PickerLaunched`]).
    pub fn run_cmdline_complete(&self, line: &str, col: usize) -> mlua::Result<CmdlineComplete> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_cmdline_complete_run")?;
        let list: Table = run.call((self.lua.create_string(line)?, lua_int(col as i64)))?;
        // The source launched the file picker rather than returning candidates.
        if list.get::<Option<bool>>("__picker")? == Some(true) {
            return Ok(CmdlineComplete::PickerLaunched);
        }
        let mut out = Vec::with_capacity(list.raw_len());
        for item in list.sequence_values::<Table>() {
            let item = item?;
            let label: String = item.get("label")?;
            // `insert` defaults to the label (complete to the whole command name);
            // `doc` defaults to empty (no docs pane until Phase 3).
            let insert: String = item
                .get::<Option<String>>("insert")?
                .unwrap_or_else(|| label.clone());
            let doc: String = item.get::<Option<String>>("doc")?.unwrap_or_default();
            out.push((label, insert, doc));
        }
        Ok(CmdlineComplete::Candidates(out))
    }

    /// Drive the open `nx.ui.input` prompt's `complete` source for `line` / `col` (the
    /// `<Tab>` wildmenu trigger). Unlike [`run_cmdline_complete`], this returns
    /// nothing synchronously: the source may resolve a promise (the DAP `completions`
    /// request is a network round-trip), so the candidates arrive via
    /// `nx._prompt_complete_show` — queued in [`Shared::prompt_complete_results`] and
    /// drained by the server on a later pass. The Lua side (`nx._run_prompt_complete`)
    /// adapts a plain list or a promise uniformly through `nx.promise.resolve`.
    ///
    /// `refresh` is true when the request narrows an already-open wildmenu (an edit),
    /// false for the initial `<Tab>` — the Lua side queries the initial one immediately
    /// but coalesces refreshes through the prompt's `complete_debounce` so an async
    /// source isn't a wire round-trip per keystroke.
    ///
    /// [`run_cmdline_complete`]: Self::run_cmdline_complete
    pub fn run_prompt_complete(&self, line: &str, col: usize, refresh: bool) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_prompt_complete")?;
        run.call((self.lua.create_string(line)?, lua_int(col as i64), refresh))
    }

    /// Populate `nx._options_catalog` with the documented option catalog, so the
    /// bundled `nx.cmdline_complete` source can offer option names (with docs) after
    /// `:set`. The server calls this once at startup from core's single source of
    /// truth (`nxvim_core::options::options_catalog()`); nxvim-lua stays decoupled
    /// from core, so the rows arrive as plain [`OptionCatalogRow`] data rather than
    /// core types. Each `rows` entry becomes a `{ name, abbrev, kind, scope, doc }`
    /// Lua table.
    pub fn set_options_catalog(&self, rows: &[OptionCatalogRow]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let list = self.lua.create_table()?;
        for (i, r) in rows.iter().enumerate() {
            let row = self.lua.create_table()?;
            row.set("name", r.name.as_str())?;
            row.set("abbrev", r.abbrev.as_deref())?;
            row.set("kind", r.kind.as_str())?;
            row.set("scope", r.scope.as_str())?;
            row.set("doc", r.doc.as_str())?;
            list.set(i + 1, row)?;
        }
        nx.set("_options_catalog", list)?;
        Ok(())
    }

    /// Whether any `nx.decor` provider is registered — the gate the server checks
    /// before dispatching a viewport-change signal off-tick. Cheap (a `bool` read),
    /// so the common no-provider config never slices visible lines or re-enters Lua
    /// on scroll. Phase 2 of `nx.decor`.
    pub fn has_decor_providers(&self) -> bool {
        self.shared.borrow().decor_active
    }

    /// Whether a workspace plugin opted this session into capturing the window/tab
    /// layout (`nx.shada.save_layout`). The server gates session capture on it.
    pub fn session_save_layout(&self) -> bool {
        self.shared.borrow().session_save_layout
    }

    /// Seed the layout-capture opt-in from the binary, before any config runs. The
    /// `--workspace` flag uses it so a directory session captures its layout without a
    /// plugin calling `nx.shada.save_layout`; the config may still toggle it afterwards.
    pub fn set_session_save_layout(&self, on: bool) {
        self.shared.borrow_mut().session_save_layout = on;
    }

    /// Seed the shada namespace + workspace root surfaced to Lua (`nx.shada.namespace()` /
    /// `nx.workspace.dir()` / `nx.workspace.active()`), before any config runs. The binary
    /// fills these from the CLI for a local session and from the daemon's cwd for a remote
    /// one — a runtime value precisely because a daemon session can't know the remote dir
    /// until after it connects (so env-stamping in the binary wouldn't see it).
    pub fn set_workspace_identity(&self, namespace: Option<String>, workspace_dir: Option<String>) {
        let mut s = self.shared.borrow_mut();
        s.shada_namespace = namespace;
        s.workspace_dir = workspace_dir;
    }

    /// Harvest the opted-in plugins' isolated shada data as `(namespace, [(key,
    /// value)…])` tuples for the server to fold into the [`PersistState`] it flushes.
    /// Plain tuples (not `nxvim-core` types) since this crate doesn't depend on the
    /// core — the edit-host maps them to `PluginNamespace` / `PluginEntry`.
    pub fn plugin_shada_export(&self) -> Vec<(String, Vec<(String, String)>)> {
        self.shared
            .borrow()
            .plugin_shada
            .iter()
            .map(|(ns, kv)| {
                (
                    ns.clone(),
                    kv.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                )
            })
            .collect()
    }

    /// Seed the plugin shada map from a loaded store, **replacing** the live map
    /// (the boot-time load, before any plugin has written this session). The server
    /// hands the [`PersistState`]'s plugin data back as the same tuples
    /// [`plugin_shada_export`](Self::plugin_shada_export) produced.
    pub fn plugin_shada_seed(&self, data: Vec<(String, Vec<(String, String)>)>) {
        let mut shared = self.shared.borrow_mut();
        shared.plugin_shada.clear();
        for (ns, entries) in data {
            let map = shared.plugin_shada.entry(ns).or_default();
            for (k, v) in entries {
                map.insert(k, v);
            }
        }
    }

    /// Merge a re-read store's plugin data into the live map (`:rshada[!]`). With
    /// `replace` a stored value overwrites a conflicting live key; otherwise it only
    /// fills a key the running session hasn't set — mirroring how `apply_persist`
    /// treats the editor's own state.
    pub fn plugin_shada_merge(&self, data: Vec<(String, Vec<(String, String)>)>, replace: bool) {
        let mut shared = self.shared.borrow_mut();
        for (ns, entries) in data {
            let map = shared.plugin_shada.entry(ns).or_default();
            for (k, v) in entries {
                if replace {
                    map.insert(k, v);
                } else {
                    map.entry(k).or_insert(v);
                }
            }
        }
    }

    /// Whether any `nx.on_key_pending` listener has been registered
    /// (`nx._key_pending_register`). The server gates the pending-key signal on this:
    /// while unset it never computes continuations or re-enters Lua.
    pub fn has_key_pending_listeners(&self) -> bool {
        self.shared.borrow().key_pending_active
    }

    /// Fire the **`KeyPending`** event into Lua (`nx._key_pending_dispatch`). The
    /// server calls this only when the pending key-context *changed* (a mapped prefix
    /// grew or cleared). The payload is `{ mode, keys, continuations, label }` where
    /// each continuation is `{ key, desc, kind = "map"|"group" }`; a *cleared* context
    /// is `keys = ""` with no continuations, which a which-key popup treats as
    /// "close". `label` is set (and `continuations` empty) for a **source B** built-in
    /// pending state — find-char, marks, registers — whose continuation set is open;
    /// sources A/C leave it `nil`.
    pub fn run_key_pending(
        &self,
        mode: &str,
        keys: &str,
        continuations: &[(&str, Option<&str>, &str, bool)],
        label: Option<&str>,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_key_pending_dispatch")?;
        let ctx = self.lua.create_table()?;
        ctx.set("mode", self.lua.create_string(mode)?)?;
        ctx.set("keys", self.lua.create_string(keys)?)?;
        // Source-B built-in states carry a label; sources A/C leave it nil.
        match label {
            Some(l) => ctx.set("label", self.lua.create_string(l)?)?,
            None => ctx.set("label", mlua::Value::Nil)?,
        }
        let arr = self.lua.create_table()?;
        for (i, (key, desc, kind, available)) in continuations.iter().enumerate() {
            let cont = self.lua.create_table()?;
            cont.set("key", self.lua.create_string(key)?)?;
            match desc {
                Some(d) => cont.set("desc", self.lua.create_string(d)?)?,
                None => cont.set("desc", mlua::Value::Nil)?,
            }
            cont.set("kind", self.lua.create_string(kind)?)?;
            // `available = false` marks a continuation that is no longer reachable in
            // this state — a mapped `g`-prefix continuation surfaced *after* the leader
            // timeout committed `g` to the built-in grammar (the maps need a faster
            // sequence to fire). which-key keeps it visible but dimmed / cued.
            cont.set("available", *available)?;
            arr.set(i + 1, cont)?;
        }
        ctx.set("continuations", arr)?;
        run.call::<()>(ctx)
    }

    /// Dispatch the registered `nx.decor` providers for a window whose visible range
    /// changed (`nx._decor_dispatch`). The server stamps the viewport `generation` in
    /// core and passes a snapshot — `win`/`buf` handles, the 0-based inclusive
    /// `top`/`bot` rows, the buffer `filetype` and `buftype` (for the provider's `bufs`
    /// filter), and `lines` (exactly the visible slice) — never live editor state. Each matching
    /// provider's `on_range(ctx, publish)` runs; the marks it publishes carry
    /// `ctx.gen` so a viewport the user scrolled past is dropped at apply time
    /// (Phase 3). Phase 2.
    #[allow(clippy::too_many_arguments)]
    pub fn run_decor_dispatch(
        &self,
        win: u64,
        buf: u64,
        top: usize,
        bot: usize,
        generation: u64,
        filetype: &str,
        buftype: &str,
        lines: &[String],
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_decor_dispatch")?;
        let ctx = self.lua.create_table()?;
        ctx.set("win", lua_int(win as i64))?;
        ctx.set("buf", lua_int(buf as i64))?;
        ctx.set("top", lua_int(top as i64))?;
        ctx.set("bot", lua_int(bot as i64))?;
        ctx.set("gen", lua_int(generation as i64))?;
        ctx.set("filetype", self.lua.create_string(filetype)?)?;
        ctx.set("buftype", self.lua.create_string(buftype)?)?;
        let arr = self.lua.create_table()?;
        for (i, line) in lines.iter().enumerate() {
            arr.set(i + 1, self.lua.create_string(line)?)?;
        }
        ctx.set("lines", arr)?;
        run.call::<()>(ctx)
    }

    /// Ask the plugin source that produced resolve-handle `id` to resolve its lazy
    /// docs (`nx._complete_resolve`). The wrapper looks up the stored
    /// `(source.resolve, item)`, invokes `resolve(item, respond)`, and `respond`
    /// queues the docs back as a [`complete_resolve_dones`](Shared::complete_resolve_dones)
    /// entry the server folds into its sidebar cache. A no-op for an unknown / stale
    /// id. Phase 4-E.
    pub fn run_complete_resolve(&self, id: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_complete_resolve")?;
        run.call::<()>(lua_int(id as i64))
    }

    /// Deliver the picker's outcome to the active source: the chosen item's
    /// **`key`** (the 1-based wrapper index — `Some`) confirms, `nil` (`None`)
    /// cancels (`nx._picker_result`). The wrapper resolves `key` to the original
    /// item and calls the source's `confirm(item)`, then clears the active picker.
    /// `mode` is the confirm gesture's open mode — `"current"` (open in the focused
    /// window) or `"tab"` (the default `<C-t>` ⇒ a new tab) — forwarded to the
    /// source's `confirm(item, mode)`. `resume_keys` are the item keys of the resume
    /// snapshot's window (empty when the picker isn't resumable) — `nx._picker_result`
    /// keeps only those item tables on `nx.picker._last` so `confirm` works after a
    /// resume while bounding retained memory to the window.
    pub fn run_picker_result(
        &self,
        key: Option<usize>,
        mode: &str,
        resume_keys: &[usize],
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_picker_result")?;
        let arg = match key {
            Some(k) => mlua::Value::Integer(lua_int(k as i64)),
            None => mlua::Value::Nil,
        };
        run.call::<()>((arg, mode, self.resume_keys_seq(resume_keys)?))
    }

    /// Deliver the picker's "send these results to a list" outcome: the matched item
    /// `keys` (in display order) and the live `query` go to `nx._picker_send`, which
    /// maps the keys back to the source item tables and builds a named list keyed
    /// `<picker>:<query>`. The bulk-result sibling of [`Self::run_picker_result`].
    /// `resume_keys` retains the snapshot window's item tables for `nx.picker.resume()`
    /// (see there).
    pub fn run_picker_send(
        &self,
        keys: Vec<usize>,
        query: &str,
        resume_keys: &[usize],
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_picker_send")?;
        let arg = self
            .lua
            .create_sequence_from(keys.into_iter().map(|k| k as i64))?;
        run.call::<()>((arg, self.resume_keys_seq(resume_keys)?, query))
    }

    /// The resume snapshot window's item keys as a Lua sequence (or `nil` when empty —
    /// a non-resumable close), handed to `nx._picker_result`/`_send` to trim the
    /// retained item tables to the window.
    fn resume_keys_seq(&self, keys: &[usize]) -> mlua::Result<mlua::Value> {
        if keys.is_empty() {
            return Ok(mlua::Value::Nil);
        }
        Ok(mlua::Value::Table(
            self.lua
                .create_sequence_from(keys.iter().map(|&k| k as i64))?,
        ))
    }

    /// Dispatch an LSP code-action `command` (Phase 8): runs
    /// `nx.lsp._dispatch_command(client_id, command)`, which routes to a
    /// client-side `vim.lsp.commands[name]` handler when registered, else issues a
    /// `workspace/executeCommand` to the client's server. `command` is the LSP
    /// `Command` (`{ title, command, arguments }`) as JSON. Errors are returned for
    /// the server to surface.
    pub fn run_lsp_command(&self, client_id: u64, command: &serde_json::Value) -> mlua::Result<()> {
        let lsp: Table = self.nx()?.get("lsp")?;
        let dispatch: mlua::Function = lsp.get("_dispatch_command")?;
        let cmd = json_to_lua(&self.lua, command)?;
        dispatch.call((client_id, cmd))
    }

    /// Run the deferred callback registered under `id` (the `run_keymap` analogue
    /// for the async runtime). Invokes `nx._run_cb(id, keep, …)`; with `keep ==
    /// false` the registry entry is dropped after firing (one-shot), so
    /// `vim.schedule` / `vim.defer_fn` / `vim.system` `on_exit` never leak. A
    /// repeating timer passes `keep == true` to retain its function across fires.
    /// `args` are forwarded to the Lua callback as its arguments. Effects the
    /// callback queues land in [`Shared`] and drain through the server's
    /// `apply_lua_effects`; a throwing callback returns its error for the server to
    /// surface (it isolates one callback, never aborting the drain).
    pub fn run_callback(&self, id: u64, keep: bool, args: CallbackArgs) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_cb")?;
        match args {
            CallbackArgs::None => run.call::<()>((id, keep)),
            CallbackArgs::Process {
                code,
                stdout,
                stderr,
            } => {
                let result = self.lua.create_table()?;
                result.set("code", code)?;
                result.set("stdout", self.lua.create_string(&stdout)?)?;
                result.set("stderr", self.lua.create_string(&stderr)?)?;
                run.call::<()>((id, keep, result))
            }
            CallbackArgs::LspReply { err, result } => {
                // `handler(err, result)`: a string-or-nil error and the JSON
                // result (nil when `err` is set), matching neovim's handler shape.
                let err = match err {
                    Some(msg) => mlua::Value::String(self.lua.create_string(&msg)?),
                    None => mlua::Value::Nil,
                };
                let result = json_to_lua(&self.lua, &result)?;
                run.call::<()>((id, keep, err, result))
            }
            CallbackArgs::FsResult { result } => {
                // `nx.fs.*` settle: fire `nx._run_cb(id, false, err, value)`. On reject
                // `err` is the `{ code, message }` table (value nil); on resolve `err`
                // is nil and `value` the marshalled Lua value. The typed-result → Lua
                // conversion happens here once, so it is identical for the native actor
                // and the wasm daemon leg.
                match result {
                    Ok(value) => {
                        let value = fs_value_to_lua(&self.lua, value)?;
                        run.call::<()>((id, keep, mlua::Value::Nil, value))
                    }
                    Err(e) => {
                        let err = self.lua.create_table()?;
                        err.set("code", e.code)?;
                        err.set("message", e.message)?;
                        run.call::<()>((id, keep, mlua::Value::Table(err), mlua::Value::Nil))
                    }
                }
            }
            CallbackArgs::HttpResult { result } => {
                // `nx.http.fetch` settle: fire `nx._run_cb(id, false, err, response)`. On a
                // transport failure `err` is the `{ message }` table (response nil); on
                // success `err` is nil and `response` the marshalled `{ status, ok,
                // statusText, headers, body }` table. A non-2xx status is a *success* here
                // (fetch semantics) — the `ok` flag carries it; only a network/transport
                // failure rejects.
                match result {
                    Ok(resp) => {
                        let response = self.lua.create_table()?;
                        response.set("status", resp.status)?;
                        response.set("ok", (200..300).contains(&resp.status))?;
                        response.set("statusText", resp.status_text)?;
                        // Headers as a `{ [name] = value }` map (lowercased names), plus a
                        // parallel ordered list under a hidden key so repeated headers and
                        // order survive if a caller wants them.
                        let headers = self.lua.create_table()?;
                        for (name, value) in &resp.headers {
                            headers.set(name.as_str(), value.as_str())?;
                        }
                        response.set("headers", headers)?;
                        response.set("body", self.lua.create_string(&resp.body)?)?;
                        run.call::<()>((id, keep, mlua::Value::Nil, response))
                    }
                    Err(e) => {
                        let err = self.lua.create_table()?;
                        err.set("message", e.message)?;
                        run.call::<()>((id, keep, mlua::Value::Table(err), mlua::Value::Nil))
                    }
                }
            }
            CallbackArgs::HttpMountResult { result } => {
                // `nx.http.mount` settle: fire `nx._run_cb(id, false, err, origin)`. Unlike
                // the fetch twin there is no "resolved failure" case — a mount either
                // published (and carries the bound origin, e.g. "http://127.0.0.1:53124")
                // or it didn't (a taken port, a duplicate name), which rejects loud.
                match result {
                    Ok(origin) => {
                        let origin = self.lua.create_string(&origin)?;
                        run.call::<()>((id, keep, mlua::Value::Nil, origin))
                    }
                    Err(e) => {
                        let err = self.lua.create_table()?;
                        err.set("message", e.message)?;
                        run.call::<()>((id, keep, mlua::Value::Table(err), mlua::Value::Nil))
                    }
                }
            }
        }
    }

    /// Fire a `'statusline'` `%@handler@…%X` click region's callback
    /// (`nx._statusline_click`) with neovim's click arguments. `handler` is the raw
    /// `v:lua.…` reference from the format; the Lua side resolves it to a function
    /// and calls it (erroring loud if it isn't a callable `v:lua` reference). `button`
    /// is the mouse button ("l"/"r"/"m"); `modifiers` the active modifier string
    /// ("s"/"c"/"a"). Effects the callback queues land in [`Shared`] for the server's
    /// `apply_lua_effects` to drain. A throwing handler returns its error for the
    /// server to surface.
    pub fn run_statusline_click(
        &self,
        handler: &str,
        minwid: u32,
        clicks: u8,
        button: char,
        modifiers: &str,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_statusline_click")?;
        run.call::<()>((
            handler.to_string(),
            minwid as i64,
            clicks as i64,
            button.to_string(),
            modifiers.to_string(),
        ))
    }

    /// Fire a streaming child's `on_stdout` callback with the latest batch of
    /// stdout `lines` (`nx._run_stdout(id, lines)`). Persistent (not a one-shot):
    /// fires once per [`LoopEvent::ProcessStdout`](crate) until the child exits,
    /// which clears the registry entry. A no-op when no `on_stdout` is registered.
    pub fn run_process_stdout(&self, id: u64, lines: Vec<String>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_stdout")?;
        let table = self.lua.create_sequence_from(lines)?;
        run.call::<()>((id, table))
    }

    /// Deliver a raw output chunk from a duplex `nx.process` child to its persistent
    /// receiver (`nx._proc_recv(id, data, is_stderr)`). `data` is handed over as a
    /// binary-safe Lua string (un-split, un-decoded — the Lua side owns the framing),
    /// `stderr` selects the stream. Persistent until [`run_process_exit`] fires. A
    /// no-op when no handler is registered under `id`.
    ///
    /// [`run_process_exit`]: LuaRuntime::run_process_exit
    pub fn run_process_recv(&self, id: u64, data: Vec<u8>, stderr: bool) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_proc_recv")?;
        let data = self.lua.create_string(&data)?;
        run.call::<()>((id, data, stderr))
    }

    /// Fire a duplex `nx.process` child's exit (`nx._proc_exit(id, code)`), then drop
    /// its handler registry entry (a one-shot — the handle is dead afterwards).
    pub fn run_process_exit(&self, id: u64, code: i32) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_proc_exit")?;
        run.call::<()>((id, code))
    }

    /// Fire an `nx.socket` connection's `on_connect` (`nx._sock_connected(id)`), once
    /// the TCP connection is established.
    pub fn run_socket_connected(&self, id: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_sock_connected")?;
        run.call::<()>(id)
    }

    /// Deliver a raw byte chunk from an `nx.socket` connection to its `on_data`
    /// receiver (`nx._sock_data(id, data)`), as a binary-safe Lua string. Persistent
    /// until [`run_socket_closed`] fires.
    ///
    /// [`run_socket_closed`]: LuaRuntime::run_socket_closed
    pub fn run_socket_data(&self, id: u64, data: Vec<u8>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_sock_data")?;
        let data = self.lua.create_string(&data)?;
        run.call::<()>((id, data))
    }

    /// Fire an `nx.socket` connection's close (`nx._sock_closed(id, err)`) — `err` is
    /// a string when the connection failed / errored, nil on a clean close. One-shot.
    pub fn run_socket_closed(&self, id: u64, error: Option<String>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_sock_closed")?;
        let err = match error {
            Some(msg) => mlua::Value::String(self.lua.create_string(&msg)?),
            None => mlua::Value::Nil,
        };
        run.call::<()>((id, err))
    }

    /// Deliver an inbound request to a mount's `on_request`
    /// (`nx._http_request(id, req_id, req)`), where `req` is the marshalled
    /// `{ name, method, path, raw_path, query, headers, body }` table. Persistent — a
    /// mount serves until `:close()`, so this fires once per request, unlike the one-shot
    /// `nx._run_cb` a fetch settles through.
    ///
    /// `req_id` is what the handler's `respond` closure carries back
    /// ([`LoopOp::HttpRespond`](crate::LoopOp::HttpRespond)); the Lua side tracks it as an
    /// open slot so a double-`respond` (or one after a timeout) can notify loud instead of
    /// vanishing.
    pub fn run_http_server_request(
        &self,
        id: u64,
        req_id: u64,
        request: HttpServerRequest,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_http_request")?;
        let req = self.lua.create_table()?;
        req.set("name", request.name)?;
        req.set("method", request.method)?;
        req.set("path", request.path)?;
        req.set("raw_path", request.raw_path)?;
        // Query params and headers as `{ [name] = value }` maps — the shape a handler
        // actually reads (`req.query.v`, `req.headers["content-type"]`). A repeated name
        // keeps the last value, matching how `fetch`'s own `Headers` collapses them.
        let query = self.lua.create_table()?;
        for (name, value) in &request.query {
            query.set(name.as_str(), value.as_str())?;
        }
        req.set("query", query)?;
        let headers = self.lua.create_table()?;
        for (name, value) in &request.headers {
            headers.set(name.as_str(), value.as_str())?;
        }
        req.set("headers", headers)?;
        // Binary-safe: a POST of an image is a Lua string of raw bytes, not UTF-8.
        req.set("body", self.lua.create_string(&request.body)?)?;
        run.call::<()>((id, req_id, req))
    }

    /// Tell the Lua side the mount listener moved to a new origin
    /// (`nx._http_rebound(origin)`) after an `'httphost'` / `'httpport'` write. Every mount
    /// is still mounted — only the origin changed — so this updates the one place
    /// `Mount:origin()` reads and every live mount's `:url()` follows at once.
    pub fn run_http_rebound(&self, origin: &str) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_http_rebound")?;
        run.call::<()>(origin)
    }

    /// Fire a `nx.fs.watch` stream's pump (`nx._run_fs_watch(id, ev, err)`): an `ev`
    /// table `{ kind, paths }` on a change, or `err` (a string) when the watch failed
    /// to arm / a backend error ended it. Persistent until the stream is `:stop()`ed
    /// (which drops the registry entry); a no-op when the id isn't registered. The
    /// `kind`/`paths` are already coalesced (the actor's 10 ms window).
    pub fn run_fs_watch_event(
        &self,
        id: u64,
        error: Option<String>,
        kind: Option<&str>,
        paths: Vec<std::path::PathBuf>,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_fs_watch")?;
        match error {
            Some(msg) => {
                let err = mlua::Value::String(self.lua.create_string(&msg)?);
                run.call::<()>((id, mlua::Value::Nil, err))
            }
            None => {
                let ev = self.lua.create_table()?;
                ev.set("kind", kind)?;
                let plist = self.lua.create_table()?;
                for p in paths {
                    plist.push(p.to_string_lossy().into_owned())?;
                }
                ev.set("paths", plist)?;
                run.call::<()>((id, mlua::Value::Table(ev), mlua::Value::Nil))
            }
        }
    }

    /// Drive the `:make` / `:grep` async producer (`nx._qf_make`): spawn `cmd` (a
    /// shell command line — the server has already expanded `'makeprg'`/`'grepprg'`
    /// and appended the `2>&1` stderr merge) and, on exit, parse its combined output
    /// against `efm` into the quickfix list, then open the window / jump to the first
    /// error per `open` / `jump`. The spawn rides the same job machinery as
    /// `nx.run`/`nx.run_stream`; the caller drains the resulting effects.
    pub fn run_qf_make(
        &self,
        cmd: &str,
        efm: &str,
        title: &str,
        open: bool,
        jump: bool,
        loclist_win: Option<u64>,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_qf_make")?;
        run.call::<()>((cmd, efm, title, open, jump, loclist_win))
    }

    /// Record the OS pid of an async `vim.system` child (keyed by its callback
    /// `id`) so the handle's `.pid` field resolves it. Delivered by the event-loop
    /// actor shortly after the spawn — the pid can't be known synchronously on the
    /// single-threaded runtime, so the handle reads `nil` until this lands.
    pub fn set_process_pid(&self, id: u64, pid: Option<u32>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let set: mlua::Function = nx.get("_set_proc_pid")?;
        set.call((id, pid))
    }

    /// Install view buffer `bufnr`'s buffer-local default activation map (`<CR>` →
    /// `on_select`) by calling the prelude `nx._install_view_keymaps`. The server
    /// calls this right after [`Editor::create_view`](nxvim_core::Editor) mints the
    /// backing buffer — the bufnr is known synchronously in core, so the map exists
    /// before the user can reach the view (no dependence on the next-tick mirror or on
    /// the view ever becoming the current buffer for a `FileType` event).
    pub fn install_view_keymaps(&self, bufnr: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        let f: mlua::Function = nx.get("_install_view_keymaps")?;
        f.call::<()>(bufnr)?;
        Ok(())
    }

    /// Fire view `id`'s `on_select` callback (`nx._view_select`) for its cursor at
    /// `line` (0-based, passed to Lua 1-based). A no-op when the view has no handler.
    /// Errors (a throwing handler) are returned for the server to surface. Called
    /// when the user hits `<CR>` on a focused `nx.view` buffer.
    pub fn run_view_select(&self, id: u64, line: usize) -> mlua::Result<()> {
        let nx = self.nx()?;
        let f: Option<mlua::Function> = nx.get("_view_select")?;
        if let Some(f) = f {
            f.call::<()>((id, line as i64 + 1))?;
        }
        Ok(())
    }

    /// Fire view `id`'s `on_close` callback (`nx._view_closed`). A no-op when the view
    /// has no handler. Errors (a throwing handler) are returned for the server to
    /// surface. Called when the user closes a focused `nx.view` window (`:q` / `:close`).
    pub fn run_view_closed(&self, id: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        let f: Option<mlua::Function> = nx.get("_view_closed")?;
        if let Some(f) = f {
            f.call::<()>(id)?;
        }
        Ok(())
    }

    /// Refresh the `nx.view` Rust→Lua mirror: `nx._view_buf[id]` (the backing
    /// buffer number, the extmark / read target), `nx._view_line[id]` (the view
    /// window's 1-based cursor line), and `nx._view_win[id]` (the window showing the
    /// view, the `ctx.wo` target; absent when unmounted) for every live view. Pushed
    /// each tick before Lua runs (like the buffer mirror), so a view's `:set_decor` /
    /// `:line()` / `:winid()` read against the current state without a server round-trip.
    pub fn set_view_mirror(&self, views: &[(u64, u64, u64, u64)]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let bufs = self.lua.create_table()?;
        let lines = self.lua.create_table()?;
        let wins = self.lua.create_table()?;
        for &(id, buf, line, win) in views {
            bufs.set(id, buf)?;
            lines.set(id, line)?;
            if win != 0 {
                wins.set(id, win)?; // unmounted views (win == 0) stay absent → winid() is nil
            }
        }
        nx.set("_view_buf", bufs)?;
        nx.set("_view_line", lines)?;
        nx.set("_view_win", wins)?;
        Ok(())
    }

    /// Refresh `nx._view_pending` — the persisted plugin views a session restore reserved
    /// a slot for but that no plugin has adopted yet. Each entry is a `{ namespace, id,
    /// win }` table (the owning plugin namespace, the plugin-chosen persist id, and the
    /// reserved window). `nx.view.pending_restores()` reads it, and the `on_restore`
    /// dispatch (Phase 2) drains it. Usually empty, so cheap.
    pub fn set_view_pending(&self, pending: &[(String, String, u64)]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let arr = self.lua.create_table()?;
        for (i, (namespace, id, win)) in pending.iter().enumerate() {
            let e = self.lua.create_table()?;
            e.set("namespace", namespace.clone())?;
            e.set("id", id.clone())?;
            e.set("win", *win)?;
            arr.set(i + 1, e)?;
        }
        nx.set("_view_pending", arr)?;
        Ok(())
    }

    /// The current `nx._keymaps_version`, bumped by every `vim.keymap.set`/`del`.
    /// The server reads it once per input batch and rebuilds its tries only when
    /// it advanced — so per keystroke it walks the cached trie, never the bridge.
    /// `0` on any error (a malformed VM simply yields no mappings).
    pub fn keymaps_version(&self) -> u64 {
        self.read_keymaps_version().unwrap_or(0)
    }

    fn read_keymaps_version(&self) -> mlua::Result<u64> {
        let nx = self.nx()?;
        Ok(nx.get::<Option<u64>>("_keymaps_version")?.unwrap_or(0))
    }

    /// The current `nx._au_version`, bumped by every autocmd register / delete /
    /// clear. The server reads it once per input batch and refreshes its cached set
    /// of registered event names ([`Self::autocmd_event_set`]) only when it advanced,
    /// so the per-key lifecycle diff gates high-frequency events (`CursorMoved` /
    /// `TextChanged`) on a cheap local set rather than re-entering Lua. `0` on any
    /// error (a malformed VM simply registers no autocmds).
    pub fn autocmd_version(&self) -> u64 {
        self.read_autocmd_version().unwrap_or(0)
    }

    fn read_autocmd_version(&self) -> mlua::Result<u64> {
        let nx = self.nx()?;
        Ok(nx.get::<Option<u64>>("_au_version")?.unwrap_or(0))
    }

    /// The distinct event names any registered autocmd currently listens for
    /// (`nx._au_event_set`). The server caches this — refreshed only when
    /// [`Self::autocmd_version`] advances — so it can skip computing / firing an event
    /// nothing wants. An empty vec on any error (no gating ⇒ events simply never fire,
    /// the safe degradation).
    pub fn autocmd_event_set(&self) -> Vec<String> {
        self.read_autocmd_event_set().unwrap_or_default()
    }

    fn read_autocmd_event_set(&self) -> mlua::Result<Vec<String>> {
        let nx = self.nx()?;
        let f: mlua::Function = nx.get("_au_event_set")?;
        f.call(())
    }

    /// Pull `nx._keymaps` across the bridge as a list of [`RawKeymap`]s for the
    /// server to compile into per-mode tries. A read error yields an empty
    /// snapshot (the editor keeps running with no user mappings).
    pub fn keymaps_snapshot(&self) -> Vec<RawKeymap> {
        self.read_keymaps().unwrap_or_default()
    }

    fn read_keymaps(&self) -> mlua::Result<Vec<RawKeymap>> {
        let nx = self.nx()?;
        let list: Table = nx.get("_keymaps")?;
        let mut out = Vec::with_capacity(list.raw_len());
        for entry in list.sequence_values::<Table>() {
            let entry = entry?;
            let modes = entry
                .get::<Option<Vec<String>>>("modes")?
                .unwrap_or_default();
            let lhs: String = entry.get("lhs")?;
            let noremap = entry.get::<Option<bool>>("noremap")?.unwrap_or(true);
            let buffer = entry.get::<Option<u64>>("buffer")?;
            let desc = entry.get::<Option<String>>("desc")?;
            let nowait = entry.get::<Option<bool>>("nowait")?.unwrap_or(false);
            let silent = entry.get::<Option<bool>>("silent")?.unwrap_or(false);
            let expr = entry.get::<Option<bool>>("expr")?.unwrap_or(false);
            let default = entry.get::<Option<bool>>("default")?.unwrap_or(false);
            let seq = entry.get::<Option<u64>>("id")?.unwrap_or(0);
            let rhs_tbl: Table = entry.get("rhs")?;
            let kind: String = rhs_tbl.get("kind")?;
            let rhs = if kind == "lua" {
                RawRhs::Lua(rhs_tbl.get::<u64>("id")?)
            } else {
                RawRhs::Str(rhs_tbl.get::<String>("str")?)
            };
            out.push(RawKeymap {
                modes,
                lhs,
                rhs,
                noremap,
                buffer,
                desc,
                nowait,
                silent,
                expr,
                default,
                seq,
            });
        }
        Ok(out)
    }

    /// Invoke the function RHS registered under `id` (the `run_user_command` /
    /// `run_panel_select` analogue), called when a Lua-backed mapping fires.
    /// Effects land in [`Shared`] and drain through the server's
    /// `apply_lua_effects`. Errors (a throwing handler) are returned to surface.
    pub fn run_keymap(&self, id: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_keymap")?;
        run.call::<()>(id)
    }

    /// Invoke an `<expr>` function RHS and return the **keys it produced** (its
    /// return value, coerced to a string; `nil`/`false` → `""`). The function runs
    /// under the prelude's `nx._expr_lock` so the editor-mutating funnels refuse
    /// (the textlock contract — see `nx._run_keymap_expr`); any effects it queued
    /// anyway are discarded by the server, which feeds only the returned keys. An
    /// error (a throwing handler, or a textlock violation) is returned to surface.
    pub fn run_keymap_expr(&self, id: u64) -> mlua::Result<String> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_run_keymap_expr")?;
        run.call::<String>(id)
    }

    /// Set `vim.g[key] = value` from Rust — used to record `g:colors_name` when
    /// `:colorscheme` loads a theme, so Lua and the editor agree on the name.
    pub fn set_global_var(&self, key: &str, value: &str) -> mlua::Result<()> {
        let g: Table = self.vim()?.get("g")?;
        g.set(key, value)
    }

    /// Install the `nx.test` plugin-test framework. `prelude/test.lua` builds the
    /// framework but exposes it only through `nx._install_test`, leaving `nx.test`
    /// nil in a normal session; the server calls this when the `--test-plugin` runner
    /// enables test mode, so the API exists only there.
    pub fn install_test_api(&self) -> mlua::Result<()> {
        let install: mlua::Function = self.nx()?.get("_install_test")?;
        install.call(())
    }

    /// Refresh the `nx._ui` mirror the plugin test framework reads (`t:float()` /
    /// `t:message()` / `t:cmdline()` / `t:statusline()`): the projected content
    /// float (an `rmpv` map, or `Nil` when none is open), the message line, the
    /// command line, and the status-line text. Pushed once per redraw. O(1) — the
    /// float is `Nil` on the common path and the rest are short strings — so it
    /// follows the same every-tick mirror discipline as `nx._bufs`.
    pub fn set_ui_mirror(
        &self,
        float: &rmpv::Value,
        message: &str,
        cmdline: &str,
        statusline: &str,
        clipboard: Option<(&str, bool)>,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let ui = self.lua.create_table()?;
        ui.set("float", crate::convert::rmpv_to_lua(&self.lua, float)?)?;
        ui.set("message", message)?;
        ui.set("cmdline", cmdline)?;
        ui.set("statusline", statusline)?;
        // The clipboard contents (for `nx.test.clipboard.peek`), or nil when empty.
        if let Some((text, linewise)) = clipboard {
            let c = self.lua.create_table()?;
            c.set("text", text)?;
            c.set("linewise", linewise)?;
            ui.set("clipboard", c)?;
        }
        nx.set("_ui", ui)
    }

    /// Current monotonic time in seconds, read back by `vim.fn.localtime()`. Shares
    /// the base the server stamps onto undo-node timestamps, so the undotree
    /// visualizer's `localtime() - node.time` elapsed math is correct.
    pub fn set_mono_secs(&self, secs: i64) -> mlua::Result<()> {
        self.nx()?.set("_mono_secs", secs)
    }

    /// Refresh the `nx._undotree` mirror that `vim.fn.undotree(bufnr)` reads.
    /// `updates` carries `(bufnr, dict)` for the trees that changed since the last
    /// push (each `dict` an `rmpv` map in neovim's `undotree()` shape); `live` is
    /// every current bufnr, so entries for closed buffers are pruned.
    pub fn set_undotree_mirror(
        &self,
        updates: &[(u64, rmpv::Value)],
        live: &[u64],
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let mirror: Table = match nx.get("_undotree")? {
            mlua::Value::Table(t) => t,
            _ => {
                let t = self.lua.create_table()?;
                nx.set("_undotree", t.clone())?;
                t
            }
        };
        for (bufnr, dict) in updates {
            mirror.set(*bufnr, crate::convert::rmpv_to_lua(&self.lua, dict)?)?;
        }
        // Prune trees for buffers that no longer exist.
        for pair in mirror.clone().pairs::<u64, mlua::Value>() {
            let (bufnr, _) = pair?;
            if !live.contains(&bufnr) {
                mirror.set(bufnr, mlua::Value::Nil)?;
            }
        }
        Ok(())
    }

    /// Fire every autocmd registered for `event` whose pattern matches
    /// `pattern` (used for `ColorScheme` when a theme loads). Delegates to the
    /// prelude's `nx._fire`, which runs callbacks / queues `command` strings;
    /// effects land in [`Shared`] and drain like any other chunk.
    pub fn fire_autocmd(&self, event: &str, pattern: &str) -> mlua::Result<()> {
        let fire: mlua::Function = self.nx()?.get("_fire")?;
        fire.call((event, pattern))
    }

    /// Fire the `nvim_buf_attach` `on_lines` callbacks for buffers that changed.
    /// `changes` is `(bufnr, changedtick, old_line_count, new_line_count)` per
    /// buffer; the prelude's `nx._buf_changed` looks up the attached callbacks and
    /// invokes each with neovim's `on_lines` argument tuple (detaching one that
    /// returns `true` or errors). A buffer with no attachment is a cheap no-op.
    pub fn fire_buf_changes(&self, changes: &[(u64, u64, usize, usize)]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let fire: mlua::Function = nx.get("_buf_changed")?;
        for &(buf, tick, old, new) in changes {
            fire.call::<()>((buf, tick, 0u64, old as u64, new as u64))?;
        }
        Ok(())
    }

    /// Fire the `nvim_buf_attach` `on_bytes` callbacks for a batch of byte-level
    /// edits. Each edit carries its bufnr/changedtick and neovim's relative
    /// `on_bytes` tuple; the prelude's `nx._buf_bytes_changed` dispatches it to that
    /// buffer's attached callbacks. Edits are forwarded in order, in sequence — a
    /// consumer that reparses incrementally requires every edit between two parses.
    /// (Treesitter highlighting is now driven by the native engine, not through this
    /// Lua channel, so the buf-attach plumbing here has no in-crate consumer.)
    pub fn fire_buf_bytes(&self, edits: &[BufBytesEdit]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let fire: mlua::Function = nx.get("_buf_bytes_changed")?;
        for e in edits {
            fire.call::<()>((
                e.bufnr,
                e.tick,
                e.start_row,
                e.start_col,
                e.start_byte,
                e.old_row,
                e.old_col,
                e.old_byte,
                e.new_row,
                e.new_col,
                e.new_byte,
            ))?;
        }
        Ok(())
    }

    /// Fire the `nvim_buf_attach` `on_reload` callbacks for buffers whose whole rope
    /// was replaced (undo/redo, `:e`), where byte deltas are meaningless. The
    /// vendored `LanguageTree`'s reload handler invalidates the tree so the next
    /// `:parse()` is a full reparse of the current snapshot.
    pub fn fire_buf_reloads(&self, bufs: &[u64]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let fire: mlua::Function = nx.get("_buf_reloaded")?;
        for &buf in bufs {
            fire.call::<()>(buf)?;
        }
        Ok(())
    }

    /// Fire an autocmd *with buffer context* — the callback `args` carry the real
    /// `buf` (bufnr) and `file` (path), and a buffer-local autocmd registered for
    /// `buf` matches. Used by the server's buffer/mode lifecycle events
    /// (`BufReadPost`, `FileType`, `BufEnter`, …), which know which buffer fired.
    pub fn fire_autocmd_buf(
        &self,
        event: &str,
        pattern: &str,
        buf: u64,
        file: &str,
    ) -> mlua::Result<()> {
        let fire: mlua::Function = self.nx()?.get("_fire")?;
        fire.call((event, pattern, buf, file))
    }

    /// Fire a **`*Cmd`** autocmd (currently `BufReadCmd`) and report whether any
    /// handler matched/ran — the "claimed the read" signal. Like
    /// [`fire_autocmd_buf`](Self::fire_autocmd_buf) but it captures `nx._fire`'s
    /// boolean return (the same mechanism as
    /// [`fire_file_changed`](Self::fire_file_changed)): `true` means a handler owns
    /// filling the buffer, so the caller skips its default read; `false` means no
    /// handler matched and the default read proceeds. `pattern` is the path the
    /// handler's `pattern` opt matches against (so `BufReadCmd` for a directory can be
    /// scoped), and it is also carried as `<afile>`/`<amatch>` (`args.file`).
    ///
    /// **Claim contract:** unlike a plain autocmd (where merely matching counts), a
    /// `*Cmd` handler **claims** the read by *returning a truthy value*. So a
    /// `pattern = "*"` handler can decide per path — claim a directory, return `nil`
    /// for a regular file so the default read still runs. `Ok(true)` ⇒ claimed.
    ///
    /// `is_dir` is surfaced to the handler as `args.isdir` — the directory-ness of
    /// `<amatch>`, the fs fact a `*Cmd` handler routinely branches on (the file
    /// explorer-as-plugin claims directories, declines files), passed in because the
    /// live Lua fs surface is async and a handler can't synchronously re-stat.
    pub fn fire_autocmd_cmd(
        &self,
        event: &str,
        path: &str,
        buf: u64,
        is_dir: bool,
    ) -> mlua::Result<bool> {
        let fire: mlua::Function = self.nx()?.get("_fire_read_cmd")?;
        fire.call((event, path, buf, is_dir))
    }

    /// Fire an autocmd with buffer context *and* an `args.data` payload — the
    /// `{ client_id = … }` table neovim's `LspAttach`/`LspDetach` carry. The
    /// server fires these at the attach (didOpen) and detach (didClose / server
    /// exit) moments; the default `nxnx.lsp.enable` autocmd reads `client_id` to
    /// resolve the client and run the config's `on_attach`.
    pub fn fire_autocmd_data(
        &self,
        event: &str,
        pattern: &str,
        buf: u64,
        file: &str,
        client_id: u64,
    ) -> mlua::Result<()> {
        let fire: mlua::Function = self.nx()?.get("_fire")?;
        let data = self.lua.create_table()?;
        data.set("client_id", client_id)?;
        fire.call((event, pattern, buf, file, data))
    }

    /// Fire `DirChanged` after a `:cd` / `:chdir` changed the working directory.
    /// Delegates to the prelude's `nx._fire_dir_changed`, which sets `v:event`
    /// (`{ cwd, scope, changed_window }` — neovim's payload, read as
    /// `vim.v.event.cwd` by project / session plugins) before firing, and passes the
    /// same table as `args.data`. `scope` is the autocmd pattern (`"global"` for
    /// `:cd`); `cwd` is the new directory, carried as `<afile>` (`args.file`).
    pub fn fire_dir_changed(&self, scope: &str, cwd: &str) -> mlua::Result<()> {
        let f: mlua::Function = self.nx()?.get("_fire_dir_changed")?;
        f.call((scope, cwd))
    }

    /// Fire `FileChangedShell` for a buffer whose file changed on disk, setting
    /// `v:fcs_reason` to `reason` and resetting `v:fcs_choice` to `""` first (neovim's
    /// `buf_check_timestamp` contract). Returns whether any handler ran — `true` means
    /// the server reads [`Self::fcs_choice`] to learn how the handler redirected the
    /// reconcile; `false` means there was no handler and the server applies the default
    /// warning / autoreload. `buf`/`file` give the buffer context a buffer-local
    /// `FileChangedShell` autocmd matches against.
    pub fn fire_file_changed(&self, reason: &str, buf: u64, file: &str) -> mlua::Result<bool> {
        let f: mlua::Function = self.nx()?.get("_fire_file_changed")?;
        f.call((reason, buf, file))
    }

    /// Read the `v:fcs_choice` a `FileChangedShell` handler set (`""` when none did) —
    /// the reply half of [`Self::fire_file_changed`]. `"reload"` / `"edit"` reload the
    /// buffer, `"ask"` falls through to the default warning, anything else (incl. `""`)
    /// means the handler took over and the reconcile does nothing further.
    pub fn fcs_choice(&self) -> mlua::Result<String> {
        let f: mlua::Function = self.nx()?.get("_fcs_choice")?;
        f.call(())
    }

    /// Run a `:au[tocmd][!]` ex-command through the prelude driver
    /// (`nx._ex_autocmd`), which parses the Vimscript argument line and drives
    /// the same `nx._autocmds` store the `nvim_create_autocmd` API uses. `bang`
    /// is the `!`; `args` is the remainder after the command name. Returns the
    /// text to surface: `""` (nothing), a one-line message/error, or a multi-line
    /// autocmd listing.
    pub fn ex_autocmd(&self, bang: bool, args: &str) -> mlua::Result<String> {
        let f: mlua::Function = self.nx()?.get("_ex_autocmd")?;
        f.call((bang, args))
    }

    /// Run a `:aug[roup][!]` ex-command through the prelude driver
    /// (`nx._ex_augroup`): enter / leave / report the current augroup, or, with
    /// a bang, delete a group and its autocmds. Returns the text to surface.
    pub fn ex_augroup(&self, bang: bool, args: &str) -> mlua::Result<String> {
        let f: mlua::Function = self.nx()?.get("_ex_augroup")?;
        f.call((bang, args))
    }

    /// Run a `:doau[tocmd]` ex-command through the prelude driver
    /// (`nx._ex_doautocmd`), firing an event now (the manual analogue of
    /// `nvim_exec_autocmds`). Returns the text to surface.
    pub fn ex_doautocmd(&self, args: &str) -> mlua::Result<String> {
        let f: mlua::Function = self.nx()?.get("_ex_doautocmd")?;
        f.call(args)
    }

    /// Run a `:com[mand][!]` ex-command through the prelude driver
    /// (`nx._ex_command`): parse the `[attrs] {Name} {repl}` line and register a
    /// user command (global, or buffer-local with `-buffer`) into the same
    /// `nx._user_commands` store the API uses. `bang` is the replace-existing
    /// `!`; `args` is the remainder after the command name; `bufnr` is the current
    /// buffer (for a `-buffer` command). Returns `""` on success, a one-line
    /// `E…` error, or a newline-joined listing for a bare `:command`.
    pub fn ex_command(&self, bang: bool, args: &str, bufnr: u64) -> mlua::Result<String> {
        let f: mlua::Function = self.nx()?.get("_ex_command")?;
        f.call((bang, args, bufnr))
    }

    /// Refresh the `nx._cur_buf` snapshot the prelude reads back through
    /// `nvim_buf_get_name(0)` / `expand('%')`. The server pushes this immediately
    /// before firing a buffer/mode autocmd so a callback can resolve the buffer
    /// that fired. `filetype` is the buffer's detected filetype (`""` when none),
    /// which `nx.lsp.enable` reads to start a server for the already-open buffer.
    /// (Interim until a real per-bufnr registry exists.)
    pub fn set_buf_snapshot(&self, bufnr: u64, name: &str, filetype: &str) -> mlua::Result<()> {
        let set: mlua::Function = self.nx()?.get("_set_cur_buf")?;
        set.call((bufnr, name, filetype))
    }

    /// Publish the editor's current effective working directory into the `nx._cwd`
    /// mirror that `vim.fn.getcwd()` reads. The server keeps this equal to
    /// [`DirState::effective`](crate) on every cwd change (startup seed, `:cd`, window /
    /// tab focus). In a local session this just tracks the process cwd; in a daemon
    /// session it is the *daemon's* cwd (a path that need not exist locally), which is
    /// why `getcwd` reads this mirror rather than `std::env::current_dir`.
    pub fn set_cwd(&self, cwd: &str) -> mlua::Result<()> {
        self.nx()?.set("_cwd", cwd)
    }

    /// Drop the Lua-side state scoped to buffer `bufnr` — its buffer-local user
    /// commands and keymaps — when the server detects the buffer was deleted, so a
    /// later buffer that reuses the bufnr can't inherit them.
    pub fn cleanup_buffer(&self, bufnr: u64) -> mlua::Result<()> {
        let f: mlua::Function = self.nx()?.get("_cleanup_buffer")?;
        f.call(bufnr)
    }

    /// Serialize a mirror struct into the Lua table the `_set_*_mirror` receivers
    /// read. Disables mlua's array metatable so the result is a *plain* table —
    /// byte-identical to the hand-rolled `create_table()` tables these setters used
    /// to build, so nothing on the Lua side (length, `pairs`, `getmetatable`) sees
    /// a difference.
    fn to_lua<T: Serialize + ?Sized>(&self, value: &T) -> mlua::Result<mlua::Value> {
        let opts = mlua::SerializeOptions::new().set_array_metatable(false);
        self.lua.to_value_with(value, opts)
    }

    /// Refresh the Rust→Lua buffer mirror the buffer-read API resolves against
    /// (Phase 6): `nx._bufs[bufnr] = { lines, name, loaded = true }` for every
    /// open buffer, plus `nx._cur_cursor = { row, col }` (row 1-based, col 0-based,
    /// neovim convention) and the current-window handle. The server pushes this
    /// before running any Lua that can read buffer/cursor state, so synchronous
    /// getters (`nvim_buf_get_lines`, `nvim_win_get_cursor`, …) read live data
    /// without reaching the `Server`. `set_lines` write-through mutates this same
    /// mirror in Lua so a read-after-write within one chunk stays consistent.
    ///
    /// `bufs` is `(bufnr, lines, name)` per open buffer. `wins` is one
    /// [`WindowMirror`] per open window in layout order, `win` the focused id, and
    /// `next_win` the id the next `nvim_open_win` will mint (so the Lua side can
    /// return the new handle synchronously while the real window is created when
    /// the queued op drains). `mode` is the editor's current `mode()` short code
    /// (`"n"`/`"i"`/`"v"`/…), stored as `nx._cur_mode` so a `%{}` statusline
    /// expression reading `vim.fn.mode()` reflects this frame. `cmdtype` is the
    /// open command line's type char (`:` / `/` / `?` / `@`, or `""` when none is
    /// open), stored as `nx._cur_cmdtype` for `vim.fn.getcmdtype()`.
    #[allow(clippy::too_many_arguments)]
    pub fn set_buf_mirror(
        &self,
        bufs: &[BufMirror],
        cursor: (u64, u64),
        win: u64,
        wins: &[WindowMirror],
        next_win: u64,
        mode: &str,
        cmdtype: &str,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let entries = self.lua.create_table()?;
        for b in bufs {
            entries.set(b.bufnr, self.to_lua(b)?)?;
        }
        // The window array (`nvim_win_*` reads it by index); each `WindowMirror`
        // serializes to the table shape `nvim_win_get_config` returns, the nested
        // float included.
        let win_arr = self.to_lua(wins)?;
        let set: mlua::Function = nx.get("_set_buf_mirror")?;
        set.call((
            entries, cursor.0, cursor.1, win, win_arr, next_win, mode, cmdtype,
        ))
    }

    /// Refresh the Rust→Lua extmark mirror (`nx._extmarks[bufnr][ns][id]`) that
    /// `nvim_buf_get_extmarks` reads. `bufs` carries only buffers that hold marks;
    /// each entry's marks come from the authoritative core
    /// [`ExtmarkStore`](nxvim_core::ExtmarkStore) with positions already shifted
    /// for any edits, so a read this chunk reflects the live buffer.
    pub fn set_extmark_mirror(&self, bufs: &[(u64, Vec<ExtmarkMirror>)]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let entries = self.lua.create_table()?;
        for (bufnr, marks) in bufs {
            entries.set(*bufnr, self.to_lua(marks)?)?;
        }
        let set: mlua::Function = nx.get("_set_extmark_mirror")?;
        set.call(entries)
    }

    /// Refresh the Rust→Lua highlight mirror (`nx._hl_defs[name]`) that
    /// `nvim_get_hl` reads. Pushed only when the core registry's generation
    /// changed (a colorscheme rarely re-runs), so the common chunk pays nothing.
    /// Each entry mirrors one [`HlDefMirror`]: colors as `0xRRGGBB` ints, the set
    /// boolean attrs, and `link` for an alias group.
    pub fn set_hl_mirror(&self, defs: &[HlDefMirror]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let entries = self.lua.create_table()?;
        for d in defs {
            entries.set(self.lua.create_string(&d.name)?, self.to_lua(d)?)?;
        }
        let set: mlua::Function = nx.get("_set_hl_mirror")?;
        set.call(entries)
    }

    /// Refresh the per-namespace highlight mirror (`nx._hl_defs_ns[ns][name]`)
    /// that `nvim_get_hl(ns, …)` reads for a non-zero namespace. Rebuilds the
    /// whole `_hl_defs_ns` map from the core registry's non-zero namespaces (the
    /// global table goes through [`set_hl_mirror`](Self::set_hl_mirror)), pushed
    /// under the same generation gate. `defs` carries one [`HlDefMirror`] per
    /// `(ns, name)`; rows are byte-identical to the global mirror's.
    pub fn set_hl_mirror_ns(&self, defs: &[HlDefMirror]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let by_ns = self.lua.create_table()?;
        for d in defs {
            let ns_table: mlua::Table = match by_ns.get::<Option<mlua::Table>>(d.ns)? {
                Some(t) => t,
                None => {
                    let t = self.lua.create_table()?;
                    by_ns.set(d.ns, &t)?;
                    t
                }
            };
            ns_table.set(self.lua.create_string(&d.name)?, self.to_lua(d)?)?;
        }
        let set: mlua::Function = nx.get("_set_hl_mirror_ns")?;
        set.call(by_ns)
    }

    /// Refresh the Rust→Lua buffer-option mirror (`nx._bo_mirror[bufnr] =
    /// { tabstop, shiftwidth, expandtab }`) that `vim.bo` / `nvim_get_option_value`
    /// read for the wired buffer-local options. Pushed alongside the buffer mirror
    /// before any Lua that can read options, so a read reflects the core's current
    /// value — the option's default until set, and a value set through the `:set`
    /// ex-command path (not just one written from Lua). `bufs` is
    /// `(bufnr, tabstop, shiftwidth, softtabstop, expandtab, modified)` per open
    /// buffer (`modified` backs `vim.bo[n].modified`, which a `'tabline'` label
    /// reads).
    pub fn set_bo_mirror(&self, bufs: &[BoMirror]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let entries = self.lua.create_table()?;
        for b in bufs {
            entries.set(b.bufnr, self.to_lua(b)?)?;
        }
        let set: mlua::Function = nx.get("_set_bo_mirror")?;
        set.call(entries)
    }

    /// Refresh the Rust→Lua global-option mirror (`nx._go_mirror = { ignorecase,
    /// smartcase, wrapscan, hlsearch, incsearch, showtabline, laststatus,
    /// statusline, tabline }`) that `vim.o` reads for the wired global options.
    /// Pushed alongside the buffer mirror before any Lua that can read options, so a
    /// read reflects the core's current value — the default until set, and a value
    /// set through the `:set` ex path, not just one written from Lua.
    pub fn set_go_mirror(&self, go: &GoMirror) -> mlua::Result<()> {
        let nx = self.nx()?;
        let entry = self.to_lua(go)?;
        let set: mlua::Function = nx.get("_set_go_mirror")?;
        set.call(entry)
    }

    /// Refresh the Rust→Lua workspace-option mirror (`nx._wso_mirror[name] = value`) that
    /// `nx.wso` reads — the per-workspace overrides currently in effect (a usually-empty
    /// `canonical name → value` map). Pushed each frame alongside [`Self::set_go_mirror`]
    /// so a `nx.wso.foo` read reflects the core's overlay (including overrides restored from
    /// the workspace shada, not just ones written this session). Replaces the whole table.
    pub fn set_wso_mirror(
        &self,
        overrides: &[(String, crate::ops::OptionValue)],
    ) -> mlua::Result<()> {
        use crate::ops::OptionValue;
        let nx = self.nx()?;
        let entries = self.lua.create_table()?;
        for (name, value) in overrides {
            match value {
                OptionValue::Bool(b) => entries.set(name.as_str(), *b)?,
                OptionValue::Number(n) => {
                    entries.set(name.as_str(), crate::convert::lua_int(*n))?
                }
                OptionValue::String(s) => entries.set(name.as_str(), s.as_str())?,
            }
        }
        let set: mlua::Function = nx.get("_set_wso_mirror")?;
        set.call(entries)
    }

    /// Refresh the Rust→Lua register mirror (`nx._registers[name] = { text, type
    /// }`, `type` being `"v"` charwise / `"V"` linewise) that `vim.fn.getreg` /
    /// `getregtype` read. Pushed alongside the buffer mirror before any Lua that
    /// can read registers, so a read reflects the core's register file (including
    /// the read-only specials the caller folds in). Keyed by the single-char
    /// register name as a string.
    pub fn set_reg_mirror(&self, regs: &[(char, String, bool)]) -> mlua::Result<()> {
        let nx = self.nx()?;
        let entries = self.lua.create_table()?;
        for (name, text, linewise) in regs {
            let entry = self.lua.create_table()?;
            entry.set("text", text.as_str())?;
            entry.set("type", if *linewise { "V" } else { "v" })?;
            entries.set(name.to_string(), entry)?;
        }
        let set: mlua::Function = nx.get("_set_reg_mirror")?;
        set.call(entries)
    }

    /// Refresh the `nx._qflist` mirror that `vim.fn.getqflist()` reads. Each entry
    /// is one dict in list order (`{filename, lnum, col, text, type, …}`); the
    /// prelude stores the array and the title behind `nx._qflist`.
    pub fn set_qflist_mirror(&self, items: &[QfMirror], title: &str) -> mlua::Result<()> {
        let nx = self.nx()?;
        let arr = self.lua.create_table()?;
        for (i, it) in items.iter().enumerate() {
            let e = self.lua.create_table()?;
            e.set("filename", it.filename.as_str())?;
            e.set("bufnr", it.bufnr)?;
            e.set("module", it.module.as_str())?;
            e.set("lnum", it.lnum)?;
            e.set("end_lnum", it.end_lnum)?;
            e.set("col", it.col)?;
            e.set("end_col", it.end_col)?;
            e.set("vcol", it.vcol)?;
            e.set("nr", it.nr)?;
            e.set("pattern", it.pattern.as_str())?;
            e.set("text", it.text.as_str())?;
            e.set("type", it.typ.as_str())?;
            e.set("valid", it.valid)?;
            arr.set(i + 1, e)?;
        }
        let set: mlua::Function = nx.get("_set_qflist_mirror")?;
        set.call((arr, title))
    }

    /// Reset the per-window location-list mirror (`nx._loclist`) so a window that
    /// lost its loclist drops out; call before re-pushing the live ones with
    /// [`Self::set_loclist_mirror`].
    pub fn clear_loclist_mirror(&self) -> mlua::Result<()> {
        let clear: mlua::Function = self.nx()?.get("_clear_loclist_mirror")?;
        clear.call(())
    }

    /// Refresh window `win`'s slot in the `nx._loclist` mirror — the location-list
    /// twin of [`Self::set_qflist_mirror`], read by `vim.fn.getloclist(win)`.
    pub fn set_loclist_mirror(
        &self,
        win: u64,
        items: &[QfMirror],
        title: &str,
    ) -> mlua::Result<()> {
        let nx = self.nx()?;
        let arr = self.lua.create_table()?;
        for (i, it) in items.iter().enumerate() {
            let e = self.lua.create_table()?;
            e.set("filename", it.filename.as_str())?;
            e.set("bufnr", it.bufnr)?;
            e.set("module", it.module.as_str())?;
            e.set("lnum", it.lnum)?;
            e.set("end_lnum", it.end_lnum)?;
            e.set("col", it.col)?;
            e.set("end_col", it.end_col)?;
            e.set("vcol", it.vcol)?;
            e.set("nr", it.nr)?;
            e.set("pattern", it.pattern.as_str())?;
            e.set("text", it.text.as_str())?;
            e.set("type", it.typ.as_str())?;
            e.set("valid", it.valid)?;
            arr.set(i + 1, e)?;
        }
        let set: mlua::Function = nx.get("_set_loclist_mirror")?;
        set.call((win, arr, title))
    }

    /// Refresh the Rust→Lua `vim.v` mirror with the editor-sourced predefined
    /// variables (`v:count` / `v:count1` / `v:register` / `v:operator`), pushed
    /// alongside the buffer mirror before any Lua that can read them. `v:vim_did_enter`
    /// is sticky (set once via [`Self::set_vim_did_enter`]) and deliberately not
    /// touched here, so the per-tick refresh can't clear it.
    pub fn set_v_mirror(
        &self,
        count: u64,
        count1: u64,
        register: &str,
        operator: &str,
    ) -> mlua::Result<()> {
        let set: mlua::Function = self.nx()?.get("_set_v_mirror")?;
        set.call((count, count1, register, operator))
    }

    /// Set `v:vim_did_enter` (`1` once the startup VimEnter point passes). Sticky:
    /// the per-tick [`Self::set_v_mirror`] preserves it.
    pub fn set_vim_did_enter(&self, entered: bool) -> mlua::Result<()> {
        let set: mlua::Function = self.nx()?.get("_set_vim_did_enter")?;
        set.call(entered)
    }

    /// Mirror the focused window cursor's screen position (1-based row/col, the
    /// whole-screen coordinates `vim.fn.screenrow()` / `vim.fn.screencol()`
    /// return), pushed alongside the buffer mirror. A popup plugin reads them to
    /// keep its popup from covering the cursor.
    /// Mirror the last mouse event's resolved position (`nx._mouse_pos`, backing
    /// `vim.fn.getmousepos()`): the global screen cell, the window it lands in, the
    /// window-relative cell, and the buffer line/column — all 1-based, `0` off a
    /// window's text. Pushed alongside the buffer mirror so a mouse mapping reads the
    /// clicked position rather than the cursor.
    #[allow(clippy::too_many_arguments)]
    pub fn set_mouse_pos(
        &self,
        screenrow: u64,
        screencol: u64,
        winid: u64,
        winrow: u64,
        wincol: u64,
        line: u64,
        column: u64,
    ) -> mlua::Result<()> {
        let set: mlua::Function = self.nx()?.get("_set_mouse_pos")?;
        set.call((screenrow, screencol, winid, winrow, wincol, line, column))
    }

    pub fn set_screen_cursor(&self, row: u64, col: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        nx.set("_cur_screenrow", row)?;
        nx.set("_cur_screencol", col)
    }

    /// Refresh the Rust→Lua tab mirror that backs `vim.api.nvim_tabpage_*` /
    /// `nvim_list_tabpages` / `nvim_get_current_tabpage`: `tabs` is one
    /// [`TabMirror`] per tab page in tabline order and `cur_tab` the active id.
    /// Pushed alongside the buffer/window mirror before any Lua that can read tab
    /// state, so a read reflects the core's current layout.
    pub fn set_tab_mirror(&self, tabs: &[TabMirror], cur_tab: u64) -> mlua::Result<()> {
        let nx = self.nx()?;
        let tab_arr = self.to_lua(tabs)?;
        let set: mlua::Function = nx.get("_set_tab_mirror")?;
        set.call((tab_arr, cur_tab))
    }

    /// Whether `name` resolves to a user command visible from buffer `bufnr`
    /// (the editor's current buffer) — a global registered via
    /// `nvim_create_user_command`, or a buffer-local one registered for `bufnr`
    /// via `nvim_buf_create_user_command`. Lets the server route a deferred
    /// `:Name …` to its Lua callback (and only in the buffer that owns it).
    pub fn has_user_command(&self, name: &str, bufnr: u64) -> bool {
        self.user_command(name, bufnr)
            .map(|v| !v.is_nil())
            .unwrap_or(false)
    }

    /// Invoke the user command `name` (resolved for buffer `bufnr`) with `args`
    /// (the text after the name). A function command is called with an opts table
    /// (`name`, `args`, `fargs`, `bang`); a string command is queued as an
    /// ex-command. Effects land in [`Shared`] and are drained by the server like
    /// any other chunk.
    ///
    /// The bang-less wire shape, kept so the server call site keeps compiling
    /// until it migrates; forwards with `bang = false`. New callers should use
    /// [`Self::run_user_command_bang`] and pass the invocation's parsed `!`.
    pub fn run_user_command(&self, name: &str, args: &str, bufnr: u64) -> mlua::Result<()> {
        self.run_user_command_bang(name, args, bufnr, false)
    }

    /// [`Self::run_user_command`], with the invocation's `!` threaded through:
    /// `bang` is whether the typed command carried a trailing bang (`:Name!`),
    /// surfaced to a function command as `opts.bang` — so a command registered
    /// with `bang = true` can actually branch on it. (A string command is queued
    /// unchanged; its body spells its own bangs.)
    pub fn run_user_command_bang(
        &self,
        name: &str,
        args: &str,
        bufnr: u64,
        bang: bool,
    ) -> mlua::Result<()> {
        match self.user_command(name, bufnr)? {
            mlua::Value::Function(f) => {
                let opts = self.lua.create_table()?;
                opts.set("name", name)?;
                opts.set("args", args)?;
                let fargs = self.lua.create_table()?;
                for (i, a) in args.split_whitespace().enumerate() {
                    fargs.set(i + 1, a)?;
                }
                opts.set("fargs", fargs)?;
                opts.set("bang", bang)?;
                f.call::<()>(opts)?;
                Ok(())
            }
            mlua::Value::String(s) => {
                self.shared
                    .borrow_mut()
                    .commands
                    .push(s.to_str()?.to_string());
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Resolve `name` to its command entry (function or string) for buffer
    /// `bufnr`, letting a buffer-local command for that buffer shadow a global of
    /// the same name — `nx._resolve_user_command` owns the precedence.
    fn user_command(&self, name: &str, bufnr: u64) -> mlua::Result<mlua::Value> {
        let resolve: mlua::Function = self.nx()?.get("_resolve_user_command")?;
        resolve.call((name, bufnr))
    }
}
