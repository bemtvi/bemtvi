//! The plain-data types the runtime queues for the server to drain, and the
//! Rust→Lua mirror payloads the server pushes in. No Lua state lives here — these
//! are the wire between [`crate::LuaRuntime`] and the server, kept free of any
//! `mlua` / transport types so the server can pattern-match on them directly.

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
/// [`crate::LuaRuntime::set_buf_mirror`] before running Lua. `set_lines` write-through
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
    /// `vim.bo[bufnr].<opt> = value` / `nvim_set_option_value(name, value, {buf})`
    /// — set a buffer-local option (`tabstop`/`shiftwidth`/`expandtab`) on the
    /// live editor's buffer `bufnr`. The Lua side has already canonicalized the
    /// name and updated its option mirror (write-through); the server applies the
    /// value to the core buffer after the chunk.
    SetOption {
        bufnr: u64,
        /// Canonical option name (`tabstop` / `shiftwidth` / `expandtab`).
        name: String,
        value: OptionValue,
    },
}

/// An extmark mutation queued by the `nvim_buf_set_extmark` / `_del_extmark` /
/// `_clear_namespace` Lua family, drained by the server into the target buffer's
/// [`ExtmarkStore`](nxvim_core::ExtmarkStore). Positions ride as neovim's 0-based
/// `(row, col)`; the server converts them to byte offsets against the live buffer
/// (the conversion needs the rope, which the Lua side can't see). The Lua front
/// has already updated its `vim._extmarks` mirror (read-after-write within the
/// chunk); this op makes the core catch up after the chunk.
#[derive(Clone, Debug)]
pub enum ExtmarkOp {
    /// Create-or-replace a mark `(bufnr, ns, id)`. `end_row`/`end_col` are absent
    /// for a point mark; `hl_group` is absent when the mark carries no highlight.
    Set {
        bufnr: u64,
        ns: u32,
        id: u64,
        row: i64,
        col: i64,
        end_row: Option<i64>,
        end_col: Option<i64>,
        hl_group: Option<String>,
        priority: u32,
    },
    /// Delete mark `(bufnr, ns, id)`.
    Del { bufnr: u64, ns: u32, id: u64 },
    /// Clear namespace `ns` over lines `[line_start, line_end)` (neovim's range:
    /// 0-based, `line_end == -1` ⇒ end of buffer).
    Clear {
        bufnr: u64,
        ns: u32,
        line_start: i64,
        line_end: i64,
    },
}

/// A scalar option value carried by [`BufOp::SetOption`] / [`GlobalOptionOp`] /
/// [`WindowOp::SetOption`]: a number for `tabstop`/`shiftwidth`, a boolean for
/// `expandtab`, a string for `statusline`. Kept free of `mlua` types (the bridge
/// converts the Lua value into this) so the server can match on it directly. Not
/// `Copy` since [`OptionValue::String`] owns its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionValue {
    Number(i64),
    Bool(bool),
    /// A string option (`statusline`, …). Only the global scope wires one today;
    /// the buffer/window bridges never produce this variant.
    String(String),
}

/// A register write queued by `vim.fn.setreg`, drained by the server into the
/// live editor's register file after the chunk — the register analogue of
/// [`BufOp`] / [`GlobalOptionOp`]. Reads (`vim.fn.getreg` / `getregtype`)
/// resolve against the `vim._registers` mirror the server pushes before running
/// Lua, so only the write needs an op. The Lua bridge has already rejected
/// read-only specials and folded an uppercase name / `a` flag into `append`.
#[derive(Clone, Debug)]
pub struct RegisterSetOp {
    /// Register name (lowercase / digit / `-` / `"`; never an uppercase or a
    /// read-only special — the Lua side resolves those first).
    pub name: char,
    pub text: String,
    /// Linewise (`V`) when set, charwise (`v`) otherwise. Blockwise is rejected
    /// at the Lua bridge until visual-block mode lands.
    pub linewise: bool,
    /// Append to the register's current contents instead of overwriting.
    pub append: bool,
}

/// A global (editor-wide) option mutation queued by `vim.o` for a search option
/// (`ignorecase` / `smartcase` / `wrapscan` / `hlsearch` / `incsearch`), the
/// global analogue of [`BufOp::SetOption`] / [`WindowOp::SetOption`]. The Lua
/// side has canonicalized `name` and written through its `vim._go_mirror`; the
/// server applies the value to the editor's global options after the chunk.
/// These are all boolean today, but the value rides as an [`OptionValue`] for
/// symmetry with the buffer/window bridges (and so a numeric global can land here
/// later without changing the wire shape).
#[derive(Clone, Debug)]
pub struct GlobalOptionOp {
    /// Canonical option name (`ignorecase` / `smartcase` / …).
    pub name: String,
    pub value: OptionValue,
}

/// A window mutation queued by the window Lua API (`vim.api.nvim_set_current_win`,
/// `nvim_win_set_buf`/`set_cursor`/`set_width`/`set_height`/`close`, `nvim_open_win`),
/// drained by the server in `apply_lua_effects` and applied to the live editor —
/// the window analogue of [`BufOp`]. Reads (`nvim_list_wins`, `nvim_win_get_*`)
/// need no op: they resolve against the `vim._wins` mirror the server pushes
/// before running Lua. `0` is the current window/buffer, resolved server-side.
#[derive(Clone, Debug)]
pub enum WindowOp {
    /// `nvim_set_current_win(win)` — focus window `win`.
    SetCurrent { win: u64 },
    /// `nvim_win_set_buf(win, buf)` — rebind window `win` to buffer `buf`.
    SetBuf { win: u64, buf: u64 },
    /// `nvim_win_set_cursor(win, {row, col})` — move window `win`'s cursor.
    /// `line` is 0-based (the prelude converts neovim's 1-based row); `col` is the
    /// 0-based byte column.
    SetCursor { win: u64, line: usize, col: usize },
    /// `nvim_win_set_width(win, width)` — set window `win`'s column width.
    SetWidth { win: u64, width: usize },
    /// `nvim_win_set_height(win, height)` — set window `win`'s text-row height.
    SetHeight { win: u64, height: usize },
    /// `vim.wo[win].<opt> = value` / `nvim_win_set_option(win, name, value)` — set a
    /// window-local option (the number gutter) on window `win`. The prelude has
    /// canonicalized `name`; a boolean rides as [`OptionValue::Bool`].
    SetOption {
        win: u64,
        name: String,
        value: OptionValue,
    },
    /// `nvim_win_close(win, force)` — close window `win`.
    Close { win: u64, force: bool },
    /// `nvim_open_win(buf, enter, config)` (split form) — split the focused window
    /// onto buffer `buf`. `vertical` makes it a vsplit; `enter == false` keeps
    /// focus on the previous window.
    Open {
        buf: u64,
        vertical: bool,
        enter: bool,
    },
    /// `nvim_open_win(buf, enter, config)` (float form) — open a floating window
    /// onto buffer `buf`, positioned by the float config. The prelude has already
    /// validated the string fields (`relative`/`anchor`/`border`) against the
    /// supported set and errored loudly otherwise, so the drain trusts them. `win`
    /// (`0` = current) is the parent for `relative == "win"`; ignored otherwise.
    OpenFloat {
        buf: u64,
        enter: bool,
        relative: String,
        win: u64,
        anchor: String,
        row: i64,
        col: i64,
        width: u64,
        height: u64,
        zindex: u32,
        focusable: bool,
        border: String,
        title: Option<String>,
    },
    /// `nvim_win_set_config(win, config)` — reconfigure window `win` from a
    /// **partial** config: every field is `Option`, `None` meaning "key absent,
    /// leave unchanged" (the merge happens in the core). `relative == Some("")` is
    /// the re-tile form (convert a float back to a split). `parent` is the
    /// `relative == "win"` anchor (`0` = current). The prelude validated the
    /// enumerated strings before queuing, so the drain trusts them.
    SetConfig {
        win: u64,
        relative: Option<String>,
        parent: u64,
        anchor: Option<String>,
        row: Option<i64>,
        col: Option<i64>,
        width: Option<u64>,
        height: Option<u64>,
        zindex: Option<u32>,
        focusable: Option<bool>,
        border: Option<String>,
        title: Option<String>,
    },
}

/// A tab-page mutation queued by the tab Lua API (`vim.api.nvim_set_current_tabpage`),
/// drained by the server in `apply_lua_effects` and applied to the live editor —
/// the tab analogue of [`WindowOp`]. Reads (`nvim_list_tabpages`,
/// `nvim_tabpage_*`) need no op: they resolve against the `vim._tabs` mirror the
/// server pushes before running Lua. `0` is the current tab, resolved server-side.
#[derive(Clone, Debug)]
pub enum TabOp {
    /// `nvim_set_current_tabpage(tab)` — make tab `tab` the active tab page.
    SetCurrent { tab: u64 },
}

/// The arguments handed to a deferred callback when the server runs it via
/// [`crate::LuaRuntime::run_callback`]. A `vim.schedule` / timer callback takes none; an
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

/// A `vim.ui.input(opts, on_confirm)` request: open a one-line prompt labelled
/// `prompt`, prefilled with `default`, and fire callback `cb_id` (a `vim._cb_fns`
/// entry wrapping `on_confirm`) with the typed text — or `nil` on cancel — when
/// the user submits. Queued in [`crate::runtime::Shared::ui_inputs`], drained by the server.
#[derive(Clone, Debug)]
pub struct UiInputReq {
    /// The prompt label shown ahead of the editable line (`opts.prompt`).
    pub prompt: String,
    /// The text the line is prefilled with (`opts.default`; empty when unset).
    pub default: String,
    /// The `vim._cb_fns` id whose `on_confirm` wrapper receives the result.
    pub cb_id: u64,
}

/// A `vim.fn.confirm(msg, choices, …)` request: open the command line as a
/// single-key button dialog showing `label` (the message plus the rendered
/// buttons, already formatted by the Lua wrapper) and fire callback `cb_id` (a
/// `vim._cb_fns` entry that resumes the blocked coroutine) with the chosen
/// button's 1-based index — or `0` on cancel. Queued in
/// [`crate::runtime::Shared::confirms`], drained by the server.
#[derive(Clone, Debug)]
pub struct ConfirmReq {
    /// The fully-rendered prompt label (message + bracketed buttons).
    pub label: String,
    /// The lowercase accelerator key for each button, in order; a keypress
    /// matching one (case-insensitively) resolves to its 1-based index.
    pub accelerators: Vec<String>,
    /// The button selected by `<CR>` (1-based; `0` = none, so `<CR>` cancels).
    pub default: i64,
    /// The `vim._cb_fns` id whose continuation receives the chosen index.
    pub cb_id: u64,
}
