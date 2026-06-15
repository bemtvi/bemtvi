//! The embedded Lua VM and its Rust-facing API. [`LuaRuntime`] owns the `mlua`
//! state and the [`Shared`] effect buffer; its methods are the only way the
//! server talks to Lua — running chunks / callbacks, pushing the Rust→Lua state
//! mirrors (buffers, diagnostics, clients), and draining the queued effects. The
//! `vim.*` surface it drives is installed by [`crate::install`] and layered with
//! the `src/prelude/` Lua modules in [`LuaRuntime::new`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, LuaOptions, LuaSerdeExt, StdLib, Table};
use serde::Serialize;

use crate::convert::{json_to_lua, lua_int, lua_to_rmpv};
use crate::host::seed_package_path;
use crate::install::{install_runtime_api, install_vim, PANEL_ON_SELECT};
use crate::ops::{
    BufOp, CallbackArgs, CompleteSetupReq, ConfirmReq, DiagnosticData, DockOp, ExtmarkOp,
    FeedKeysOp, GlobalOptionOp, HlSet, InlayHintMirrorData, LoopOp, LspClientData, LspOp, PanelOp,
    PickerOpenReq, PickerPush, RawKeymap, RawRhs, RegisterSetOp, SemanticTokenData, TabOp,
    TerminalOpenReq, TsOp, UiInputReq, UiSelectReq, WindowOp,
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
    pub modified: bool,
    /// The buffer's filetype (the treesitter language noun) — explicit override
    /// or extension-derived, mirrored so `nx.bo.filetype` / `vim.bo.filetype` read
    /// the core's value regardless of who set it (`:set`, `nx.bo`, `:setf`). Empty
    /// when the buffer has no filetype.
    pub filetype: String,
    /// Whether treesitter highlighting is enabled for the buffer (the
    /// `ts_highlight` noun); mirrored so `nx.bo.ts_highlight` reads the core flag.
    pub ts_highlight: bool,
}

/// One buffer change projected into neovim's `nvim_buf_attach` `on_bytes`
/// argument tuple, fired through [`LuaRuntime::fire_buf_bytes`] so the
/// `vim.treesitter` parser edits its trees and reparses incrementally. The
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
    /// `'scrollanim'` — whether viewport scrolls animate as a slide. Backs
    /// `vim.o.scrollanim`.
    pub scrollanim: bool,
    /// `'scrollanimduration'` — the scroll-animation duration ceiling in ms (`0`
    /// disables). Backs `vim.o.scrollanimduration`.
    pub scrollanimduration: u64,
    /// `'scrollback'` — the terminal scrollback cap in rows. Backs `vim.o.scrollback`.
    pub scrollback: u64,
    /// The editor screen extent backing `vim.o.columns` / `vim.o.lines`, so a
    /// float-positioning plugin can center its windows.
    pub columns: u64,
    pub lines: u64,
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
    // The Rust↔Lua mirror state, shared resolvers, context lock, and the scalar
    // surfaces (variables / options / registers) the entity API reads.
    ("nxvim:prelude/state", include_str!("prelude/state.lua")),
    // The namespace / highlight / extmark surface (`nx.ns` / `nx.hl` / the extmark
    // decoration layer) and the two current-handle getters the option/variable
    // scopes read. Reads the mirror state / resolvers from state.lua above.
    ("nxvim:prelude/api", include_str!("prelude/api.lua")),
    // Autocmds / augroups / user commands / ex-command drivers / vim.cmd.
    ("nxvim:prelude/autocmd", include_str!("prelude/autocmd.lua")),
    ("nxvim:prelude/keymap", include_str!("prelude/keymap.lua")),
    // nx.timer (vim.defer_fn) over the event-loop bridge.
    ("nxvim:prelude/ui", include_str!("prelude/ui.lua")),
    // nx.promise / nx.async — Promises/A+ over the microtask (nx.schedule) and
    // timer (nx.timer) primitives installed just above.
    ("nxvim:prelude/promise", include_str!("prelude/promise.lua")),
    // nx.picker: the fuzzy finder (sources + open) over the float-list widget.
    ("nxvim:prelude/picker", include_str!("prelude/picker.lua")),
    // nx.complete: the native completion engine (Phase 4-A, buffer source).
    (
        "nxvim:prelude/complete",
        include_str!("prelude/complete.lua"),
    ),
    (
        "nxvim:prelude/diagnostic",
        include_str!("prelude/diagnostic.lua"),
    ),
    // vim.fn editor-query read builtins (line / col / expand / pos / …) that read
    // the state mirror; loads after the surfaces they build on.
    ("nxvim:prelude/vimfn", include_str!("prelude/vimfn.lua")),
    // Loads last: the `nx.*` namespace's remaining surface (events, commands,
    // async). The variable/option/dispatch/keymap nouns are authored as `nx.*`
    // directly in the chunks above, each aliasing `vim.*` onto itself.
    ("nxvim:prelude/nx", include_str!("prelude/nx.lua")),
];

/// Side effects produced by running Lua, drained by the server.
#[derive(Default)]
pub(crate) struct Shared {
    /// Ex-commands requested via `vim.cmd(...)`.
    pub(crate) commands: Vec<String>,
    /// Text emitted via `print(...)` / `vim.api.nvim_echo(...)`.
    pub(crate) output: Vec<String>,
    /// Highlight-group definitions from `nvim_set_hl`, applied to the core
    /// registry after the chunk drains (so the core stays the sole mutator).
    pub(crate) highlights: Vec<HlSet>,
    /// Panel requests from `vim.panel.*`, applied to the core after the chunk.
    pub(crate) panel_ops: Vec<PanelOp>,
    /// Dock requests from `nx.dock.*`, applied to the core after the chunk.
    pub(crate) dock_ops: Vec<DockOp>,
    /// Terminal-open requests from `nx.terminal.open`, applied to the core
    /// (`Editor::open_terminal`) after the chunk.
    pub(crate) terminal_ops: Vec<TerminalOpenReq>,
    /// Server-start requests from `vim.lsp.start` (driven by `vim.lsp.enable`),
    /// drained by the server into its `LspManager` after the chunk.
    pub(crate) lsp_ops: Vec<LspOp>,
    /// Async-runtime requests from `vim.schedule` / `vim.defer_fn` / `vim.uv`
    /// timers / async `vim.system`, drained by the server into its scheduled-work
    /// queue and event-loop actor after the chunk.
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
    /// Treesitter bridge requests from `vim.treesitter.start` / `stop`, drained
    /// by the server into the editor's per-buffer treesitter override.
    pub(crate) ts_ops: Vec<TsOp>,
    /// Register writes from `vim.fn.setreg`, drained by the server into the
    /// editor's register file after the chunk. Reads resolve from the
    /// `nx._registers` mirror, so only the write needs an op.
    pub(crate) reg_ops: Vec<RegisterSetOp>,
    /// `vim.ui.input` prompt requests, drained by the server into the editor's
    /// command line (`Editor::open_prompt`) after the chunk (Phase 8).
    pub(crate) ui_inputs: Vec<UiInputReq>,
    /// `nx.ui.select` requests, drained by the server into the editor's floating
    /// selectable-list widget (`Editor::open_menu`) after the chunk.
    pub(crate) ui_selects: Vec<UiSelectReq>,
    /// `nx.picker.open` requests, drained by the server into the editor's fuzzy
    /// finder (`Editor::open_picker`) after the chunk.
    pub(crate) picker_opens: Vec<PickerOpenReq>,
    /// `nx.complete.setup{}` configurations, drained by the server into
    /// `Editor::configure_complete` (the native completion engine, Phase 4-A).
    pub(crate) complete_setups: Vec<CompleteSetupReq>,
    /// Streamed picker candidates (`nx.picker` source `push`), drained by the
    /// server (generation-gated) into `Editor::menu_push` after the chunk / a
    /// streaming `on_stdout`.
    pub(crate) picker_pushes: Vec<PickerPush>,
    /// Generations whose source run has completed (`done()`), drained by the server
    /// into `Editor::menu_finish` so a query that matched nothing clears the now
    /// stale results (one that matched swaps them via `picker_pushes` instead).
    pub(crate) picker_finishes: Vec<u64>,
    /// `vim.fn.confirm` button-dialog requests, drained by the server into the
    /// editor's command line (`Editor::open_confirm`) after the chunk.
    pub(crate) confirms: Vec<ConfirmReq>,
    /// `nvim_feedkeys` typeahead requests, drained by the server into its feed
    /// buffer and processed (through the mapping engine, or straight to the
    /// editor) after the chunk / off-tick settle.
    pub(crate) feedkeys: Vec<FeedKeysOp>,
    /// The backend the **blocking** `nx._system` shell-out runs through. `None` (the
    /// default) spawns the process locally
    /// ([`StdBlockingSystem`](crate::StdBlockingSystem)); a daemon session injects a
    /// blocking bridge ([`set_blocking_system`](LuaRuntime::set_blocking_system)) so a
    /// `root_dir` shell-out (`cargo metadata`) runs on the remote where the project
    /// files are. Held here (not a `LoopOp`) because the call is synchronous — it
    /// returns the child's output inline, not on a later tick. `!Send`, like the rest
    /// of [`Shared`], which lives only on the server's single thread.
    pub(crate) blocking_system: Option<Rc<dyn crate::BlockingSystem>>,
    /// The backend the **project-facing** Lua filesystem surface (`vim.uv.fs_*`,
    /// `vim.fn.readfile`/`glob`/`filereadable`/…) runs through. `None` (the default)
    /// resolves to a persistent local [`StdLuaFs`](crate::StdLuaFs) via
    /// [`resolve_lua_fs`]; a daemon session injects a blocking bridge
    /// ([`set_lua_fs`](LuaRuntime::set_lua_fs)) so those calls hit the *remote* project
    /// where the files live. Held here (not a `LoopOp`) because the calls are
    /// synchronous — they return their value inline on the Lua tick. `!Send`, like the
    /// rest of [`Shared`]. The first resolve installs the default so fd state (an
    /// `fs_open` handle read back by `fs_read`) persists across calls.
    pub(crate) lua_fs: Option<Rc<dyn crate::LuaFs>>,
}

/// Resolve the active [`LuaFs`](crate::LuaFs): the injected daemon bridge, or a
/// persistent local [`StdLuaFs`](crate::StdLuaFs) lazily installed on first use (so the
/// open-fd table outlives a single call). Project-facing `vim.uv`/`vim.fn` fs closures
/// call this, mirroring how `nx._system` resolves `blocking_system`.
pub(crate) fn resolve_lua_fs(shared: &Rc<RefCell<Shared>>) -> Rc<dyn crate::LuaFs> {
    let mut sh = shared.borrow_mut();
    sh.lua_fs
        .get_or_insert_with(|| Rc::new(crate::StdLuaFs::new()))
        .clone()
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
    runtimepath: Vec<PathBuf>,
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
        install_vim(&lua, &shared)?;
        install_runtime_api(&lua, &shared, &runtimepath)?;
        seed_package_path(&lua, &runtimepath)?;
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
    /// machinery to locate `colors/<name>.lua` and friends).
    pub fn runtimepath(&self) -> &[PathBuf] {
        &self.runtimepath
    }

    /// Inject the backend the **blocking** `nx._system` runs through — the daemon's
    /// blocking bridge. Without this the default local spawn
    /// ([`StdBlockingSystem`](crate::StdBlockingSystem)) is used, so a bare/local
    /// session is unchanged. The server calls this once at startup when a daemon is
    /// present, so a synchronous `vim.system(...):wait()` (an LSP `root_dir` shell-out)
    /// runs on the remote where the project files live.
    pub fn set_blocking_system(&self, sys: Rc<dyn crate::BlockingSystem>) {
        self.shared.borrow_mut().blocking_system = Some(sys);
    }

    /// Inject the backend the **project-facing** Lua filesystem surface runs through —
    /// the daemon's blocking fs bridge. Without this the default persistent local
    /// [`StdLuaFs`](crate::StdLuaFs) is used (a bare/local session is unchanged). The
    /// server calls this once at startup when a daemon is present, so `vim.uv.fs_*` /
    /// `vim.fn.readfile` / root detection see the *remote* project, not the local disk.
    pub fn set_lua_fs(&self, fs: Rc<dyn crate::LuaFs>) {
        self.shared.borrow_mut().lua_fs = Some(fs);
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

    /// Mirror one LSP client into `vim.lsp._clients[id]` (the Rust→Lua client
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

    /// Forget an LSP client (`vim.lsp._clients[id] = nil`) when its server exits,
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
    /// `vim.lsp._clients` (before [`Self::remove_lsp_client`]). `code`/`signal`
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
        /// Take captured `print` output since the last drain.
        take_output -> Vec<String> = output
    }

    take_queue! {
        /// Take the highlight-group definitions queued by `nvim_set_hl` since the
        /// last drain, for the server to apply to the core registry.
        take_highlights -> Vec<HlSet> = highlights
    }

    take_queue! {
        /// Take the panel requests queued by `vim.panel.*` since the last drain, for
        /// the server to apply to the core (which owns the panel state).
        take_panel_ops -> Vec<PanelOp> = panel_ops
    }

    take_queue! {
        /// Take the dock requests queued by `nx.dock.*` since the last drain, for the
        /// server to apply to the core (which owns the dock state).
        take_dock_ops -> Vec<DockOp> = dock_ops
    }

    take_queue! {
        /// Take the terminal-open requests queued by `nx.terminal.open` since the
        /// last drain, for the server to apply to the core (`Editor::open_terminal`).
        take_terminal_open_reqs -> Vec<TerminalOpenReq> = terminal_ops
    }

    take_queue! {
        /// Take the server-start requests queued by `vim.lsp.start` since the last
        /// drain, for the server to apply to its `LspManager`.
        take_lsp_ops -> Vec<LspOp> = lsp_ops
    }

    take_queue! {
        /// Take the async-runtime requests queued by `vim.schedule` / `vim.defer_fn` /
        /// `vim.uv` timers / `vim.system` since the last drain, for the server to
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
        /// Take the `vim.ui.input` prompt requests queued since the last drain, for
        /// the server to open as command-line prompts (Phase 8).
        take_ui_inputs -> Vec<UiInputReq> = ui_inputs
    }

    take_queue! {
        /// Take the `nx.ui.select` requests queued since the last drain, for the
        /// server to open as floating selectable-list menus.
        take_ui_selects -> Vec<UiSelectReq> = ui_selects
    }

    take_queue! {
        /// Take the `nx.picker.open` requests queued since the last drain, for the
        /// server to open as fuzzy-finder widgets.
        take_picker_opens -> Vec<PickerOpenReq> = picker_opens
    }

    take_queue! {
        /// Take the `nx.complete.setup{}` configurations queued since the last
        /// drain, for the server to apply to the native completion engine.
        take_complete_setups -> Vec<CompleteSetupReq> = complete_setups
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

    /// Whether any `vim.on_key` observer is registered — the cheap guard the
    /// server checks per key before paying for [`Self::run_on_key`]. `false` on
    /// any error (a malformed VM simply has no observers).
    pub fn has_on_key(&self) -> bool {
        self.read_has_on_key().unwrap_or(false)
    }

    fn read_has_on_key(&self) -> mlua::Result<bool> {
        let f: mlua::Function = self.nx()?.get("_has_on_key")?;
        f.call::<bool>(())
    }

    /// Fire every `vim.on_key` observer with `key` (vim key-notation), passed as
    /// both the `(key, typed)` arguments neovim's on_key callback receives. A
    /// throwing observer is detached (matching neovim) inside `nx._run_on_key`;
    /// any other error is returned for the server to surface.
    pub fn run_on_key(&self, key: &str) -> mlua::Result<()> {
        let run: mlua::Function = self.nx()?.get("_run_on_key")?;
        let k = mlua::Value::String(self.lua.create_string(key)?);
        run.call::<()>((k.clone(), k))
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

    /// Deliver the picker's outcome to the active source: the chosen item's
    /// **`key`** (the 1-based wrapper index — `Some`) confirms, `nil` (`None`)
    /// cancels (`nx._picker_result`). The wrapper resolves `key` to the original
    /// item and calls the source's `confirm(item)`, then clears the active picker.
    pub fn run_picker_result(&self, key: Option<usize>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let run: mlua::Function = nx.get("_picker_result")?;
        let arg = match key {
            Some(k) => mlua::Value::Integer(lua_int(k as i64)),
            None => mlua::Value::Nil,
        };
        run.call::<()>((arg,))
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
        }
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

    /// Record the OS pid of an async `vim.system` child (keyed by its callback
    /// `id`) so the handle's `.pid` field resolves it. Delivered by the event-loop
    /// actor shortly after the spawn — the pid can't be known synchronously on the
    /// single-threaded runtime, so the handle reads `nil` until this lands.
    pub fn set_process_pid(&self, id: u64, pid: Option<u32>) -> mlua::Result<()> {
        let nx = self.nx()?;
        let set: mlua::Function = nx.get("_set_proc_pid")?;
        set.call((id, pid))
    }

    /// Fire the panel's `on_select` callback for the line at `index` (0-based,
    /// passed to Lua 1-based) with text `line`. A no-op when no callback is
    /// registered. Errors (a throwing handler) are returned for the server to
    /// surface. Called when the user hits `<CR>` on a select-enabled panel.
    pub fn run_panel_select(&self, index: usize, line: &str) -> mlua::Result<()> {
        let cb: Option<mlua::Function> = self.lua.named_registry_value(PANEL_ON_SELECT)?;
        if let Some(f) = cb {
            f.call::<()>((line.to_string(), index as i64 + 1))?;
        }
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

    /// Pull `nx._keymaps` across the bridge as a list of [`RawKeymap`]s for the
    /// server to compile into per-mode tries. A read error yields an empty
    /// snapshot (the editor keeps running with no user mappings).
    pub fn keymaps_snapshot(&self) -> Vec<RawKeymap> {
        self.read_keymaps().unwrap_or_default()
    }

    fn read_keymaps(&self) -> mlua::Result<Vec<RawKeymap>> {
        let nx = self.nx()?;
        let list: Table = nx.get("_keymaps")?;
        let mut out = Vec::new();
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
    /// edits (the `vim.treesitter` parser's incremental-reparse channel). Each edit
    /// carries its bufnr/changedtick and neovim's relative `on_bytes` tuple; the
    /// prelude's `nx._buf_bytes_changed` dispatches it to that buffer's attached
    /// callbacks (the vendored `LanguageTree:_on_bytes` among them, which edits its
    /// trees). Edits are forwarded in order — tree-sitter requires every edit
    /// between two parses, in sequence, or the incremental tree corrupts.
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

    /// Fire an autocmd with buffer context *and* an `args.data` payload — the
    /// `{ client_id = … }` table neovim's `LspAttach`/`LspDetach` carry. The
    /// server fires these at the attach (didOpen) and detach (didClose / server
    /// exit) moments; the default `nxvim.lsp.enable` autocmd reads `client_id` to
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
    /// which `vim.lsp.enable` reads to start a server for the already-open buffer.
    /// (Interim until a real per-bufnr registry exists.)
    pub fn set_buf_snapshot(&self, bufnr: u64, name: &str, filetype: &str) -> mlua::Result<()> {
        let set: mlua::Function = self.nx()?.get("_set_cur_buf")?;
        set.call((bufnr, name, filetype))
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
    pub fn run_user_command(&self, name: &str, args: &str, bufnr: u64) -> mlua::Result<()> {
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
                opts.set("bang", false)?;
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
