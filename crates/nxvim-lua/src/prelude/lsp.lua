-- nxvim Lua prelude — the LSP control surface (`nx.lsp`), Phase A.
-- docs/specs/2026-06-14-nx-lsp-design.md. Re-introduces the LSP *control surface*
-- in the canonical `nx.*` namespace over the intact engine: the per-`(name, root)`
-- `nxvim-lsp` manager, the `lsp/` server subtree, and the queued `LspOp` seam are
-- all unchanged — this file only builds *what the user and plugins write* and
-- drives it down the existing `nx._lsp_*` bridges (crates/nxvim-lua/src/install.rs).
--
-- Three pieces (the spec's "surface"):
--   1. an accumulating, deep-merged config registry — `nx.lsp.config` / `enable` /
--      `disable`, with the engine-side FileType -> Start dispatch (no autocmd the
--      user writes);
--   2. the flattened language verbs — `nx.lsp.definition`/`references`/`hover`/… —
--      each a thin enqueue of an `LspOp`, with the SERVER owning where the answer
--      lands (jump / loclist / float / select), so there is no result handling here;
--   3. the introspection + escape hatch — `nx.lsp.clients` / `request` / `notify`
--      and the per-client `:request`/`:notify` handles, plus the `on_attach` /
--      `on_init` / `on_exit` lifecycle hooks the engine fires back into Lua.
--
-- Nothing blocks (ADR 0002 rule 3): root resolution walks the fs seam, the rename
-- prompt is the `nx.ui.input` promise — no `getcharstr`, no `vim.wait`. A muscle-
-- memory `vim.lsp.*` alias layer (ADR 0002 §4 whitelist) sits at the bottom.
local vim = vim
nx = nx or {}
nx.lsp = nx.lsp or {}

-- ----- internal registry state -----------------------------------------------
-- The user's override layers, keyed by config name (`"*"` is the all-clients
-- base). Each `nx.lsp.config(name, opts)` call deep-merges `opts` into the layer;
-- the resolved config a server starts with is `"*"` ⊕ runtimepath base ⊕ named.
nx.lsp._config = nx.lsp._config or {}
-- name -> the cached `lsp/<name>.lua` runtimepath preset (`false` = none found),
-- so the bundled-preset read happens once per name, not per attach.
nx.lsp._base_cache = nx.lsp._base_cache or {}
-- name -> enabled?  Set by `enable`/`disable`; read by the FileType dispatcher.
nx.lsp._enabled = nx.lsp._enabled or {}
-- id -> the mirrored client handle (`nx.lsp._set_client` writes it once the server
-- finishes `initialize`). `nx.lsp.clients` / `request` / `notify` read it.
nx.lsp._clients = nx.lsp._clients or {}
-- bufnr -> { client_id = true } — which clients have attached to which buffer,
-- maintained by the LspAttach/LspDetach hooks so `nx.lsp.clients({bufnr})` filters.
nx.lsp._attached = nx.lsp._attached or {}

-- ----- small helpers ---------------------------------------------------------

-- Resolve a `0`/`nil` bufnr to the current buffer id (matching the diagnostics
-- and semantic-token surfaces).
local function cur_bufnr(bufnr)
  if bufnr == nil or bufnr == 0 then
    return nx.buf.current()
  end
  return bufnr
end

-- A usable spawn argv — a non-empty list of strings? Guards the start queue against
-- the config shapes nxvim can't spawn (an empty/nil cmd, or a builder that failed):
-- those are skipped with a loud notify rather than erroring at the Rust boundary.
local function is_argv(cmd)
  if type(cmd) ~= "table" or #cmd == 0 then
    return false
  end
  for _, a in ipairs(cmd) do
    if type(a) ~= "string" then
      return false
    end
  end
  return true
end

-- `t` if it is a non-empty table, else nil. Guards the `settings`/`init_options`/
-- `capabilities` payloads threaded to `nx._lsp_start`: an absent or empty table
-- becomes nil so the server forwards nothing, rather than an empty `{}` the
-- lua_to_json bridge would emit as `[]`.
local function nonempty(t)
  if type(t) == "table" and next(t) ~= nil then
    return t
  end
  return nil
end

-- The directory part of a path (strip the trailing `/component`); "" at the root.
local function dirname(path)
  return (path:gsub("/[^/]*$", ""))
end

-- The keyword run (`[%w_]`) under the cursor — neovim's `<cword>`, used to prefill
-- the rename prompt. Empty when the cursor isn't on a word char (ASCII-keyword
-- approximation; multibyte identifiers aren't expanded).
local function cursor_word()
  local pos = vim.api.nvim_win_get_cursor(0)
  local row, col = pos[1], pos[2]
  local line = (vim.api.nvim_buf_get_lines(0, row - 1, row, false))[1] or ""
  local b = col + 1 -- 1-based byte index of the char under the cursor
  if not line:sub(b, b):match("[%w_]") then
    return ""
  end
  local s, e = b, b
  while s > 1 and line:sub(s - 1, s - 1):match("[%w_]") do
    s = s - 1
  end
  while e < #line and line:sub(e + 1, e + 1):match("[%w_]") do
    e = e + 1
  end
  return line:sub(s, e)
end

-- Upward `root_markers` search from the buffer's file, walking the project tree
-- through the async `nx.fs` seam (local on native-bare, the daemon's `luafs_op` over
-- the wire otherwise — so this works on every front end with NO editor-thread block).
-- `nx.async` makes this an async *function* (the Lua analogue of a JS `async
-- function`): calling `find_root(bufnr, markers)` runs the body as a coroutine and
-- returns a PROMISE of the first ancestor directory holding one of `markers`, or nil
-- (the server then falls back to the file's directory). Each `nx.fs.readdir` that
-- rejects (an unreadable / non-directory ancestor) is treated as "no markers here" and
-- the walk continues upward. The caller invokes it normally and `:next`s the result.
local find_root = nx.async(function(bufnr, markers)
  local file = nx.buf.name(bufnr)
  if type(file) ~= "string" or file == "" then
    return nil
  end
  local dir = dirname(file)
  while dir and dir ~= "" do
    local present = {}
    local entries = nx.await(nx.fs.readdir(dir):catch(function()
      return {}
    end))
    for _, e in ipairs(entries) do
      present[e.name] = true
    end
    for _, m in ipairs(markers) do
      if present[m] then
        return dir
      end
    end
    local parent = dirname(dir)
    if parent == dir then
      break
    end
    dir = parent
  end
  return nil
end)

-- ----- config registry -------------------------------------------------------

-- The bundled preset for `name`: the table returned by the runtimepath
-- `lsp/<name>.lua` (nxvim's nx-native lspconfig replacement — `cmd` / `filetypes` /
-- `root_markers` shipped as data). Cached per name (`false` = no preset). A read or
-- parse failure is reported loud, never silently swallowed.
local function base_config(name)
  local cached = nx.lsp._base_cache[name]
  if cached ~= nil then
    return cached or nil
  end
  local cfg = false
  local files = nx.runtime_file("lsp/" .. name .. ".lua", false)
  local file = files and files[1]
  if file then
    local src = nx._read_file(file)
    if src == nil then
      nx.notify("nx.lsp: could not read preset " .. file, vim.log.levels.ERROR)
    else
      local chunk, perr = loadstring(src, "@" .. file)
      if not chunk then
        nx.notify("nx.lsp: parse error in " .. file .. ": " .. tostring(perr), vim.log.levels.ERROR)
      else
        local ok, ret = pcall(chunk)
        if not ok then
          nx.notify("nx.lsp: error loading " .. file .. ": " .. tostring(ret), vim.log.levels.ERROR)
        elseif type(ret) ~= "table" then
          nx.notify("nx.lsp: " .. file .. " did not return a table", vim.log.levels.ERROR)
        else
          cfg = ret
        end
      end
    end
  end
  nx.lsp._base_cache[name] = cfg
  return cfg or nil
end

-- The fully-resolved config for `name`: the `"*"` all-clients base, then the
-- runtimepath preset, then the user's accumulated override — deep-merged with the
-- rightmost winning. Maps merge recursively, lists replace, scalars overwrite
-- (exactly neovim's `tbl_deep_extend("force", …)`). Computed here, in Lua, so the
-- `LspOp::Start` seam underneath receives one already-resolved config.
local function resolve(name)
  return vim.tbl_deep_extend(
    "force",
    nx.lsp._config["*"] or {},
    base_config(name) or {},
    nx.lsp._config[name] or {}
  )
end

-- nx.lsp.config(name, opts): accumulate `opts` into `name`'s override layer
-- (deep-merged over any prior call — configs compose across files and plugins).
-- `"*"` is the all-clients base inherited by every server. Function-call form
-- only: there is no `nx.lsp.config[name] = {…}` table-assignment sugar.
function nx.lsp.config(name, opts)
  if type(name) ~= "string" then
    error("nx.lsp.config: name must be a string", 2)
  end
  if opts ~= nil and type(opts) ~= "table" then
    error("nx.lsp.config: opts must be a table", 2)
  end
  local prev = nx.lsp._config[name] or {}
  nx.lsp._config[name] = vim.tbl_deep_extend("force", prev, opts or {})
end

-- ----- enable / the engine-side FileType -> Start dispatch --------------------

-- Resolve `cfg.cmd` to an argv. A function `cmd` is neovim's `cmd(dispatchers,
-- config)` builder (the many `node_modules/.bin` resolvers); nxvim does its own
-- stdio spawn, so the dispatchers are a stub and `vim.lsp.rpc.start` (below)
-- returns the argv it is handed. A throwing builder yields `nil, reason`.
local function resolve_cmd(cfg, root)
  local cmd = cfg.cmd
  if type(cmd) == "function" then
    local config = {}
    for k, v in pairs(cfg) do
      config[k] = v
    end
    config.root_dir = root
    local ok, result = pcall(cmd, {}, config)
    if not ok then
      return nil, "cmd builder errored: " .. tostring(result)
    end
    cmd = result
  end
  return cmd
end

-- Queue a start for `bufnr` from a resolved config (root already computed). A cmd
-- that isn't a spawnable argv is reported loud (the server enabled but unspawnable
-- is visible, never a silent no-op) and skipped — it never errors the whole enable.
local function start_resolved(name, cfg, bufnr, ft, root)
  local cmd, reason = resolve_cmd(cfg, root)
  if not is_argv(cmd) then
    nx.notify(
      "nx.lsp: not starting '" .. name .. "': " .. (reason or "cmd is not a spawnable argv"),
      vim.log.levels.WARN
    )
    return
  end
  nx._lsp_start(
    name,
    cmd,
    root,
    ft or "",
    bufnr,
    nonempty(cfg.init_options),
    nonempty(cfg.settings),
    nonempty(cfg.capabilities)
  )
end

-- Resolve `cfg`'s root and start the server for `bufnr`. `root_dir` may be a
-- string, a `function(bufnr, done)` (the async escape hatch — it calls `done(dir)`,
-- or never, to decline a buffer), or absent with `root_markers` driving the upward
-- fs-seam search. With none of those, the root is nil (the server uses the file's
-- directory).
local function start_for(name, cfg, bufnr, ft)
  local rd = cfg.root_dir
  if type(rd) == "function" then
    rd(bufnr, function(root)
      start_resolved(name, cfg, bufnr, ft, root)
    end)
  elseif type(rd) == "string" then
    start_resolved(name, cfg, bufnr, ft, rd)
  elseif cfg.root_markers then
    find_root(bufnr, cfg.root_markers):next(function(root)
      start_resolved(name, cfg, bufnr, ft, root)
    end)
  else
    start_resolved(name, cfg, bufnr, ft, nil)
  end
end

-- The shared FileType dispatcher body: for every enabled config whose resolved
-- `filetypes` includes `ft`, resolve the root and start the server for `bufnr`.
-- This is the engine's declarative FileType -> start step (neovim wires an internal
-- autocmd; nxvim keeps it here so it behaves identically under the wasm edit-host).
function nx.lsp._on_filetype(bufnr, ft)
  if not ft or ft == "" then
    return
  end
  for name, on in pairs(nx.lsp._enabled) do
    if on then
      local cfg = resolve(name)
      if cfg.filetypes and vim.tbl_contains(cfg.filetypes, ft) then
        start_for(name, cfg, bufnr, ft)
      end
    end
  end
end

-- The built-in LSP keymaps. They are installed *buffer-local* when a server first
-- attaches to a buffer (and removed when the last one detaches), not as global Rust
-- native defaults — so a buffer no server serves keeps `gd`/`K`/… as their core
-- meanings (`gd` stays the `g`-motion grammar, never a dead "no client" map), and a
-- which-key never lists them without a server. `default = true` puts each at the
-- overridable rung, so a user's own map for the same key wins. The RHS reads
-- `nx.lsp.*` at call time (defined later in this file), so the builder is a function.
local function lsp_default_keymaps()
  return {
    { "n", "gd", nx.lsp.definition, "Go to definition" },
    { "n", "gD", nx.lsp.declaration, "Go to declaration" },
    { "n", "gr", nx.lsp.references, "Find references" },
    { "n", "K", nx.lsp.hover, "Hover documentation" },
    { "i", "<C-k>", nx.lsp.signature_help, "Signature help" },
  }
end

local function install_lsp_keymaps(buf)
  for _, m in ipairs(lsp_default_keymaps()) do
    nx.keymap.set(m[1], m[2], m[3], { buffer = buf, default = true, desc = m[4] })
  end
end

local function remove_lsp_keymaps(buf)
  for _, m in ipairs(lsp_default_keymaps()) do
    nx.keymap.del(m[1], m[2], { buffer = buf })
  end
end

-- Install the single shared FileType / LspAttach / LspDetach autocmds that drive
-- every enabled config (idempotent — `enable` may be called many times).
local function ensure_dispatcher()
  if nx.lsp._dispatcher_installed then
    return
  end
  nx.lsp._dispatcher_installed = true
  local group = nx.augroup.create("nxvim.lsp.enable", { clear = true })
  -- A matching buffer's filetype settles -> start its enabled servers.
  nx.autocmd.create("FileType", {
    group = group,
    callback = function(args)
      nx.lsp._on_filetype(args.buf, args.match)
    end,
  })
  -- The server bound to a buffer finished its first sync -> the engine fires
  -- LspAttach with `data.client_id`; record the attachment and run the config's
  -- `on_attach(client, bufnr)` (the call site that sets buffer-local LSP keymaps).
  nx.autocmd.create("LspAttach", {
    group = group,
    callback = function(args)
      local id = args.data and args.data.client_id
      local client = id and nx.lsp._clients[id]
      if not client then
        return
      end
      local buf = args.buf
      local attached = nx.lsp._attached[buf]
      -- The first server to serve this buffer installs the built-in keymaps; later
      -- clients on the same buffer reuse them (idempotent — install once).
      local first = not attached or next(attached) == nil
      nx.lsp._attached[buf] = attached or {}
      nx.lsp._attached[buf][id] = true
      if first then
        install_lsp_keymaps(buf)
      end
      local cfg = resolve(client.name)
      if type(cfg.on_attach) == "function" then
        cfg.on_attach(client, buf)
      end
    end,
  })
  -- The inverse: a buffer detaches from a client -> forget the attachment.
  nx.autocmd.create("LspDetach", {
    group = group,
    callback = function(args)
      local id = args.data and args.data.client_id
      local set = nx.lsp._attached[args.buf]
      if id and set then
        set[id] = nil
        -- The last server left this buffer: drop the built-in keymaps so they don't
        -- linger (and fire "no client") on a buffer no server serves any more.
        if next(set) == nil then
          remove_lsp_keymaps(args.buf)
        end
      end
    end,
  })
end

-- nx.lsp.enable(names): mark configs for auto-activation on current and future
-- buffers and install the dispatcher. `names` is a string or a list. `"*"` is the
-- base layer, not an activatable server, so it is rejected here.
function nx.lsp.enable(names)
  if type(names) == "string" then
    names = { names }
  end
  for _, n in ipairs(names) do
    if n == "*" then
      error("nx.lsp.enable: '*' is the base layer, not a server name", 2)
    end
    nx.lsp._enabled[n] = true
  end
  ensure_dispatcher()
  -- The current buffer's FileType has already fired, so the dispatcher just
  -- installed won't catch it; process it on the spot (a start is idempotent
  -- server-side, so overlapping the startup FileType from init.lua is harmless).
  local cur = nx._cur_buf
  if cur and cur.filetype and cur.filetype ~= "" then
    nx.lsp._on_filetype(cur.bufnr, cur.filetype)
  end
end

-- nx.lsp.disable(names): the inverse of `enable` — future buffers won't start the
-- named servers (already-running servers keep serving until their buffers close).
function nx.lsp.disable(names)
  if type(names) == "string" then
    names = { names }
  end
  for _, n in ipairs(names) do
    nx.lsp._enabled[n] = nil
  end
end

-- nx.lsp.start(cfg, opts): the low-level, un-merged direct start (the raw
-- `LspOp::Start`) for advanced/manual use — bypasses the registry. `cfg` is a
-- resolved config (`{ name, cmd, root_dir, filetypes, settings, … }`); `opts.bufnr`
-- is the buffer to attach (default the current one), `opts.filetype` its languageId.
function nx.lsp.start(cfg, opts)
  opts = opts or {}
  local bufnr = opts.bufnr or cur_bufnr(0)
  local ft = opts.filetype or (nx._cur_buf and nx._cur_buf.filetype) or ""
  start_resolved(cfg.name or "?", cfg, bufnr, ft, cfg.root_dir)
end

-- ----- language verbs — thin enqueues; the server owns the result surface -----
-- Each verb queues an `LspOp` the server drains on the same input tick (reading the
-- cursor where the key fired) and routes into its existing surface: a single
-- location jumps, many open the loclist; hover/signature open the cursor float;
-- code actions open the select menu; format/rename apply edits. There is no reply
-- handling in Lua. The verbs are *bare* (no implicit args) so
-- `nx.keymap.set("n", "gd", nx.lsp.definition)` works. `kind` ints mirror
-- `LspReqKind::as_u16` (crates/nxvim-server/src/lsp/mod.rs) — keep the two in step.
function nx.lsp.definition()
  nx._lsp_buf(0)
end
function nx.lsp.declaration()
  nx._lsp_buf(1)
end
function nx.lsp.type_definition()
  nx._lsp_buf(2)
end
function nx.lsp.implementation()
  nx._lsp_buf(3)
end
function nx.lsp.references()
  nx._lsp_buf(4)
end
function nx.lsp.hover()
  nx._lsp_buf(5)
end
function nx.lsp.signature_help()
  nx._lsp_buf(6)
end
function nx.lsp.format()
  nx._lsp_buf_format()
end
function nx.lsp.code_action()
  nx._lsp_buf_code_action()
end

-- nx.lsp.document_symbol(): the symbols defined in the current document, opened in
-- nx.picker (kind 16 mirrors LspReqKind::DocumentSymbol::as_u16).
function nx.lsp.document_symbol()
  nx._lsp_buf(16)
end

-- nx.lsp.workspace_symbol(query): symbols across the workspace matching `query`,
-- opened in nx.picker. With no query, prompt for one via nx.ui.input (non-blocking)
-- — an empty/cancelled prompt does nothing.
function nx.lsp.workspace_symbol(query)
  if type(query) == "string" then
    nx._lsp_workspace_symbol(query)
    return
  end
  nx.ui.input({ prompt = "Workspace symbol: " }):next(function(q)
    if type(q) == "string" and q ~= "" then
      nx._lsp_workspace_symbol(q)
    end
  end)
end

-- nx.lsp.rename(new_name): rename the symbol under the cursor. With a name, request
-- it straight away; with none (the bare `nx.keymap.set("n", "<leader>rn",
-- nx.lsp.rename)` case), prompt for it via `nx.ui.input` (non-blocking promise),
-- prefilled with the symbol under the cursor, and rename on confirm. An empty /
-- cancelled prompt does nothing.
function nx.lsp.rename(new_name)
  if type(new_name) == "string" and new_name ~= "" then
    nx._lsp_buf_rename(new_name)
    return
  end
  nx.ui.input({ prompt = "New Name: ", default = cursor_word() }):next(function(name)
    if type(name) == "string" and name ~= "" then
      nx._lsp_buf_rename(name)
    end
  end)
end

-- ----- client handles, introspection & escape hatch --------------------------

-- Build the snapshot handle mirrored into `nx.lsp._clients[id]`. It carries the
-- server's resolved capabilities and the generic `:request` / `:notify` escape
-- hatch (engine Decision 3: callback-shaped, a generation token, stale replies
-- dropped server-side). `on_attach` / `on_init` receive this same handle.
local function make_client(id, name, capabilities)
  local client = { id = id, name = name, server_capabilities = capabilities or {} }
  -- client:request(method, params, handler): issue a generic LSP request; the
  -- reply runs `handler(err, result)` off-tick (err a message string on failure,
  -- result the server's value on success — exactly one set). An unimplemented or
  -- uncapable method fails loud through `err`, never a silent no-op.
  function client:request(method, params, handler)
    local cb_id = nx._next_cb_id()
    nx._cb_fns[cb_id] = handler or function() end
    nx._lsp_client_request(self.id, method, params, cb_id)
  end
  -- client:notify(method, params): fire-and-forget a generic LSP notification.
  function client:notify(method, params)
    nx._lsp_client_notify(self.id, method, params)
  end
  return client
end

-- nx.lsp.clients(filter): a snapshot list of active clients, narrowable by
-- `filter.bufnr` (the clients attached to that buffer; `0`/nil = current) and/or
-- `filter.name` (the config name). Reads the mirror — no request is issued.
function nx.lsp.clients(filter)
  filter = filter or {}
  local out = {}
  if filter.bufnr ~= nil then
    for id in pairs(nx.lsp._attached[cur_bufnr(filter.bufnr)] or {}) do
      local c = nx.lsp._clients[id]
      if c and (not filter.name or c.name == filter.name) then
        out[#out + 1] = c
      end
    end
  else
    for _, c in pairs(nx.lsp._clients) do
      if not filter.name or c.name == filter.name then
        out[#out + 1] = c
      end
    end
  end
  return out
end

-- nx.lsp.request(method, params, handler, bufnr): sugar resolving the buffer's
-- primary client and issuing `client:request`. No attached client fails loud.
function nx.lsp.request(method, params, handler, bufnr)
  local client = nx.lsp.clients({ bufnr = bufnr or 0 })[1]
  if not client then
    nx.notify("nx.lsp.request: no LSP client attached to the buffer", vim.log.levels.ERROR)
    return
  end
  client:request(method, params, handler)
end

-- nx.lsp.notify(method, params, bufnr): the fire-and-forget sibling of `request`.
function nx.lsp.notify(method, params, bufnr)
  local client = nx.lsp.clients({ bufnr = bufnr or 0 })[1]
  if not client then
    nx.notify("nx.lsp.notify: no LSP client attached to the buffer", vim.log.levels.ERROR)
    return
  end
  client:notify(method, params)
end

-- ----- engine -> Lua mirror hooks (called by nxvim-server) -------------------
-- The server drives these once per client lifecycle event (runtime.rs:
-- set_lsp_client / run_lsp_on_init / run_lsp_on_exit / remove_lsp_client). They
-- keep `nx.lsp._clients` in sync and run the config's lifecycle hooks.

-- A server finished `initialize`: mirror its handle with the translated provider
-- capabilities (the `*Provider` booleans neovim configs probe).
function nx.lsp._set_client(id, name, capabilities)
  nx.lsp._clients[id] = make_client(id, name, capabilities)
end

-- A server exited: forget its handle and drop it from every buffer's attach set.
function nx.lsp._remove_client(id)
  nx.lsp._clients[id] = nil
  for _, set in pairs(nx.lsp._attached) do
    set[id] = nil
  end
end

-- Right after the client is mirrored: run the config's `on_init(client, result)`
-- with the raw `initialize` result, so the hook can read `result.capabilities` /
-- `result.offsetEncoding` and tweak the client.
function nx.lsp._run_on_init(id, result)
  local client = nx.lsp._clients[id]
  if not client then
    return
  end
  local cfg = resolve(client.name)
  if type(cfg.on_init) == "function" then
    cfg.on_init(client, result)
  end
end

-- The server is exiting (handle still in `_clients`): run `on_exit(code, signal,
-- client)` before `_remove_client` clears it.
function nx.lsp._run_on_exit(id, code, signal)
  local client = nx.lsp._clients[id]
  if not client then
    return
  end
  local cfg = resolve(client.name)
  if type(cfg.on_exit) == "function" then
    cfg.on_exit(code, signal, client)
  end
end

-- ----- semantic tokens & inlay hints — buffer state, read mirrors ------------
-- Direct application of the treesitter precedent (design §"buffer nouns"): the
-- projection lives entirely in the engine (lsp/semantic.rs, lsp/inlay.rs) — these
-- surfaces only (1) flip the per-buffer/editor toggle ops the engine reads and
-- (2) expose the decoded read mirrors the server pushes each reply, so a plugin
-- can answer "what token / hint is here?" synchronously without a round-trip.

-- The read mirrors, keyed by bufnr. The server hard-calls these once per
-- `semanticTokens`/`inlayHint` reply (runtime.rs set_semantic_tokens /
-- set_inlay_hints) — they were dangling before this surface, so the push silently
-- errored and the getters had nothing to read.
nx._semantic_tokens = nx._semantic_tokens or {}
function nx._set_semantic_tokens(bufnr, list)
  nx._semantic_tokens[bufnr or 0] = list or {}
end
nx._inlay_hints = nx._inlay_hints or {}
function nx._set_inlay_hints(bufnr, list)
  nx._inlay_hints[bufnr or 0] = list or {}
end

-- Per-buffer enabled intent, so `is_enabled` / a noun read answers without a
-- server round-trip. Semantic tokens default on (the engine paints once a server
-- with the capability attaches); inlay hints default off (opt-in, like neovim).
nx.lsp._semantic_on = nx.lsp._semantic_on or {}
nx.lsp._inlay_on = nx.lsp._inlay_on or {}

-- nx.lsp.semantic_tokens.* — the per-buffer projection (start/stop), the
-- editor-wide gate (enable), a manual refresh, and the synchronous read getter.
nx.lsp.semantic_tokens = nx.lsp.semantic_tokens or {}

-- start/stop the per-buffer semantic-token paint (neovim's start/stop verbs).
function nx.lsp.semantic_tokens.start(bufnr)
  bufnr = cur_bufnr(bufnr)
  nx.lsp._semantic_on[bufnr] = true
  nx._lsp_semantic_enable(bufnr, true)
end
function nx.lsp.semantic_tokens.stop(bufnr)
  bufnr = cur_bufnr(bufnr)
  nx.lsp._semantic_on[bufnr] = false
  nx._lsp_semantic_enable(bufnr, false)
end

-- nxvim's editor-wide gate (default on) — neovim has only the per-buffer verbs.
-- Off ⇒ no semantic paint anywhere; flipping back on re-requests every buffer.
function nx.lsp.semantic_tokens.enable(enabled)
  if enabled == nil then
    enabled = true
  end
  nx._lsp_semantic_config(enabled and true or false)
end

-- Drop the cached result_id and re-request the whole token set (neovim's
-- force_refresh) — the one operation with no readable state to model as a noun.
function nx.lsp.semantic_tokens.force_refresh(bufnr)
  nx._lsp_semantic_refresh(cur_bufnr(bufnr))
end

-- get_at_pos(bufnr, row, col): the decoded tokens covering the 0-based (row, col)
-- (neovim's `vim.lsp.semantic_tokens.get_at_pos`). `col` is a 0-based byte column;
-- a token covers `[start_col, end_col)`. Returns a list (possibly empty).
function nx.lsp.semantic_tokens.get_at_pos(bufnr, row, col)
  local toks = nx._semantic_tokens[cur_bufnr(bufnr)] or {}
  local out = {}
  for _, t in ipairs(toks) do
    if t.line == row and col >= t.start_col and col < t.end_col then
      out[#out + 1] = t
    end
  end
  return out
end

-- nx.lsp.inlay_hint.* — the per-buffer inline hints (off by default), the
-- synchronous read getter, and the enabled-state probe.
nx.lsp.inlay_hint = nx.lsp.inlay_hint or {}

-- enable(enable?, filter?): flip the per-buffer inlay-hint paint. `enable`
-- defaults to true; `filter.bufnr` (0/nil = current) targets the buffer
-- (neovim's modern `vim.lsp.inlay_hint.enable(enable, { bufnr })`).
function nx.lsp.inlay_hint.enable(enable, filter)
  if enable == nil then
    enable = true
  end
  local bufnr = cur_bufnr(filter and filter.bufnr)
  enable = enable and true or false
  nx.lsp._inlay_on[bufnr] = enable
  nx._lsp_inlay_hint_enable(bufnr, enable)
end

-- is_enabled(filter?): whether inlay hints are on for the buffer.
function nx.lsp.inlay_hint.is_enabled(filter)
  local bufnr = cur_bufnr(filter and filter.bufnr)
  return nx.lsp._inlay_on[bufnr] == true
end

-- get(filter?): the decoded inlay hints in a buffer (`filter.bufnr`, 0/nil =
-- current), each `{ bufnr, client_id, inlay_hint = <decoded entry> }` to match
-- neovim's shape. `filter.range` ({ start_line, end_line }, 0-based inclusive)
-- narrows by line. Reads the mirror — no request is issued.
function nx.lsp.inlay_hint.get(filter)
  filter = filter or {}
  local bufnr = cur_bufnr(filter.bufnr)
  local range = filter.range
  local out = {}
  for _, h in ipairs(nx._inlay_hints[bufnr] or {}) do
    if not range or (h.line >= range.start_line and h.line <= range.end_line) then
      out[#out + 1] = { bufnr = bufnr, client_id = h.client_id, inlay_hint = h }
    end
  end
  return out
end

-- ----- locations → nx.picker (design principle 4: dogfood the shared engine) --
-- A goto-family reply with >1 hit, `references`, and document/workspace symbols
-- all resolve to a location list the server hands here. Rather than a server-owned
-- loclist, the result flows into nx.picker with the built-in "location" preview
-- (scroll + range-highlight the match), confirm jumps via nx.picker.edit. A single
-- goto hit never reaches here — the server jumps straight to it.
nx.lsp._location_items = nx.lsp._location_items or {}

-- Register the shared `lsp_locations` source once (lazily, so picker.lua's load
-- order relative to this chunk doesn't matter). Its `items` replays the list the
-- server last stashed; `confirm` opens the chosen location.
local function ensure_location_source()
  if nx.lsp._location_source then
    return
  end
  nx.lsp._location_source = true
  nx.picker.source({
    name = "lsp_locations",
    preview = "location",
    items = function(ctx)
      for _, it in ipairs(nx.lsp._location_items) do
        ctx.push(it)
      end
    end,
    confirm = function(item)
      nx.picker.edit(item)
    end,
  })
end

-- nx.lsp._show_locations(items): open the picker over a server-resolved location
-- list. Each item is `{ text, path, row (1-based), col (1-based) }`. Called by the
-- server (runtime.rs `show_lsp_locations`) when a reply carries more than one hit.
function nx.lsp._show_locations(items)
  nx.lsp._location_items = items or {}
  ensure_location_source()
  nx.picker.open("lsp_locations")
end

-- ----- vim.* muscle-memory aliases (ADR 0002 §4 whitelist) -------------------
-- The bounded neovim-shaped surface, routed onto the nx verbs above. `vim.lsp.buf`
-- is the `.buf`-namespaced spelling muscle memory reaches for; `vim.lsp.config`
-- here is the callable override-merge form (not neovim's indexable table).
vim.lsp = vim.lsp or {}
vim.lsp.config = function(name, opts)
  return nx.lsp.config(name, opts)
end
vim.lsp.enable = function(names)
  return nx.lsp.enable(names)
end
vim.lsp.start = nx.lsp.start
vim.lsp.buf = vim.lsp.buf or {}
vim.lsp.buf.definition = nx.lsp.definition
vim.lsp.buf.declaration = nx.lsp.declaration
vim.lsp.buf.type_definition = nx.lsp.type_definition
vim.lsp.buf.implementation = nx.lsp.implementation
vim.lsp.buf.references = nx.lsp.references
vim.lsp.buf.hover = nx.lsp.hover
vim.lsp.buf.signature_help = nx.lsp.signature_help
vim.lsp.buf.format = function(_opts)
  return nx.lsp.format()
end
vim.lsp.buf.code_action = function(_opts)
  return nx.lsp.code_action()
end
vim.lsp.buf.rename = function(name, _opts)
  return nx.lsp.rename(name)
end
vim.lsp.buf.document_symbol = nx.lsp.document_symbol
vim.lsp.buf.workspace_symbol = function(query)
  return nx.lsp.workspace_symbol(query)
end
vim.lsp.get_clients = function(filter)
  return nx.lsp.clients(filter)
end
vim.lsp.get_client_by_id = function(id)
  return nx.lsp._clients[id]
end
-- Semantic tokens & inlay hints keep neovim's table shape (start/stop/get_at_pos,
-- enable/is_enabled/get) — the same tables, so a config written either way agrees.
vim.lsp.semantic_tokens = nx.lsp.semantic_tokens
vim.lsp.inlay_hint = nx.lsp.inlay_hint
