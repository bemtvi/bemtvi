//! The Lua runtime and the beginnings of the `vim.*` standard library.
//!
//! nxvim embeds Lua 5.1 (mlua's `lua51`, the dialect LuaJIT — and therefore
//! neovim — is compatible with). Scripts run inside the *server*, exactly as in
//! neovim, and influence the editor through the same mechanisms RPC clients use.
//!
//! The surface is split in two: editor-touching functions are installed from
//! Rust here (`vim.cmd`, `vim.api.nvim_command`/`nvim_echo`/`nvim_set_hl`,
//! `vim.fn.*`, and the `print` capture), while the broad pure-Lua part of
//! `vim.*` — the table / list / string helpers, `vim.g`/`vim.o`/`vim.opt`/
//! `vim.env`, `vim.iter`, and the registration APIs (`nvim_create_user_command`,
//! `nvim_create_autocmd`, the `vim._fire` autocmd dispatcher) — lives in
//! [`prelude.lua`], run once at init. The data-flow stays "Lua -> queued
//! commands / output / highlights -> core mutation": effects are buffered in
//! [`Shared`] and drained by the server after each chunk.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, LuaOptions, StdLib, Table, Variadic};

/// A highlight-group definition produced by `nvim_set_hl(0, name, opts)`, in
/// the wire-ish form the server translates into `nxvim_core`'s `HlDef`. Colors
/// are kept as the strings the opts table carried (`"#rrggbb"` / `"NONE"` /
/// named, with integer colors normalized to `#rrggbb`); the core parses them,
/// so this crate stays free of any color/registry types.
#[derive(Clone, Debug, Default)]
pub struct HlSet {
    pub name: String,
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub sp: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub undercurl: bool,
    pub strikethrough: bool,
    pub reverse: bool,
    pub link: Option<String>,
}

/// A request to the bottom message panel, queued by the `vim.panel.*` functions
/// and drained by the server into the core (which owns the panel state). nxvim's
/// own surface — the panel is not a neovim concept.
#[derive(Clone, Debug)]
pub enum PanelOp {
    /// `vim.panel.open(title, lines[, on_select[, cursor]])` — open (or replace)
    /// and focus the panel. `wants_select` is set when an `on_select` callback
    /// was given, enabling `<CR>` select events. `cursor` is the initially
    /// selected line (0-based; the panel scrolls to keep it visible).
    Open {
        title: String,
        lines: Vec<String>,
        wants_select: bool,
        cursor: usize,
    },
    /// `vim.panel.set_lines(lines)` — replace the open panel's content.
    SetLines(Vec<String>),
    /// `vim.panel.on_select(fn|nil)` — enable/disable `<CR>` select events on the
    /// open panel (the callback itself lives in the Lua registry).
    OnSelect(bool),
    /// `vim.panel.set_cursor(line)` — move the open panel's selection to the
    /// given line (0-based) and scroll it into view.
    SetCursor(usize),
    /// `vim.panel.close()` — close the panel.
    Close,
}

/// A request to start (or attach a buffer to) a language server, queued by
/// `vim.lsp.start` after user Lua — directly, or through the `vim.lsp.enable`
/// FileType dispatcher — resolved the config. The root is resolved **in Lua**
/// (string / `root_markers` upward search / a `function(bufnr, on_dir)`), so the
/// server never re-resolves it; it only ensures the `(name, root)` client exists
/// and binds `bufnr` to it. The server's analogue of [`PanelOp`] for LSP.
#[derive(Clone, Debug)]
pub enum LspOp {
    /// `vim.lsp.start({name, cmd, root_dir}, {bufnr, filetype})`.
    Start {
        /// The config name (`vim.lsp.config('<name>', …)`), the client identity.
        name: String,
        /// The full server argv (program + args). The server may override this
        /// with `$NXVIM_LSP_CMD` (the test/mock hook), as the old built-in table did.
        cmd: Vec<String>,
        /// The workspace root, already resolved in Lua (`None` ⇒ the server falls
        /// back to the file's directory).
        root: Option<String>,
        /// The buffer's filetype, used verbatim as the LSP `languageId`.
        filetype: String,
        /// The buffer to attach, as the `BufferId` the server snapshotted into
        /// `vim._cur_buf` before firing `FileType` (so it round-trips exactly).
        bufnr: u64,
        /// The config's `init_options`, sent verbatim as `initialization_options`
        /// at `initialize` (Phase 2). `None` when the config sets none.
        init_options: Option<serde_json::Value>,
        /// The config's `settings`: the fallback `initialization_options` when
        /// `init_options` is absent, and the payload of the post-`initialized`
        /// `workspace/didChangeConfiguration` (Phase 2). `None` when unset.
        settings: Option<serde_json::Value>,
        /// The config's `capabilities`, deep-merged OVER nxvim's base client
        /// capabilities at `initialize` (Phase 2). `None` when the config adds none.
        capabilities: Option<serde_json::Value>,
    },
    /// A `vim.lsp.buf.*` language-feature request (definition, references,
    /// hover, …) on the current buffer. `kind` is `LspReqKind::as_u16` — the same
    /// int that rides the request token — so the wire stays one number; the server
    /// reads `self.editor.cursor` at apply time (the tick the key fired).
    BufRequest {
        /// `LspReqKind::as_u16` of the position-family feature to request.
        kind: u16,
    },
    /// `vim.lsp.buf.format()` — request `textDocument/formatting`.
    Format,
    /// `vim.lsp.buf.rename(name)` — request `textDocument/rename` with `new_name`.
    Rename {
        /// The new identifier (the required argument; `vim.lsp.buf.rename()` with
        /// no name is rejected in Lua, never reaching this op).
        new_name: String,
    },
    /// `vim.lsp.buf.code_action()` — request `textDocument/codeAction` at the cursor.
    CodeAction,
    /// `vim.diagnostic.goto_next()` / `goto_prev()` — move the cursor to the next
    /// (`forward`) or previous diagnostic in the current buffer, wrapping.
    DiagnosticGoto {
        /// `true` for `goto_next`, `false` for `goto_prev`.
        forward: bool,
        /// Restrict to a single severity (`vim.diagnostic.severity.*`, 1=ERROR…
        /// 4=HINT); `None` considers all severities.
        severity: Option<u8>,
    },
    /// `vim.diagnostic.setloclist()` — open the current buffer's diagnostics as
    /// the navigable panel location list (the `:LspDiagnostics` surface).
    DiagnosticSetloclist,
    /// `vim.diagnostic.config({ underline = … })` — toggle the one diagnostic
    /// surface nxvim has (the underline spans). Other config keys (virt-text,
    /// signs) have no surface yet and are stored Lua-side without an op.
    DiagnosticConfig {
        /// Whether diagnostic underline spans are painted (neovim's `underline`,
        /// default on; `false` disables the squiggles).
        underline: bool,
    },
    /// `client:request(method, params, handler)` (Phase 5) — issue a generic LSP
    /// request to the client `client_id`'s server and route its reply to the Lua
    /// callback `cb_id` (a `vim._cb_fns` entry holding the resolved handler). The
    /// server resolves the client's [`ServerKey`] and forwards a raw request; the
    /// reply comes back off-tick as a [`CallbackArgs::LspReply`].
    ClientRequest {
        /// The target client id (`client.id`, the handle `LspAttach` resolves).
        client_id: u64,
        /// The LSP method (e.g. `workspace/executeCommand`).
        method: String,
        /// The request params as JSON (`Null` when the caller passed none).
        params: serde_json::Value,
        /// The `vim._cb_fns` id the reply's handler is registered under.
        cb_id: u64,
    },
    /// `client:notify(method, params)` (Phase 5) — fire-and-forget a generic LSP
    /// notification to the client `client_id`'s server.
    ClientNotify {
        /// The target client id.
        client_id: u64,
        /// The LSP notification method (e.g. `$/setTrace`).
        method: String,
        /// The notification params as JSON (`Null` when the caller passed none).
        params: serde_json::Value,
    },
    /// `vim.lsp.util.apply_workspace_edit(edit)` (Phase 7) — apply a `WorkspaceEdit`
    /// across the open buffers it names, reusing the native rename / code-action
    /// application path. The server deserializes the JSON into `lsp_types`,
    /// normalizes it, and applies the per-document edits.
    ApplyWorkspaceEdit {
        /// The `WorkspaceEdit` as JSON (`changes` / `documentChanges`), converted
        /// through the same `lua_to_json` bridge `vim.json.encode` uses.
        edit: serde_json::Value,
    },
    /// `vim.lsp.util.show_document(location)` (Phase 7) — jump the cursor to an LSP
    /// location (opening the file if needed), reusing the native single-location
    /// goto path.
    ShowDocument {
        /// The target document URI (`file://…`).
        uri: String,
        /// 0-based line within the document.
        line: u32,
        /// The start position's character, in `encoding`.
        character: u32,
        /// The position offset encoding (`utf-8` / `utf-16` / `utf-32`).
        encoding: String,
    },
}

/// A request to the async runtime (the "event loop"), queued by the `vim.schedule`
/// / `vim.defer_fn` / `vim.uv` timer / `vim.system` family and drained by the
/// server in `apply_lua_effects`. Each op carries a `cb_id` into `vim._cb_fns`
/// (the deferred-callback registry); the server either services it directly
/// ([`LoopOp::Schedule`], same-convergence deferral) or forwards it to the
/// background event-loop actor (timers and processes, which take wall-clock time).
/// The async analogue of [`PanelOp`]/[`LspOp`] — Lua queues, the server drives.
#[derive(Clone, Debug)]
pub enum LoopOp {
    /// `vim.schedule(fn)` — run callback `id` once, at the end of the current
    /// convergence (no wall-clock wait, no actor; serviced inside `run_pending`).
    Schedule { id: u64 },
    /// `vim.defer_fn` / `vim.uv` timer `:start` — arm a timer that fires callback
    /// `id` after `delay_ms`, then every `repeat_ms` while `repeat_ms > 0` (a
    /// one-shot when `repeat_ms == 0`). Forwarded to the event-loop actor.
    TimerStart {
        id: u64,
        delay_ms: u64,
        repeat_ms: u64,
    },
    /// `vim.uv` timer `:stop`/`:close` (or a `defer_fn` handle's `:stop`) — cancel
    /// the timer armed under `id`. A no-op if it already fired or was never armed.
    TimerStop { id: u64 },
    /// `vim.system(cmd, opts, on_exit)` with an `on_exit` — spawn `cmd` in the
    /// actor (off the server thread) and run callback `id` with the result when it
    /// exits. The pid is returned synchronously by the bridge; only the *wait* is
    /// async.
    Spawn {
        id: u64,
        cmd: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    },
    /// `handle:kill(signal)` on a `vim.system` handle spawned async — terminate the
    /// child running under `id`. A no-op if it already exited. The `signal`
    /// argument is accepted at the Lua surface for call-compatibility but not
    /// carried here: the actor terminates the child unconditionally (it has no
    /// libc binding to deliver an arbitrary signal), a documented approximation.
    Kill { id: u64 },
}

/// A buffer mutation queued by the buffer Lua API (`vim.api.nvim_buf_set_lines`),
/// drained by the server in `apply_lua_effects` and applied to the live editor —
/// the buffer-text analogue of [`LspOp`]/[`LoopOp`]. Reads (`nvim_buf_get_lines`,
/// the cursor) need no op: they read the Rust→Lua *mirror* the server pushes via
/// [`LuaRuntime::set_buf_mirror`] before running Lua. `set_lines` write-through
/// updates that mirror in Lua first (so read-after-write within a chunk is
/// consistent) and queues this op so the real buffer catches up after the chunk.
#[derive(Clone, Debug)]
pub enum BufOp {
    /// `nvim_buf_set_lines(bufnr, start, end, strict, repl)` — replace lines
    /// `[start, end)` of buffer `bufnr` with `repl`. Indices are neovim's: 0-based,
    /// `end` exclusive, negatives count from the end, `end == -1` is the last line.
    /// The server normalizes them against the real line count and converts the line
    /// range to a byte range before calling `Editor::apply_edits_to`.
    SetLines {
        bufnr: u64,
        start: i64,
        end: i64,
        repl: Vec<String>,
    },
}

/// The arguments handed to a deferred callback when the server runs it via
/// [`LuaRuntime::run_callback`]. A `vim.schedule` / timer callback takes none; an
/// async `vim.system` `on_exit` takes the finished child's result, built into the
/// `{ code, stdout, stderr }` table neovim's `on_exit` receives. Keeping the
/// payload typed here (rather than passing `mlua::Value`s) lets the actor — which
/// produced the bytes off-thread — stay free of any Lua types.
pub enum CallbackArgs {
    /// No arguments (`vim.schedule`, `vim.defer_fn`, `vim.uv` timers).
    None,
    /// An async `vim.system` exit: the result table its `on_exit` is called with.
    Process {
        code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    /// The reply to a `client:request` (Phase 5): the handler is called as
    /// `handler(err, result)` — `err` a message string (the request failed) with
    /// `result` nil, or `err` nil with `result` the server's JSON value. Exactly
    /// one is set, mirroring neovim's `(err, result, ctx)` handler signature.
    LspReply {
        /// `Some(message)` when the request failed (unsupported method, transport
        /// error, or the server replied an error); `None` on success.
        err: Option<String>,
        /// The server's JSON result on success; `Null` when `err` is set.
        result: serde_json::Value,
    },
}

/// One diagnostic mirrored into the Lua `vim._diagnostics[bufnr]` table so the
/// synchronous getter `vim.diagnostic.get` can read it without reaching the live
/// `Server` (the Rust→Lua mirror, the analogue of `vim._set_cur_buf`). Fields
/// match neovim's `vim.diagnostic.get` shape: 0-based positions and severity
/// numbered 1=ERROR…4=HINT. `col`/`end_col` are in the server's negotiated
/// position encoding — byte offsets under the UTF-8 nxvim advertises first.
#[derive(Clone, Debug)]
pub struct DiagnosticData {
    /// 0-based start line.
    pub lnum: i64,
    /// 0-based start column.
    pub col: i64,
    /// 0-based end line.
    pub end_lnum: i64,
    /// 0-based (exclusive) end column.
    pub end_col: i64,
    /// Severity, 1=ERROR…4=HINT.
    pub severity: u8,
    /// The diagnostic message (may be multi-line).
    pub message: String,
    /// The reporting source (server/linter name), if any.
    pub source: Option<String>,
}

/// One LSP client mirrored into `vim.lsp._clients[id]` so `on_attach` (and any
/// Lua) can read `client.server_capabilities` (Phase 7b Slice 3). Pushed once per
/// server when it finishes `initialize`; the server translates its `ProviderCaps`
/// into these booleans so nxvim-lua stays free of the LSP crate.
#[derive(Clone, Debug)]
pub struct LspClientData {
    /// The numeric client id, stable per server instance — the handle
    /// `LspAttach`'s `args.data.client_id` resolves through `get_client_by_id`.
    pub id: u64,
    /// The config name (`vim.lsp.config('<name>', …)`), which the default
    /// `LspAttach` autocmd uses to find the config's `on_attach`.
    pub name: String,
    /// Per-feature provider flags, surfaced as the camelCase
    /// `server_capabilities.*Provider` keys neovim configs probe.
    pub capabilities: LspServerCapabilities,
}

/// The per-feature provider flags of an [`LspClientData`], one bool per feature
/// nxvim implements. The server fills these from `nxvim_lsp::ProviderCaps`.
#[derive(Clone, Debug, Default)]
pub struct LspServerCapabilities {
    pub definition: bool,
    pub declaration: bool,
    pub type_definition: bool,
    pub implementation: bool,
    pub references: bool,
    pub hover: bool,
    pub signature_help: bool,
    pub completion: bool,
    pub document_formatting: bool,
    pub rename: bool,
    pub code_action: bool,
}

/// Lua registry key under which the panel's `on_select` callback is stored.
const PANEL_ON_SELECT: &str = "nxvim_panel_on_select";

/// One `vim.keymap.set` entry, read back from `vim._keymaps` as plain data for
/// the server to compile into its per-mode prefix tries. Unlike autocmds — whose
/// *matching* stays in Lua (`vim._fire`) — keymap matching happens in Rust, so
/// the runtime exposes the registry as a snapshot rather than a dispatcher.
#[derive(Clone, Debug)]
pub struct RawKeymap {
    /// Mode codes the entry applies to (single-char: `"n"`, `"i"`, …).
    pub modes: Vec<String>,
    /// The unparsed LHS notation (`"gd"`, `"<Space>x"`); the server runs it
    /// through `parse_keys` to get the trie path.
    pub lhs: String,
    /// What fires when the LHS matches.
    pub rhs: RawRhs,
    /// `true` to feed a string RHS straight to the editor (no re-mapping).
    pub noremap: bool,
    /// Buffer handle for a buffer-local map; `None` for a global one.
    pub buffer: Option<u64>,
    /// The `desc` opt — stored, surfaced later; unused by matching.
    pub desc: Option<String>,
    /// `<nowait>`: fire this mapping the moment it completes, even when it is a
    /// prefix of a longer one (the matcher reads this in `classify`).
    pub nowait: bool,
    /// `<silent>`: suppress the message line the mapping's execution produces.
    pub silent: bool,
    /// `<expr>`: a function RHS computes the keys to feed (its return value) rather
    /// than acting directly; the server runs it via `run_keymap_expr` and feeds the
    /// result. Ignored for a string RHS (nxvim has no expression evaluator).
    pub expr: bool,
    /// A built-in default (overridable by a user map); `false` for user maps.
    pub default: bool,
    /// The registry sequence id — also the function-RHS key and the
    /// last-set-wins tiebreaker in the precedence ladder.
    pub seq: u64,
}

/// A keymap's right-hand side, as carried in the snapshot.
#[derive(Clone, Debug)]
pub enum RawRhs {
    /// A function RHS, keyed by id in `vim._keymap_fns` (run via `run_keymap`).
    Lua(u64),
    /// A string RHS — key notation the server parses and feeds.
    Str(String),
}

/// Side effects produced by running Lua, drained by the server.
#[derive(Default)]
struct Shared {
    /// Ex-commands requested via `vim.cmd(...)`.
    commands: Vec<String>,
    /// Text emitted via `print(...)` / `vim.api.nvim_echo(...)`.
    output: Vec<String>,
    /// Highlight-group definitions from `nvim_set_hl`, applied to the core
    /// registry after the chunk drains (so the core stays the sole mutator).
    highlights: Vec<HlSet>,
    /// Panel requests from `vim.panel.*`, applied to the core after the chunk.
    panel_ops: Vec<PanelOp>,
    /// Server-start requests from `vim.lsp.start` (driven by `vim.lsp.enable`),
    /// drained by the server into its `LspManager` after the chunk.
    lsp_ops: Vec<LspOp>,
    /// Async-runtime requests from `vim.schedule` / `vim.defer_fn` / `vim.uv`
    /// timers / async `vim.system`, drained by the server into its scheduled-work
    /// queue and event-loop actor after the chunk.
    loop_ops: Vec<LoopOp>,
    /// Buffer mutations from `vim.api.nvim_buf_set_lines`, drained by the server
    /// into the live editor after the chunk (Phase 6).
    buf_ops: Vec<BufOp>,
    /// `vim.ui.input` prompt requests, drained by the server into the editor's
    /// command line (`Editor::open_prompt`) after the chunk (Phase 8).
    ui_inputs: Vec<UiInputReq>,
}

/// A `vim.ui.input(opts, on_confirm)` request: open a one-line prompt labelled
/// `prompt`, prefilled with `default`, and fire callback `cb_id` (a `vim._cb_fns`
/// entry wrapping `on_confirm`) with the typed text — or `nil` on cancel — when
/// the user submits. Queued in [`Shared::ui_inputs`], drained by the server.
#[derive(Clone, Debug)]
pub struct UiInputReq {
    /// The prompt label shown ahead of the editable line (`opts.prompt`).
    pub prompt: String,
    /// The text the line is prefilled with (`opts.default`; empty when unset).
    pub default: String,
    /// The `vim._cb_fns` id whose `on_confirm` wrapper receives the result.
    pub cb_id: u64,
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
        // The pure-Lua half of `vim.*`, layered over the Rust bridge above.
        lua.load(include_str!("prelude.lua"))
            .set_name("nxvim:prelude")
            .exec()?;
        Ok(LuaRuntime {
            lua,
            shared,
            runtimepath,
        })
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
        let vim: Table = self.lua.globals().get("vim")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let lsp: Table = vim.get("lsp")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let lsp: Table = vim.get("lsp")?;
        let remove: mlua::Function = lsp.get("_remove_client")?;
        remove.call(id)
    }

    /// Run the config's `on_init(client, result)` hook for client `id` (Phase 3),
    /// passing the raw `initialize` result as a Lua table. Called when the server
    /// finishes `initialize`, right after the client is mirrored — so the hook can
    /// read `result.capabilities` / `result.offsetEncoding` and tweak the client.
    pub fn run_lsp_on_init(&self, id: u64, result: &serde_json::Value) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
        let lsp: Table = vim.get("lsp")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let lsp: Table = vim.get("lsp")?;
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

    /// Take the `vim.ui.input` prompt requests queued since the last drain, for
    /// the server to open as command-line prompts (Phase 8).
    pub fn take_ui_inputs(&self) -> Vec<UiInputReq> {
        std::mem::take(&mut self.shared.borrow_mut().ui_inputs)
    }

    /// Deliver a `vim.ui.input` result to its callback `id`: the typed line
    /// (`Some`) on `<CR>`, or `nil` (`None`) on cancel. Runs `vim._run_cb(id,
    /// false, text)` — a one-shot, so the callback registry entry is dropped after
    /// firing (Phase 8). Effects it queues drain through `apply_lua_effects`.
    pub fn run_ui_input(&self, id: u64, result: Option<String>) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let lsp: Table = vim.get("lsp")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        Ok(vim.get::<Option<u64>>("_keymaps_version")?.unwrap_or(0))
    }

    /// Pull `vim._keymaps` across the bridge as a list of [`RawKeymap`]s for the
    /// server to compile into per-mode tries. A read error yields an empty
    /// snapshot (the editor keeps running with no user mappings).
    pub fn keymaps_snapshot(&self) -> Vec<RawKeymap> {
        self.read_keymaps().unwrap_or_default()
    }

    fn read_keymaps(&self) -> mlua::Result<Vec<RawKeymap>> {
        let vim: Table = self.lua.globals().get("vim")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let run: mlua::Function = vim.get("_run_keymap_expr")?;
        run.call::<String>(id)
    }

    /// Set `vim.g[key] = value` from Rust — used to record `g:colors_name` when
    /// `:colorscheme` loads a theme, so Lua and the editor agree on the name.
    pub fn set_global_var(&self, key: &str, value: &str) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
        let g: Table = vim.get("g")?;
        g.set(key, value)
    }

    /// Fire every autocmd registered for `event` whose pattern matches
    /// `pattern` (used for `ColorScheme` when a theme loads). Delegates to the
    /// prelude's `vim._fire`, which runs callbacks / queues `command` strings;
    /// effects land in [`Shared`] and drain like any other chunk.
    pub fn fire_autocmd(&self, event: &str, pattern: &str) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
        let fire: mlua::Function = vim.get("_fire")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let fire: mlua::Function = vim.get("_fire")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let fire: mlua::Function = vim.get("_fire")?;
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
        let vim: Table = self.lua.globals().get("vim")?;
        let set: mlua::Function = vim.get("_set_cur_buf")?;
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
    pub fn set_buf_mirror(
        &self,
        bufs: &[(u64, Option<Vec<String>>, String)],
        cursor: (u64, u64),
        win: u64,
    ) -> mlua::Result<()> {
        let vim: Table = self.lua.globals().get("vim")?;
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
        let set: mlua::Function = vim.get("_set_buf_mirror")?;
        set.call((entries, cursor.0, cursor.1, win))
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
                f.call::<()>(opts)
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
        let vim: Table = self.lua.globals().get("vim")?;
        let commands: Table = vim.get("_user_commands")?;
        commands.get(name)
    }
}

fn install_vim(lua: &Lua, shared: &Rc<RefCell<Shared>>) -> mlua::Result<()> {
    let vim = lua.create_table()?;

    let sh = shared.clone();
    vim.set(
        "cmd",
        lua.create_function(move |_, cmd: String| {
            sh.borrow_mut().commands.push(cmd);
            Ok(())
        })?,
    )?;

    vim.set("version", "nxvim 0.1.0")?;

    // A minimal `vim.api` namespace; grows toward the full nvim_* surface.
    let api = lua.create_table()?;
    let sh = shared.clone();
    api.set(
        "nvim_command",
        lua.create_function(move |_, cmd: String| {
            sh.borrow_mut().commands.push(cmd);
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    api.set(
        "nvim_echo",
        lua.create_function(move |_, msg: String| {
            sh.borrow_mut().output.push(msg);
            Ok(())
        })?,
    )?;
    // `nvim_set_hl(ns, name, opts)`: capture the group definition for the server
    // to fold into the core registry. `ns` is accepted but ignored (namespace 0
    // only); the full opts shape — colors, the boolean attrs, and `link` — is
    // read here so a colorscheme's hundreds of calls all land.
    let sh = shared.clone();
    api.set(
        "nvim_set_hl",
        lua.create_function(move |_, (_ns, name, opts): (i64, String, Option<Table>)| {
            let mut def = HlSet {
                name,
                ..Default::default()
            };
            if let Some(opts) = &opts {
                def.fg = color_field(opts, "fg")?;
                def.bg = color_field(opts, "bg")?;
                def.sp = color_field(opts, "sp")?;
                def.bold = flag_field(opts, "bold")?;
                def.italic = flag_field(opts, "italic")?;
                def.underline = flag_field(opts, "underline")?;
                def.undercurl = flag_field(opts, "undercurl")?;
                def.strikethrough = flag_field(opts, "strikethrough")?;
                def.reverse = flag_field(opts, "reverse")?;
                def.link = opts.get::<Option<String>>("link")?;
            }
            sh.borrow_mut().highlights.push(def);
            Ok(())
        })?,
    )?;
    vim.set("api", api)?;

    // `vim.panel`: nxvim's scriptable handle on the bottom message panel
    // (`:messages` / `:ls`'s home). Each call queues a [`PanelOp`] the server
    // drains into the core after the chunk runs — same "Lua queues, core
    // mutates" flow as `vim.cmd` / `nvim_set_hl`.
    let panel = lua.create_table()?;
    // `open(title, lines[, on_select[, cursor]])`: `on_select` is a
    // `function(line, index)` called when the user hits `<CR>` on a line (index
    // is 1-based). It is stored in the Lua registry; passing it enables select
    // events for this panel. `cursor` is the initially selected line (1-based,
    // matching the `on_select` index); it defaults to the first line.
    let sh = shared.clone();
    panel.set(
        "open",
        lua.create_function(
            move |lua,
                  (title, lines, on_select, cursor): (
                String,
                Option<Vec<String>>,
                Option<mlua::Function>,
                Option<usize>,
            )| {
                store_panel_callback(lua, on_select.clone())?;
                sh.borrow_mut().panel_ops.push(PanelOp::Open {
                    title,
                    lines: lines.unwrap_or_default(),
                    wants_select: on_select.is_some(),
                    cursor: cursor.map(|c| c.saturating_sub(1)).unwrap_or(0),
                });
                Ok(())
            },
        )?,
    )?;
    let sh = shared.clone();
    panel.set(
        "set_lines",
        lua.create_function(move |_, lines: Vec<String>| {
            sh.borrow_mut().panel_ops.push(PanelOp::SetLines(lines));
            Ok(())
        })?,
    )?;
    // `on_select(fn|nil)`: set or clear the open panel's `<CR>` handler.
    let sh = shared.clone();
    panel.set(
        "on_select",
        lua.create_function(move |lua, on_select: Option<mlua::Function>| {
            store_panel_callback(lua, on_select.clone())?;
            sh.borrow_mut()
                .panel_ops
                .push(PanelOp::OnSelect(on_select.is_some()));
            Ok(())
        })?,
    )?;
    // `set_cursor(line)`: move the open panel's selection (1-based, matching the
    // `on_select` index) and scroll it into view.
    let sh = shared.clone();
    panel.set(
        "set_cursor",
        lua.create_function(move |_, line: usize| {
            sh.borrow_mut()
                .panel_ops
                .push(PanelOp::SetCursor(line.saturating_sub(1)));
            Ok(())
        })?,
    )?;
    let sh = shared.clone();
    panel.set(
        "close",
        lua.create_function(move |lua, ()| {
            store_panel_callback(lua, None)?; // drop the handler with the panel
            sh.borrow_mut().panel_ops.push(PanelOp::Close);
            Ok(())
        })?,
    )?;
    vim.set("panel", panel)?;

    // ----- the async runtime bridge (the "event loop") -----------------------
    // Lua queues a [`LoopOp`] carrying a callback id; the server drains it in
    // `apply_lua_effects` and either services it directly (`Schedule`) or forwards
    // it to the background event-loop actor (timers, processes). Same "Lua queues,
    // the server drives" flow as `vim.cmd` / panel / lsp ops — the callback itself
    // stays in the Lua registry (`vim._cb_fns[id]`) and runs on the server thread.

    // `vim._schedule(id)`: defer callback `id` to the end of the current
    // convergence (the strict, non-nested `vim.schedule`).
    let sh = shared.clone();
    vim.set(
        "_schedule",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::Schedule { id });
            Ok(())
        })?,
    )?;
    // `vim._timer_start(id, delay_ms, repeat_ms)`: arm a timer firing callback
    // `id` after `delay_ms`, then every `repeat_ms` (`0` ⇒ one-shot).
    let sh = shared.clone();
    vim.set(
        "_timer_start",
        lua.create_function(move |_, (id, delay_ms, repeat_ms): (u64, u64, u64)| {
            sh.borrow_mut().loop_ops.push(LoopOp::TimerStart {
                id,
                delay_ms,
                repeat_ms,
            });
            Ok(())
        })?,
    )?;
    // `vim._timer_stop(id)`: cancel the timer armed under `id`.
    let sh = shared.clone();
    vim.set(
        "_timer_stop",
        lua.create_function(move |_, id: u64| {
            sh.borrow_mut().loop_ops.push(LoopOp::TimerStop { id });
            Ok(())
        })?,
    )?;
    // `vim._system_async(id, cmd, cwd, env)`: spawn `cmd` (an argv list) in the
    // event-loop actor and run callback `id` with `{ code, stdout, stderr }` when
    // it exits — the off-tick `vim.system`. Returns the child's OS pid immediately
    // (the actor sends it back over a oneshot the bridge blocks on *briefly* — only
    // until the spawn itself completes, not the run), so the `vim.system` handle
    // carries a real pid while the wait stays async. A spawn failure surfaces as a
    // `nil` pid (the `on_exit` still fires later with `code = -1`).
    let sh = shared.clone();
    vim.set(
        "_system_async",
        lua.create_function(
            move |_, (id, cmd, cwd, env): (u64, Vec<String>, Option<String>, Option<Table>)| {
                let env = env_pairs(env)?;
                sh.borrow_mut()
                    .loop_ops
                    .push(LoopOp::Spawn { id, cmd, cwd, env });
                Ok(())
            },
        )?,
    )?;
    // `vim._system_kill(id, signal)`: terminate the async child running under
    // `id`. `signal` is accepted (neovim's `handle:kill(signal)`) but ignored —
    // the actor terminates the child unconditionally (see [`LoopOp::Kill`]).
    let sh = shared.clone();
    vim.set(
        "_system_kill",
        lua.create_function(move |_, (id, _signal): (u64, Option<i32>)| {
            sh.borrow_mut().loop_ops.push(LoopOp::Kill { id });
            Ok(())
        })?,
    )?;

    // `vim.fn`: the Vimscript builtins the load path calls. Only the ones that
    // need real filesystem / environment access are Rust-backed; the rest of
    // `vim.*` is pure Lua in the prelude.
    let func = lua.create_table()?;
    func.set(
        "stdpath",
        lua.create_function(|_, what: String| Ok(stdpath(&what)))?,
    )?;
    func.set(
        "getftime",
        lua.create_function(|_, path: String| Ok(getftime(&path)))?,
    )?;
    func.set(
        "isdirectory",
        lua.create_function(|_, path: String| {
            Ok(if std::path::Path::new(&path).is_dir() {
                1i64
            } else {
                0
            })
        })?,
    )?;
    func.set(
        "mkdir",
        lua.create_function(
            |_, (path, _flags, prot): (String, Option<String>, Option<mlua::Value>)| {
                // `mkdir(path, "p" [, prot])` — create parents; return 1 on
                // success, 0 on failure (Vimscript's truthy/falsey convention).
                // `prot` is the permission mask (neovim's third arg): an octal
                // string like "0700" or a numeric mode. Honoring it means a
                // private data/state dir isn't silently created world-readable.
                Ok(if create_dir_all_mode(&path, parse_mode(prot)) {
                    1i64
                } else {
                    0
                })
            },
        )?,
    )?;
    func.set(
        "has",
        lua.create_function(|_, feature: String| {
            // Claim a modern neovim so version-gated code takes its full path;
            // unknown features report absent. Refined as needs surface.
            Ok(if feature.starts_with("nvim-") {
                1i64
            } else {
                0
            })
        })?,
    )?;
    vim.set("fn", func)?;

    lua.globals().set("vim", vim)?;

    // Capture `print` so output can be shown on the message line.
    let sh = shared.clone();
    lua.globals().set(
        "print",
        lua.create_function(move |lua, args: Variadic<mlua::Value>| {
            let parts: Vec<String> = args.iter().map(|v| stringify(lua, v)).collect();
            sh.borrow_mut().output.push(parts.join("\t"));
            Ok(())
        })?,
    )?;

    Ok(())
}

/// The argument tuple of `vim._lsp_start`: the original five
/// (`name`, `cmd`, `root`, `filetype`, `bufnr`) plus the Phase-2 config payloads
/// (`init_options`, `settings`, `capabilities`, each a table or `nil`).
type LspStartArgs = (
    String,
    Vec<String>,
    Option<String>,
    String,
    u64,
    Option<Table>,
    Option<Table>,
    Option<Table>,
);

/// Install the `vim.*` functions that need the host filesystem / environment /
/// runtimepath and feed the LSP framework (Phase 7a): `nvim_get_runtime_file`
/// (runtimepath `lsp/` discovery), `vim.fn.getcwd`, the `vim._read_file` /
/// `vim._readdir` filesystem primitives the pure-Lua `vim.fs` builds on, and
/// `vim._lsp_start` (the queue `vim.lsp.start` pushes onto). Separated from
/// [`install_vim`] because these capture the runtimepath, known only here.
fn install_runtime_api(
    lua: &Lua,
    shared: &Rc<RefCell<Shared>>,
    runtimepath: &[PathBuf],
) -> mlua::Result<()> {
    let vim: Table = lua.globals().get("vim")?;
    let api: Table = vim.get("api")?;
    let func: Table = vim.get("fn")?;

    // `nvim_get_runtime_file(name, all)`: full paths of files matching `name`
    // (a runtimepath-relative path, the final component optionally globbed with
    // `*`) across the runtimepath. `all=false` returns the first match only. The
    // `lsp/<server>.lua` config-discovery primitive.
    let rtp = runtimepath.to_vec();
    api.set(
        "nvim_get_runtime_file",
        lua.create_function(move |lua, (name, all): (String, Option<bool>)| {
            let hits = get_runtime_file(&rtp, &name, all.unwrap_or(false));
            lua.create_sequence_from(hits)
        })?,
    )?;

    // `vim.fn.getcwd()`: the process working directory (the root fallback and the
    // base for relative->absolute path math in `vim.fs`/`fnamemodify`).
    func.set(
        "getcwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default())
        })?,
    )?;

    // `vim._read_file(path)`: the file's contents, or nil if unreadable. Backs the
    // pure-Lua loader that sources an `lsp/<name>.lua` config (via `loadstring`),
    // sidestepping any `loadfile` sandbox question.
    vim.set(
        "_read_file",
        lua.create_function(|_, path: String| Ok(std::fs::read_to_string(&path).ok()))?,
    )?;

    // `vim._readdir(path)`: the entry names directly under `path` (no `.`/`..`),
    // or an empty list if it can't be read. Backs `vim.fs.find`/predicate markers.
    vim.set(
        "_readdir",
        lua.create_function(|lua, path: String| {
            let names: Vec<String> = std::fs::read_dir(&path)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            lua.create_sequence_from(names)
        })?,
    )?;

    // `vim._lsp_start(name, cmd, root, filetype, bufnr, init_options, settings,
    // capabilities)`: queue an [`LspOp::Start`] for the server to drain. The
    // Lua-facing `vim.lsp.start` wrapper (prelude) resolves the config and root,
    // then calls this. The trailing three are the config's `init_options` /
    // `settings` / `capabilities` tables (each `nil` when unset); they convert
    // through the same `lua_to_json` bridge `vim.json.encode` uses, so the server
    // forwards them at `initialize` exactly as the config wrote them (Phase 2).
    let sh = shared.clone();
    vim.set(
        "_lsp_start",
        lua.create_function(move |_, args: LspStartArgs| {
            let (name, cmd, root, filetype, bufnr, init_options, settings, capabilities) = args;
            sh.borrow_mut().lsp_ops.push(LspOp::Start {
                name,
                cmd,
                root,
                filetype,
                bufnr,
                init_options: opt_table_to_json(init_options)?,
                settings: opt_table_to_json(settings)?,
                capabilities: opt_table_to_json(capabilities)?,
            });
            Ok(())
        })?,
    )?;

    // `vim._buf_set_lines(bufnr, start, end, repl)`: queue a [`BufOp::SetLines`]
    // for the server to apply to the live editor (Phase 6). The Lua-facing
    // `vim.api.nvim_buf_set_lines` wrapper (prelude) has already updated the
    // `vim._bufs` mirror (write-through) and resolved `bufnr` to a concrete id; the
    // server normalizes the indices and converts the line range to a byte range.
    let sh = shared.clone();
    vim.set(
        "_buf_set_lines",
        lua.create_function(
            move |_, (bufnr, start, end, repl): (u64, i64, i64, Vec<String>)| {
                sh.borrow_mut().buf_ops.push(BufOp::SetLines {
                    bufnr,
                    start,
                    end,
                    repl,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._lsp_buf(kind)`: queue a position-family `vim.lsp.buf.*` request
    // ([`LspOp::BufRequest`]) or one of the edit ops (`Format`/`CodeAction`),
    // selected by the `LspReqKind::as_u16` the prelude passes. The single Rust
    // entry the bare `vim.lsp.buf` functions route through (rename has its own,
    // below, since it carries an argument).
    let sh = shared.clone();
    vim.set(
        "_lsp_buf",
        lua.create_function(move |_, kind: u16| {
            sh.borrow_mut().lsp_ops.push(LspOp::BufRequest { kind });
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf_format()`: queue [`LspOp::Format`]. Kept distinct from
    // `_lsp_buf` because formatting has no `{uri, position}` shape (it routes to
    // `request_lsp_format`, not `request_lsp`).
    let sh = shared.clone();
    vim.set(
        "_lsp_buf_format",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::Format);
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf_code_action()`: queue [`LspOp::CodeAction`].
    let sh = shared.clone();
    vim.set(
        "_lsp_buf_code_action",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::CodeAction);
            Ok(())
        })?,
    )?;

    // `vim._lsp_buf_rename(name)`: queue [`LspOp::Rename`]. The prelude requires
    // the argument (echoing `E471` on nil), so a name always arrives here.
    let sh = shared.clone();
    vim.set(
        "_lsp_buf_rename",
        lua.create_function(move |_, new_name: String| {
            sh.borrow_mut().lsp_ops.push(LspOp::Rename { new_name });
            Ok(())
        })?,
    )?;

    // `vim._diagnostic_goto(forward, severity)`: queue [`LspOp::DiagnosticGoto`]
    // — the cursor move `vim.diagnostic.goto_next`/`goto_prev` drive.
    let sh = shared.clone();
    vim.set(
        "_diagnostic_goto",
        lua.create_function(move |_, (forward, severity): (bool, Option<u16>)| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticGoto {
                forward,
                severity: severity.map(|s| s as u8),
            });
            Ok(())
        })?,
    )?;

    // `vim._diagnostic_setloclist()`: queue [`LspOp::DiagnosticSetloclist`] — open
    // the diagnostics location-list panel.
    let sh = shared.clone();
    vim.set(
        "_diagnostic_setloclist",
        lua.create_function(move |_, ()| {
            sh.borrow_mut().lsp_ops.push(LspOp::DiagnosticSetloclist);
            Ok(())
        })?,
    )?;

    // `vim._diagnostic_config(underline)`: queue [`LspOp::DiagnosticConfig`] — the
    // prelude resolves the merged `underline` to a bool and pushes it so the
    // server gates the squiggle rendering.
    let sh = shared.clone();
    vim.set(
        "_diagnostic_config",
        lua.create_function(move |_, underline: bool| {
            sh.borrow_mut()
                .lsp_ops
                .push(LspOp::DiagnosticConfig { underline });
            Ok(())
        })?,
    )?;

    // `vim._lsp_client_request(client_id, method, params, cb_id)`: queue a generic
    // `client:request` ([`LspOp::ClientRequest`]). The handler is already stored in
    // `vim._cb_fns[cb_id]` by the Lua wrapper; the server forwards the request and
    // runs the callback with `(err, result)` when the reply lands (Phase 5).
    // `params` is any Lua value (a table / nil), converted through the same
    // `lua_to_json` bridge `vim.json.encode` uses.
    let sh = shared.clone();
    vim.set(
        "_lsp_client_request",
        lua.create_function(
            move |_, (client_id, method, params, cb_id): (u64, String, mlua::Value, u64)| {
                sh.borrow_mut().lsp_ops.push(LspOp::ClientRequest {
                    client_id,
                    method,
                    params: lua_to_json(&params)?,
                    cb_id,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._lsp_client_notify(client_id, method, params)`: queue a generic
    // fire-and-forget `client:notify` ([`LspOp::ClientNotify`]).
    let sh = shared.clone();
    vim.set(
        "_lsp_client_notify",
        lua.create_function(
            move |_, (client_id, method, params): (u64, String, mlua::Value)| {
                sh.borrow_mut().lsp_ops.push(LspOp::ClientNotify {
                    client_id,
                    method,
                    params: lua_to_json(&params)?,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._lsp_apply_workspace_edit(edit)`: queue [`LspOp::ApplyWorkspaceEdit`]
    // (Phase 7). `edit` is the LSP-shape WorkspaceEdit table, converted to JSON
    // through the same `lua_to_json` bridge `client:request` params use; the server
    // deserializes, normalizes, and applies it across the open buffers it names.
    let sh = shared.clone();
    vim.set(
        "_lsp_apply_workspace_edit",
        lua.create_function(move |_, edit: mlua::Value| {
            sh.borrow_mut().lsp_ops.push(LspOp::ApplyWorkspaceEdit {
                edit: lua_to_json(&edit)?,
            });
            Ok(())
        })?,
    )?;

    // `vim._lsp_show_document(uri, line, character, encoding)`: queue
    // [`LspOp::ShowDocument`] (Phase 7) — the server builds an LSP location and
    // reuses the native single-location goto (open + cursor jump).
    let sh = shared.clone();
    vim.set(
        "_lsp_show_document",
        lua.create_function(
            move |_, (uri, line, character, encoding): (String, u32, u32, String)| {
                sh.borrow_mut().lsp_ops.push(LspOp::ShowDocument {
                    uri,
                    line,
                    character,
                    encoding,
                });
                Ok(())
            },
        )?,
    )?;

    // `vim._ui_input(prompt, default, cb_id)`: queue a `vim.ui.input` prompt
    // ([`UiInputReq`]). The server opens the editor's command line labelled
    // `prompt` (prefilled with `default`) and fires `vim._cb_fns[cb_id]` with the
    // typed text — or `nil` on cancel — when the user submits (Phase 8).
    let sh = shared.clone();
    vim.set(
        "_ui_input",
        lua.create_function(move |_, (prompt, default, cb_id): (String, String, u64)| {
            sh.borrow_mut().ui_inputs.push(UiInputReq {
                prompt,
                default,
                cb_id,
            });
            Ok(())
        })?,
    )?;

    // `vim._ui_opener()`: the OS file/URL opener argv prefix `vim.ui.open` spawns
    // (via the async `vim.system`), chosen by platform — `open` on macOS,
    // `xdg-open` elsewhere (Phase 8). The path is appended by the Lua wrapper.
    vim.set(
        "_ui_opener",
        lua.create_function(|_, ()| {
            Ok(match std::env::consts::OS {
                "macos" => vec!["open".to_string()],
                "windows" => vec!["explorer".to_string()],
                _ => vec!["xdg-open".to_string()],
            })
        })?,
    )?;

    // `vim._system(cmd, cwd, env, text)`: spawn `cmd` (an argv list — no shell),
    // block until it exits, and return `{ code, stdout, stderr }`. The pure-Lua
    // `vim.system` wrapper layers neovim's object shape (`:wait()` / `on_exit`)
    // over it. It is synchronous because the Lua VM has no event loop yet (see
    // `vim.schedule`): an `lsp/<server>.lua` `root_dir` that shells out — e.g.
    // rust_analyzer's `cargo metadata` / `rustc --print sysroot` — runs to
    // completion inline on the input tick. Unlike neovim, a spawn failure (a
    // missing tool) degrades to `code = -1` with the message on `stderr` instead
    // of raising, so it can never break `vim.lsp.enable` on a machine that lacks
    // the toolchain. stdout/stderr are returned as Lua byte strings (so non-UTF-8
    // output survives), independent of the `text` flag, which is accepted and
    // ignored.
    vim.set(
        "_system",
        lua.create_function(
            |lua,
             (cmd, cwd, env, _text): (
                Vec<String>,
                Option<String>,
                Option<Table>,
                Option<bool>,
            )| {
                let Some((program, args)) = cmd.split_first() else {
                    return Err(mlua::Error::external(
                        "vim.system: cmd must be a non-empty list",
                    ));
                };
                let mut command = std::process::Command::new(program);
                command.args(args).stdin(std::process::Stdio::null());
                if let Some(dir) = cwd {
                    command.current_dir(dir);
                }
                if let Some(env_tbl) = env {
                    for (k, v) in env_tbl.pairs::<String, String>().flatten() {
                        command.env(k, v);
                    }
                }
                let result = lua.create_table()?;
                // Spawn (capturing the real pid) then wait, rather than `output()`,
                // so the synchronous handle's `result.pid` is a real pid — parity
                // with the async path. The wait is short by construction (a
                // `root_dir` shell-out), so blocking here is acceptable.
                match command
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                {
                    Ok(child) => {
                        result.set("pid", child.id())?;
                        match child.wait_with_output() {
                            Ok(output) => {
                                result.set("code", output.status.code().unwrap_or(-1))?;
                                result.set("stdout", lua.create_string(&output.stdout)?)?;
                                result.set("stderr", lua.create_string(&output.stderr)?)?;
                            }
                            Err(e) => {
                                result.set("code", -1)?;
                                result.set("stdout", "")?;
                                result.set(
                                    "stderr",
                                    format!("vim.system: wait failed for {program}: {e}"),
                                )?;
                            }
                        }
                    }
                    Err(e) => {
                        result.set("code", -1)?;
                        result.set("stdout", "")?;
                        result.set("stderr", format!("vim.system: failed to spawn {program}: {e}"))?;
                    }
                }
                Ok(result)
            },
        )?,
    )?;

    // `vim._json_decode(str)`: parse a JSON document into the equivalent Lua value
    // (objects -> string-keyed tables, arrays -> sequences, `null` -> nil). Backs
    // `vim.json.decode`; raises on malformed input, matching neovim. The config
    // path that reaches for it is rust_analyzer's `root_dir`, decoding the
    // `cargo metadata` output to read `workspace_root`.
    vim.set(
        "_json_decode",
        lua.create_function(|lua, text: String| {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(mlua::Error::external)?;
            json_to_lua(lua, &value)
        })?,
    )?;

    // `vim._json_encode(value)`: serialize a Lua value to a JSON string, using the
    // same array-vs-object rule as [`lua_to_rmpv`]. Backs `vim.json.encode`.
    vim.set(
        "_json_encode",
        lua.create_function(|_, value: mlua::Value| {
            serde_json::to_string(&lua_to_json(&value)?).map_err(mlua::Error::external)
        })?,
    )?;

    // ----- vim.uv: the libuv-style host primitives configs reach for ----------
    // The `lsp/<server>.lua` configs probe the filesystem/home/cwd through
    // `vim.uv` (and its legacy alias `vim.loop`) while building defaults and
    // resolving roots. Only the handful actually used are provided.
    let uv = lua.create_table()?;
    // `vim.uv.fs_stat(path)`: a stat table (at least `type`/`size`), or nil when
    // `path` can't be stat'd. Follows symlinks (the `fs_stat`, not `fs_lstat`,
    // semantics); configs read `.type == 'file'`/`'directory'`.
    uv.set(
        "fs_stat",
        lua.create_function(|lua, path: String| match std::fs::metadata(&path) {
            Ok(md) => {
                let t = lua.create_table()?;
                t.set("type", if md.is_dir() { "directory" } else { "file" })?;
                t.set("size", md.len())?;
                Ok(mlua::Value::Table(t))
            }
            Err(_) => Ok(mlua::Value::Nil),
        })?,
    )?;
    // `vim.uv.os_homedir()`: the user's home directory.
    uv.set(
        "os_homedir",
        lua.create_function(|_, ()| {
            Ok(std::env::var("HOME")
                .ok()
                .or_else(|| std::env::var("USERPROFILE").ok()))
        })?,
    )?;
    // `vim.uv.cwd()`: the process working directory.
    uv.set(
        "cwd",
        lua.create_function(|_, ()| {
            Ok(std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().into_owned()))
        })?,
    )?;
    // `vim.uv.fs_realpath(path)`: the canonical path (symlinks resolved), or nil.
    uv.set(
        "fs_realpath",
        lua.create_function(|_, path: String| {
            Ok(std::fs::canonicalize(&path)
                .ok()
                .map(|p| p.to_string_lossy().into_owned()))
        })?,
    )?;
    // `vim.uv.os_uname()`: a uname table. `lspconfig.util` reads `.version` and
    // matches it against "Windows" to detect the platform, so `version` carries a
    // Windows marker only on Windows.
    uv.set(
        "os_uname",
        lua.create_function(|lua, ()| {
            let t = lua.create_table()?;
            t.set("sysname", std::env::consts::OS)?;
            t.set("machine", std::env::consts::ARCH)?;
            t.set("release", "")?;
            t.set(
                "version",
                if cfg!(windows) {
                    "Windows"
                } else {
                    std::env::consts::OS
                },
            )?;
            Ok(t)
        })?,
    )?;
    vim.set("uv", uv.clone())?;
    vim.set("loop", uv)?; // `vim.loop` is the pre-0.10 name for `vim.uv`.

    // ----- additional vim.fn (filesystem / process / PATH) --------------------
    // `vim.fn.executable(name)`: 1 if `name` is an executable on $PATH (or an
    // executable file path), else 0. Configs use it to prefer a project-local
    // `node_modules/.bin/<server>` over the global one.
    func.set(
        "executable",
        lua.create_function(|_, name: String| Ok(i64::from(find_executable(&name).is_some())))?,
    )?;
    // `vim.fn.exepath(name)`: the resolved path to `name` on $PATH, or "".
    func.set(
        "exepath",
        lua.create_function(|_, name: String| Ok(find_executable(&name).unwrap_or_default()))?,
    )?;
    // `vim.fn.getpid()`: this (editor) process's id.
    func.set(
        "getpid",
        lua.create_function(|_, ()| Ok(std::process::id() as i64))?,
    )?;
    // `vim.fn.resolve(path)`: `path` with symlinks resolved, or unchanged if it
    // can't be canonicalized.
    func.set(
        "resolve",
        lua.create_function(|_, path: String| {
            Ok(std::fs::canonicalize(&path)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or(path))
        })?,
    )?;
    // `vim.fn.filereadable(path)`: 1 if `path` is a readable regular file, else 0.
    func.set(
        "filereadable",
        lua.create_function(|_, path: String| {
            Ok(i64::from(std::path::Path::new(&path).is_file()))
        })?,
    )?;
    // `vim.fn.readblob(path)`: the file's raw bytes as a string; errors if
    // unreadable (callers `pcall` it).
    func.set(
        "readblob",
        lua.create_function(|lua, path: String| {
            lua.create_string(std::fs::read(&path).map_err(mlua::Error::external)?)
        })?,
    )?;
    // `vim.fn.glob(pattern[, nosuf, list])`: existing paths matching a shell-style
    // glob (`*`/`?` wildcards, per path component). Returns a list when `list` is
    // truthy (the form `lspconfig.util.root_pattern` uses), else the default
    // newline-joined string. `nosuf` is accepted and ignored.
    func.set(
        "glob",
        lua.create_function(
            |lua, (pattern, _nosuf, list): (String, Option<bool>, Option<bool>)| {
                let paths = glob_paths(&pattern);
                if list.unwrap_or(false) {
                    Ok(mlua::Value::Table(lua.create_sequence_from(paths)?))
                } else {
                    Ok(mlua::Value::String(lua.create_string(paths.join("\n"))?))
                }
            },
        )?,
    )?;

    Ok(())
}

/// Resolve `name` to an executable path: an explicit path is accepted when it is
/// an executable file; a bare name is searched across `$PATH`. Backs
/// `vim.fn.executable`/`vim.fn.exepath`.
fn find_executable(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    if name.contains('/') {
        let p = std::path::Path::new(name);
        return is_executable_file(p).then(|| name.to_string());
    }
    for dir in std::env::split_paths(&std::env::var_os("PATH")?) {
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(unix)]
fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable_file(p: &std::path::Path) -> bool {
    p.is_file()
}

/// Expand a shell-style glob (only `*` and `?`, matched per path component) into
/// the existing paths it matches. Enough for the `lib/python*/site-packages`-
/// style patterns the config files build; a relative pattern resolves against the
/// cwd. Backs `vim.fn.glob`.
fn glob_paths(pattern: &str) -> Vec<String> {
    let absolute = pattern.starts_with('/');
    let mut frontier = vec![if absolute {
        String::from("/")
    } else {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".into())
    }];
    for seg in pattern.split('/').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        if seg.contains('*') || seg.contains('?') {
            for base in &frontier {
                if let Ok(rd) = std::fs::read_dir(base) {
                    for entry in rd.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if wildcard_match(seg, &name) {
                            next.push(join_path(base, &name));
                        }
                    }
                }
            }
        } else {
            for base in &frontier {
                let cand = join_path(base, seg);
                if std::path::Path::new(&cand).exists() {
                    next.push(cand);
                }
            }
        }
        frontier = next;
    }
    frontier.sort();
    frontier
}

fn join_path(base: &str, name: &str) -> String {
    if base == "/" {
        format!("/{name}")
    } else if base.ends_with('/') {
        format!("{base}{name}")
    } else {
        format!("{base}/{name}")
    }
}

/// Glob match for one path component: `*` matches any run of non-`/` chars, `?`
/// any single char. A small backtracking matcher over bytes.
fn wildcard_match(pat: &str, s: &str) -> bool {
    let (pat, s) = (pat.as_bytes(), s.as_bytes());
    let (mut pi, mut si) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while si < s.len() {
        if pi < pat.len() && (pat[pi] == b'?' || pat[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < pat.len() && pat[pi] == b'*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(sp) = star {
            pi = sp + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < pat.len() && pat[pi] == b'*' {
        pi += 1;
    }
    pi == pat.len()
}

/// Full paths of the files matching `name` across `runtimepath`, the engine of
/// `nvim_get_runtime_file`. `name` is a runtimepath-relative path whose final
/// component may contain a single `*` glob; earlier components are matched
/// literally. Stops at the first hit when `!all`.
fn get_runtime_file(runtimepath: &[PathBuf], name: &str, all: bool) -> Vec<String> {
    let (dir_part, file_part) = name.rsplit_once('/').unwrap_or(("", name));
    let mut out = Vec::new();
    for rt in runtimepath {
        let base = if dir_part.is_empty() {
            rt.clone()
        } else {
            rt.join(dir_part)
        };
        if file_part.contains('*') {
            let Ok(entries) = std::fs::read_dir(&base) else {
                continue;
            };
            for entry in entries.flatten() {
                let fname = entry.file_name();
                if glob_match(file_part, &fname.to_string_lossy()) {
                    out.push(entry.path().to_string_lossy().into_owned());
                    if !all {
                        return out;
                    }
                }
            }
        } else {
            let full = base.join(file_part);
            if full.exists() {
                out.push(full.to_string_lossy().into_owned());
                if !all {
                    return out;
                }
            }
        }
    }
    out
}

/// Match a single path component against a glob with at most one `*` (the only
/// form `nvim_get_runtime_file` callers use, e.g. `lsp/*.lua`).
fn glob_match(pat: &str, name: &str) -> bool {
    match pat.split_once('*') {
        Some((pre, suf)) => {
            name.len() >= pre.len() + suf.len() && name.starts_with(pre) && name.ends_with(suf)
        }
        None => pat == name,
    }
}

/// Store (or clear) the panel's `on_select` callback in the Lua registry. `None`
/// stores `nil`, so [`LuaRuntime::run_panel_select`] reads it back as "no
/// handler" — keeping a closed/replaced panel from firing a stale callback.
fn store_panel_callback(lua: &Lua, cb: Option<mlua::Function>) -> mlua::Result<()> {
    match cb {
        Some(f) => lua.set_named_registry_value(PANEL_ON_SELECT, f),
        None => lua.set_named_registry_value(PANEL_ON_SELECT, mlua::Value::Nil),
    }
}

/// Prepend each runtimepath entry's `lua/` directory to Lua's `package.path`,
/// so `require("foo")` finds `<rt>/lua/foo.lua` or `<rt>/lua/foo/init.lua`. The
/// stock `package.path` is kept as a suffix. No-op when the runtimepath is empty.
fn seed_package_path(lua: &Lua, runtimepath: &[PathBuf]) -> mlua::Result<()> {
    if runtimepath.is_empty() {
        return Ok(());
    }
    let mut patterns: Vec<String> = Vec::with_capacity(runtimepath.len() * 2);
    for rt in runtimepath {
        let lua_dir = rt.join("lua");
        patterns.push(lua_dir.join("?.lua").to_string_lossy().into_owned());
        patterns.push(
            lua_dir
                .join("?")
                .join("init.lua")
                .to_string_lossy()
                .into_owned(),
        );
    }
    let package: Table = lua.globals().get("package")?;
    let existing: String = package.get("path").unwrap_or_default();
    let combined = if existing.is_empty() {
        patterns.join(";")
    } else {
        format!("{};{existing}", patterns.join(";"))
    };
    package.set("path", combined)?;
    Ok(())
}

/// Resolve a `vim.fn.stdpath(what)` directory under an `nxvim` subdir, the way
/// neovim derives its standard paths from XDG (with `$HOME` fallbacks). `config`
/// additionally honors `$NXVIM_CONFIG`. Unknown `what` falls back to the cache
/// dir rather than erroring.
fn stdpath(what: &str) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = |var: &str, fallback: &str| -> PathBuf {
        if let Some(dir) = std::env::var_os(var) {
            PathBuf::from(dir).join("nxvim")
        } else if let Some(home) = &home {
            home.join(fallback).join("nxvim")
        } else {
            PathBuf::from("nxvim")
        }
    };
    let path = match what {
        "config" => std::env::var_os("NXVIM_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config")),
        "data" => xdg("XDG_DATA_HOME", ".local/share"),
        "state" => xdg("XDG_STATE_HOME", ".local/state"),
        _ => xdg("XDG_CACHE_HOME", ".cache"),
    };
    path.to_string_lossy().into_owned()
}

/// `vim.fn.getftime(path)`: the file's mtime in whole seconds since the Unix
/// epoch, or `-1` if it can't be stat'd (matching Vimscript).
fn getftime(path: &str) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(-1)
}

/// Resolve `mkdir`'s `prot` argument to a permission mode. Accepts an octal
/// string (`"0700"`, `"700"`) or a numeric mode; defaults to `0o755` (neovim's
/// default) when absent or unparseable.
fn parse_mode(prot: Option<mlua::Value>) -> u32 {
    const DEFAULT: u32 = 0o755;
    match prot {
        Some(mlua::Value::Integer(n)) => n as u32,
        Some(mlua::Value::Number(n)) => n as u32,
        Some(mlua::Value::String(s)) => s
            .to_str()
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0o"), 8).ok())
            .unwrap_or(DEFAULT),
        _ => DEFAULT,
    }
}

/// Create `path` (and parents) with permission `mode`. On Unix the mode is
/// applied to every directory created; elsewhere `mode` is ignored. Returns
/// whether the directory now exists.
fn create_dir_all_mode(path: &str, mode: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(mode)
            .create(path)
            .is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir_all(path).is_ok()
    }
}

/// Read a color field (`fg`/`bg`/`sp`) from an `nvim_set_hl` opts table. A
/// string (`"#rrggbb"` / `"NONE"` / a name) is kept verbatim; an integer color
/// is normalized to `#rrggbb`; anything else (incl. absent) is `None`. The core
/// does the actual parsing.
fn color_field(opts: &Table, key: &str) -> mlua::Result<Option<String>> {
    match opts.get::<mlua::Value>(key)? {
        mlua::Value::String(s) => Ok(Some(s.to_str()?.to_string())),
        mlua::Value::Integer(n) => Ok(Some(format!("#{:06x}", n & 0xff_ffff))),
        mlua::Value::Number(n) => Ok(Some(format!("#{:06x}", (n as i64) & 0xff_ffff))),
        _ => Ok(None),
    }
}

/// Read a boolean attribute (`bold`, `italic`, …) from an opts table; absent or
/// non-boolean reads as `false`.
fn flag_field(opts: &Table, key: &str) -> mlua::Result<bool> {
    Ok(opts.get::<Option<bool>>(key)?.unwrap_or(false))
}

fn stringify(lua: &Lua, value: &mlua::Value) -> String {
    // Prefer Lua's own tostring (honors __tostring); fall back to a debug form.
    match lua.coerce_string(value.clone()) {
        Ok(Some(s)) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        _ => format!("{value:?}"),
    }
}

/// Convert an `mlua::Value` to an RPC [`rmpv::Value`] for `nvim_exec_lua`. A
/// table with contiguous `1..=n` integer keys becomes an array (a Lua sequence);
/// any other table becomes a map; an empty table becomes an empty array.
/// Functions / userdata / threads (not representable over msgpack) collapse to
/// nil. Covers the scalar-and-table shapes nxvim's synchronous getters return.
fn lua_to_rmpv(value: &mlua::Value) -> mlua::Result<rmpv::Value> {
    use mlua::Value as L;
    Ok(match value {
        L::Nil => rmpv::Value::Nil,
        L::Boolean(b) => rmpv::Value::from(*b),
        L::Integer(i) => rmpv::Value::from(*i),
        L::Number(n) => rmpv::Value::from(*n),
        L::String(s) => rmpv::Value::from(s.to_str()?.to_string()),
        L::Table(t) => lua_table_to_rmpv(t)?,
        // Non-serializable Lua values have no msgpack representation.
        _ => rmpv::Value::Nil,
    })
}

/// Table half of [`lua_to_rmpv`]: array iff every key is an integer in `1..=len`.
fn lua_table_to_rmpv(t: &mlua::Table) -> mlua::Result<rmpv::Value> {
    let len = t.raw_len() as i64;
    let mut entries: Vec<(i64, rmpv::Value)> = Vec::new();
    let mut map: Vec<(rmpv::Value, rmpv::Value)> = Vec::new();
    let mut is_seq = true;
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        let rv = lua_to_rmpv(&v)?;
        match &k {
            mlua::Value::Integer(i) if *i >= 1 && *i <= len => entries.push((*i, rv)),
            _ => {
                is_seq = false;
                map.push((lua_to_rmpv(&k)?, rv));
            }
        }
    }
    if is_seq {
        entries.sort_by_key(|(i, _)| *i);
        Ok(rmpv::Value::Array(
            entries.into_iter().map(|(_, v)| v).collect(),
        ))
    } else {
        // Re-emit the integer-keyed entries we provisionally treated as sequence.
        for (i, v) in entries {
            map.push((rmpv::Value::from(i), v));
        }
        Ok(rmpv::Value::Map(map))
    }
}

/// Convert a parsed [`serde_json::Value`] into the equivalent `mlua::Value` for
/// `vim.json.decode`: objects become string-keyed tables, arrays become Lua
/// sequences, and JSON `null` becomes `nil` (so a null-valued object key reads
/// back absent — fine for the `cargo metadata` shape the `lsp/<server>.lua`
/// configs decode, which only index present string/array fields).
fn json_to_lua(lua: &Lua, value: &serde_json::Value) -> mlua::Result<mlua::Value> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => mlua::Value::Nil,
        J::Bool(b) => mlua::Value::Boolean(*b),
        J::Number(n) => match n.as_i64() {
            Some(i) => mlua::Value::Integer(i),
            None => mlua::Value::Number(n.as_f64().unwrap_or(0.0)),
        },
        J::String(s) => mlua::Value::String(lua.create_string(s)?),
        J::Array(items) => {
            let t = lua.create_table()?;
            for (i, item) in items.iter().enumerate() {
                t.raw_set(i + 1, json_to_lua(lua, item)?)?;
            }
            mlua::Value::Table(t)
        }
        J::Object(map) => {
            let t = lua.create_table()?;
            for (k, v) in map {
                t.raw_set(k.as_str(), json_to_lua(lua, v)?)?;
            }
            mlua::Value::Table(t)
        }
    })
}

/// Convert an optional Lua config table (`init_options` / `settings` /
/// `capabilities` from `vim._lsp_start`) to JSON for [`LspOp::Start`]. `None`
/// passes through; a present table goes through [`lua_to_json`] (the same bridge
/// `vim.json.encode` uses), so what the config wrote reaches the server verbatim.
fn opt_table_to_json(t: Option<Table>) -> mlua::Result<Option<serde_json::Value>> {
    match t {
        Some(t) => Ok(Some(lua_to_json(&mlua::Value::Table(t))?)),
        None => Ok(None),
    }
}

/// Flatten a `vim.system` `opts.env` table (`{ VAR = value }`) into the
/// `(key, value)` pairs the event-loop actor layers onto the child's inherited
/// environment — the async `vim._system_async` analogue of the inline loop in the
/// blocking `vim._system`. An absent table yields no pairs.
fn env_pairs(env: Option<Table>) -> mlua::Result<Vec<(String, String)>> {
    let Some(env) = env else {
        return Ok(Vec::new());
    };
    let mut pairs = Vec::new();
    for kv in env.pairs::<String, String>() {
        pairs.push(kv?);
    }
    Ok(pairs)
}

/// Convert an `mlua::Value` to a [`serde_json::Value`] for `vim.json.encode`,
/// using the same array-vs-object rule as [`lua_to_rmpv`]: a table whose keys are
/// exactly `1..=len` is an array, anything else an object (keys coerced to
/// strings); non-serializable values (functions / userdata) collapse to `null`.
fn lua_to_json(value: &mlua::Value) -> mlua::Result<serde_json::Value> {
    use mlua::Value as L;
    Ok(match value {
        L::Nil => serde_json::Value::Null,
        L::Boolean(b) => serde_json::Value::Bool(*b),
        L::Integer(i) => serde_json::Value::from(*i),
        L::Number(n) => serde_json::Value::from(*n),
        L::String(s) => serde_json::Value::from(s.to_str()?.to_string()),
        L::Table(t) => lua_table_to_json(t)?,
        _ => serde_json::Value::Null,
    })
}

/// Table half of [`lua_to_json`]: array iff every key is an integer in `1..=len`.
fn lua_table_to_json(t: &mlua::Table) -> mlua::Result<serde_json::Value> {
    let len = t.raw_len() as i64;
    let mut entries: Vec<(i64, serde_json::Value)> = Vec::new();
    let mut map = serde_json::Map::new();
    let mut is_seq = true;
    for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        let jv = lua_to_json(&v)?;
        match &k {
            mlua::Value::Integer(i) if *i >= 1 && *i <= len => entries.push((*i, jv)),
            _ => {
                is_seq = false;
                map.insert(json_key(&k)?, jv);
            }
        }
    }
    if is_seq {
        entries.sort_by_key(|(i, _)| *i);
        Ok(serde_json::Value::Array(
            entries.into_iter().map(|(_, v)| v).collect(),
        ))
    } else {
        // Re-emit the integer-keyed entries we provisionally treated as sequence.
        for (i, v) in entries {
            map.insert(i.to_string(), v);
        }
        Ok(serde_json::Value::Object(map))
    }
}

/// Coerce a Lua table key to the JSON object key string `vim.json.encode` uses.
fn json_key(k: &mlua::Value) -> mlua::Result<String> {
    Ok(match k {
        mlua::Value::String(s) => s.to_str()?.to_string(),
        mlua::Value::Integer(i) => i.to_string(),
        mlua::Value::Number(n) => n.to_string(),
        _ => {
            return Err(mlua::Error::external(
                "vim.json.encode: table key is not a string or number",
            ))
        }
    })
}
