-- nxvim Lua prelude — vim.lsp framework.
-- The Neovim-0.11 vim.lsp config framework (vim.lsp.config/enable/start, the enable dispatcher, on_attach plumbing) and the vim.lsp.buf entry points to the native features.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- vim.lsp: the config framework (Neovim 0.11 core) ----------------------
-- nxvim's LSP machinery (the nxvim-lsp client + server-side document sync) is
-- driven entirely from this Lua surface, exactly like neovim 0.11: a user calls
-- `vim.lsp.config(name, …)` / `vim.lsp.enable(name)` (or drops an
-- `lsp/<name>.lua` on the runtimepath), and an opened file of a matching
-- filetype starts the server. There is no built-in server table — zero config
-- means no LSP. `vim.lsp.start` queues an `LspOp` (Rust `vim._lsp_start`) the
-- server drains into its `LspManager`.

vim.lsp = vim.lsp or {}
vim.lsp.protocol = vim.lsp.protocol or {}

-- vim.lsp.commands: the client-side command registry (a config's `before_init`
-- may populate it, e.g. rust_analyzer's `rust-analyzer.runSingle`). A code action's
-- `command` (or an explicit `vim.lsp.buf.execute_command`) checks here first: a
-- registered `vim.lsp.commands[name]` handler `(command, ctx)` runs locally, else
-- the command is relayed to the server as `workspace/executeCommand` (Phase 8).
vim.lsp.commands = vim.lsp.commands or {}

-- vim.lsp.handlers: the default response-handler registry, keyed by LSP method
-- (`handler(err, result, ctx)`). A `client:request` with no explicit handler falls
-- back to the config's `handlers[method]`, then this global table (Phase 5). A
-- config may register a global default handler here; the per-config layer wins.
vim.lsp.handlers = vim.lsp.handlers or {}

-- Client capabilities are owned and advertised by the Rust client at
-- `initialize`; this stub lets a config that merges into them run without error.
-- INCOMPLETE: returns an EMPTY table, not nxvim's real default capabilities. A
-- config that *replaces* whole subtrees (caps = tbl_deep_extend("force", caps,
-- cmp_caps)) works — its deltas flow through to the server (lib.rs `capabilities`
-- is deep-merged over the Rust base). But a config that *indexes* a nested field
-- (caps.textDocument.completion.completionItem.snippetSupport = true) crashes on
-- nil, because the tree isn't populated. A real impl would mirror the Rust base
-- client capabilities here so reads and writes both see the true advertised tree.
function vim.lsp.protocol.make_client_capabilities() return {} end

-- vim.lsp.protocol.MessageType: the window/logMessage severity enum. A config
-- may name it as a literal value (smithy_ls sets `message_level =
-- vim.lsp.protocol.MessageType.Log`), so the table must exist at load time.
vim.lsp.protocol.MessageType = {
  Error = 1,
  Warning = 2,
  Info = 3,
  Log = 4,
  Debug = 5,
  [1] = "Error",
  [2] = "Warning",
  [3] = "Info",
  [4] = "Log",
  [5] = "Debug",
}

-- vim.lsp.protocol.Methods: the request/notification method-name table. Real
-- neovim maps e.g. `textDocument_diagnostic` -> "textDocument/diagnostic"; the
-- metatable reproduces that (first underscore -> slash) for any key, so a config
-- that names a method (in a deferred handler) gets the wire string, not nil.
vim.lsp.protocol.Methods = setmetatable({}, {
  __index = function(_, k) return (tostring(k):gsub("_", "/", 1)) end,
})

-- vim.lsp.rpc: the transport entry points a config's `cmd` builder calls.
-- nxvim does its own (stdio) process spawning in Rust, so it does not need
-- neovim's RPC client — it only needs the argv. `start(cmd, dispatchers, extra)`
-- therefore returns `cmd` unchanged: a `cmd = function(d, c) … return
-- vim.lsp.rpc.start({argv}, d) end` builder (ts_ls, eslint, jsonls, html, biome,
-- tailwindcss, … — 20-plus servers) resolves straight to its argv. `connect`
-- (a TCP transport, e.g. gdscript) can't be driven by the stdio spawner; it
-- raises (see below), so a config that builds its cmd through it surfaces as a
-- load error (vim._lsp_load_errors) rather than crashing `enable`.
vim.lsp.rpc = vim.lsp.rpc or {}
function vim.lsp.rpc.start(cmd, _dispatchers, _extra) return cmd end
-- `connect` is a TCP transport (e.g. gdscript). nxvim's spawner is stdio-only,
-- so there is no argv to hand back — returning a sentinel let the gap pass
-- silently. It raises via vim._notimpl: a config that calls it at load (gdscript)
-- surfaces as a real, allowlisted gap (TCP transport) rather than a "skip".
function vim.lsp.rpc.connect(_host, _port) vim._notimpl("vim.lsp.rpc.connect") end

-- vim.lsp.util: helpers a config reaches for inside on_attach / command / handler
-- callbacks (Phase 7). nxvim drives its core LSP features natively (vim.lsp.buf.*);
-- these compute LSP params from the real cursor/buffer state (the Phase-6 mirror)
-- and drive nxvim's own surfaces — the panel for previews, and the native
-- workspace-edit / single-location goto paths (queued as LspOps) for edits and
-- navigation. The Phase-0 vim._notimpl raises are gone.
vim.lsp.util = vim.lsp.util or {}

-- Convert a 0-based *byte* column on `line` to a position character in the LSP
-- `encoding` (utf-16 default; utf-8 → the byte index unchanged; utf-32 →
-- codepoints). nxvim stores text as UTF-8 bytes, so this walks the prefix
-- [0, byte_col) one UTF-8 lead byte at a time, counting code units (a 4-byte
-- char is a surrogate pair — 2 units — under utf-16, 1 under utf-32).
function vim._byte_to_position_char(line, byte_col, encoding)
  if encoding == nil or encoding == "utf-8" then return byte_col end
  local utf16 = encoding ~= "utf-32"
  local count, i = 0, 1
  local limit = math.min(byte_col, #line)
  while i <= limit do
    local b = string.byte(line, i)
    local size, units
    if b < 0x80 then
      size, units = 1, 1
    elseif b < 0xE0 then
      size, units = 2, 1
    elseif b < 0xF0 then
      size, units = 3, 1
    else
      size, units = 4, utf16 and 2 or 1
    end
    count = count + units
    i = i + size
  end
  return count
end

-- The inverse: a position `character` in `encoding` back to a 0-based byte column
-- on `line` (used to address loclist columns by byte). Clamps at end-of-line.
function vim._position_char_to_byte(line, character, encoding)
  if encoding == nil or encoding == "utf-8" then return math.min(character, #line) end
  local utf16 = encoding ~= "utf-32"
  local count, i = 0, 1
  while i <= #line and count < character do
    local b = string.byte(line, i)
    local size, units
    if b < 0x80 then
      size, units = 1, 1
    elseif b < 0xE0 then
      size, units = 2, 1
    elseif b < 0xF0 then
      size, units = 3, 1
    else
      size, units = 4, utf16 and 2 or 1
    end
    count = count + units
    i = i + size
  end
  return i - 1
end

-- The text of 0-based `row` in the loaded buffer whose name maps to `uri`, or nil
-- when no open buffer backs it. Scans the Phase-6 mirror (`vim._bufs`, which
-- carries each buffer's name and line array) — the loclist `text` field for a
-- location in an unopened file is left empty rather than read off disk.
function vim._line_text_for_uri(uri, row)
  local fname = vim.uri_to_fname(uri)
  for _, buf in pairs(vim._bufs) do
    if buf.lines and buf.name == fname then return buf.lines[row + 1] end
  end
  return nil
end

-- make_text_document_params(bufnr): the `{ uri }` a request's `textDocument` field
-- carries, from the buffer's file path.
-- INCOMPLETE: a *non-current* bufnr yields an empty URI — `nvim_buf_get_name` is
-- backed by the current-buffer snapshot only, so it can't name another buffer's
-- file. Faithful once buffer names come from a real multi-buffer registry.
function vim.lsp.util.make_text_document_params(bufnr)
  return { uri = vim.uri_from_bufnr(bufnr or 0) }
end

-- make_position_params(window, encoding): the `{ textDocument, position }` a
-- cursor-relative request (definition, hover, …) carries. The cursor comes from
-- the real editor (Phase-6 mirror); its byte column is converted to `encoding`
-- (utf-16 default). `window` is ignored — always the current window's cursor.
-- INCOMPLETE: `window` is ignored — this helper always reads the *current*
-- window (`nvim_win_get_cursor(0)`), so a config passing a specific window
-- handle gets the current one's position instead. (nxvim has multiple windows
-- now; the helper just isn't window-arg-aware.)
function vim.lsp.util.make_position_params(_window, encoding)
  encoding = encoding or "utf-16"
  local bufnr = vim.api.nvim_get_current_buf()
  local c = vim.api.nvim_win_get_cursor(0) -- { row (1-based), col (0-based byte) }
  local line = vim.api.nvim_buf_get_lines(bufnr, c[1] - 1, c[1], false)[1] or ""
  return {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
    position = { line = c[1] - 1, character = vim._byte_to_position_char(line, c[2], encoding) },
  }
end

-- make_given_range_params(start_pos, end_pos, bufnr, encoding): the
-- `{ textDocument, range }` a range request (range formatting, range code action)
-- carries. `start_pos`/`end_pos` are `{ row (1-based), col (0-based byte) }` (the
-- neovim mark shape); the columns convert to `encoding` and the end is made
-- exclusive (marks are inclusive), matching neovim.
function vim.lsp.util.make_given_range_params(start_pos, end_pos, bufnr, encoding)
  encoding = encoding or "utf-16"
  bufnr = vim._resolve_bufnr(bufnr or 0)
  local function pos_at(p)
    local row = p[1] - 1
    local line = vim.api.nvim_buf_get_lines(bufnr, row, row + 1, false)[1] or ""
    return { line = row, character = vim._byte_to_position_char(line, p[2], encoding) }
  end
  local s = pos_at(start_pos)
  local e = pos_at(end_pos)
  e.character = e.character + 1 -- inclusive mark → exclusive LSP range end
  return {
    textDocument = vim.lsp.util.make_text_document_params(bufnr),
    range = { start = s, ["end"] = e },
  }
end

-- locations_to_items(locations, encoding): turn LSP `Location` / `LocationLink`s
-- into loclist items (`{ filename, lnum, col, text }`), sorted by file then
-- position. The byte `col` and the `text` come from the open buffer backing each
-- location (empty `text` for an unopened file). `user_data` keeps the raw location.
-- INCOMPLETE: a location in an *unopened* file gets an empty `text` (and a `col`
-- computed against ""), because the line text is read from open buffers only — a
-- result list spanning files you haven't visited shows blank previews. Faithful
-- once line text can be read from disk for unopened files.
function vim.lsp.util.locations_to_items(locations, encoding)
  encoding = encoding or "utf-16"
  local items = {}
  for _, loc in ipairs(locations or {}) do
    local uri = loc.uri or loc.targetUri
    local range = loc.range or loc.targetRange
    if uri and range then
      local row = range.start.line
      local text = vim._line_text_for_uri(uri, row) or ""
      items[#items + 1] = {
        filename = vim.uri_to_fname(uri),
        lnum = row + 1,
        col = vim._position_char_to_byte(text, range.start.character, encoding) + 1,
        text = text,
        user_data = loc,
      }
    end
  end
  table.sort(items, function(a, b)
    if a.filename ~= b.filename then return a.filename < b.filename end
    if a.lnum ~= b.lnum then return a.lnum < b.lnum end
    return a.col < b.col
  end)
  return items
end

-- get_effective_tabstop(bufnr): the indent width for the buffer — `shiftwidth`
-- when set (> 0), else `tabstop`, read through vim.bo (so it reflects the core's
-- buffer-local values, defaulting to 8).
function vim.lsp.util.get_effective_tabstop(bufnr)
  local bo = vim.bo[vim._resolve_bufnr(bufnr or 0)]
  local sw = bo.shiftwidth or 0
  if sw > 0 then return sw end
  return bo.tabstop or 8
end

-- open_floating_preview(contents, syntax, opts): show `contents` (a list of lines)
-- in nxvim's panel — the surface that stands in for neovim's floating window.
-- neovim returns `(float_bufnr, win_id)`; nxvim has one panel and no per-float
-- handle, so it returns `0` and the current window handle for call-site shape.
-- INCOMPLETE: returns `(0, curwin)` placeholders, not a real float buffer/window
-- pair — a config that closes/relocates/styles the returned float by its handles
-- can't (there's one shared panel, no per-float identity). `syntax` is ignored
-- (the panel has no per-preview filetype). Faithful once floats are real windows.
function vim.lsp.util.open_floating_preview(contents, _syntax, opts)
  opts = opts or {}
  local lines = type(contents) == "table" and contents or { tostring(contents) }
  vim.panel.open(opts.title or "Preview", lines)
  return 0, vim.api.nvim_get_current_win()
end

-- apply_workspace_edit(workspace_edit, encoding): apply a `WorkspaceEdit` across
-- the open buffers it names, reusing the native rename / code-action path (queued
-- as an LspOp the server normalizes and applies). Edits to unopened files are a
-- follow-up (the native path edits open buffers only); `encoding` is carried by
-- the edit's positions and resolved server-side, so the arg is accepted here.
-- INCOMPLETE: edits land only in *open* buffers — a workspace edit that touches
-- files you haven't opened (a project-wide rename) silently skips them. Each call
-- is also its own undo step (no `undojoin` coalescing). Faithful once edits can
-- be applied to files on disk without opening them.
function vim.lsp.util.apply_workspace_edit(workspace_edit, _encoding)
  vim._lsp_apply_workspace_edit(workspace_edit or {})
end

-- show_document(location, encoding, opts): jump the cursor to an LSP location
-- (`Location` or `LocationLink`), opening the file if needed — the native
-- single-location goto, queued as an LspOp. An `external = true` location (open in
-- a browser/program) has no nxvim surface, so it raises rather than no-op.
function vim.lsp.util.show_document(location, encoding, _opts)
  if type(location) ~= "table" then return false end
  if location.external then vim._notimpl("vim.lsp.util.show_document (external)") end
  local uri = location.uri or location.targetUri
  local range = location.range or location.targetRange
  if not uri then return false end
  local line = range and range.start.line or 0
  local character = range and range.start.character or 0
  vim._lsp_show_document(uri, line, character, encoding or "utf-16")
  return true
end

-- convert_input_to_markdown_lines(input, contents): flatten an LSP hover/doc value
-- into a list of markdown lines, the shape `open_floating_preview` and completion
-- sources (cmp_luasnip) feed to a preview. `input` is a string, a `MarkupContent`
-- ({ kind, value }), a `MarkedString` ({ language, value } — fenced as a code
-- block), or an array of any of those (recursed in order). `contents` is an
-- optional accumulator. Mirrors neovim's algorithm so plugins get identical lines.
local function split_md_lines(s)
  -- Normalize CRLF/CR to LF, then split on newlines (each element is one line).
  return vim.split((s:gsub("\r\n?", "\n")), "\n", { plain = true })
end

function vim.lsp.util.convert_input_to_markdown_lines(input, contents)
  contents = contents or {}
  if type(input) == "string" then
    vim.list_extend(contents, split_md_lines(input))
  elseif type(input) == "table" then
    if input.kind then
      -- MarkupContent (markdown / plaintext) — take its value verbatim.
      vim.list_extend(contents, split_md_lines(input.value or ""))
    elseif input.language then
      -- MarkedString — a fenced code block in the given language.
      contents[#contents + 1] = "```" .. input.language
      vim.list_extend(contents, split_md_lines(input.value or ""))
      contents[#contents + 1] = "```"
    else
      -- An array of the above, in order.
      for _, item in ipairs(input) do
        vim.lsp.util.convert_input_to_markdown_lines(item, contents)
      end
    end
  else
    error("convert_input_to_markdown_lines: expected string or table, got " .. type(input))
  end
  -- A single empty line means "no content".
  if #contents == 1 and (contents[1] == "" or contents[1] == nil) then return {} end
  return contents
end

-- vim.lsp.omnifunc: the legacy `i_CTRL-X_CTRL-O` (Vimscript-era omni-completion)
-- entry point. nxvim has no omnifunc path yet; returning -1 ("no completion")
-- masked the gap, so it raises via vim._notimpl.
--
-- NOTE: this is NOT nxvim's completion menu. The native insert-mode menu is real
-- and works against live servers: <C-Space> fires `textDocument/completion` via
-- LspReqKind::Completion, opening the server-owned `CompletionMenu`
-- (crates/nxvim-server/src/lsp.rs). This raise only covers the legacy omnifunc
-- integration point — do not read it as "completion is missing." The one real
-- completion-menu gap is per-item documentation; see
-- docs/plans/2026-06-06-completion-documentation.md.
function vim.lsp.omnifunc(_findstart, _base) vim._notimpl("vim.lsp.omnifunc") end

vim._lsp_user_config = vim._lsp_user_config or {} -- name -> user override layer
vim._lsp_base_cache = vim._lsp_base_cache or {} -- name -> lsp/<name>.lua result (false = none)
vim._lsp_enabled = vim._lsp_enabled or {} -- name -> enabled?

-- Phase 1 visibility surfaces: a config that errors at load, and a server skipped
-- at start, are recorded here (keyed by name, so a re-resolve never duplicates)
-- instead of silently degrading to `{}` / a bare `return`. `vim._report`
-- reads them back. See docs/plans/2026-06-05-lsp-completion.md (Phase 1).
vim._lsp_load_errors = vim._lsp_load_errors or {} -- name -> load error message
vim._lsp_skipped = vim._lsp_skipped or {} -- name -> skip reason

-- Record that `name`'s lsp/<name>.lua failed to load, and echo a one-line
-- warning. One broken config must not wedge startup — the editor keeps running
-- and the other servers still start — but the failure is loud, not swallowed into
-- an empty config. Idempotent (lsp_base_config caches, so it records once).
local function lsp_record_load_error(name, err)
  vim._lsp_load_errors[name] = err
  vim.api.nvim_echo("nxvim LSP: config '" .. name .. "' failed to load: " .. err)
end

-- Record that `name` was skipped at start with `reason` (its cmd didn't resolve
-- to a spawnable stdio argv), and echo a one-line warning. Deduped on the reason
-- so a server that skips on every buffer open doesn't spam the panel.
local function lsp_record_skip(name, reason)
  if vim._lsp_skipped[name] == reason then return end
  vim._lsp_skipped[name] = reason
  vim.api.nvim_echo("nxvim LSP: server '" .. name .. "' skipped: " .. reason)
end

-- Errors raised inside a config's lifecycle hook (`before_init` / `on_init` /
-- `on_exit`), keyed by "name:hook" → message. A hook that throws (e.g. one that
-- reaches a Phase-0 gap like `vim.uv`) must not wedge the start/exit path, but the
-- failure is recorded and echoed, never swallowed. Surfaced by vim._report.
vim._lsp_hook_errors = vim._lsp_hook_errors or {}
local function lsp_record_hook_error(name, hook, err)
  local key = (name or "?") .. ":" .. hook
  vim._lsp_hook_errors[key] = tostring(err)
  vim.api.nvim_echo(
    "nxvim LSP: " .. hook .. " for '" .. (name or "?") .. "' errored: " .. tostring(err)
  )
end

-- The client registry: id -> { id, name, server_capabilities }, mirrored from
-- Rust (`LuaRuntime::set_lsp_client`) when a server finishes `initialize`. The
-- handle `LspAttach`'s `args.data.client_id` resolves through `get_client_by_id`.
vim.lsp._clients = vim.lsp._clients or {}

-- The handler for a `client:request` reply on `method`: the config's
-- `handlers[method]`, else the global `vim.lsp.handlers[method]`, else nil (the
-- reply is discarded after firing). The per-config layer wins (Phase 5).
function vim.lsp._resolve_handler(name, method)
  local cfg = name and vim.lsp.config[name]
  if cfg and type(cfg.handlers) == "table" and cfg.handlers[method] ~= nil then
    return cfg.handlers[method]
  end
  return vim.lsp.handlers[method]
end

-- client:request(method, params, handler, bufnr): issue a generic LSP request to
-- this client's server and route the reply to `handler(err, result, ctx)` when it
-- lands off-tick (Phase 5). With no handler, falls back to the config's
-- `handlers[method]` then `vim.lsp.handlers[method]`. The handler is registered in
-- the deferred-callback registry (`vim._cb_fns`), dropped after one fire (no leak).
-- Returns `true, request_id`; the reply won't arrive if the server exits first
-- (the same liveness caveat neovim has).
function vim.lsp._client_request(self, method, params, handler, bufnr)
  if type(method) ~= "string" then error("client:request: method must be a string", 2) end
  handler = handler or vim.lsp._resolve_handler(self.name, method)
  local cb = vim._next_cb_id()
  local client_id, client_name = self.id, self.name
  vim._cb_fns[cb] = function(err, result)
    if handler then
      handler(err, result, {
        method = method,
        client_id = client_id,
        client_name = client_name,
        bufnr = bufnr,
      })
    end
  end
  vim._lsp_client_request(self.id, method, params, cb)
  return true, cb
end

-- client:notify(method, params): fire-and-forget a generic LSP notification to
-- this client's server (Phase 5). Returns true (queued).
function vim.lsp._client_notify(self, method, params)
  if type(method) ~= "string" then error("client:notify: method must be a string", 2) end
  vim._lsp_client_notify(self.id, method, params)
  return true
end

-- vim.lsp._dispatch_command(client_id, command): run an LSP command (Phase 8),
-- the shared path for a code-action `command` (called from Rust when an applied
-- action carries one) and `vim.lsp.buf.execute_command`. `command` is the LSP
-- `Command` table (`{ command = name, arguments = {...}, title = ... }`). A
-- registered `vim.lsp.commands[name]` handler `(command, ctx)` wins (client-side,
-- e.g. a config-provided runner); otherwise the command is relayed to the client's
-- server as `workspace/executeCommand`. A missing command name or client is loud.
function vim.lsp._dispatch_command(client_id, command)
  local name = type(command) == "table" and command.command
  if type(name) ~= "string" then error("vim.lsp: execute_command needs a command string", 2) end
  local ctx = { client_id = client_id, bufnr = vim.api.nvim_get_current_buf() }
  local handler = vim.lsp.commands[name]
  if handler then
    handler(command, ctx)
    return
  end
  local client = vim.lsp.get_client_by_id(client_id)
  if not client then
    error("vim.lsp: no client " .. tostring(client_id) .. " for command " .. name, 2)
  end
  client:request(
    "workspace/executeCommand",
    { command = name, arguments = command.arguments },
    nil,
    ctx.bufnr
  )
end

-- Build a client table carrying the real request/notify methods. Shared by
-- `_set_client` (the entry `get_client_by_id`/`on_attach` resolve) and
-- `get_clients`, so `client:request`/`client:notify` work from every call site.
function vim.lsp._make_client(id, name, server_capabilities)
  return {
    id = id,
    name = name,
    server_capabilities = server_capabilities or {},
    request = vim.lsp._client_request,
    notify = vim.lsp._client_notify,
  }
end

function vim.lsp._set_client(id, name, server_capabilities)
  vim.lsp._clients[id] = vim.lsp._make_client(id, name, server_capabilities)
end
function vim.lsp._remove_client(id) vim.lsp._clients[id] = nil end

-- vim.lsp.get_client_by_id(id): the registered client table (with `name` and
-- `server_capabilities`), or nil once its server has exited.
function vim.lsp.get_client_by_id(id) return vim.lsp._clients[id] end

-- vim.lsp._run_on_init(id, result): call the config's `on_init(client, result)`
-- hook (Phase 3), invoked from Rust (`LuaRuntime::run_lsp_on_init`) right after the
-- client is mirrored on `initialize`. `result` is the raw `initialize` result. A
-- throwing hook is recorded, never fatal. No-op if the client/hook is absent.
function vim.lsp._run_on_init(id, result)
  local client = vim.lsp._clients[id]
  if not client then return end
  local cfg = vim.lsp.config[client.name]
  if cfg and type(cfg.on_init) == "function" then
    local ok, err = pcall(cfg.on_init, client, result)
    if not ok then lsp_record_hook_error(client.name, "on_init", err) end
  end
end

-- vim.lsp._run_on_exit(id, code, signal): call the config's
-- `on_exit(code, signal, client)` hook (Phase 3), invoked from Rust
-- (`LuaRuntime::run_lsp_on_exit`) when the server exits, while the client is still
-- registered (before it is removed). A throwing hook is recorded, never fatal.
function vim.lsp._run_on_exit(id, code, signal)
  local client = vim.lsp._clients[id]
  if not client then return end
  local cfg = vim.lsp.config[client.name]
  if cfg and type(cfg.on_exit) == "function" then
    local ok, err = pcall(cfg.on_exit, code, signal, client)
    if not ok then lsp_record_hook_error(client.name, "on_exit", err) end
  end
end

-- vim.lsp.get_clients(filter): the list of active clients, each a
-- `{ id, name, server_capabilities, config, request, notify }` table. `filter`
-- narrows by `id` and/or `name`; a `bufnr` filter is accepted but not honored —
-- nxvim has no Lua-side buffer->client map yet, so it returns the name/id matches
-- across all buffers. `config` is the resolved `vim.lsp.config[name]`; `request` /
-- `notify` are the real Phase-5 client methods (a server-specific command like
-- rust_analyzer's `:LspCargoReload` issues `client:request` through them).
-- `get_active_clients` is the deprecated neovim alias, kept for configs that still
-- call it.
function vim.lsp.get_clients(filter)
  filter = filter or {}
  local out = {}
  for id, c in pairs(vim.lsp._clients) do
    if (filter.id == nil or filter.id == id) and (filter.name == nil or filter.name == c.name) then
      local client = vim.lsp._make_client(c.id, c.name, c.server_capabilities)
      client.config = vim.lsp.config[c.name]
      out[#out + 1] = client
    end
  end
  return out
end

vim.lsp.get_active_clients = vim.lsp.get_clients

-- vim._report(): the runtime scoreboard — a snapshot of what the LSP layer is
-- doing and where it fell short (plus the global not-implemented hits, which are
-- not LSP-specific), so no failure stays silent. `enabled` lists the
-- configs marked for auto-activation; `started` the servers that reached
-- `initialize` (the live clients); `load_errors` the configs that failed to load
-- (name -> message); `skipped` the servers whose cmd didn't resolve to a
-- spawnable argv (name -> reason); `notimpl_hits` the not-implemented functions a
-- real config actually called (the Phase-0 set). A `:LspInfo`-style command can
-- render this later; for now it backs `:lua print(vim.inspect(vim._report()))`
-- and the tests.
function vim._report()
  local enabled = {}
  for name, on in pairs(vim._lsp_enabled) do
    if on then enabled[#enabled + 1] = name end
  end
  table.sort(enabled)
  local started = {}
  for _, c in pairs(vim.lsp._clients) do
    started[#started + 1] = c.name
  end
  table.sort(started)
  local notimpl = vim.tbl_keys(vim._notimpl_hits)
  table.sort(notimpl)
  return {
    enabled = enabled,
    started = started,
    load_errors = vim._lsp_load_errors,
    skipped = vim._lsp_skipped,
    hook_errors = vim._lsp_hook_errors,
    notimpl_hits = notimpl,
  }
end

-- Load and cache `lsp/<name>.lua` off the runtimepath (the base config layer).
-- Returns its returned table, or nil when the file is simply absent. A file that
-- IS present but fails — unreadable, a parse error, a runtime error (now possible
-- since Phase 0 made gaps raise), or one that doesn't return a table — is no
-- longer swallowed into an empty config: it is recorded in vim._lsp_load_errors
-- and echoed (lsp_record_load_error). The result is still cached (`false`) so the
-- load is attempted — and reported — only once.
local function lsp_base_config(name)
  local cached = vim._lsp_base_cache[name]
  if cached ~= nil then return cached or nil end
  local cfg = false
  local files = vim.api.nvim_get_runtime_file("lsp/" .. name .. ".lua", false)
  if files and files[1] then
    local file = files[1]
    local src = vim._read_file(file)
    if src == nil then
      lsp_record_load_error(name, "could not read " .. file)
    else
      local chunk, perr = loadstring(src, "@" .. file)
      if not chunk then
        lsp_record_load_error(name, "parse: " .. tostring(perr))
      else
        local ok, ret = pcall(chunk)
        if not ok then
          lsp_record_load_error(name, tostring(ret))
        elseif type(ret) ~= "table" then
          lsp_record_load_error(name, "config did not return a table (got " .. type(ret) .. ")")
        else
          cfg = ret
        end
      end
    end
  end
  vim._lsp_base_cache[name] = cfg
  return cfg or nil
end

-- The resolved config for `name`: the `'*'` wildcard layer, then the
-- `lsp/<name>.lua` runtimepath base, then the user override — deep-merged with
-- the rightmost winning (neovim's `vim.lsp.config[name]` chain).
local function lsp_resolve(name)
  return vim.tbl_deep_extend(
    "force",
    vim._lsp_user_config["*"] or {},
    lsp_base_config(name) or {},
    vim._lsp_user_config[name] or {}
  )
end

-- vim.lsp.config: callable to merge an override (`vim.lsp.config(name, opts)` —
-- `'*'` is the all-clients layer), indexable for the resolved config
-- (`vim.lsp.config[name]`), and assignable to redefine (`vim.lsp.config[name] =
-- opts`, which replaces the override layer and drops the runtimepath base).
vim.lsp.config = setmetatable({}, {
  __call = function(_, name, opts)
    if type(name) ~= "string" then error("vim.lsp.config: name must be a string") end
    local prev = vim._lsp_user_config[name] or {}
    vim._lsp_user_config[name] = vim.tbl_deep_extend("force", prev, opts or {})
  end,
  __index = function(_, name) return lsp_resolve(name) end,
  __newindex = function(_, name, opts)
    vim._lsp_user_config[name] = opts or {}
    vim._lsp_base_cache[name] = false -- a redefine overrides the resolved chain
  end,
})

-- Is `cmd` a usable argv — a non-empty list of strings? Guards the start queue
-- against the config shapes nxvim can't spawn: an empty/nil cmd or a builder that
-- failed. Those skip the start rather than erroring at the Rust boundary.
local function lsp_is_argv(cmd)
  if type(cmd) ~= "table" or #cmd == 0 then return false end
  for _, a in ipairs(cmd) do
    if type(a) ~= "string" then return false end
  end
  return true
end

-- `t` if it is a non-empty table, else nil. Guards the config payloads
-- (`settings` / `init_options` / `capabilities`) threaded to `vim._lsp_start`: an
-- absent or empty table becomes nil → the server forwards nothing, rather than an
-- empty `{}` that the lua_to_json bridge would emit as `[]`.
local function lsp_nonempty(t)
  if type(t) == "table" and next(t) ~= nil then return t end
  return nil
end

-- Why `cmd` is not a spawnable argv — the human-readable reason recorded in
-- vim._lsp_skipped so a skipped server isn't a silent mystery.
local function lsp_argv_reason(cmd)
  if cmd == nil then return "cmd did not resolve (nil)" end
  if type(cmd) ~= "table" then return "cmd is not an argv list (got " .. type(cmd) .. ")" end
  if #cmd == 0 then return "cmd is an empty argv list" end
  return "cmd has a non-string element"
end

-- Resolve a config's `cmd` to an argv list. A function `cmd` is neovim's
-- `cmd(dispatchers, config)` builder: nxvim does its own (stdio) spawning, so the
-- dispatchers are a stub and `vim.lsp.rpc.start` returns the argv it was given
-- (see its shim) — letting the many `node_modules/.bin` resolvers run unchanged.
-- Run the config's `before_init(init_params, config)` hook (Phase 3) if present,
-- and return the `(init_options, settings, capabilities)` to forward — honoring
-- whatever the hook left in `init_params.initializationOptions` / `.capabilities`
-- and any mutation of `config.settings` (rust_analyzer copies
-- `settings['rust-analyzer'] → init_params.initializationOptions`; eslint mutates
-- `config.settings`). nxvim runs this synchronously on the editor thread just
-- before the start is queued (no event loop needed), so the mutations are baked
-- into the `initialize` Phase 2 forwards. A throwing hook is recorded (not fatal)
-- and the pre-hook values are forwarded unchanged. `init_params` is the minimal
-- shape the common hooks touch; a `config.cmd` mutation here is too late (the cmd
-- is already resolved) and is not honored — a documented approximation.
-- INCOMPLETE: a `config.cmd` mutation inside `before_init` is ignored (cmd is
-- already resolved by the time the hook runs). `init_params` is also a minimal
-- shape, not neovim's full initialize params. Relatedly, `on_exit` does not fire
-- on an *intentional* shutdown — only on a server exit/crash — since the clean
-- path registers no client to notify. Faithful once cmd resolution moves after
-- before_init and shutdown routes through the client registry.
local function lsp_before_init(config)
  local init_options, settings, capabilities =
    config.init_options, config.settings, config.capabilities
  if type(config.before_init) == "function" then
    local init_params = {
      initializationOptions = init_options or settings,
      capabilities = capabilities or {},
    }
    local ok, err = pcall(config.before_init, init_params, config)
    if ok then
      init_options = init_params.initializationOptions
      capabilities = init_params.capabilities
      settings = config.settings -- the hook may have mutated it in place
    else
      lsp_record_hook_error(config.name, "before_init", err)
    end
  end
  return init_options, settings, capabilities
end

-- The builder gets the resolved config with `root_dir` filled in (the field those
-- resolvers read). A throwing builder yields `nil, reason` so the caller can
-- record exactly why the server was skipped (instead of a bare nil that looks the
-- same as "no cmd").
local function lsp_resolve_cmd(cfg, root)
  local cmd = cfg.cmd
  if type(cmd) == "function" then
    -- Shallow-copy and set root_dir to the *resolved* root. A direct assignment
    -- (not tbl_extend) so a nil root CLEARS the field rather than leaving cfg's
    -- root_dir function in place — otherwise a builder that does
    -- `joinpath(config.root_dir, …)` would join against a function. With it nil,
    -- those builders fall back to the global binary, which is correct.
    local config = {}
    for k, v in pairs(cfg) do
      config[k] = v
    end
    config.root_dir = root
    local ok, result = pcall(cmd, {}, config)
    if not ok then return nil, "cmd builder errored: " .. tostring(result) end
    cmd = result
  end
  return cmd
end

-- Queue a start for `bufnr` from a fully-resolved config (root already computed).
-- When the cmd doesn't resolve to a spawnable argv, the server is recorded in
-- vim._lsp_skipped with the reason (and a warning echoed) rather than vanishing —
-- so enabling a server whose binary/transport nxvim can't drive is visible, not a
-- silent no-op, and still never errors the whole enable.
local function lsp_start_resolved(name, cfg, bufnr, ft, root)
  local cmd, reason = lsp_resolve_cmd(cfg, root)
  if not lsp_is_argv(cmd) then
    lsp_record_skip(name, reason or lsp_argv_reason(cmd))
    return
  end
  vim.lsp.start({
    name = name,
    cmd = cmd,
    root_dir = root,
    filetypes = cfg.filetypes,
    -- Carry what the config configures so the server runs configured, not on
    -- defaults (Phase 2): vim.lsp.start reads these and forwards them to Rust.
    settings = cfg.settings,
    init_options = cfg.init_options,
    capabilities = cfg.capabilities,
    -- The lifecycle hook run just before initialize (Phase 3).
    before_init = cfg.before_init,
  }, { bufnr = bufnr, filetype = ft })
end

-- Resolve `cfg`'s root_dir (string | `function(bufnr, on_dir)` | `root_markers`
-- upward search) and start the server. A function root_dir drives the start
-- through its `on_dir` callback, so it can decline (never calling it) to skip a
-- buffer — the mechanism `vim.lsp.enable`'s docs describe.
local function lsp_start_for(name, cfg, bufnr, ft)
  local rd = cfg.root_dir
  if type(rd) == "function" then
    rd(bufnr, function(root) lsp_start_resolved(name, cfg, bufnr, ft, root) end)
  elseif type(rd) == "string" then
    lsp_start_resolved(name, cfg, bufnr, ft, rd)
  elseif cfg.root_markers then
    lsp_start_resolved(name, cfg, bufnr, ft, vim.fs.root(bufnr, cfg.root_markers))
  else
    lsp_start_resolved(name, cfg, bufnr, ft, nil)
  end
end

-- The shared FileType dispatcher body: for every enabled config whose resolved
-- `filetypes` includes `ft`, resolve the root and start the server for `bufnr`.
function vim.lsp._on_filetype(bufnr, ft)
  if not ft or ft == "" then return end
  for name, on in pairs(vim._lsp_enabled) do
    if on then
      local cfg = vim.lsp.config[name]
      if cfg.filetypes and vim.tbl_contains(cfg.filetypes, ft) then
        lsp_start_for(name, cfg, bufnr, ft)
      end
    end
  end
end

-- Install the single shared FileType autocmd that drives all enabled configs
-- (idempotent — `vim.lsp.enable` may be called many times).
local function lsp_ensure_dispatcher()
  if vim._lsp_dispatcher_installed then return end
  vim._lsp_dispatcher_installed = true
  local group = vim.api.nvim_create_augroup("nxvim.lsp.enable", { clear = true })
  vim.api.nvim_create_autocmd("FileType", {
    group = group,
    callback = function(args) vim.lsp._on_filetype(args.buf, args.match) end,
  })
  -- The attach hook: when the server bound to a buffer finishes its first
  -- `didOpen`, the server fires `LspAttach` with `data.client_id`; resolve the
  -- client and run its config's `on_attach(client, bufnr)` — the call site that
  -- lets a config set buffer-local LSP keymaps (`vim.keymap.set('n','gd',
  -- vim.lsp.buf.definition, {buffer=args.buf})`).
  vim.api.nvim_create_autocmd("LspAttach", {
    group = group,
    callback = function(args)
      local client = vim.lsp.get_client_by_id(args.data and args.data.client_id)
      if not client then return end
      local cfg = vim.lsp.config[client.name]
      if cfg and type(cfg.on_attach) == "function" then cfg.on_attach(client, args.buf) end
    end,
  })
end

-- vim.lsp.enable(name|list[, enable]): mark configs for auto-activation (on
-- current and future buffers) and install the FileType dispatcher. `enable=false`
-- turns a config off (future buffers won't start it). `'*'` is not a valid name.
function vim.lsp.enable(name, enable)
  local names = type(name) == "table" and name or { name }
  local on = enable ~= false
  for _, n in ipairs(names) do
    if n == "*" then error("vim.lsp.enable: '*' is not a valid LSP config name") end
    vim._lsp_enabled[n] = on
  end
  lsp_ensure_dispatcher()
  -- Process the already-open current buffer on the spot (neovim parity): its
  -- `FileType` has already fired, so the dispatcher just installed won't catch
  -- it, and an interactive `vim.lsp.enable(...)` would otherwise be a no-op until
  -- the next file opened. A start is idempotent server-side, so the overlap with
  -- the startup `FileType` (when this runs from `init.lua`) is harmless. Only on
  -- an *enable* — a disable must not start anything.
  if on then
    local cur = vim._cur_buf
    if cur and cur.filetype and cur.filetype ~= "" then
      vim.lsp._on_filetype(cur.bufnr, cur.filetype)
    end
  end
end

-- vim.lsp.start(config[, opts]): start (or reuse) the server for `config`
-- (`{name, cmd, root_dir}`) and attach a buffer (`opts.bufnr`, default the
-- snapshot buffer). `opts.filetype` is the buffer's filetype (the LSP
-- languageId). Reuse on `(name, root)` is the server's job; here it just queues.
function vim.lsp.start(config, opts)
  opts = opts or {}
  local bufnr = opts.bufnr or (vim._cur_buf and vim._cur_buf.bufnr) or 0
  local cmd, reason = config.cmd or {}, nil
  if type(cmd) == "function" then
    cmd, reason = lsp_resolve_cmd(config, config.root_dir)
  end
  -- Only queue a spawnable argv (see lsp_is_argv): a non-stdio/empty cmd would
  -- otherwise fail at the Rust `vim._lsp_start` boundary. A skip is recorded
  -- (vim._lsp_skipped) rather than returning silently.
  if not lsp_is_argv(cmd) then
    lsp_record_skip(config.name or "?", reason or lsp_argv_reason(cmd))
    return
  end
  -- Run before_init (Phase 3) and forward the (possibly hook-mutated)
  -- init_options / settings / capabilities the server applies at initialize.
  local init_options, settings, capabilities = lsp_before_init(config)
  vim._lsp_start(
    config.name,
    cmd,
    config.root_dir,
    opts.filetype or "",
    bufnr,
    lsp_nonempty(init_options),
    lsp_nonempty(settings),
    lsp_nonempty(capabilities)
  )
end

-- ----- vim.lsp.buf: Lua entry points to the native features -------------------
-- Each function enqueues an `LspOp` (Rust `vim._lsp_buf*`) that the server drains
-- on the same input tick and routes into the existing `request_lsp*` paths — so
-- the request reads the cursor where the key fired. The functions are *bare*
-- (no implicit args) so `vim.keymap.set('n', 'gd', vim.lsp.buf.definition)` works:
-- the keymap RHS is called with no arguments and just queues the op.
--
-- `kind` ints mirror `LspReqKind::as_u16` (Rust); keep the two in lockstep.
vim.lsp.buf = vim.lsp.buf or {}

function vim.lsp.buf.definition() vim._lsp_buf(0) end
function vim.lsp.buf.declaration() vim._lsp_buf(1) end
function vim.lsp.buf.type_definition() vim._lsp_buf(2) end
function vim.lsp.buf.implementation() vim._lsp_buf(3) end
function vim.lsp.buf.references() vim._lsp_buf(4) end
function vim.lsp.buf.hover() vim._lsp_buf(5) end
function vim.lsp.buf.signature_help() vim._lsp_buf(6) end

-- format()/code_action() take an options table in neovim (async, range, filter,
-- …); none have behavior in nxvim yet (the request is synchronous-issue,
-- async-reply), so the argument is accepted and ignored for call-site
-- compatibility — see the Phase 7b follow-ups.
function vim.lsp.buf.format(_opts) vim._lsp_buf_format() end
function vim.lsp.buf.code_action(_opts) vim._lsp_buf_code_action() end

-- execute_command(command, opts): run an LSP command (`{ command = name,
-- arguments = {...} }`) — the entry a config's `:Format`-style command or a code
-- action's `command` calls (Phase 8). `opts.client_id` targets a specific client;
-- otherwise the first attached client is used. Dispatch goes through
-- `vim.lsp._dispatch_command`: a registered `vim.lsp.commands[name]` handler runs
-- client-side, else the command is relayed to the server as
-- `workspace/executeCommand`.
function vim.lsp.buf.execute_command(command, opts)
  opts = opts or {}
  local client_id = opts.client_id
  if not client_id then
    local client = vim.lsp.get_clients()[1]
    if not client then
      vim.api.nvim_echo("No active LSP client for execute_command")
      return
    end
    client_id = client.id
  end
  vim.lsp._dispatch_command(client_id, command)
end

-- vim.lsp._cursor_word(): the keyword run (`[%w_]`) under the cursor, read from
-- the Phase-6 cursor / buffer mirror — neovim's `<cword>`, used to prefill the
-- rename prompt. Empty when the cursor isn't on a word char (ASCII-keyword
-- approximation; multibyte identifiers aren't expanded).
function vim.lsp._cursor_word()
  local pos = vim.api.nvim_win_get_cursor(0)
  local row, col = pos[1], pos[2]
  local line = (vim.api.nvim_buf_get_lines(0, row - 1, row, false))[1] or ""
  local b = col + 1 -- 1-based byte index of the char under the cursor
  if not line:sub(b, b):match("[%w_]") then return "" end
  local s, e = b, b
  while s > 1 and line:sub(s - 1, s - 1):match("[%w_]") do
    s = s - 1
  end
  while e < #line and line:sub(e + 1, e + 1):match("[%w_]") do
    e = e + 1
  end
  return line:sub(s, e)
end

-- rename(new_name): rename the symbol under the cursor. With a name, request it
-- straight away; with none (the common `vim.keymap.set('n', '<leader>rn',
-- vim.lsp.buf.rename)` bare-RHS case), prompt for it via `vim.ui.input` (Phase 8),
-- prefilled with the symbol under the cursor, and rename on confirm — matching
-- neovim. An empty / cancelled prompt does nothing.
function vim.lsp.buf.rename(new_name, _opts)
  if type(new_name) == "string" and new_name ~= "" then
    vim._lsp_buf_rename(new_name)
    return
  end
  vim.ui.input({ prompt = "New Name: ", default = vim.lsp._cursor_word() }, function(name)
    if type(name) == "string" and name ~= "" then vim._lsp_buf_rename(name) end
  end)
end

-- ----- vim.lsp.semantic_tokens: the control surface (Phase 3) ----------------
-- The projection is automatic: a server that advertises `semanticTokensProvider`
-- lights its buffers up over the treesitter floor without any call here. This
-- surface is the override — turn it off/on per buffer (`stop`/`start`), force a
-- re-request (`force_refresh`), read the tokens under a position (`get_at_pos`),
-- or gate the whole feature editor-wide (`enable`). The decode/projection live in
-- the server; these enqueue an `LspOp` it applies (the `vim.diagnostic.*` shape).
vim.lsp.semantic_tokens = vim.lsp.semantic_tokens or {}

-- The mirror the server pushes into on every `semanticTokens/full`(/delta) reply,
-- keyed by bufnr → list of `{ line, start_col, end_col, type, modifiers, client_id }`
-- (0-based, byte columns). `get_at_pos` reads it; nothing else should write it.
vim._semantic_tokens = vim._semantic_tokens or {}
function vim._set_semantic_tokens(bufnr, list) vim._semantic_tokens[bufnr or 0] = list or {} end

-- Resolve a `0`/`nil` bufnr to the current buffer (the id the mirror is keyed by),
-- matching how the diagnostics surface resolves its buffer.
local function semantic_bufnr(bufnr)
  if bufnr == nil or bufnr == 0 then return vim.api.nvim_get_current_buf() end
  return bufnr
end

-- start(bufnr, client_id, opts): (re-)enable the semantic-token projection for a
-- buffer and request a fresh token set if the cache is cold. `client_id`/`opts`
-- are accepted for neovim-signature compatibility; nxvim has one semantic cache
-- per buffer, so they don't select among clients.
function vim.lsp.semantic_tokens.start(bufnr, _client_id, _opts)
  vim._lsp_semantic_enable(semantic_bufnr(bufnr), true)
end

-- stop(bufnr, client_id): hide the buffer's semantic paint (the cache survives, so
-- a later `start` repaints without a round-trip).
function vim.lsp.semantic_tokens.stop(bufnr, _client_id)
  vim._lsp_semantic_enable(semantic_bufnr(bufnr), false)
end

-- force_refresh(bufnr): drop the cached result id and re-request the whole token
-- set, repainting from the server's fresh classification.
function vim.lsp.semantic_tokens.force_refresh(bufnr)
  vim._lsp_semantic_refresh(semantic_bufnr(bufnr))
end

-- get_at_pos(bufnr, row, col): the cached semantic tokens covering a position, as a
-- list of `{ line, start_col, end_col, type, modifiers, client_id }` (0-based, byte
-- columns; `modifiers` is both a list and a `[name]=true` set). `row`/`col` default
-- to the cursor when omitted. Reads the mirror — no request is issued.
function vim.lsp.semantic_tokens.get_at_pos(bufnr, row, col)
  bufnr = semantic_bufnr(bufnr)
  if row == nil or col == nil then
    local pos = vim.api.nvim_win_get_cursor(0)
    row, col = pos[1] - 1, pos[2]
  end
  local out = {}
  for _, t in ipairs(vim._semantic_tokens[bufnr] or {}) do
    if t.line == row and col >= t.start_col and col < t.end_col then out[#out + 1] = t end
  end
  return out
end

-- enable(enabled): nxvim's editor-wide gate for the whole feature (neovim has only
-- the per-buffer start/stop). `false` hides all semantic paint and stops refresh
-- requests; `true` (the default) restores it, re-requesting every attached buffer.
function vim.lsp.semantic_tokens.enable(enabled) vim._lsp_semantic_config(enabled ~= false) end

-- highlight_token: the per-token highlight-customization hook stays a loud gap — it
-- would put a Lua callback on the decode hot path, out of scope for Phase 3.
function vim.lsp.semantic_tokens.highlight_token()
  vim._notimpl("vim.lsp.semantic_tokens.highlight_token")
end

-- ----- vim.lsp.inlay_hint: the control surface (Phase 1) ---------------------
-- Inlay hints are the inline `: i32` type / `name:` parameter labels a server
-- injects between the buffer's glyphs. Unlike semantic tokens they are **opt-in**:
-- a buffer shows none until `enable(true)`. The decode/projection/inline render
-- live in the server; these enqueue an `LspOp` it applies, and keep a Lua mirror
-- of the per-buffer enabled state so `is_enabled` answers without a round-trip.
vim.lsp.inlay_hint = vim.lsp.inlay_hint or {}

-- The per-buffer enabled mirror, keyed by bufnr → true. Written through by
-- `enable`; read by `is_enabled`. Nothing else should write it.
vim._inlay_hint_enabled = vim._inlay_hint_enabled or {}

-- Resolve a `filter.bufnr` (or a bare bufnr / `0`/`nil`) to a concrete buffer id.
local function inlay_bufnr(filter)
  local b = type(filter) == "table" and filter.bufnr or filter
  if b == nil or b == 0 then return vim.api.nvim_get_current_buf() end
  return b
end

-- enable(enable, filter): turn inlay hints on (`enable ~= false`) or off for a
-- buffer (`filter.bufnr`, default current). Enabling requests a fresh set;
-- disabling clears it. Matches neovim's `vim.lsp.inlay_hint.enable` signature
-- (the per-client granularity of `filter` is an approximation — nxvim keeps one
-- inlay cache per buffer).
function vim.lsp.inlay_hint.enable(enable, filter)
  if enable == nil then enable = true end
  local bufnr = inlay_bufnr(filter)
  vim._inlay_hint_enabled[bufnr] = enable and true or nil
  vim._lsp_inlay_hint_enable(bufnr, enable ~= false)
end

-- is_enabled(filter): whether inlay hints are enabled for a buffer (`filter.bufnr`,
-- default current). Reads the Lua mirror — no request is issued.
function vim.lsp.inlay_hint.is_enabled(filter)
  return vim._inlay_hint_enabled[inlay_bufnr(filter)] == true
end

-- The mirror the server pushes into on every `textDocument/inlayHint` reply (and
-- after a lazy hint resolves), keyed by bufnr → list of
-- `{ line, col, label, kind, client_id }` (0-based; `col` is a byte column).
-- `get` reads it; nothing else should write it.
vim._inlay_hints = vim._inlay_hints or {}
function vim._set_inlay_hints(bufnr, list) vim._inlay_hints[bufnr or 0] = list or {} end

-- get(filter): the cached inlay hints for a buffer (`filter.bufnr`, default
-- current), optionally narrowed to `filter.range` (a `{ start = { line, character },
-- ["end"] = { line, character } }` span, 0-based — `character` compared as a byte
-- column, the mirror's convention). Returns neovim's shape — a list of
-- `{ bufnr, client_id, inlay_hint = { position = { line, character }, label, kind } }`.
-- Reads the mirror; no request is issued. (Per-part `location`/`tooltip`/`textEdits`
-- are not surfaced — nxvim renders label-only; recorded as an approximation.)
function vim.lsp.inlay_hint.get(filter)
  filter = filter or {}
  local bufnr = inlay_bufnr(filter)
  local range = filter.range
  local out = {}
  for _, h in ipairs(vim._inlay_hints[bufnr] or {}) do
    local keep = true
    if range then
      -- Inclusive line span; on the boundary lines, clip by the byte column.
      local s, e = range.start, range["end"]
      if h.line < s.line or h.line > e.line then
        keep = false
      elseif h.line == s.line and h.col < s.character then
        keep = false
      elseif h.line == e.line and h.col > e.character then
        keep = false
      end
    end
    if keep then
      out[#out + 1] = {
        bufnr = bufnr,
        client_id = h.client_id,
        inlay_hint = {
          position = { line = h.line, character = h.col },
          label = h.label,
          kind = h.kind,
        },
      }
    end
  end
  return out
end

-- Make the `vim.lsp` namespaces requirable by module name. In neovim each of these
-- is its own file (`vim/lsp/util.lua`, …) so plugins `require("vim.lsp.util")`
-- rather than reaching through the global; nxvim builds them all in this prelude,
-- so we seed `package.loaded` to return the already-built tables (cmp_luasnip's
-- `require("vim.lsp.util")` is what surfaced this). No new behavior — just the
-- module-path alias for tables that already exist.
for name, mod in pairs({
  ["vim.lsp"] = vim.lsp,
  ["vim.lsp.util"] = vim.lsp.util,
  ["vim.lsp.protocol"] = vim.lsp.protocol,
  ["vim.lsp.buf"] = vim.lsp.buf,
  ["vim.lsp.handlers"] = vim.lsp.handlers,
  ["vim.lsp.semantic_tokens"] = vim.lsp.semantic_tokens,
  ["vim.lsp.inlay_hint"] = vim.lsp.inlay_hint,
}) do
  package.loaded[name] = mod
end
