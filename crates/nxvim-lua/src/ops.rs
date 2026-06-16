//! The plain-data types the runtime queues for the server to drain, and the
//! Rust→Lua mirror payloads the server pushes in. No Lua state lives here — these
//! are the wire between [`crate::LuaRuntime`] and the server, kept free of any
//! `mlua` / transport types so the server can pattern-match on them directly.

/// A highlight-group definition produced by `nvim_set_hl(ns, name, opts)`, in
/// the wire-ish form the server translates into `nxvim_core`'s `HlDef`. Colors
/// are kept as the strings the opts table carried (`"#rrggbb"` / `"NONE"` /
/// named, with integer colors normalized to `#rrggbb`); the core parses them,
/// so this crate stays free of any color/registry types.
#[derive(Clone, Debug, Default)]
pub struct HlSet {
    /// The namespace the group is defined in (`0` is the global table). A
    /// non-zero namespace is kept separate by the core, never folded into global.
    pub ns: u32,
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

/// A request to a permanent **dock** (the VSCode-style edge panels), queued by
/// the `nx.dock.*` functions and drained by the server into the core (which owns
/// the dock state). nxvim's own surface — docks are not a neovim concept. `side`
/// is one of `left`/`right`/`top`/`bottom`, validated when drained.
#[derive(Clone, Debug)]
pub enum DockOp {
    /// `nx.dock.open{ side, size?, buf? }` — open (or resize/refocus) and focus the
    /// dock. `size` is columns (left/right) or rows (top/bottom); `None` keeps the
    /// current/default. `buf` is the buffer to show (`None` ⇒ a fresh scratch).
    Open {
        side: String,
        size: Option<u64>,
        buf: Option<u64>,
    },
    /// `nx.dock.close(side)` — close the dock (its buffer stays loaded).
    Close { side: String },
    /// `nx.dock.focus(side)` — focus the dock (no-op if it isn't open).
    Focus { side: String },
    /// `nx.dock.toggle(side)` — hide a visible dock / show a hidden one (preserving
    /// its content); a no-op (reported) on a side with no dock.
    Toggle { side: String },
    /// `nx.dock.hide(side)` — collapse a visible dock, keeping its content parked.
    Hide { side: String },
    /// `nx.dock.show(side)` — un-hide and focus a hidden dock, restoring its content.
    Show { side: String },
    /// `nx.dock.opt(side).<name> = <value>` (and the inline keys of
    /// `nx.dock.open{...}`) — set a dock-scoped option. `name` is `showtabline` /
    /// `size` (numbers), `title` / `winhighlight` (strings); the core validates the
    /// name and reports an unknown one.
    SetOption {
        side: String,
        name: String,
        value: OptionValue,
    },
}

/// A request to open a terminal job, queued by `nx.terminal.open{...}` and
/// drained by the server into [`Editor::open_terminal`](nxvim_core::Editor) — the
/// programmatic twin of the `:terminal` ex-command, on the same "Lua queues, core
/// mutates" flow as [`DockOp`]. nxvim's own surface (no neovim `termopen` shape).
#[derive(Clone, Debug)]
pub struct TerminalOpenReq {
    /// Program + args, run directly (no shell). Empty ⇒ the default shell
    /// (`$SHELL` / `%COMSPEC%`, resolved by the transport).
    pub argv: Vec<String>,
    /// Working directory; `None` ⇒ the editor's working directory (filled in
    /// server-side, since `nxvim-core` can't read process state).
    pub cwd: Option<String>,
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
        /// `nx._cur_buf` before firing `FileType` (so it round-trips exactly).
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
    /// `vim.diagnostic.open_float()` — open a float listing the cursor line's
    /// diagnostics in full (the multi-line messages with `source`/`code`). A loud
    /// no-op when the line is clean.
    DiagnosticOpenFloat,
    /// `vim.diagnostic.config({ underline = …, virtual_text = …, signs = … })` —
    /// toggle the diagnostic surfaces nxvim renders. Other config keys (float) are
    /// stored Lua-side without an op until their surface lands.
    DiagnosticConfig {
        /// Whether diagnostic underline spans are painted (neovim's `underline`,
        /// default on; `false` disables the squiggles).
        underline: bool,
        /// Whether the inline end-of-line virtual-text message is painted
        /// (neovim's `virtual_text`, default off).
        virtual_text: bool,
        /// The leader glyph the virtual text is prefixed with (`prefix` in the
        /// table form of `virtual_text`; default `■ `).
        virt_prefix: String,
        /// Whether the gutter sign column is painted (neovim's `signs`, default
        /// on; `false` reserves no column).
        signs: bool,
        /// The per-severity gutter glyphs, indexed by severity code minus one
        /// (`[error, warn, info, hint]`) — the `text` map of the `signs` table, or
        /// the built-in `E`/`W`/`I`/`H` letters. Always exactly four entries.
        sign_text: [String; 4],
    },
    /// `client:request(method, params, handler)` (Phase 5) — issue a generic LSP
    /// request to the client `client_id`'s server and route its reply to the Lua
    /// callback `cb_id` (a `nx._cb_fns` entry holding the resolved handler). The
    /// server resolves the client's [`ServerKey`] and forwards a raw request; the
    /// reply comes back off-tick as a [`CallbackArgs::LspReply`].
    ClientRequest {
        /// The target client id (`client.id`, the handle `LspAttach` resolves).
        client_id: u64,
        /// The LSP method (e.g. `workspace/executeCommand`).
        method: String,
        /// The request params as JSON (`Null` when the caller passed none).
        params: serde_json::Value,
        /// The `nx._cb_fns` id the reply's handler is registered under.
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
    /// `vim.lsp.semantic_tokens.start(bufnr)` / `stop(bufnr)` (Phase 3) — flip the
    /// per-buffer semantic-token projection on or off. `start` also re-requests the
    /// token set if the cache is cold; `stop` leaves the cache but hides the paint.
    SemanticTokensEnable {
        /// The target buffer (already resolved from `0`/`nil` → current in Lua).
        bufnr: u64,
        /// `true` to start (project + request), `false` to stop (hide the paint).
        enabled: bool,
    },
    /// `vim.lsp.semantic_tokens.force_refresh(bufnr)` (Phase 3) — drop the cached
    /// `result_id` and re-request the whole `full` token set, repainting from the
    /// server's fresh classification.
    SemanticTokensRefresh {
        /// The target buffer (already resolved from `0`/`nil` → current in Lua).
        bufnr: u64,
    },
    /// `vim.lsp.semantic_tokens.enable(enabled)` (Phase 3) — nxvim's editor-wide
    /// gate for the whole semantic-tokens feature (neovim has only the per-buffer
    /// `start`/`stop`). Off ⇒ no semantic paint anywhere; flipping back on
    /// re-requests every attached buffer so the paint returns.
    SemanticTokensConfig {
        /// Whether semantic tokens are enabled editor-wide (default on).
        enabled: bool,
    },
    /// `vim.lsp.inlay_hint.enable(enable, { bufnr })` — flip the per-buffer inlay-
    /// hint projection on or off (off by default, unlike semantic tokens). `enable`
    /// requests a fresh set; disabling clears the cache and hides the paint.
    InlayHintEnable {
        /// The target buffer (already resolved from `0`/`nil` → current in Lua).
        bufnr: u64,
        /// `true` to enable (project + request), `false` to disable (clear + hide).
        enabled: bool,
    },
}

/// A request to the async runtime (the "event loop"), queued by the `vim.schedule`
/// / `vim.defer_fn` / `vim.uv` timer / `vim.system` family and drained by the
/// server in `apply_lua_effects`. Each op carries a `cb_id` into `nx._cb_fns`
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
    /// async. `stdin` is fed to the child's standard input then closed (empty for
    /// `vim.system`, which takes no stdin; non-empty for a `uv.spawn` pipe written
    /// by a plugin's job runner).
    Spawn {
        id: u64,
        cmd: Vec<String>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
        stdin: Vec<u8>,
        /// Stream the child's stdout as it arrives (`nx.spawn`'s `on_stdout`):
        /// each newline-delimited batch fires the persistent stdout callback under
        /// `id`, and the final exit carries empty stdout (already delivered). When
        /// `false` (`vim.system`) the child runs to completion and the whole stdout
        /// is delivered once with the exit — the original one-shot behavior.
        stream: bool,
    },
    /// `handle:kill(signal)` on a `vim.system` handle spawned async — terminate the
    /// child running under `id`. A no-op if it already exited. The `signal`
    /// argument is accepted at the Lua surface for call-compatibility but not
    /// carried here: the actor terminates the child unconditionally (it has no
    /// libc binding to deliver an arbitrary signal), a documented approximation.
    Kill { id: u64 },
}

/// A buffer-local *option* write queued by the Lua side, drained by the server in
/// `apply_lua_effects` and applied to the live editor — the buffer analogue of
/// [`LspOp`]/[`LoopOp`]. Reads (`nvim_buf_get_lines`, the cursor) need no op: they read
/// the Rust→Lua *mirror* the server pushes via
/// [`crate::LuaRuntime::set_buf_mirror`] before running Lua.
///
/// The buffer-*text* / lifecycle mutation surface (`nvim_buf_set_lines` / `set_text`,
/// `nvim_create_buf`, `nvim_buf_delete`, …) is **not** part of nxvim's Lua API — it is
/// intentionally absent (see `crates/nxvim-lua/src/prelude/api.lua`'s header); the only
/// buffer mutation a config reaches is the option write below (`vim.bo`).
#[derive(Clone, Debug)]
pub enum BufOp {
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
/// has already updated its `nx._extmarks` mirror (read-after-write within the
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
/// resolve against the `nx._registers` mirror the server pushes before running
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

/// A `nvim_feedkeys(keys, mode, …)` request: enqueue `keys` (vim key-notation)
/// into the server's typeahead, to be processed at the end of the current input
/// batch / off-tick settle. `remap` (the `m`/default flag, cleared by `n`) routes
/// the fed keys through the mapping engine; `insert` (the `i` flag) puts them at
/// the front of the typeahead instead of the back. The server parses `keys` and
/// drains them after the chunk — Lua queues, the server feeds.
#[derive(Clone, Debug)]
pub struct FeedKeysOp {
    /// The keys to feed, as vim notation (`parse_keys` turns them into `Key`s).
    pub keys: String,
    /// Route the keys through the mapping engine (the `m`/default flag) rather
    /// than straight to the editor (the `n` noremap flag).
    pub remap: bool,
    /// Insert at the FRONT of the typeahead (the `i` flag) rather than appending.
    pub insert: bool,
}

/// A global (editor-wide) option mutation queued by `vim.o` for a search option
/// (`ignorecase` / `smartcase` / `wrapscan` / `hlsearch` / `incsearch`), the
/// global analogue of [`BufOp::SetOption`] / [`WindowOp::SetOption`]. The Lua
/// side has canonicalized `name` and written through its `nx._go_mirror`; the
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

/// One structured quickfix item from `setqflist(list, …)` — the Lua-side dict
/// form, before the server resolves it into a `nxvim_core::QfEntry`. Mirrors the
/// keys vim's `setqflist` accepts; absent keys arrive as their zero value.
#[derive(Clone, Debug, Default)]
pub struct QfItem {
    pub filename: Option<String>,
    pub bufnr: i32,
    pub module: String,
    pub lnum: usize,
    pub end_lnum: usize,
    pub col: usize,
    pub end_col: usize,
    pub vcol: bool,
    pub nr: i32,
    pub pattern: String,
    pub text: String,
    /// Type char as a (possibly empty) string — `"E"`/`"W"`/`"I"`/`"N"`.
    pub typ: String,
    /// Whether vim considers the item valid (jumpable). The Lua side defaults this
    /// to "has a line number" when the dict omits `valid`.
    pub valid: bool,
}

/// A `setqflist(list, action, what)` request, drained by the server into the
/// editor's quickfix list after the chunk. Exactly one of `items` (the structured
/// form) or `lines` (raw text parsed against `efm`) is set.
#[derive(Clone, Debug)]
pub struct QfSetOp {
    /// Structured items (the `list` / `what.items` form).
    pub items: Option<Vec<QfItem>>,
    /// Raw output lines to parse (the `what.lines` form).
    pub lines: Option<Vec<String>>,
    /// `'errorformat'` for `lines`; `None` uses the editor's option.
    pub efm: Option<String>,
    /// Action: `' '` (new / replace) / `'a'` (append) / `'r'` (replace current).
    pub action: char,
    /// List title (`what.title`), if given.
    pub title: Option<String>,
    /// After populating, open the quickfix window iff the list is non-empty
    /// (`:cwindow`). Set by `:make`/`:grep`; always `false` for plain `setqflist`.
    pub open: bool,
    /// After populating, jump to the first valid entry (`:cfirst`). Set by a
    /// no-bang `:make`/`:grep`; always `false` for plain `setqflist`.
    pub goto_first: bool,
    /// Target a window's **location list** instead of the global quickfix list:
    /// `Some(window_id)` for `setloclist`/`:lmake`/`:lgrep`, `None` (the common
    /// case) for the quickfix list. `Some(0)` means "the current window at drain
    /// time" (vim's `winnr` 0).
    pub loclist_win: Option<u64>,
}

/// A treesitter bridge request queued by `vim.treesitter.start` / `stop`, the
/// `nx.treesitter.set_query` → native-engine seam. The Lua side queues an
/// override that the server pushes straight to the engine — no Lua merge or
/// runtimepath resolution (highlight on/off and the language are declarative
/// buffer state now: `nx.bo.ts_highlight` / `nx.bo.filetype`, not ops).
#[derive(Clone, Debug)]
pub enum TsOp {
    /// `nx.treesitter.set_query(lang, name, text|nil)`: install `text` as the
    /// `(lang, name)` query override directly on the engine. `text = None` drops
    /// the override, reverting to the engine's on-disk query.
    SetQuery {
        lang: String,
        name: String,
        text: Option<String>,
    },
}

/// A window mutation queued by the window Lua API (`vim.api.nvim_set_current_win`,
/// `nvim_win_set_buf`/`set_cursor`/`set_width`/`set_height`/`close`, `nvim_open_win`),
/// drained by the server in `apply_lua_effects` and applied to the live editor —
/// the window analogue of [`BufOp`]. Reads (`nvim_list_wins`, `nvim_win_get_*`)
/// need no op: they resolve against the `nx._wins` mirror the server pushes
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
    /// `vim.fn.winrestview({ topline = N })` (run via `nvim_win_call`) — scroll
    /// window `win` so its first visible line is `top` (0-based; the prelude
    /// converts neovim's 1-based `topline`).
    SetTopline { win: u64, top: usize },
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
/// `nvim_tabpage_*`) need no op: they resolve against the `nx._tabs` mirror the
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

/// One diagnostic mirrored into the Lua `nx._diagnostics[bufnr]` table so the
/// synchronous getter `vim.diagnostic.get` can read it without reaching the live
/// `Server` (the Rust→Lua mirror, the analogue of `nx._set_cur_buf`). Fields
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

/// One decoded semantic token mirrored into `nx._semantic_tokens[bufnr]` so the
/// synchronous getter `vim.lsp.semantic_tokens.get_at_pos` can read it from pure
/// Lua (the Rust→Lua mirror, the analogue of `nx._diagnostics`). Positions are
/// 0-based; `start_col`/`end_col` are line-local **byte** offsets (already
/// converted from the server's encoding when the tokens were decoded), matching
/// neovim's byte-column `get_at_pos` shape.
#[derive(Clone, Debug)]
pub struct SemanticTokenData {
    /// 0-based buffer line the token sits on.
    pub line: u32,
    /// 0-based start byte column.
    pub start_col: u32,
    /// 0-based (exclusive) end byte column.
    pub end_col: u32,
    /// The legend token-type name (e.g. `"function"`).
    pub token_type: String,
    /// The active modifier names (e.g. `["readonly", "static"]`), legend order.
    pub modifiers: Vec<String>,
    /// The owning LSP client id (the buffer's server), matching neovim's per-token
    /// `client_id`.
    pub client_id: u64,
}

/// One decoded inlay hint mirrored into `nx._inlay_hints[bufnr]` so the
/// synchronous getter `vim.lsp.inlay_hint.get` can read it from pure Lua (the
/// Rust→Lua mirror, the analogue of [`SemanticTokenData`] / `nx._diagnostics`).
/// Pushed on every `textDocument/inlayHint` reply (and after a lazy hint resolves).
/// `line` is 0-based; `col` is a line-local **byte** offset (already converted from
/// the server's encoding when the hint was decoded), a documented approximation of
/// neovim's encoding-native `position.character`.
#[derive(Clone, Debug)]
pub struct InlayHintMirrorData {
    /// 0-based buffer line the hint anchors on.
    pub line: u32,
    /// 0-based byte column the hint anchors at.
    pub col: u32,
    /// The rendered label (padding already folded into a leading/trailing space).
    pub label: String,
    /// `1`=type, `2`=parameter, `0`=unspecified — neovim's `InlayHintKind`.
    pub kind: u8,
    /// The owning LSP client id (the buffer's server), matching neovim's per-hint
    /// `client_id`.
    pub client_id: u64,
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
    pub semantic_tokens: bool,
    pub inlay_hints: bool,
}

/// One `vim.keymap.set` entry, read back from `nx._keymaps` as plain data for
/// the server to compile into its per-mode prefix tries. Unlike autocmds — whose
/// *matching* stays in Lua (`nx._fire`) — keymap matching happens in Rust, so
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
    /// A function RHS, keyed by id in `nx._keymap_fns` (run via `run_keymap`).
    Lua(u64),
    /// A string RHS — key notation the server parses and feeds.
    Str(String),
}

/// A `vim.ui.input(opts, on_confirm)` request: open a one-line prompt labelled
/// `prompt`, prefilled with `default`, and fire callback `cb_id` (a `nx._cb_fns`
/// entry wrapping `on_confirm`) with the typed text — or `nil` on cancel — when
/// the user submits. Queued in [`crate::runtime::Shared::ui_inputs`], drained by the server.
#[derive(Clone, Debug)]
pub struct UiInputReq {
    /// The prompt label shown ahead of the editable line (`opts.prompt`).
    pub prompt: String,
    /// The text the line is prefilled with (`opts.default`; empty when unset).
    pub default: String,
    /// The `nx._cb_fns` id whose `on_confirm` wrapper receives the result.
    pub cb_id: u64,
}

/// A `nx.ui.select(items, opts, on_choice)` request: open the floating
/// selectable-list widget showing `items` (the display labels the Lua wrapper
/// already rendered through `opts.format_item`) and fire callback `cb_id` (a
/// `nx._cb_fns` entry wrapping `on_choice`) with the chosen **1-based index** —
/// or `nil` on cancel — when the user confirms. The wrapper maps that index back
/// to the original item, so only the labels and the index cross the bridge.
/// Queued in [`crate::runtime::Shared::ui_selects`], drained by the server into
/// [`Editor::open_menu`](nxvim_core::Editor::open_menu).
#[derive(Clone, Debug)]
pub struct UiSelectReq {
    /// The display labels, in order, one per selectable row (non-empty: the Lua
    /// wrapper resolves an empty list to an immediate cancel without queuing).
    pub items: Vec<String>,
    /// The prompt label (`opts.prompt`); empty when unset. Shown as the menu's
    /// title. (Phase 1 has no prompt input — this is a static title only.)
    pub prompt: String,
    /// The `nx._cb_fns` id whose `on_choice` wrapper receives the chosen index.
    pub cb_id: u64,
}

/// A `nx.ui.float(contents, opts)` request: open (or update / close) the
/// list-less **content float** rendering `lines` (the sibling of the
/// selectable-list widget — hover / signature help / arbitrary plugin content).
///
/// Two lifetimes share this one queue, ordered so an open-then-close within a
/// single chunk is honoured:
/// - **Transient** (`id == 0`): fire-and-forget, dismissed by the next key — the
///   hover / signature / plain-`nx.ui.float` shape.
/// - **Persistent** (`id != 0`): a `persist`-flagged float keyed by a Lua handle
///   id; it survives keystrokes and is closed only by an explicit `close` op (or a
///   replacement). An `:update` is just another open with the same `id`.
///
/// Queued in [`crate::runtime::Shared::ui_floats`], drained by the server into
/// [`Editor::open_content_float`](nxvim_core::Editor::open_content_float) /
/// [`open_persistent_float`](nxvim_core::Editor::open_persistent_float) /
/// [`close_content_float_id`](nxvim_core::Editor::close_content_float_id).
#[derive(Clone, Debug)]
pub struct UiFloatReq {
    /// The handle id for a persistent float, or `0` for a transient one. A `close`
    /// op targets the float with this id.
    pub id: u64,
    /// `true` to **close** the persistent float keyed by `id` (the other fields are
    /// ignored); `false` to open / update.
    pub close: bool,
    /// The content lines to render, in order (non-empty for an open: the Lua
    /// wrapper resolves empty content to a no-op / close without queuing an open).
    pub lines: Vec<String>,
    /// The title drawn on the top border (`opts.title`), or `None` when untitled.
    pub title: Option<String>,
    /// The border keyword (`opts.border`, defaulted to `"rounded"` by the wrapper):
    /// `"none"` / `"single"` / `"rounded"` / `"double"` / `"solid"`. Parsed (and
    /// validated loud) server-side via `BorderStyle::from_keyword`.
    pub border: String,
    /// Placement: `false` anchors at the cursor (the default — hover shape), `true`
    /// centers over the editor (`opts.relative = "editor"`).
    pub editor: bool,
}

/// A `nx.picker.open(name)` request: open the fuzzy-finder widget (a centered
/// float with a prompt) over the unified float-list widget
/// ([`Editor::open_picker`](nxvim_core::Editor::open_picker)). The source's
/// candidates, `confirm`, and `on_cancel` all stay Lua-side (the wrapper's
/// `nx._picker` state); only this open signal and the `dynamic` flag cross the
/// bridge. Queued in [`crate::runtime::Shared::picker_opens`].
#[derive(Clone, Debug)]
pub struct PickerOpenReq {
    /// Whether the active source is dynamic (forward each query edit to the source,
    /// bypassing the local matcher) or static (matched locally in Rust).
    pub dynamic: bool,
    /// The picker box's fixed width / height, each as the raw spec the server
    /// parses: a cell count (`"100"`), a CSS-style viewport fraction (`"80vw"` /
    /// `"60vh"` / `"50%"`), or empty for the picker default. Never content-derived.
    pub width: String,
    pub height: String,
    /// Whether the prompt sits **below** the results list (the telescope-style
    /// layout) rather than above it (the default). The Lua wrapper resolves the
    /// `prompt_pos = "top" | "bottom"` option down to this flag.
    pub prompt_bottom: bool,
    /// Whether the source declared a `preview` kind (`"file"` / `"location"`), so
    /// the widget reserves a preview column and each push may carry a
    /// [`PreviewPush`]. `false` for a preview-less picker — rendered exactly as
    /// before. The kind itself (file vs location) is implicit in whether the
    /// pushes carry a `loc`.
    pub preview: bool,
}

/// A `nx.complete.setup{}` configuration request for the native completion engine
/// (Phase 4-A). The Lua wrapper validates the source list (only `"buffer"` so far)
/// and resolves the options; the server parses the key `notation` lists into
/// [`Key`](nxvim_core::input::Key)s and applies a
/// [`CompleteConfig`](nxvim_core::CompleteConfig). An empty key list for an action
/// keeps that action's built-in default. Queued in
/// [`crate::runtime::Shared::complete_setups`].
#[derive(Clone, Debug)]
pub struct CompleteSetupReq {
    /// Complete as you type (the engine auto-opens / refreshes on word keystrokes).
    pub auto: bool,
    /// The prefix must reach this many characters before the menu opens.
    pub min_chars: usize,
    /// Key notation (`"<C-n>"`, `"<Tab>"`, …) for each engine action; empty ⇒ keep
    /// the built-in default for that action.
    pub next: Vec<String>,
    pub prev: Vec<String>,
    pub confirm: Vec<String>,
    pub abort: Vec<String>,
    /// Whether at least one configured source needs off-input-path dispatch — a Lua
    /// `complete` function (`nx.complete.source{}`) or the built-in `lsp` source — so
    /// the engine emits a `(gen, ctx)` onto `complete_query_changes` for the server to
    /// dispatch. `false` for a buffer-only config — the whole keystroke path stays
    /// pure core. Phase 4-B.
    pub has_async: bool,
    /// Whether the built-in `lsp` source is configured, so the server issues
    /// `textDocument/completion` on each trigger and streams the results into the
    /// engine menu (delegated accept applies the item's `textEdit`). Phase 4-C.
    pub lsp: bool,
    /// Merge priority of the `buffer` source (`0` when not configured) — stamped onto
    /// its rows so the merged view ranks higher-priority sources first. Phase 4-C.
    pub buffer_priority: i32,
    /// Merge priority of the `lsp` source (`0` when not configured). Phase 4-C.
    pub lsp_priority: i32,
    /// Show the docs sidebar beside the popup (the selected item's documentation,
    /// rendered server-side from the `lsp` source's cache + `completionItem/resolve`).
    /// On by default; a `buffer`-only config simply never has docs to show. Phase 4-D.
    pub docs: bool,
    /// The union of every active source's **trigger chars** as a plain string (each
    /// char is one trigger), e.g. `":"` for an emoji source's `trigger = { chars = {
    /// ":" } }`. The server splits it into the engine's `trigger_chars`. Empty when no
    /// source declares one (the prefix is the plain word run). Phase 4-E.
    pub trigger_chars: String,
    /// Whether the built-in `snippets` source is configured, so the server offers the
    /// registered snippet triggers for the buffer's filetype and expands the body on
    /// accept (the native snippet engine).
    pub snippets: bool,
    /// Merge priority of the `snippets` source (`0` when not configured).
    pub snippets_priority: i32,
}

/// A `nx.snippet.setup{}` request: the tabstop-jump keys as vim notation lists
/// (`"<Tab>"`, …), empty ⇒ keep the built-in default. Queued in
/// [`crate::runtime::Shared::snippet_setups`].
#[derive(Clone, Debug)]
pub struct SnippetSetupReq {
    pub next: Vec<String>,
    pub prev: Vec<String>,
}

/// A `nx.snippet.add(ft, …)` registration: the filetype and parallel `triggers` /
/// `bodies` arrays (string bodies only in this phase). Queued in
/// [`crate::runtime::Shared::snippet_adds`]; the server stores them for the
/// `snippets` completion source.
#[derive(Clone, Debug)]
pub struct SnippetAddReq {
    pub filetype: String,
    pub triggers: Vec<String>,
    pub bodies: Vec<String>,
}

/// One streamed completion candidate (`nx.complete` async source `push`): its
/// display `label` and the `insert` text applied on accept (`label` when the
/// source pushed a bare string), stamped with the `gen`eration of the trigger that
/// produced it so the server drops a batch from a superseded prefix. Queued in
/// [`crate::runtime::Shared::complete_pushes`]. Phase 4-B.
#[derive(Clone, Debug)]
pub struct CompletePush {
    pub gen: u64,
    pub label: String,
    pub insert: String,
    /// Optional inline documentation for the docs sidebar (`push { doc = … }`),
    /// rendered beside the popup when this row is selected. `None` for a bare
    /// candidate. Phase 4-E.
    pub doc: Option<String>,
    /// Optional **lazy-docs resolve handle** — set when this row's source has a
    /// `resolve` callback but no inline `doc`. The server asks Lua to resolve the
    /// docs (`nx._complete_resolve(id)`) once the row is selected. `None` for an
    /// inline-doc / no-resolve row. Phase 4-E.
    pub resolve: Option<u64>,
}

/// The preview target a picker push carries for one candidate, when the source
/// declared a `preview` kind: the file `path` and, for the `"location"` kind, the
/// 0-based `loc` (row, col) to scroll to and range-highlight. `loc` is `None` for
/// the `"file"` kind (show the head). Becomes
/// [`nxvim_core::PreviewTarget`](nxvim_core::PreviewTarget) on the way to the widget.
#[derive(Clone, Debug)]
pub struct PreviewPush {
    pub path: String,
    pub loc: Option<(usize, usize)>,
}

/// One streamed picker candidate (`nx.picker` source `push`): its display `label`
/// and the wrapper's `key` (the 1-based index into the source run's Lua item
/// array), stamped with the `gen`eration of the query run that produced it so the
/// server can drop a batch from a superseded run. Queued in
/// [`crate::runtime::Shared::picker_pushes`].
#[derive(Clone, Debug)]
pub struct PickerPush {
    pub gen: u64,
    pub label: String,
    pub key: usize,
    /// The candidate's preview target ([`PreviewPush`]) when the picker carries a
    /// preview pane and this row supplied a `path`; `None` otherwise (preview-less
    /// picker, or a row with no path — e.g. an unnamed buffer).
    pub preview: Option<PreviewPush>,
}

/// A `vim.fn.confirm(msg, choices, …)` request: open the command line as a
/// single-key button dialog showing `label` (the message plus the rendered
/// buttons, already formatted by the Lua wrapper) and fire callback `cb_id` (a
/// `nx._cb_fns` entry that resumes the blocked coroutine) with the chosen
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
    /// The `nx._cb_fns` id whose continuation receives the chosen index.
    pub cb_id: u64,
}
