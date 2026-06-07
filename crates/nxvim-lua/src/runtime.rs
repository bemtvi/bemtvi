//! The embedded Lua VM and its Rust-facing API. [`LuaRuntime`] owns the `mlua`
//! state and the [`Shared`] effect buffer; its methods are the only way the
//! server talks to Lua — running chunks / callbacks, pushing the Rust→Lua state
//! mirrors (buffers, diagnostics, clients), and draining the queued effects. The
//! `vim.*` surface it drives is installed by [`crate::install`] and layered with
//! the `src/prelude/` Lua modules in [`LuaRuntime::new`].

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, LuaOptions, StdLib, Table};

use crate::convert::{json_to_lua, lua_to_rmpv};
use crate::host::seed_package_path;
use crate::install::{install_runtime_api, install_vim, PANEL_ON_SELECT};
use crate::ops::{
    BufOp, CallbackArgs, ConfirmReq, DiagnosticData, GlobalOptionOp, HlSet, LoopOp, LspClientData,
    LspOp, PanelOp, RawKeymap, RawRhs, UiInputReq, WindowOp,
};

/// One window's row in the Rust→Lua window mirror, in layout order. The
/// number/relativenumber flags back `vim.wo`'s wired window-local options;
/// `float` carries a floating window's placement so `nvim_win_get_config` reads
/// it from Lua (`None` for a tiled window).
#[derive(Clone, Debug, Default)]
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
    pub float: Option<FloatMirror>,
}

/// A floating window's placement for the [`WindowMirror`], pre-formatted into the
/// strings `nvim_win_get_config` returns (the server translates the core's
/// `FloatConfig` enums into these so nxvim-lua stays free of the core's types).
#[derive(Clone, Debug)]
pub struct FloatMirror {
    /// `"editor"` / `"win"` / `"cursor"`.
    pub relative: String,
    /// The parent window for `relative == "win"`, else `0`.
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
    pub title: Option<String>,
}

/// The pure-Lua `vim.*` prelude, split into focused modules under `src/prelude/`
/// and loaded in this order at VM init — the order is significant (a later module
/// reads `vim.*` an earlier one installed), so it mirrors the original single
/// file top to bottom: the core stdlib first, the runtime/registry services, then
/// the editor-facing surfaces. `(chunk name, source)`; the name shows up in Lua
/// tracebacks.
const PRELUDE_MODULES: &[(&str, &str)] = &[
    ("nxvim:prelude/stdlib", include_str!("prelude/stdlib.lua")),
    ("nxvim:prelude/runtime", include_str!("prelude/runtime.lua")),
    ("nxvim:prelude/api", include_str!("prelude/api.lua")),
    ("nxvim:prelude/keymap", include_str!("prelude/keymap.lua")),
    ("nxvim:prelude/fs", include_str!("prelude/fs.lua")),
    ("nxvim:prelude/system", include_str!("prelude/system.lua")),
    ("nxvim:prelude/timer", include_str!("prelude/timer.lua")),
    ("nxvim:prelude/lsp", include_str!("prelude/lsp.lua")),
    (
        "nxvim:prelude/diagnostic",
        include_str!("prelude/diagnostic.lua"),
    ),
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
    /// Server-start requests from `vim.lsp.start` (driven by `vim.lsp.enable`),
    /// drained by the server into its `LspManager` after the chunk.
    pub(crate) lsp_ops: Vec<LspOp>,
    /// Async-runtime requests from `vim.schedule` / `vim.defer_fn` / `vim.uv`
    /// timers / async `vim.system`, drained by the server into its scheduled-work
    /// queue and event-loop actor after the chunk.
    pub(crate) loop_ops: Vec<LoopOp>,
    /// Buffer mutations from `vim.api.nvim_buf_set_lines`, drained by the server
    /// into the live editor after the chunk (Phase 6).
    pub(crate) buf_ops: Vec<BufOp>,
    /// Window mutations from the `vim.api.nvim_win_*` / `nvim_open_win` /
    /// `nvim_set_current_win` API, drained by the server into the live editor
    /// after the chunk (Phase 5).
    pub(crate) window_ops: Vec<WindowOp>,
    /// Global-option writes from `vim.o` for a wired search option, drained by
    /// the server into the editor's global options after the chunk.
    pub(crate) global_ops: Vec<GlobalOptionOp>,
    /// `vim.ui.input` prompt requests, drained by the server into the editor's
    /// command line (`Editor::open_prompt`) after the chunk (Phase 8).
    pub(crate) ui_inputs: Vec<UiInputReq>,
    /// `vim.fn.confirm` button-dialog requests, drained by the server into the
    /// editor's command line (`Editor::open_confirm`) after the chunk.
    pub(crate) confirms: Vec<ConfirmReq>,
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
        let lua = unsafe {
            Lua::unsafe_new_with(StdLib::ALL_SAFE | StdLib::DEBUG, LuaOptions::default())
        };
        let shared = Rc::new(RefCell::new(Shared::default()));
        install_vim(&lua, &shared)?;
        install_runtime_api(&lua, &shared, &runtimepath)?;
        seed_package_path(&lua, &runtimepath)?;
        // The pure-Lua half of `vim.*`, layered over the Rust bridge above. Split
        // across focused modules but loaded in source order — each is its own
        // chunk (its own `local` scope), so the order is what one big chunk's was.
        // Their chunk names carry into Lua tracebacks (`nxvim:prelude/lsp:42`).
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

    /// The runtimepath this VM searches (read by the colorscheme/`require`
    /// machinery to locate `colors/<name>.lua` and friends).
    pub fn runtimepath(&self) -> &[PathBuf] {
        &self.runtimepath
    }

    /// Run a Lua chunk. Errors are returned for the server to surface.
    pub fn exec(&self, chunk: &str) -> mlua::Result<()> {
        self.lua.load(chunk).exec()
    }

    /// Compile `chunk` to a callable function, trying the expression form
    /// (`return <chunk>`) first and falling back to statements — the same
    /// dual-mode load `Chunk::eval` does, so both an expression and a statement
    /// block become a function we can run inside a coroutine.
    fn load_callable(&self, chunk: &str) -> mlua::Result<mlua::Function> {
        if let Ok(f) = self.lua.load(format!("return {chunk}")).into_function() {
            return Ok(f);
        }
        self.lua.load(chunk).into_function()
    }

    /// Run `func` through the prompt pump (`vim._pump`), which executes it inside a
    /// coroutine so a `vim.fn.input` / `vim.fn.confirm` in it can park on the
    /// command line and resume with the answer. Returns the function's first
    /// return value (`Some`) when it ran to completion, or `None` when it parked
    /// on a prompt (the prompt-result callback resumes the coroutine later). A
    /// throwing function propagates its error (re-raised by `vim._pump`).
    fn pump(&self, func: mlua::Function) -> mlua::Result<Option<mlua::Value>> {
        let pump: mlua::Function = self.vim()?.get("_pump")?;
        let (completed, value): (bool, mlua::Value) = pump.call(func)?;
        Ok(completed.then_some(value))
    }

    /// Run a `:lua` chunk under the prompt pump — the [`exec`](Self::exec)
    /// analogue for the queued `:lua` drain, so a `vim.fn.input` / `vim.fn.confirm`
    /// in the chunk parks on the command line instead of erroring "outside a
    /// coroutine". Errors are returned for the server to surface.
    pub fn exec_pumped(&self, chunk: &str) -> mlua::Result<()> {
        let func = self.lua.load(chunk).into_function()?;
        self.pump(func)?;
        Ok(())
    }

    /// [`eval_to_value`](Self::eval_to_value) under the prompt pump — the
    /// `nvim_exec_lua` entry. When the chunk parks on a prompt its return value is
    /// not available synchronously, so `Nil` is returned and the chunk's eventual
    /// value is discarded (a documented limit of blocking from a synchronous RPC
    /// getter; drive prompts from a keymap / `:lua` instead).
    pub fn eval_to_value_pumped(&self, chunk: &str) -> mlua::Result<rmpv::Value> {
        let func = self.load_callable(chunk)?;
        match self.pump(func)? {
            Some(value) => lua_to_rmpv(&value),
            None => Ok(rmpv::Value::Nil),
        }
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

    /// Mirror a buffer's diagnostics into `vim._diagnostics[bufnr]` as the plain
    /// data `vim.diagnostic.get` reads back (the Rust→Lua state mirror). Called on
    /// every `publishDiagnostics`; keyed by `bufnr`, so it never goes stale on a
    /// buffer switch (the getter resolves `0` → current via `vim._cur_buf`).
    pub fn set_diagnostics(&self, bufnr: u64, diags: &[DiagnosticData]) -> mlua::Result<()> {
        let vim = self.vim()?;
        let set: mlua::Function = vim.get("_set_diagnostics")?;
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

    /// Mirror one LSP client into `vim.lsp._clients[id]` (the Rust→Lua client
    /// registry) so `get_client_by_id` — and the `LspAttach` `on_attach` it feeds
    /// — can read `client.server_capabilities`. Pushed once per server when it
    /// finishes `initialize`. The provider flags become the camelCase
    /// `*Provider` keys neovim configs probe.
    pub fn set_lsp_client(&self, client: &LspClientData) -> mlua::Result<()> {
        let lsp: Table = self.vim()?.get("lsp")?;
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
        set.call((client.id, client.name.clone(), caps))
    }

    /// Forget an LSP client (`vim.lsp._clients[id] = nil`) when its server exits,
    /// so a stale `get_client_by_id` after a `LspDetach` returns `nil`.
    pub fn remove_lsp_client(&self, id: u64) -> mlua::Result<()> {
        let lsp: Table = self.vim()?.get("lsp")?;
        let remove: mlua::Function = lsp.get("_remove_client")?;
        remove.call(id)
    }

    /// Run the config's `on_init(client, result)` hook for client `id` (Phase 3),
    /// passing the raw `initialize` result as a Lua table. Called when the server
    /// finishes `initialize`, right after the client is mirrored — so the hook can
    /// read `result.capabilities` / `result.offsetEncoding` and tweak the client.
    pub fn run_lsp_on_init(&self, id: u64, result: &serde_json::Value) -> mlua::Result<()> {
        let lsp: Table = self.vim()?.get("lsp")?;
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
        let lsp: Table = self.vim()?.get("lsp")?;
        let run: mlua::Function = lsp.get("_run_on_exit")?;
        run.call((id, code, signal))
    }

    /// Take ex-commands queued by `vim.cmd` since the last drain.
    pub fn take_commands(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.borrow_mut().commands)
    }

    /// Take captured `print` output since the last drain.
    pub fn take_output(&self) -> Vec<String> {
        std::mem::take(&mut self.shared.borrow_mut().output)
    }

    /// Take the highlight-group definitions queued by `nvim_set_hl` since the
    /// last drain, for the server to apply to the core registry.
    pub fn take_highlights(&self) -> Vec<HlSet> {
        std::mem::take(&mut self.shared.borrow_mut().highlights)
    }

    /// Take the panel requests queued by `vim.panel.*` since the last drain, for
    /// the server to apply to the core (which owns the panel state).
    pub fn take_panel_ops(&self) -> Vec<PanelOp> {
        std::mem::take(&mut self.shared.borrow_mut().panel_ops)
    }

    /// Take the server-start requests queued by `vim.lsp.start` since the last
    /// drain, for the server to apply to its `LspManager`.
    pub fn take_lsp_ops(&self) -> Vec<LspOp> {
        std::mem::take(&mut self.shared.borrow_mut().lsp_ops)
    }

    /// Take the async-runtime requests queued by `vim.schedule` / `vim.defer_fn` /
    /// `vim.uv` timers / `vim.system` since the last drain, for the server to
    /// service directly (`Schedule`) or forward to the event-loop actor.
    pub fn take_loop_ops(&self) -> Vec<LoopOp> {
        std::mem::take(&mut self.shared.borrow_mut().loop_ops)
    }

    /// Take the buffer mutations queued by `nvim_buf_set_lines` since the last
    /// drain, for the server to apply to the live editor (Phase 6).
    pub fn take_buf_ops(&self) -> Vec<BufOp> {
        std::mem::take(&mut self.shared.borrow_mut().buf_ops)
    }

    /// Take the window mutations queued by the `vim.api.nvim_win_*` family since
    /// the last drain, for the server to apply to the live editor (Phase 5).
    pub fn take_window_ops(&self) -> Vec<WindowOp> {
        std::mem::take(&mut self.shared.borrow_mut().window_ops)
    }

    /// Take the global-option writes queued by `vim.o` since the last drain, for
    /// the server to apply to the editor's global options.
    pub fn take_global_ops(&self) -> Vec<GlobalOptionOp> {
        std::mem::take(&mut self.shared.borrow_mut().global_ops)
    }

    /// Take the `vim.ui.input` prompt requests queued since the last drain, for
    /// the server to open as command-line prompts (Phase 8).
    pub fn take_ui_inputs(&self) -> Vec<UiInputReq> {
        std::mem::take(&mut self.shared.borrow_mut().ui_inputs)
    }

    /// Take the `vim.fn.confirm` button-dialog requests queued since the last
    /// drain, for the server to open as command-line confirm prompts.
    pub fn take_confirms(&self) -> Vec<ConfirmReq> {
        std::mem::take(&mut self.shared.borrow_mut().confirms)
    }

    /// Deliver a `vim.ui.input` result to its callback `id`: the typed line
    /// (`Some`) on `<CR>`, or `nil` (`None`) on cancel. Runs `vim._run_cb(id,
    /// false, text)` — a one-shot, so the callback registry entry is dropped after
    /// firing (Phase 8). Effects it queues drain through `apply_lua_effects`.
    pub fn run_ui_input(&self, id: u64, result: Option<String>) -> mlua::Result<()> {
        let vim = self.vim()?;
        let run: mlua::Function = vim.get("_run_cb")?;
        let arg = match result {
            Some(s) => mlua::Value::String(self.lua.create_string(&s)?),
            None => mlua::Value::Nil,
        };
        run.call::<()>((id, false, arg))
    }

    /// Dispatch an LSP code-action `command` (Phase 8): runs
    /// `vim.lsp._dispatch_command(client_id, command)`, which routes to a
    /// client-side `vim.lsp.commands[name]` handler when registered, else issues a
    /// `workspace/executeCommand` to the client's server. `command` is the LSP
    /// `Command` (`{ title, command, arguments }`) as JSON. Errors are returned for
    /// the server to surface.
    pub fn run_lsp_command(&self, client_id: u64, command: &serde_json::Value) -> mlua::Result<()> {
        let lsp: Table = self.vim()?.get("lsp")?;
        let dispatch: mlua::Function = lsp.get("_dispatch_command")?;
        let cmd = json_to_lua(&self.lua, command)?;
        dispatch.call((client_id, cmd))
    }

    /// Run the deferred callback registered under `id` (the `run_keymap` analogue
    /// for the async runtime). Invokes `vim._run_cb(id, keep, …)`; with `keep ==
    /// false` the registry entry is dropped after firing (one-shot), so
    /// `vim.schedule` / `vim.defer_fn` / `vim.system` `on_exit` never leak. A
    /// repeating timer passes `keep == true` to retain its function across fires.
    /// `args` are forwarded to the Lua callback as its arguments. Effects the
    /// callback queues land in [`Shared`] and drain through the server's
    /// `apply_lua_effects`; a throwing callback returns its error for the server to
    /// surface (it isolates one callback, never aborting the drain).
    pub fn run_callback(&self, id: u64, keep: bool, args: CallbackArgs) -> mlua::Result<()> {
        let vim = self.vim()?;
        let run: mlua::Function = vim.get("_run_cb")?;
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

    /// Record the OS pid of an async `vim.system` child (keyed by its callback
    /// `id`) so the handle's `.pid` field resolves it. Delivered by the event-loop
    /// actor shortly after the spawn — the pid can't be known synchronously on the
    /// single-threaded runtime, so the handle reads `nil` until this lands.
    pub fn set_process_pid(&self, id: u64, pid: Option<u32>) -> mlua::Result<()> {
        let vim = self.vim()?;
        let set: mlua::Function = vim.get("_set_proc_pid")?;
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

    /// The current `vim._keymaps_version`, bumped by every `vim.keymap.set`/`del`.
    /// The server reads it once per input batch and rebuilds its tries only when
    /// it advanced — so per keystroke it walks the cached trie, never the bridge.
    /// `0` on any error (a malformed VM simply yields no mappings).
    pub fn keymaps_version(&self) -> u64 {
        self.read_keymaps_version().unwrap_or(0)
    }

    fn read_keymaps_version(&self) -> mlua::Result<u64> {
        let vim = self.vim()?;
        Ok(vim.get::<Option<u64>>("_keymaps_version")?.unwrap_or(0))
    }

    /// Pull `vim._keymaps` across the bridge as a list of [`RawKeymap`]s for the
    /// server to compile into per-mode tries. A read error yields an empty
    /// snapshot (the editor keeps running with no user mappings).
    pub fn keymaps_snapshot(&self) -> Vec<RawKeymap> {
        self.read_keymaps().unwrap_or_default()
    }

    fn read_keymaps(&self) -> mlua::Result<Vec<RawKeymap>> {
        let vim = self.vim()?;
        let list: Table = vim.get("_keymaps")?;
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
        let vim = self.vim()?;
        let run: mlua::Function = vim.get("_run_keymap")?;
        run.call::<()>(id)
    }

    /// Invoke an `<expr>` function RHS and return the **keys it produced** (its
    /// return value, coerced to a string; `nil`/`false` → `""`). The function runs
    /// under the prelude's `vim._expr_lock` so the editor-mutating funnels refuse
    /// (the textlock contract — see `vim._run_keymap_expr`); any effects it queued
    /// anyway are discarded by the server, which feeds only the returned keys. An
    /// error (a throwing handler, or a textlock violation) is returned to surface.
    pub fn run_keymap_expr(&self, id: u64) -> mlua::Result<String> {
        let vim = self.vim()?;
        let run: mlua::Function = vim.get("_run_keymap_expr")?;
        run.call::<String>(id)
    }

    /// Set `vim.g[key] = value` from Rust — used to record `g:colors_name` when
    /// `:colorscheme` loads a theme, so Lua and the editor agree on the name.
    pub fn set_global_var(&self, key: &str, value: &str) -> mlua::Result<()> {
        let g: Table = self.vim()?.get("g")?;
        g.set(key, value)
    }

    /// Fire every autocmd registered for `event` whose pattern matches
    /// `pattern` (used for `ColorScheme` when a theme loads). Delegates to the
    /// prelude's `vim._fire`, which runs callbacks / queues `command` strings;
    /// effects land in [`Shared`] and drain like any other chunk.
    pub fn fire_autocmd(&self, event: &str, pattern: &str) -> mlua::Result<()> {
        let fire: mlua::Function = self.vim()?.get("_fire")?;
        fire.call((event, pattern))
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
        let fire: mlua::Function = self.vim()?.get("_fire")?;
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
        let fire: mlua::Function = self.vim()?.get("_fire")?;
        let data = self.lua.create_table()?;
        data.set("client_id", client_id)?;
        fire.call((event, pattern, buf, file, data))
    }

    /// Refresh the `vim._cur_buf` snapshot the prelude reads back through
    /// `nvim_buf_get_name(0)` / `expand('%')`. The server pushes this immediately
    /// before firing a buffer/mode autocmd so a callback can resolve the buffer
    /// that fired. `filetype` is the buffer's detected filetype (`""` when none),
    /// which `vim.lsp.enable` reads to start a server for the already-open buffer.
    /// (Interim until a real per-bufnr registry exists.)
    pub fn set_buf_snapshot(&self, bufnr: u64, name: &str, filetype: &str) -> mlua::Result<()> {
        let set: mlua::Function = self.vim()?.get("_set_cur_buf")?;
        set.call((bufnr, name, filetype))
    }

    /// Refresh the Rust→Lua buffer mirror the buffer-read API resolves against
    /// (Phase 6): `vim._bufs[bufnr] = { lines, name, loaded = true }` for every
    /// open buffer, plus `vim._cur_cursor = { row, col }` (row 1-based, col 0-based,
    /// neovim convention) and the current-window handle. The server pushes this
    /// before running any Lua that can read buffer/cursor state, so synchronous
    /// getters (`nvim_buf_get_lines`, `nvim_win_get_cursor`, …) read live data
    /// without reaching the `Server`. `set_lines` write-through mutates this same
    /// mirror in Lua so a read-after-write within one chunk stays consistent.
    ///
    /// `bufs` is `(bufnr, lines, name)` per open buffer; `lines` may be empty when
    /// the caller is only refreshing the cheap cursor/window fields (the server
    /// gates the line arrays on `changedtick`), in which case the existing mirror
    /// `lines` are kept.
    /// `wins` is one [`WindowMirror`] per open window in layout order. `cur_win`
    /// is the focused id and `next_win` the id the next `nvim_open_win` will mint
    /// (so the Lua side can return the new handle synchronously while the real
    /// window is created when the queued op drains).
    #[allow(clippy::too_many_arguments)]
    pub fn set_buf_mirror(
        &self,
        bufs: &[(u64, Option<Vec<String>>, String)],
        cursor: (u64, u64),
        win: u64,
        wins: &[WindowMirror],
        next_win: u64,
    ) -> mlua::Result<()> {
        let vim = self.vim()?;
        let entries = self.lua.create_table()?;
        for (bufnr, lines, name) in bufs {
            let entry = self.lua.create_table()?;
            if let Some(lines) = lines {
                let arr = self.lua.create_table()?;
                for (i, line) in lines.iter().enumerate() {
                    arr.set(i + 1, self.lua.create_string(line)?)?;
                }
                entry.set("lines", arr)?;
            }
            entry.set("name", self.lua.create_string(name)?)?;
            entry.set("bufnr", *bufnr)?;
            entries.set(*bufnr, entry)?;
        }
        let win_arr = self.lua.create_table()?;
        for (i, m) in wins.iter().enumerate() {
            let w = self.lua.create_table()?;
            w.set("id", m.id)?;
            w.set("buffer", m.buffer)?;
            w.set("row", m.row)?;
            w.set("col", m.col)?;
            w.set("width", m.width)?;
            w.set("height", m.height)?;
            w.set("number", m.number)?;
            w.set("relativenumber", m.relativenumber)?;
            // A float carries its placement as a nested table, the shape
            // `nvim_win_get_config` returns (and that `nvim_win_set_config`'s
            // write-through merges into).
            if let Some(f) = &m.float {
                let ft = self.lua.create_table()?;
                ft.set("relative", self.lua.create_string(&f.relative)?)?;
                if f.win != 0 {
                    ft.set("win", f.win)?;
                }
                ft.set("anchor", self.lua.create_string(&f.anchor)?)?;
                ft.set("row", f.row)?;
                ft.set("col", f.col)?;
                ft.set("width", f.width)?;
                ft.set("height", f.height)?;
                ft.set("zindex", f.zindex)?;
                ft.set("focusable", f.focusable)?;
                ft.set("border", self.lua.create_string(&f.border)?)?;
                if let Some(title) = &f.title {
                    ft.set("title", self.lua.create_string(title)?)?;
                }
                w.set("float", ft)?;
            }
            win_arr.set(i + 1, w)?;
        }
        let set: mlua::Function = vim.get("_set_buf_mirror")?;
        set.call((entries, cursor.0, cursor.1, win, win_arr, next_win))
    }

    /// Refresh the Rust→Lua buffer-option mirror (`vim._bo_mirror[bufnr] =
    /// { tabstop, shiftwidth, expandtab }`) that `vim.bo` / `nvim_get_option_value`
    /// read for the wired buffer-local options. Pushed alongside the buffer mirror
    /// before any Lua that can read options, so a read reflects the core's current
    /// value — the option's default until set, and a value set through the `:set`
    /// ex-command path (not just one written from Lua). `bufs` is
    /// `(bufnr, tabstop, shiftwidth, softtabstop, expandtab)` per open buffer.
    pub fn set_bo_mirror(&self, bufs: &[(u64, usize, usize, isize, bool)]) -> mlua::Result<()> {
        let vim = self.vim()?;
        let entries = self.lua.create_table()?;
        for (bufnr, tabstop, shiftwidth, softtabstop, expandtab) in bufs {
            let entry = self.lua.create_table()?;
            entry.set("tabstop", *tabstop)?;
            entry.set("shiftwidth", *shiftwidth)?;
            entry.set("softtabstop", *softtabstop)?;
            entry.set("expandtab", *expandtab)?;
            entries.set(*bufnr, entry)?;
        }
        let set: mlua::Function = vim.get("_set_bo_mirror")?;
        set.call(entries)
    }

    /// Refresh the Rust→Lua global-option mirror (`vim._go_mirror = { ignorecase,
    /// smartcase, wrapscan, hlsearch, incsearch }`) that `vim.o` reads for the
    /// wired global search options. Pushed alongside the buffer mirror before any
    /// Lua that can read options, so a read reflects the core's current value —
    /// the default until set, and a value set through the `:set` ex path, not just
    /// one written from Lua.
    pub fn set_go_mirror(&self, opts: (bool, bool, bool, bool, bool)) -> mlua::Result<()> {
        let vim = self.vim()?;
        let (ignorecase, smartcase, wrapscan, hlsearch, incsearch) = opts;
        let entry = self.lua.create_table()?;
        entry.set("ignorecase", ignorecase)?;
        entry.set("smartcase", smartcase)?;
        entry.set("wrapscan", wrapscan)?;
        entry.set("hlsearch", hlsearch)?;
        entry.set("incsearch", incsearch)?;
        let set: mlua::Function = vim.get("_set_go_mirror")?;
        set.call(entry)
    }

    /// Whether `name` was registered via `nvim_create_user_command` (so the
    /// server can route a deferred `:Name …` to its Lua callback).
    pub fn has_user_command(&self, name: &str) -> bool {
        self.user_command(name)
            .map(|v| !v.is_nil())
            .unwrap_or(false)
    }

    /// Invoke the user command `name` with `args` (the text after the name).
    /// A function command is called with an opts table (`name`, `args`,
    /// `fargs`, `bang`); a string command is queued as an ex-command. Effects
    /// land in [`Shared`] and are drained by the server like any other chunk.
    pub fn run_user_command(&self, name: &str, args: &str) -> mlua::Result<()> {
        match self.user_command(name)? {
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
                // Run through the prompt pump so a `vim.fn.input` / `vim.fn.confirm`
                // in the command's body can block and use the answer.
                let pump: mlua::Function = self.vim()?.get("_pump")?;
                pump.call::<mlua::MultiValue>((f, opts))?;
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

    /// Look up the stored `vim._user_commands[name]` entry (function or string).
    fn user_command(&self, name: &str) -> mlua::Result<mlua::Value> {
        let commands: Table = self.vim()?.get("_user_commands")?;
        commands.get(name)
    }
}
