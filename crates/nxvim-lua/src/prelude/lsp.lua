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

-- Upward `root_markers` search from the buffer's file, walking the project tree
-- through the async `nx.fs` seam (local on native-bare, the daemon's `luafs_op` over
-- the wire otherwise — so this works on every front end with NO editor-thread block).
-- `nx.async` makes this an async *function* (the Lua analogue of a JS
-- `async function`): calling `find_root(bufnr, markers)` runs the body as a coroutine and
-- returns a PROMISE of the first ancestor directory holding one of `markers`, or nil
-- (the server then falls back to the file's directory). Each `nx.fs.readdir` that
-- rejects (an unreadable / non-directory ancestor) is treated as "no markers here" and
-- the walk continues upward. The caller invokes it normally and `:next`s the result.
local find_root = nx.async(function(bufnr, markers)
  local file = nx.buf.name(bufnr)
  if type(file) ~= "string" or file == "" then
    return nil
  end
  for dir in nx.utils.ancestors(file) do
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

-- `nx.lsp.config(name, opts)`: accumulate `opts` into `name`'s override layer
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

-- Resolve `cfg.cmd` to an argv. A function `cmd` is neovim's `cmd(dispatchers, config)`
-- builder (the many `node_modules/.bin` resolvers); nxvim does its own
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

-- `nx.lsp.enable(names)`: mark configs for auto-activation on current and future
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

-- `nx.lsp.disable(names)`: the inverse of `enable` — future buffers won't start the
-- named servers (already-running servers keep serving until their buffers close).
function nx.lsp.disable(names)
  if type(names) == "string" then
    names = { names }
  end
  for _, n in ipairs(names) do
    nx.lsp._enabled[n] = nil
  end
end

-- `nx.lsp.restart(name)`: tear down and respawn every running server with config
-- `name`, re-starting it from the config in force NOW. A server that reads its whole
-- configuration only at startup (efm-langserver's `languages` map is the canonical
-- case) does not see a `nx.lsp.config` change applied to an already-running instance;
-- restarting it makes the grown config take effect. Each buffer bound to the server
-- re-attaches to the fresh process. A no-op when nothing named `name` is running.
function nx.lsp.restart(name)
  if type(name) ~= "string" or name == "" then
    error("nx.lsp.restart: name must be a non-empty string", 2)
  end
  -- Pass the config in force NOW (init_options/settings/capabilities), resolved from
  -- the registry, so the respawn applies a config changed since the server started —
  -- without depending on an async FileType/root-resolution having refreshed the
  -- server-side spawn cache first.
  local cfg = resolve(name)
  nx._lsp_restart(
    name,
    nonempty(cfg.init_options),
    nonempty(cfg.settings),
    nonempty(cfg.capabilities)
  )
end

-- `nx.lsp.start(cfg, opts)`: the low-level, un-merged direct start (the raw
-- `LspOp::Start`) for advanced/manual use — bypasses the registry. `cfg` is a
-- resolved config (`{ name, cmd, root_dir, filetypes, settings, … }`); `opts.bufnr`
-- is the buffer to attach (default the current one), `opts.filetype` its languageId.
function nx.lsp.start(cfg, opts)
  opts = opts or {}
  local bufnr = opts.bufnr or cur_bufnr(0)
  local ft = opts.filetype or (nx._cur_buf and nx._cur_buf.filetype) or ""
  start_resolved(cfg.name or "?", cfg, bufnr, ft, cfg.root_dir)
end

-- ----- language verbs — async; return a promise, the server owns the surface ----
-- Each verb queues an `LspOp` the server drains on the same input tick (reading the
-- cursor where the key fired) and routes into its existing surface: a single
-- location jumps, many open the picker; hover/signature open the cursor float;
-- format/rename apply edits. Each returns an `nx.promise` that **resolves** when the
-- round-trip completes and its effect is applied/presented, so actions can be run in
-- sequence:
--
-- ```lua
-- nx.lsp.format():next(function()
--   return nx.lsp.rename("Foo")   -- runs only after format's edits land
-- end)
-- nx.lsp.references():next(function(items) --[[ items = { {text,path,row,col}, … } ]] end)
-- ```
--
-- The promise is **resolve-only** — it never rejects, so a bare keymap use
-- (`nx.keymap.set("n", "gd", nx.lsp.definition)`, the common case) can't raise an
-- unhandled-rejection warning. A benign no-op (no server attached, the request
-- superseded by a newer one, the cursor moving / buffer changing before the reply, an
-- empty result, a cancelled prompt) resolves `nil`. Navigation/symbol verbs resolve
-- with the `{ text, path, row, col }` item list (a 1-element list for a single goto
-- jump); `hover`/`signature_help` with the shown text; `format`/`rename` with `nil`.
-- `kind` ints mirror `LspReqKind::as_u16` (crates/nxvim-server/src/lsp/mod.rs) — keep
-- the two in step.
--
-- Build the promise a verb returns: `issue(cb_id)` queues the op with the callback id;
-- the server settles it by running `nx._cb_fns[cb_id](nil, result)` once the effect is
-- applied. Resolve-only (the `err` arg is always nil), matching the contract above.
local function lsp_promise(issue)
  return nx.promise.new(function(fulfil)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(_err, result)
      fulfil(result)
    end
    issue(id)
  end)
end

function nx.lsp.definition()
  return lsp_promise(function(id)
    nx._lsp_buf(0, id)
  end)
end
function nx.lsp.declaration()
  return lsp_promise(function(id)
    nx._lsp_buf(1, id)
  end)
end
function nx.lsp.type_definition()
  return lsp_promise(function(id)
    nx._lsp_buf(2, id)
  end)
end
function nx.lsp.implementation()
  return lsp_promise(function(id)
    nx._lsp_buf(3, id)
  end)
end
function nx.lsp.references()
  return lsp_promise(function(id)
    nx._lsp_buf(4, id)
  end)
end
function nx.lsp.hover()
  return lsp_promise(function(id)
    nx._lsp_buf(5, id)
  end)
end
function nx.lsp.signature_help()
  return lsp_promise(function(id)
    nx._lsp_buf(6, id)
  end)
end

-- `nx.lsp.signature_help_autotrigger(enable)`: opt into auto-showing signature help as
-- you type a call (after `(`, refreshed at each `,`), instead of only on `<C-k>`. It is
-- driven by the *server's* advertised `signatureHelpProvider.triggerCharacters`, so it
-- only fires for servers that offer signature help. `enable` defaults to true; pass
-- false to turn it back off. Off unless a config opts in.
function nx.lsp.signature_help_autotrigger(enable)
  nx._signature_autotrigger(enable ~= false)
end
-- `nx.lsp.format([opts])`: format the buffer, resolving `nil` once the edits apply.
--
-- ```lua
-- opts.name   format with the attached server of this config name, instead of the
--             first one advertising `documentFormatting`. Meaningful when a buffer
--             carries several servers — `pyright` + `ruff`, `ts_ls` + `eslint` —
--             where the default pick is not necessarily the formatter you want.
-- ```
--
-- A `name` that is not attached to the buffer reports so on the message line and
-- resolves `nil`; it never falls back to formatting with a different server, since
-- silently using the wrong formatter is the failure the option exists to prevent.
-- Any other key is rejected loudly (as in `nx.lsp.code_action`).
function nx.lsp.format(opts)
  local name
  if opts ~= nil then
    if type(opts) ~= "table" then
      error("nx.lsp.format: opts must be a table, got " .. type(opts), 2)
    end
    for k in pairs(opts) do
      if k ~= "name" then
        error("nx.lsp.format: unsupported option '" .. tostring(k) .. "'", 2)
      end
    end
    if opts.name ~= nil and type(opts.name) ~= "string" then
      error("nx.lsp.format: opts.name must be a server-name string", 2)
    end
    name = opts.name
  end
  return lsp_promise(function(id)
    nx._lsp_buf_format(id, name)
  end)
end
-- `nx.lsp.code_action(opts)`: run a code action at the cursor. Bare, it is
-- interactive — the reply only *opens the chooser menu*, and the returned promise
-- resolves once you pick an action and its edit applies (through a
-- `codeAction/resolve` round-trip if the action is lazy), or `nil` if you cancel the
-- chooser (Esc) — so you can, e.g., organize imports then format:
--
-- ```lua
-- nx.lsp.code_action():next(function() return nx.lsp.format() end)
-- ```
--
-- `opts` narrows it to a specific action, which is what makes it usable
-- **non-interactively** (a format-on-save chain, say):
--
-- ```lua
-- nx.lsp.code_action({ context = { only = { "source.fixAll" } }, apply = true })
-- ```
--
-- A code action is asked about a **range**, and the refactor kinds
-- (`refactor.extract`, `refactor.inline`) are exactly the ones a server offers only
-- over a non-empty one. Called with a live Visual / Select selection — from a
-- `"v"`-mode keymap, say — the selection *is* the range (and is consumed, like any
-- `:` command acting on a selection); `opts.range` states one explicitly; with
-- neither, the request is a point at the cursor. `:'<,'>LspCodeAction` is the
-- ex-command form, scoped to the addressed **whole lines**.
--
-- ```lua
-- nx.keymap.set({ "n", "v" }, "<leader>ca", function() nx.lsp.code_action() end)
-- ```
--
-- ```
-- context.only   list of code-action kinds to ask for; sent as the request's
--                `context.only` AND re-applied to the reply (honoring it is a
--                protocol "should", so a server that ignores it can't turn a
--                one-shot into a chooser). Matching follows the LSP kind
--                hierarchy: `"source.fixAll"` matches the kind `source.fixAll`
--                and `source.fixAll.ruff`. An action with no kind never matches.
-- apply          when exactly ONE action survives the filter, apply it directly
--                with no chooser. Two or more still open the chooser (there is a
--                real choice to make); none echoes "No code actions available"
--                and the promise resolves `nil`.
-- range          the range to ask about, stated outright:
--                { start_row = 0, start_col = 0, end_row = 2, end_col = 0 }
--                0-based rows, 0-based BYTE columns, end-EXCLUSIVE (the
--                `nx.win.select_range` convention). All four fields are
--                required, and it wins over both a live selection and the
--                cursor — the non-interactive way to act on a computed span.
-- ```
--
-- Anything else in `opts` (neovim's `filter`, `context.diagnostics`,
-- `context.triggerKind`) is **rejected loudly** rather than silently ignored — nxvim
-- doesn't model it yet, and a quietly-dropped filter would silently do the wrong thing.
function nx.lsp.code_action(opts)
  local only, apply, range = {}, false, nil
  if opts ~= nil then
    if type(opts) ~= "table" then
      error("nx.lsp.code_action: opts must be a table, got " .. type(opts), 2)
    end
    for k in pairs(opts) do
      if k ~= "context" and k ~= "apply" and k ~= "range" then
        error("nx.lsp.code_action: unsupported option '" .. tostring(k) .. "'", 2)
      end
    end
    if opts.range ~= nil then
      local r = opts.range
      local shape = "opts.range must be a table "
        .. "{ start_row =, start_col =, end_row =, end_col = } of non-negative integers"
      if type(r) ~= "table" then
        error("nx.lsp.code_action: " .. shape .. ", got " .. type(r), 2)
      end
      range = {}
      for i, field in ipairs({ "start_row", "start_col", "end_row", "end_col" }) do
        local v = r[field]
        if type(v) ~= "number" or v < 0 or v % 1 ~= 0 then
          error(
            "nx.lsp.code_action: " .. shape .. " ('" .. field .. "' is " .. tostring(v) .. ")",
            2
          )
        end
        range[i] = v
      end
    end
    if opts.apply ~= nil then
      if type(opts.apply) ~= "boolean" then
        error("nx.lsp.code_action: opts.apply must be a boolean", 2)
      end
      apply = opts.apply
    end
    local ctx = opts.context
    if ctx ~= nil then
      if type(ctx) ~= "table" then
        error("nx.lsp.code_action: opts.context must be a table", 2)
      end
      for k in pairs(ctx) do
        if k ~= "only" then
          error("nx.lsp.code_action: unsupported option 'context." .. tostring(k) .. "'", 2)
        end
      end
      if ctx.only ~= nil then
        if type(ctx.only) ~= "table" then
          error("nx.lsp.code_action: opts.context.only must be a list of kind strings", 2)
        end
        for _, kind in ipairs(ctx.only) do
          if type(kind) ~= "string" then
            error("nx.lsp.code_action: opts.context.only must be a list of kind strings", 2)
          end
          only[#only + 1] = kind
        end
      end
    end
  end
  return lsp_promise(function(id)
    nx._lsp_buf_code_action(id, only, apply, range)
  end)
end

-- `nx.lsp.document_symbol()`: the symbols defined in the current document, opened in
-- `nx.picker` (kind 16 mirrors `LspReqKind::DocumentSymbol::as_u16`). Resolves with the
-- symbol `{ text, path, row, col }` item list.
function nx.lsp.document_symbol()
  return lsp_promise(function(id)
    nx._lsp_buf(16, id)
  end)
end

-- `nx.lsp.workspace_symbol(query)`: symbols across the workspace matching `query`,
-- opened in `nx.picker`. With no query, prompt for one via `nx.ui.input` (non-blocking)
-- — an empty/cancelled prompt resolves `nil`. Returns a promise of the matched symbol
-- item list.
function nx.lsp.workspace_symbol(query)
  if type(query) == "string" then
    return lsp_promise(function(id)
      nx._lsp_workspace_symbol(query, id)
    end)
  end
  return nx.ui.input({ prompt = "Workspace symbol: " }):next(function(q)
    if type(q) == "string" and q ~= "" then
      return lsp_promise(function(id)
        nx._lsp_workspace_symbol(q, id)
      end)
    end
  end)
end

-- `nx.lsp.rename(new_name)`: rename the symbol under the cursor. With a name, request
-- it straight away; with none (the bare
-- `nx.keymap.set("n", "<leader>rn", nx.lsp.rename)` case), prompt for it via
-- `nx.ui.input` (non-blocking promise), prefilled with the symbol under the cursor, and
-- rename on confirm. Returns a promise that resolves `nil` once the rename applies (or
-- immediately, `nil`, on an empty / cancelled prompt).
function nx.lsp.rename(new_name)
  if type(new_name) == "string" and new_name ~= "" then
    return lsp_promise(function(id)
      nx._lsp_buf_rename(new_name, id)
    end)
  end
  -- Prefill with the identifier under the cursor — `nx.expand("<cword>")` (vimfn.lua,
  -- loaded after this module; only called at prompt time).
  return nx.ui.input({ prompt = "New Name: ", default = nx.expand("<cword>") }):next(function(name)
    if type(name) == "string" and name ~= "" then
      return lsp_promise(function(id)
        nx._lsp_buf_rename(name, id)
      end)
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
  -- `client:request(method, params, handler)`: issue a generic LSP request; the
  -- reply runs `handler(err, result)` off-tick (err a message string on failure,
  -- result the server's value on success — exactly one set). An unimplemented or
  -- uncapable method fails loud through `err`, never a silent no-op.
  function client:request(method, params, handler)
    local cb_id = nx._next_cb_id()
    nx._cb_fns[cb_id] = handler or function() end
    nx._lsp_client_request(self.id, method, params, cb_id)
  end
  -- `client:notify(method, params)`: fire-and-forget a generic LSP notification.
  function client:notify(method, params)
    nx._lsp_client_notify(self.id, method, params)
  end
  return client
end

-- `nx.lsp.clients(filter)`: a snapshot list of active clients, narrowable by
-- `filter.bufnr` (the clients attached to that buffer; `0`/nil = current) and/or
-- `filter.name` (the config name). Reads the mirror — no request is issued.
--
-- A buffer can have **several** clients attached (`pyright` + `ruff`, `ts_ls` +
-- `eslint`): every server enabled for its filetype attaches, so a `bufnr` filter
-- may return more than one. Don't index `[1]` expecting "the" server — filter by
-- `name`, or by what the client advertises in `server_capabilities`.
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

-- `nx.lsp.request(method, params, handler, bufnr)`: sugar resolving the buffer's
-- primary client and issuing `client:request`. No attached client fails loud.
function nx.lsp.request(method, params, handler, bufnr)
  local client = nx.lsp.clients({ bufnr = bufnr or 0 })[1]
  if not client then
    nx.notify("nx.lsp.request: no LSP client attached to the buffer", vim.log.levels.ERROR)
    return
  end
  client:request(method, params, handler)
end

-- `nx.lsp.notify(method, params, bufnr)`: the fire-and-forget sibling of `request`.
function nx.lsp.notify(method, params, bufnr)
  local client = nx.lsp.clients({ bufnr = bufnr or 0 })[1]
  if not client then
    nx.notify("nx.lsp.notify: no LSP client attached to the buffer", vim.log.levels.ERROR)
    return
  end
  client:notify(method, params)
end

-- ----- code-action commands --------------------------------------------------

-- `nx.lsp.commands[name] = function(command, ctx)`: client-side handlers for the
-- code-action `command`s a server asks the *editor* to run.
--
-- A code action can carry a `command` instead of (or besides) an edit. Most are
-- executed by the server through `workspace/executeCommand`, but some are defined
-- to run client-side — the server has no way to do them itself (`editor.action.*`
-- style commands that open a file, start a rename, or reveal a location). A
-- handler registered here wins over the round trip; anything unregistered goes to
-- the server that offered the action.
--
-- `command` is the raw LSP `Command` (`{ title, command, arguments }`) and `ctx`
-- carries `{ client_id }` — the server that offered it, since the same command
-- name can mean different things to two servers on one buffer.
--
-- ```lua
-- nx.lsp.commands["rust-analyzer.gotoLocation"] = function(command, ctx)
--   local loc = command.arguments and command.arguments[1]
--   if loc then vim.lsp.util.show_document(loc) end
-- end
-- ```
nx.lsp.commands = nx.lsp.commands or {}

-- Run a code action's `command` (called by the engine when an action is applied):
-- a registered `nx.lsp.commands` handler if there is one, else
-- `workspace/executeCommand` on the client that OFFERED the action.
--
-- `client_id` is the offering server, not the buffer's first: a command's name and
-- arguments are that server's own vocabulary, so executing ruff's `source.fixAll`
-- on pyright is a wrong request rather than a degraded one.
--
-- Every failure path is loud (an unknown client, a handler that errors, a server
-- that rejects the command): a code action that silently does nothing looks like
-- one that worked.
function nx.lsp._dispatch_command(client_id, command)
  local name = type(command) == "table" and command.command or nil
  if type(name) ~= "string" then
    nx.notify("nx.lsp: code action carried a malformed command", vim.log.levels.ERROR)
    return
  end
  local handler = nx.lsp.commands[name]
  if handler then
    local ok, err = pcall(handler, command, { client_id = client_id })
    if not ok then
      nx.notify("nx.lsp.commands['" .. name .. "']: " .. tostring(err), vim.log.levels.ERROR)
    end
    return
  end
  local client = nx.lsp._clients[client_id]
  if not client then
    nx.notify(
      "nx.lsp: no client to execute '" .. name .. "' (its server is gone)",
      vim.log.levels.ERROR
    )
    return
  end
  client:request("workspace/executeCommand", {
    command = name,
    arguments = command.arguments,
  }, function(err)
    if err then
      nx.notify("nx.lsp: '" .. name .. "' failed: " .. tostring(err), vim.log.levels.ERROR)
    end
  end)
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

-- The server is exiting (handle still in `_clients`): run `on_exit(code, signal, client)`
-- before `_remove_client` clears it.
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

-- `nx.lsp.semantic_tokens.*` — the per-buffer projection (start/stop), the
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

-- `get_at_pos(bufnr, row, col)`: the decoded tokens covering the 0-based (row, col)
-- (neovim's `vim.lsp.semantic_tokens.get_at_pos`). `col` is a 0-based byte column;
-- a token covers `[start_col, end_col)`. Returns a list (possibly empty).
--
-- A buffer served by several language servers carries **every** capable server's
-- tokens, each tagged with its `client_id`, so one column can hold more than one
-- token. Filter on `client_id` when you mean a specific server's.
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

-- `nx.lsp.inlay_hint.*` — the per-buffer inline hints (off by default), the
-- synchronous read getter, and the enabled-state probe.
nx.lsp.inlay_hint = nx.lsp.inlay_hint or {}

-- `enable(enable?, filter?)`: flip the per-buffer inlay-hint paint. `enable`
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

-- `is_enabled(filter?)`: whether inlay hints are on for the buffer.
function nx.lsp.inlay_hint.is_enabled(filter)
  local bufnr = cur_bufnr(filter and filter.bufnr)
  return nx.lsp._inlay_on[bufnr] == true
end

-- `get(filter?)`: the decoded inlay hints in a buffer (`filter.bufnr`, 0/nil =
-- current), each `{ bufnr, client_id, inlay_hint = <decoded entry> }` to match
-- neovim's shape. `filter.range` ({ start_line, end_line }, 0-based inclusive)
-- narrows by line. Reads the mirror — no request is issued.
--
-- Every capable server's hints are here, in line-then-column order, each tagged
-- with the `client_id` that produced it — a buffer served by two servers shows
-- both sets.
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
-- loclist, the result flows into `nx.picker` with the built-in `"location"` preview
-- (scroll + range-highlight the match), confirm jumps via `nx.picker.edit`. A single
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

-- `nx.lsp._show_locations(items)`: open the picker over a server-resolved location
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
-- `opts` is checked, not swallowed. `name` is **modeled** — it selects which of the
-- buffer's attached servers formats, which is meaningful now that a buffer can carry
-- several. `async` is accepted: nxvim never blocks, and the returned promise is what
-- orders a follow-up — a gated `BufWritePre` awaits it, so `async = false`'s intent
-- (the edits land before the write) holds. `bufnr` / `range` / `filter` are still
-- rejected: nxvim formats the current buffer whole, so honoring them would take a
-- core change, and silently ignoring them would format the wrong thing.
vim.lsp.buf.format = function(opts)
  local name
  if opts ~= nil then
    if type(opts) ~= "table" then
      error("vim.lsp.buf.format: opts must be a table, got " .. type(opts), 2)
    end
    for k in pairs(opts) do
      if k ~= "async" and k ~= "name" then
        error("vim.lsp.buf.format: unsupported option '" .. tostring(k) .. "'", 2)
      end
    end
    name = opts.name
  end
  return nx.lsp.format(name and { name = name } or nil)
end
-- The alias forwards `opts` — `context.only` / `apply` / `range` are modeled (see
-- `nx.lsp.code_action`, whose `range` is nxvim's own 0-based end-exclusive shape, NOT
-- neovim's mark-style one); neovim's `filter` is not, and is rejected there rather
-- than silently dropped.
vim.lsp.buf.code_action = function(opts)
  return nx.lsp.code_action(opts)
end
-- As with `format`: `nx.lsp.rename` renames the symbol under the cursor through the
-- current buffer's server, so neovim's `filter` / `name` / `bufnr` have nothing to
-- select and are rejected rather than quietly ignored.
vim.lsp.buf.rename = function(name, opts)
  if opts ~= nil then
    if type(opts) ~= "table" then
      error("vim.lsp.buf.rename: opts must be a table, got " .. type(opts), 2)
    end
    local k = next(opts)
    if k ~= nil then
      error("vim.lsp.buf.rename: unsupported option '" .. tostring(k) .. "'", 2)
    end
  end
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

-- `nx.lsp.foldexpr` is the canonical LSP foldexpr, the `foldmethod=expr` fold
-- source backed by `textDocument/foldingRange`:
--
-- ```lua
-- nx.bo.foldmethod = "expr"
-- nx.bo.foldexpr   = "v:lua.nx.lsp.foldexpr()"
-- ```
--
-- nxvim recognizes that exact reference and folds the buffer from the language
-- server's folding ranges (requested on open/change while the buffer wants LSP
-- folds — see crates/nxvim-core/src/editor/fold.rs and crates/nxvim-server's
-- lsp/folding.rs). Like the tree-sitter marker it is never evaluated per line, so
-- calling it directly is a usage error — fail loud rather than return a wrong
-- level. `vim.lsp.foldexpr` is the muscle-memory alias; nxvim recognizes both.
function nx.lsp.foldexpr(_lnum)
  error(
    "nx.lsp.foldexpr is a native marker for 'foldmethod=expr' — set it as the "
      .. "'foldexpr' string ('v:lua.nx.lsp.foldexpr()'), don't call it",
    2
  )
end
vim.lsp.foldexpr = nx.lsp.foldexpr
-- The same table, not a copy: a config that registers through either spelling must
-- be seen by the dispatcher, which reads `nx.lsp.commands`.
vim.lsp.commands = nx.lsp.commands
