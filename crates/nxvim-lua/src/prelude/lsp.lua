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
-- client id -> the list of `$/progress` tasks that client is running RIGHT NOW
-- (`nx.lsp._set_progress` replaces the whole list per update; the server clears the
-- slot when the last task ends and when the server exits). Read by `nx.lsp.progress`.
nx.lsp._progress = nx.lsp._progress or {}

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

-- Build the promise an async `nx.lsp` op returns: `issue(cb_id)` queues the op with a
-- fresh callback id, and the server settles it by running
-- `nx._cb_fns[cb_id](nil, result)` once the effect is applied. **Resolve-only** (the
-- `err` arg is always nil), so a bare keymap use — `nx.keymap.set("n", "gd",
-- nx.lsp.definition)`, the common case — can't raise an unhandled-rejection warning.
-- Shared by `nx.lsp.stop` and every language verb below.
local function lsp_promise(issue)
  return nx.promise.new(function(fulfil)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(_err, result)
      fulfil(result)
    end
    issue(id)
  end)
end

-- Validate the `{ name = "<client>" }` option every language verb accepts and return
-- the name (nil when there is none). `verb` names the caller in the error messages.
-- Unsupported keys fail loud — a quietly-dropped option would silently ask the wrong
-- server, which is the whole thing naming one exists to prevent. `extra` lists the
-- verb's OWN option keys (`nx.lsp.code_action` validates those itself).
local function route_name(opts, verb, extra)
  if opts == nil then
    return nil
  end
  if type(opts) ~= "table" then
    error(verb .. ": opts must be a table, got " .. type(opts), 3)
  end
  for k in pairs(opts) do
    if k ~= "name" and not (extra and extra[k]) then
      error(verb .. ": unsupported option '" .. tostring(k) .. "'", 3)
    end
  end
  if opts.name ~= nil and type(opts.name) ~= "string" then
    error(verb .. ": opts.name must be a server-name string", 3)
  end
  return opts.name
end

-- Normalize a `root_markers` value to a list of PRIORITY TIERS. The flat form
-- (`{ ".git", "Cargo.toml" }`) is one tier of equals; the nested form
-- (`{ { "package-lock.json", "yarn.lock" }, { ".git" } }`) is several, and the
-- distinction is load-bearing — see `find_root`. Every marker must be a string, so a
-- half-nested list (`{ { "a" }, "b" }`) fails loud here instead of silently matching
-- nothing at spawn time.
local function marker_tiers(markers, name)
  if type(markers) ~= "table" or #markers == 0 then
    error("nx.lsp: '" .. name .. "' root_markers must be a non-empty list", 0)
  end
  local tiers = type(markers[1]) == "table" and markers or { markers }
  for i, tier in ipairs(tiers) do
    if type(tier) ~= "table" then
      error(
        string.format(
          "nx.lsp: '%s' root_markers mixes plain markers with priority tiers "
            .. "(entry %d is a %s) — use either a flat list of names or a list of "
            .. "lists, not both",
          name,
          i,
          type(tier)
        ),
        0
      )
    end
    for _, m in ipairs(tier) do
      if type(m) ~= "string" then
        error("nx.lsp: '" .. name .. "' root_markers must contain strings", 0)
      end
    end
  end
  return tiers
end

-- `nx.lsp.find_root(bufnr, markers)` -> promise of the workspace root: the nearest
-- ancestor directory of the buffer's file holding one of `markers`, or nil.
--
-- The upward search behind the declarative `root_markers` config key, public because
-- a config whose root depends on something the declarative form can't express —
-- "the lockfile root, unless a Deno config is nearer" — needs to run the same search
-- by hand inside its `root_dir`. It walks the project tree through the async `nx.fs`
-- seam (local on native-bare, the daemon's `luafs_op` over the wire otherwise), so it
-- works on every front end with NO editor-thread block: the `nx.async` body runs as a
-- coroutine and the caller `:next`s the result (or `nx.await`s it). Each
-- `nx.fs.readdir` that rejects — an unreadable or non-directory ancestor — is treated
-- as "no markers here" and the walk continues upward.
--
-- `markers` may carry PRIORITY TIERS — a list of lists rather than a flat list of
-- names. A tier is exhausted over the WHOLE tree before the next one is tried
-- anywhere, which is the entire point of the form: with
-- `{ { "package-lock.json" }, { ".git" } }` a lockfile six directories up beats a
-- nested `.git` one directory up, so a package inside a monorepo attaches at the
-- monorepo root rather than at its own sub-repo. A flat list is one tier, where the
-- nearest directory holding *any* marker wins.
--
-- ```lua
-- root_dir = function(bufnr, on_dir)
--   nx.lsp.find_root(bufnr, { { "package-lock.json" }, { ".git" } }):next(on_dir)
-- end
-- ```
--
-- Each level's listing is read once and memoized, so a second tier re-walks the
-- cached entries instead of re-reading the tree — N directory listings for any
-- number of tiers, not N per tier.
nx.lsp.find_root = nx.async(function(bufnr, markers, name)
  local file = nx.buf.name(bufnr)
  if type(file) ~= "string" or file == "" then
    return nil
  end
  local tiers = marker_tiers(markers, name or "?")
  local levels, walk, walked_out = {}, nx.utils.ancestors(file), false
  for _, tier in ipairs(tiers) do
    local i = 1
    while true do
      if i > #levels then
        if walked_out then
          break -- this tier saw every ancestor and matched none
        end
        local dir = walk()
        if not dir then
          walked_out = true
          break
        end
        local present = {}
        local entries = nx.await(nx.fs.readdir(dir):catch(function()
          return {}
        end))
        for _, e in ipairs(entries) do
          present[e.name] = true
        end
        levels[#levels + 1] = { dir = dir, present = present }
      end
      local level = levels[i]
      for _, m in ipairs(tier) do
        if level.present[m] then
          return level.dir
        end
      end
      i = i + 1
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
  return nx.tbl.deep_extend(
    "force",
    nx.lsp._config["*"] or {},
    base_config(name) or {},
    nx.lsp._config[name] or {}
  )
end

-- `nx.lsp.get_config(name)` -> the fully-resolved config for `name` — the `"*"` base,
-- the runtimepath `lsp/<name>.lua` preset, and every `nx.lsp.config(name, …)` override
-- so far, deep-merged. This is exactly what a server would start with right now, so it
-- is the honest answer to "what is this server configured as?" (neovim's indexable
-- `vim.lsp.config[name]`, as a function — nxvim's `nx.lsp.config` is call-only).
--
-- Reads through the same cached preset loader the dispatcher uses, so there is one
-- copy of the "source a preset off the runtimepath" logic and a caller can't observe a
-- config the dispatcher wouldn't use. Returns a fresh table each call: mutating it
-- changes nothing, which is what `nx.lsp.config` is for.
--
-- ```lua
-- local cfg = nx.lsp.get_config("gopls")
-- if cfg.filetypes and nx.tbl.contains(cfg.filetypes, "go") then … end
-- ```
--
-- A name with no preset and no override resolves to an empty table (`{}`), not nil —
-- "configured as nothing" rather than "unknown", since a config can be built up
-- purely from `nx.lsp.config` calls with no bundled preset behind it at all.
function nx.lsp.get_config(name)
  if type(name) ~= "string" or name == "" then
    error("nx.lsp.get_config: name must be a non-empty string", 2)
  end
  return resolve(name)
end

-- `nx.lsp.config(name, opts)`: accumulate `opts` into `name`'s override layer
-- (deep-merged over any prior call — configs compose across files and plugins).
-- `"*"` is the all-clients base inherited by every server. Function-call form
-- only: there is no `nx.lsp.config[name] = {…}` table-assignment sugar.
--
-- Two keys are easy to confuse, because they are the same word in different worlds:
--
-- ```lua
-- root_dir / root_markers  -- the WORKSPACE root, sent as `rootUri`
-- cmd_cwd                  -- the directory the server PROCESS is launched in
-- ```
--
-- They are unrelated, exactly as in vim. The root is a protocol fact: it tells the
-- server which project to index, and resolving it is what `root_markers` does. The
-- spawn directory defaults to the **editor's own** working directory (`:cd`-aware)
-- and only matters to a `cmd` that cares where it was invoked from — a launcher
-- resolving a virtualenv, or `uvx`, which refuses to run at all when its cwd sits
-- inside its own cache. Set `cmd_cwd` to pin it; a relative value resolves against
-- the editor's cwd.
--
-- ```lua
-- nx.lsp.config("pylsp", {
--   cmd = { "uvx", "--from", "python-lsp-server", "pylsp" },
--   cmd_cwd = nx.workspace.dir(),   -- launch it here, whatever buffer attached
-- })
-- ```
--
-- `opts.priority` (an integer, default `0`) is the **routing rank** among the servers
-- attached to one buffer, higher first. A buffer carries every server enabled for its
-- filetype, and when several can answer the same request the default order is the
-- config name alphabetically — stable, but an arbitrary preference. `priority` states
-- the real one:
--
-- ```lua
-- nx.lsp.config("pyright", { priority = 10, … })   -- the type-checker leads
-- nx.lsp.config("ruff",    { priority = 5,  … })   -- the linter follows
-- ```
--
-- It decides the order the merging verbs present in — the hover float's sections, the
-- signature lines, the code-action chooser's rows, the reference, symbol and goto
-- lists — and which server the verbs that ACT (`format`, `rename`) pick, since two
-- servers' edits cannot both be applied. It does not decide *whether* a server is
-- asked: that is capability, and a server that doesn't advertise the feature is
-- skipped whatever its rank. `nx.lsp.hover{ name = … }` / `:LspHover <server>`
-- override the rank outright for one call. A change takes effect on the next start,
-- so `nx.lsp.restart(name)` applies it to a server already running.
function nx.lsp.config(name, opts)
  if type(name) ~= "string" then
    error("nx.lsp.config: name must be a string", 2)
  end
  if opts ~= nil and type(opts) ~= "table" then
    error("nx.lsp.config: opts must be a table", 2)
  end
  local prev = nx.lsp._config[name] or {}
  nx.lsp._config[name] = nx.tbl.deep_extend("force", prev, opts or {})
end

-- ----- the config schema -----------------------------------------------------

-- Every key `nx.lsp` reads off a resolved config. The set is closed so that a key
-- outside it is *reported* rather than silently dropped — a config whose typo'd
-- `filetype` (singular) never matches anything looks exactly like a server that
-- won't start, and the difference is an hour of debugging.
local KNOWN_KEYS = {
  cmd = true,
  cmd_env = true,
  cmd_cwd = true,
  filetypes = true,
  root_dir = true,
  root_markers = true,
  workspace_required = true,
  init_options = true,
  settings = true,
  capabilities = true,
  commands = true,
  name = true,
  get_language_id = true,
  before_init = true,
  on_init = true,
  on_attach = true,
  on_exit = true,
  offset_encoding = true,
  priority = true,
}

-- Keys nxvim knows about and deliberately does NOT act on, each with the reason and
-- the consequence. These are neovim concepts a ported config may still carry; saying
-- what will happen instead beats both a hard error (which would make an otherwise
-- working server unusable over one unused field) and silence.
local UNSUPPORTED_KEYS = {
  handlers = "nxvim does not route server-initiated messages into Lua, so these "
    .. "handlers never run; the messages are logged and ignored",
  reuse_client = "nxvim always reuses one client per (config name, root), which is "
    .. "what this predicate is computing in every known config",
}

-- Report a config's unknown / unsupported keys, once per (config, key) — the check
-- runs on every FileType dispatch, and a repeated warning on every buffer open would
-- be worse than none. Never fatal: the server still starts on the keys that ARE read.
nx.lsp._warned_keys = nx.lsp._warned_keys or {}
local function warn_config_keys(name, cfg)
  local seen = nx.lsp._warned_keys[name]
  if not seen then
    seen = {}
    nx.lsp._warned_keys[name] = seen
  end
  for key in pairs(cfg) do
    if not KNOWN_KEYS[key] and not seen[key] then
      seen[key] = true
      local why = UNSUPPORTED_KEYS[key]
      nx.notify(
        why and ("nx.lsp: '" .. name .. "' sets `" .. key .. "` — " .. why)
          or (
            "nx.lsp: '"
            .. name
            .. "' sets `"
            .. key
            .. "`, which nx.lsp does not read — if this config's own cmd / before_init / "
            .. "on_attach doesn't consume it, it has no effect (a misspelled key?)"
          ),
        vim.log.levels.WARN
      )
    end
  end
end

-- ----- enable / the engine-side FileType -> Start dispatch --------------------

-- The client name a config starts its server under: `cfg.name` when it sets one,
-- else the registry key. They are the same for almost every config; `name` exists for
-- the few that register under one key and want the server (and `nx.lsp.clients`,
-- `:LspRestart`, the log) to report another.
local function client_name(name, cfg)
  return type(cfg.name) == "string" and cfg.name ~= "" and cfg.name or name
end

-- client name -> the registry key its config lives under. Only differs when a config
-- sets `name`; without it the lifecycle hooks — which resolve a config from the name
-- the SERVER reports — would look up the wrong (empty) config and silently skip a
-- renamed config's `on_attach` / `on_init` / `commands`.
nx.lsp._config_key = nx.lsp._config_key or {}

-- The resolved config behind a live client's reported name.
local function config_of_client(name)
  return resolve(nx.lsp._config_key[name] or name)
end

-- Resolve `cfg.cmd` to an argv. A function `cmd` is the config's own builder, called
-- as neovim calls it — `cmd(dispatchers, config)`, with `config.root_dir` filled in —
-- except that nxvim owns the stdio spawn, so the dispatchers are a stub and the
-- builder simply **returns the argv** (there is no `vim.lsp.rpc.start` to wrap it in).
--
-- The builder may return the argv directly OR a **promise** of one. That is what keeps
-- the common shape — "use the project-local `node_modules/.bin` copy if it exists,
-- else the one on `$PATH`" — non-blocking: the lookup is `nx.fs.which`, which is I/O,
-- and blocking the editor on it is exactly what nxvim doesn't do.
--
-- Resolves `{ cmd = argv }`, or `{ err = reason }` when the builder throws or rejects.
local resolve_cmd = nx.async(function(cfg, root)
  local cmd = cfg.cmd
  if type(cmd) == "function" then
    local config = {}
    for k, v in pairs(cfg) do
      config[k] = v
    end
    config.root_dir = root
    local ok, result = pcall(cmd, {}, config)
    if not ok then
      return { err = "cmd builder errored: " .. tostring(result) }
    end
    -- `nx.await` passes a plain value straight through, so this covers both the
    -- synchronous and the promise-returning builder with one path.
    ok, result = pcall(nx.await, result)
    if not ok then
      return { err = "cmd builder rejected: " .. tostring(result) }
    end
    cmd = result
  end
  return { cmd = cmd }
end)

-- Run a config's `before_init(init_params, config)` — its last chance to shape what
-- crosses at `initialize`, now that the root is known. It mutates `config` in place
-- (rust-analyzer mirrors `settings["rust-analyzer"]` into the initialization options
-- this way; the typescript-adjacent servers compute a `tsdk` path), and nxvim reads
-- `init_options` / `settings` / `capabilities` back off it afterwards. Writing through
-- `init_params.initializationOptions` works too, and wins.
--
-- It may return a promise, awaited before the spawn — so a hook that has to run a tool
-- (`nx.run`) or read a file (`nx.fs`) to decide does it without blocking, which is the
-- whole reason the upstream `vim.system(…):wait()` versions can't be carried over.
--
-- Returns the config to start from, or nil when the hook failed (reported loud — a
-- server started with half-applied options is worse than one that visibly didn't).
local run_before_init = nx.async(function(name, cfg, root)
  if type(cfg.before_init) ~= "function" then
    return cfg
  end
  local start_cfg = {}
  for k, v in pairs(cfg) do
    start_cfg[k] = v
  end
  start_cfg.root_dir = root
  local init_params = {
    initializationOptions = start_cfg.init_options,
    rootUri = root and ("file://" .. root) or nil,
  }
  local ok, ret = pcall(cfg.before_init, init_params, start_cfg)
  if ok then
    ok, ret = pcall(nx.await, ret)
  end
  if not ok then
    nx.notify(
      "nx.lsp: '" .. name .. "' before_init failed: " .. tostring(ret),
      vim.log.levels.ERROR
    )
    return nil
  end
  if init_params.initializationOptions ~= nil then
    start_cfg.init_options = init_params.initializationOptions
  end
  return start_cfg
end)

-- The LSP `languageId` for this buffer's `didOpen` — the filetype, unless the config
-- maps it. `get_language_id(bufnr, filetype)` exists because a handful of servers name
-- a language differently from vim (`objc` -> `objective-c`, `cuda` -> `cuda-cpp`), and
-- sending the wrong id makes a server ignore the document entirely. A hook that throws
-- or returns a non-string is reported and the plain filetype used.
local function language_id_for(name, cfg, bufnr, ft)
  ft = ft or ""
  if type(cfg.get_language_id) ~= "function" then
    return ft
  end
  local ok, id = pcall(cfg.get_language_id, bufnr, ft)
  if not ok then
    nx.notify(
      "nx.lsp: '" .. name .. "' get_language_id failed: " .. tostring(id),
      vim.log.levels.ERROR
    )
    return ft
  end
  if type(id) ~= "string" or id == "" then
    nx.notify(
      string.format(
        "nx.lsp: '%s' get_language_id returned %s, expected a non-empty string — "
          .. "using the filetype '%s'",
        name,
        type(id),
        ft
      ),
      vim.log.levels.WARN
    )
    return ft
  end
  return id
end

-- Normalize `cmd_env` to the `{ NAME = "value" }` string map the spawn takes. Numbers
-- and booleans are accepted and stringified (configs write `NODE_OPTIONS = 4096` and
-- `DEBUG = true`); anything else is dropped with a warning, since an environment entry
-- silently missing is a server that misbehaves for no visible reason.
local function env_map(name, cmd_env)
  if cmd_env == nil then
    return nil
  end
  if type(cmd_env) ~= "table" then
    nx.notify("nx.lsp: '" .. name .. "' cmd_env must be a table (ignored)", vim.log.levels.WARN)
    return nil
  end
  local out, any = {}, false
  for k, v in pairs(cmd_env) do
    local t = type(v)
    if type(k) ~= "string" then
      nx.notify(
        "nx.lsp: '" .. name .. "' cmd_env keys must be strings (dropped one)",
        vim.log.levels.WARN
      )
    elseif t == "string" or t == "number" then
      out[k] = tostring(v)
      any = true
    elseif t == "boolean" then
      out[k] = v and "1" or "0"
      any = true
    else
      nx.notify(
        "nx.lsp: '" .. name .. "' cmd_env." .. k .. " is a " .. t .. " (dropped)",
        vim.log.levels.WARN
      )
    end
  end
  return any and out or nil
end

-- The config's `cmd_cwd` as a plain string, or nil for "the editor's own working
-- directory" (the default, as in vim). It is the directory the SERVER PROCESS is
-- launched in — unrelated to `root_dir`, which only reaches the server as `rootUri` —
-- so a tool that cares where it was invoked (uvx, a wrapper script resolving a venv)
-- can be pinned. A relative value resolves against the editor's cwd server-side.
local function cwd_value(name, cmd_cwd)
  if cmd_cwd == nil then
    return nil
  end
  if type(cmd_cwd) ~= "string" then
    nx.notify("nx.lsp: '" .. name .. "' cmd_cwd must be a string (ignored)", vim.log.levels.WARN)
    return nil
  end
  return cmd_cwd
end

-- The config's `priority` — the routing rank among the servers attached to one
-- buffer, higher first (default `0`). A non-integer is rejected loud rather than
-- silently ranked `0`: a config that meant to state a preference and didn't would
-- otherwise look like it worked, and the symptom (the wrong server answers) is a long
-- way from the cause.
local function config_priority(name, cfg)
  local p = cfg.priority
  if p == nil then
    return nil
  end
  if type(p) ~= "number" or p % 1 ~= 0 then
    error("nx.lsp: '" .. name .. "' priority must be an integer, got " .. tostring(p), 0)
  end
  return p
end

-- Queue a start for `bufnr` from a resolved config (root already computed): build the
-- argv, run `before_init`, then hand the whole spawn across `nx._lsp_start`. A cmd that
-- isn't a spawnable argv is reported loud (a server enabled but unspawnable is visible,
-- never a silent no-op) and skipped — it never errors the whole enable.
local start_resolved = nx.async(function(name, cfg, bufnr, ft, root)
  local built = nx.await(resolve_cmd(cfg, root))
  if built.err then
    nx.notify("nx.lsp: not starting '" .. name .. "': " .. built.err, vim.log.levels.WARN)
    return
  end
  if not is_argv(built.cmd) then
    nx.notify(
      "nx.lsp: not starting '" .. name .. "': cmd is not a spawnable argv",
      vim.log.levels.WARN
    )
    return
  end
  local start_cfg = nx.await(run_before_init(name, cfg, root))
  if start_cfg == nil then
    return
  end
  local reported = client_name(name, cfg)
  nx.lsp._config_key[reported] = name
  nx._lsp_start(
    reported,
    built.cmd,
    root,
    language_id_for(name, cfg, bufnr, ft),
    bufnr,
    nonempty(start_cfg.init_options),
    nonempty(start_cfg.settings),
    nonempty(start_cfg.capabilities),
    env_map(name, start_cfg.cmd_env),
    cwd_value(name, start_cfg.cmd_cwd),
    config_priority(name, start_cfg)
  )
end)

-- Resolve `cfg`'s root and start the server for `bufnr`. `root_dir` may be:
--
--   * a string — used as-is;
--   * a `function(bufnr, on_dir)` — the async escape hatch, which calls `on_dir(dir)`,
--     or **never calls it at all** to decline the buffer outright (how a config says
--     "this file belongs to a different server": ts_ls stepping aside for a Deno tree);
--   * a `function(bufnr)` returning a **promise** of the directory — the same thing in
--     the shape the rest of `nx.*` speaks. Resolving nil means "no root found" (not a
--     decline), so `workspace_required` decides what happens next;
--   * absent, with `root_markers` driving the upward fs-seam search.
--
-- With none of those the root is nil and the server uses the file's directory.
--
-- `workspace_required` gates the last step: a server that resolves its configuration
-- and imports from the workspace is useless without one, so a buffer with no root is
-- DECLINED rather than served by a rootless instance that answers confidently and
-- wrongly (eslint linting with no config, tailwindcss completing no classes).
local function start_for(name, cfg, bufnr, ft)
  local function go(root)
    if cfg.workspace_required and (root == nil or root == "") then
      return
    end
    start_resolved(name, cfg, bufnr, ft, root)
  end
  local rd = cfg.root_dir
  if type(rd) == "function" then
    -- Accept both shapes at once. `fired` keeps a config that calls `on_dir` AND
    -- returns a promise from starting the server twice.
    local fired = false
    local function once(root)
      if fired then
        return
      end
      fired = true
      go(root)
    end
    local ok, ret = pcall(rd, bufnr, once)
    if not ok then
      nx.notify(
        "nx.lsp: '" .. name .. "' root_dir errored: " .. tostring(ret),
        vim.log.levels.ERROR
      )
      return
    end
    if type(ret) == "table" and type(ret.next) == "function" then
      ret:next(once, function(err)
        nx.notify(
          "nx.lsp: '" .. name .. "' root_dir rejected: " .. tostring(err),
          vim.log.levels.ERROR
        )
      end)
    end
  elseif type(rd) == "string" then
    go(rd)
  elseif cfg.root_markers then
    -- A malformed `root_markers` raises out of `marker_tiers`; surface it as this
    -- config's problem rather than an unhandled rejection with no name attached.
    nx.lsp.find_root(bufnr, cfg.root_markers, name):next(go, function(err)
      nx.notify(tostring(err), vim.log.levels.ERROR)
    end)
  else
    go(nil)
  end
end

-- The enabled configs, resolved once into `{ name = , cfg = }` entries. `only` (a set
-- of config names, optional) narrows it to those names.
--
-- Split out from the dispatch below so a caller that sweeps MANY buffers resolves each
-- config once for the whole sweep rather than once per buffer: `resolve` deep-merges
-- three layers and deep-COPIES the result, and a config's `settings` is not always
-- small (nxvim-efmls-configs hands efm one `languages` map covering every loaded
-- filetype). Per-event dispatch touches one buffer and doesn't care; `enable`'s
-- catch-up touches every open one.
local function enabled_entries(only)
  local entries = {}
  for name, on in pairs(nx.lsp._enabled) do
    if on and (only == nil or only[name]) then
      entries[#entries + 1] = { name = name, cfg = resolve(name) }
    end
  end
  return entries
end

-- Start every config in `entries` whose `filetypes` includes `ft`, for `bufnr`.
local function start_matching(entries, bufnr, ft)
  if not ft or ft == "" then
    return
  end
  for _, e in ipairs(entries) do
    if e.cfg.filetypes and nx.tbl.contains(e.cfg.filetypes, ft) then
      -- Report the config's unknown / unsupported keys the first time it is actually
      -- reached for a filetype it serves — here rather than at registration, so a
      -- config that never matches anything never nags, and here rather than inside
      -- `start_for`, so a `workspace_required` decline still names a typo'd key.
      warn_config_keys(e.name, e.cfg)
      start_for(e.name, e.cfg, bufnr, ft)
    end
  end
end

-- The shared FileType dispatcher body: for every enabled config whose resolved
-- `filetypes` includes `ft`, resolve the root and start the server for `bufnr`.
-- This is the engine's declarative FileType -> start step (neovim wires an internal
-- autocmd; nxvim keeps it here so it behaves identically under the wasm edit-host).
function nx.lsp._on_filetype(bufnr, ft)
  if not ft or ft == "" then -- ahead of `enabled_entries`, which is the expensive half
    return
  end
  start_matching(enabled_entries(nil), bufnr, ft)
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
      local cfg = config_of_client(client.name)
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
  local named = {}
  for _, n in ipairs(names) do
    if n == "*" then
      error("nx.lsp.enable: '*' is the base layer, not a server name", 2)
    end
    nx.lsp._enabled[n] = true
    named[n] = true
  end
  ensure_dispatcher()
  -- Every buffer already read has had its `FileType` fired, so the dispatcher just
  -- installed will never see any of them; catch them all up on the spot (a start is
  -- idempotent server-side, so overlapping the startup FileType from init.lua is
  -- harmless).
  --
  -- ALL of them, not just the current one: a config enabled late — a plugin that
  -- resolves its tools over the async `nx.fs` seam before registering the server, as
  -- nxvim-efmls-configs does for efm — lands several ticks after the files were read,
  -- and nothing re-fires `FileType` for a buffer whose filetype is already set. Miss
  -- a buffer here and it is served by nothing for the rest of the session.
  --
  -- Scoped to the names being enabled (`named`) and resolved ONCE for the whole
  -- sweep, because this pass is per-BUFFER where the dispatcher is per-event. Every
  -- other enabled config was already caught up when *it* was enabled, so including
  -- them here would only re-resolve their configs and re-walk their `root_markers`
  -- roots (an ancestor walk with a `readdir` per level) once more per open buffer —
  -- and a plugin that enables lazily calls `enable` once per language it loads.
  local entries = enabled_entries(named)
  local seen = {}
  local cur = nx._cur_buf
  if cur and cur.filetype and cur.filetype ~= "" then
    seen[cur.bufnr] = true
    start_matching(entries, cur.bufnr, cur.filetype)
  end
  for bufnr in pairs(nx._bufs or {}) do
    if not seen[bufnr] then
      -- `nx._bo_mirror` is the canonical per-buffer filetype (what `nx.bo[buf]`
      -- reads); `nx._cur_buf` carries only the current one's.
      local ft = ((nx._bo_mirror or {})[bufnr] or {}).filetype
      if ft and ft ~= "" then
        start_matching(entries, bufnr, ft)
      end
    end
  end
end

-- `nx.lsp.disable(names)`: the inverse of `enable` — future buffers won't start the
-- named servers (already-running servers keep serving until their buffers close).
--
-- To shut a server down **now**, use `nx.lsp.stop` — on its own, `disable` closes the
-- gate without touching what is already through it.
function nx.lsp.disable(names)
  if type(names) == "string" then
    names = { names }
  end
  for _, n in ipairs(names) do
    nx.lsp._enabled[n] = nil
  end
end

-- `nx.lsp.stop(name[, opts])`: shut down every running server with config `name`,
-- detaching it from the buffers it was serving. Returns a promise that resolves with
-- the NUMBER of servers stopped (0 when nothing by that name was running) — so a
-- caller can report "no server named X is running" rather than a silent success.
--
-- ```lua
-- nx.lsp.stop("gopls"):next(function(n)
--   nx.notify(n > 0 and ("stopped " .. n) or "gopls was not running")
-- end)
-- ```
--
-- A stopped server can come back: the config stays enabled, so the next buffer whose
-- filetype matches starts a fresh one. Pass `opts.disable = true` to stop it *and*
-- close the gate (`nx.lsp.disable`), which is what `:LspStop` does — otherwise the
-- very next matching buffer would silently restart what you just stopped.
function nx.lsp.stop(name, opts)
  if type(name) ~= "string" or name == "" then
    error("nx.lsp.stop: name must be a non-empty string", 2)
  end
  if opts ~= nil and type(opts) ~= "table" then
    error("nx.lsp.stop: opts must be a table", 2)
  end
  if opts and opts.disable then
    nx.lsp.disable(name)
  end
  return lsp_promise(function(id)
    nx._lsp_stop(name, id)
  end)
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
    nonempty(cfg.capabilities),
    config_priority(name, cfg)
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
-- the two in step. Each is built with `lsp_promise` (defined up with the helpers,
-- since `nx.lsp.stop` above needs it too).
--
-- Every verb takes an optional `{ name = "<client>" }` that **routes** the request to
-- the attached client of that config name:
--
-- ```lua
-- nx.lsp.hover({ name = "pyright" })          -- not ruff, whatever key order says
-- nx.lsp.references({ name = "ts_ls" })       -- one server's list, not the merge
-- nx.keymap.set("n", "K", function() return nx.lsp.hover({ name = "pyright" }) end)
-- ```
--
-- A buffer carries **every** server enabled for its filetype, and every verb here asks
-- them all, merging the answers in `priority` order (see `nx.lsp.config`). The goto
-- family jumps when the merged list holds one place, so a one-server buffer behaves
-- exactly as before. Only the verbs that *act* on the buffer — `format`, `rename` —
-- pick a single server, since two servers' edits cannot both be applied; there the
-- highest-`priority` capable one wins. Naming one overrides either: the single-target
-- pick, or the round narrowed to that client alone.
-- A name that isn't attached to the buffer reports so on the message line and resolves
-- `nil`; it never falls back to a different server. The ex-commands take the same
-- route as a bare argument (`:LspHover pyright`).

-- `nx.lsp.definition([opts])`: jump to the definition of the symbol under the cursor,
-- resolving with the `{ text, path, row, col }` item list.
--
-- Asks **every** capable server and merges: a definition can genuinely live in two
-- places to two servers (a generated stub and its source, a `.d.ts` and its
-- implementation). Duplicates collapse, so the merged list holding ONE place still
-- jumps — the one-server case is unchanged — and only a real disagreement opens
-- `nx.picker`. `opts.name` narrows the round to that client.
function nx.lsp.definition(opts)
  local name = route_name(opts, "nx.lsp.definition")
  return lsp_promise(function(id)
    nx._lsp_buf(0, id, name)
  end)
end
-- `nx.lsp.declaration([opts])`: the `definition` twin for `textDocument/declaration`
-- (C headers, `extern` declarations). Merges across servers and `opts.name` routes it,
-- exactly as `nx.lsp.definition` does.
function nx.lsp.declaration(opts)
  local name = route_name(opts, "nx.lsp.declaration")
  return lsp_promise(function(id)
    nx._lsp_buf(1, id, name)
  end)
end
-- `nx.lsp.type_definition([opts])`: jump to the definition of the *type* of the symbol
-- under the cursor. Merges across servers and `opts.name` routes it, exactly as
-- `nx.lsp.definition` does.
function nx.lsp.type_definition(opts)
  local name = route_name(opts, "nx.lsp.type_definition")
  return lsp_promise(function(id)
    nx._lsp_buf(2, id, name)
  end)
end
-- `nx.lsp.implementation([opts])`: the implementations of the interface / trait method
-- under the cursor. Merges across servers and `opts.name` routes it, exactly as
-- `nx.lsp.definition` does.
function nx.lsp.implementation(opts)
  local name = route_name(opts, "nx.lsp.implementation")
  return lsp_promise(function(id)
    nx._lsp_buf(3, id, name)
  end)
end
-- `nx.lsp.references([opts])`: the references to the symbol under the cursor, opened in
-- `nx.picker`. Fans out to **every** capable server and merges (two servers indexing one
-- project each know references the other does not); `opts.name` narrows the round to
-- that one client's list.
function nx.lsp.references(opts)
  local name = route_name(opts, "nx.lsp.references")
  return lsp_promise(function(id)
    nx._lsp_buf(4, id, name)
  end)
end
-- `nx.lsp.hover([opts])`: the hover documentation for the symbol under the cursor, in a
-- float, resolving with the shown text.
--
-- Asks **every** attached server advertising `hoverProvider` and composes one float:
-- on a `pyright` + `ruff` buffer each knows something the other doesn't (a type, a lint
-- rationale), and answering from one silently hides the other. With more than one
-- contributor each section is headed `# <client name>`, in `priority` order; with one
-- it renders bare. `opts.name` narrows the round to that client alone.
function nx.lsp.hover(opts)
  local name = route_name(opts, "nx.lsp.hover")
  return lsp_promise(function(id)
    nx._lsp_buf(5, id, name)
  end)
end
-- `nx.lsp.signature_help([opts])`: the signature of the call under the cursor, in a
-- float. `opts.name` routes it; see `nx.lsp.signature_help_autotrigger` for showing it
-- as you type instead of on demand.
--
-- A call with more than one parameter is laid out **one parameter per line**, with a
-- `▸` marking the parameter the cursor is inside (the `LspSignatureActiveParameter`
-- highlight group):
--
-- ```
-- def connect(
--     host: str,
--   ▸ port: int = 5432,
--     timeout: float = 30.0,
-- ) -> Connection
-- ```
--
-- The split reads the server's own parameter ranges, not commas, so
-- `f(items: dict[str, int])` stays one parameter. A one-parameter call — or a server
-- whose parameters can't be located in the signature it sent — stays on a single line
-- with the active parameter named in brackets.
--
-- Asks every capable server, like `nx.lsp.hover`. With more than one answering they
-- are shown together in `priority` order, each labelled with its client — where neovim
-- shows one at a time and binds `<C-s>` to cycle. nxvim's float is passive (the next
-- keystroke dismisses it), so there is nothing to cycle through and no mode to leave.
function nx.lsp.signature_help(opts)
  local name = route_name(opts, "nx.lsp.signature_help")
  return lsp_promise(function(id)
    nx._lsp_buf(6, id, name)
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
  local name = route_name(opts, "nx.lsp.format")
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
-- name           ask only the attached client of this config name, instead of
--                merging every capable server's actions into one chooser — "run
--                eslint's fixes", not ts_ls's refactors as well.
-- ```
--
-- Anything else in `opts` (neovim's `filter`, `context.diagnostics`,
-- `context.triggerKind`) is **rejected loudly** rather than silently ignored — nxvim
-- doesn't model it yet, and a quietly-dropped filter would silently do the wrong thing.
function nx.lsp.code_action(opts)
  local only, apply, range = {}, false, nil
  local name =
    route_name(opts, "nx.lsp.code_action", { context = true, apply = true, range = true })
  if opts ~= nil then
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
    nx._lsp_buf_code_action(id, only, apply, range, name)
  end)
end

-- `nx.lsp.document_symbol([opts])`: the symbols defined in the current document,
-- opened in `nx.picker` (kind 16 mirrors `LspReqKind::DocumentSymbol::as_u16`).
-- Resolves with the symbol `{ text, path, row, col }` item list. `opts.name` lists
-- only that client's symbols instead of merging every capable server's.
function nx.lsp.document_symbol(opts)
  local name = route_name(opts, "nx.lsp.document_symbol")
  return lsp_promise(function(id)
    nx._lsp_buf(16, id, name)
  end)
end

-- `nx.lsp.workspace_symbol([query], [opts])`: symbols across the workspace matching
-- `query`, opened in `nx.picker`. With no query, prompt for one via `nx.ui.input`
-- (non-blocking) — an empty/cancelled prompt resolves `nil`. Returns a promise of the
-- matched symbol item list. `opts.name` searches only that client's index instead of
-- merging every capable server's; since a query is always a string, the options table
-- may take the first argument's place when you want the prompt *and* a route
-- (`nx.lsp.workspace_symbol({ name = "ts_ls" })`).
function nx.lsp.workspace_symbol(query, opts)
  if type(query) == "table" and opts == nil then
    query, opts = nil, query
  end
  local name = route_name(opts, "nx.lsp.workspace_symbol")
  if type(query) == "string" then
    return lsp_promise(function(id)
      nx._lsp_workspace_symbol(query, id, name)
    end)
  end
  return nx.ui.input({ prompt = "Workspace symbol: " }):next(function(q)
    if type(q) == "string" and q ~= "" then
      return lsp_promise(function(id)
        nx._lsp_workspace_symbol(q, id, name)
      end)
    end
  end)
end

-- `nx.lsp.rename([new_name], [opts])`: rename the symbol under the cursor. With a
-- name, request it straight away; with none (the bare
-- `nx.keymap.set("n", "<leader>rn", nx.lsp.rename)` case), prompt for it via
-- `nx.ui.input` (non-blocking promise), prefilled with the symbol under the cursor, and
-- rename on confirm. Returns a promise that resolves `nil` once the rename applies (or
-- immediately, `nil`, on an empty / cancelled prompt).
--
-- `opts.name` routes the request to one attached client. A new name is always a
-- string, so the options table may take the first argument's place when you want the
-- prompt *and* a route (`nx.lsp.rename({ name = "ts_ls" })`).
function nx.lsp.rename(new_name, opts)
  if type(new_name) == "table" and opts == nil then
    new_name, opts = nil, new_name
  end
  local server = route_name(opts, "nx.lsp.rename")
  if type(new_name) == "string" and new_name ~= "" then
    return lsp_promise(function(id)
      nx._lsp_buf_rename(new_name, id, server)
    end)
  end
  -- Prefill with the identifier under the cursor — `nx.expand("<cword>")` (vimfn.lua,
  -- loaded after this module; only called at prompt time).
  return nx.ui.input({ prompt = "New Name: ", default = nx.expand("<cword>") }):next(function(name)
    if type(name) == "string" and name ~= "" then
      return lsp_promise(function(id)
        nx._lsp_buf_rename(name, id, server)
      end)
    end
  end)
end

-- ----- client handles, introspection & escape hatch --------------------------

-- The `server_capabilities` key that decides whether a client answers a method.
-- Only the methods nxvim actually mirrors a provider flag for are listed; anything
-- else — a server's OWN extension (`textDocument/switchSourceHeader`,
-- `ocamllsp/switchImplIntf`, `deno/virtualTextDocument`) — is not describable by a
-- standard capability at all, and `supports_method` answers **true** for it: those
-- are exactly the requests a per-server config exists to make, and a blanket false
-- would refuse every one of them.
local METHOD_CAPABILITY = {
  ["textDocument/definition"] = "definitionProvider",
  ["textDocument/declaration"] = "declarationProvider",
  ["textDocument/typeDefinition"] = "typeDefinitionProvider",
  ["textDocument/implementation"] = "implementationProvider",
  ["textDocument/references"] = "referencesProvider",
  ["textDocument/hover"] = "hoverProvider",
  ["textDocument/signatureHelp"] = "signatureHelpProvider",
  ["textDocument/completion"] = "completionProvider",
  ["textDocument/formatting"] = "documentFormattingProvider",
  ["textDocument/rangeFormatting"] = "documentFormattingProvider",
  ["textDocument/rename"] = "renameProvider",
  ["textDocument/codeAction"] = "codeActionProvider",
  ["textDocument/semanticTokens/full"] = "semanticTokensProvider",
  ["textDocument/inlayHint"] = "inlayHintProvider",
  ["textDocument/documentSymbol"] = "documentSymbolProvider",
  ["workspace/symbol"] = "workspaceSymbolProvider",
}

local client_handle = {}

-- `client:request(method, params, handler[, bufnr])`: issue a generic LSP request;
-- the reply runs `handler(err, result)` off-tick (err a message string on failure,
-- result the server's value on success — exactly one set). An unimplemented or
-- uncapable method fails loud through `err`, never a silent no-op.
--
-- `bufnr` is accepted for source compatibility with neovim's fourth argument and
-- has no effect: the request is addressed to THIS client, and nxvim cancels a
-- client's in-flight requests when the client goes away rather than per buffer.
function client_handle:request(method, params, handler, _bufnr)
  local cb_id = nx._next_cb_id()
  nx._cb_fns[cb_id] = handler or function() end
  nx._lsp_client_request(self.id, method, params, cb_id)
end

-- `client:notify(method, params)`: fire-and-forget a generic LSP notification.
function client_handle:notify(method, params)
  nx._lsp_client_notify(self.id, method, params)
end

-- `client:supports_method(method)` -> boolean: does this server answer `method`?
--
-- Read from what the server advertised at `initialize`
-- (`client.server_capabilities`), so a config guards its own commands against a
-- server build that lacks the feature instead of firing a request that comes back
-- as an error the user has to interpret. A method nxvim maps no capability for is
-- **supported** — see `METHOD_CAPABILITY`.
function client_handle:supports_method(method)
  local cap = METHOD_CAPABILITY[method]
  if not cap then
    return true
  end
  return self.server_capabilities[cap] and true or false
end

-- `client:exec_cmd(command[, context[, handler]])`: run an LSP `Command`
-- (`{ title, command, arguments }`) — the verb behind a code action that carries a
-- command, and the one a per-server `:Lsp…` command uses to drive its server's own
-- vocabulary (`deno.cache`, `texlab.cleanArtifacts`, `_typescript.goToSourceDefinition`).
--
-- Resolved in precedence order, which is the same order an applied code action
-- takes:
--
-- ```
-- 1. the OFFERING config's own `commands` table   (this client's config)
-- 2. the global `nx.lsp.commands` registry
-- 3. `workspace/executeCommand` on this client
-- ```
--
-- A client-side handler (1 or 2) is called as `handler(command, ctx)` and the round
-- trip never happens — that is the whole point of registering one. Otherwise the
-- command goes to the server and `handler(err, result, ctx)` receives the reply.
-- `ctx` carries `{ client_id, bufnr, params }`, so a failing handler can report
-- *which* arguments failed.
--
-- `context.bufnr` names the buffer the command acts on (`0`/nil = current). Unlike
-- neovim, a command the server did not list in `executeCommandProvider` is still
-- sent: nxvim reports whatever the server answers rather than refusing locally, since
-- servers under-advertise this list routinely and a local refusal reads as "the
-- command did nothing".
function client_handle:exec_cmd(command, context, handler)
  local name = type(command) == "table" and command.command or nil
  if type(name) ~= "string" then
    nx.notify(
      "client:exec_cmd: command must be a table with a `command` string",
      vim.log.levels.ERROR
    )
    return
  end
  context = context or {}
  local params = { command = name, arguments = command.arguments }
  local ctx = { client_id = self.id, bufnr = cur_bufnr(context.bufnr), params = params }
  local commands = config_of_client(self.name).commands
  local fn = (type(commands) == "table" and commands[name]) or nx.lsp.commands[name]
  if fn then
    local ok, err = pcall(fn, command, ctx)
    if not ok then
      nx.notify("nx.lsp.commands['" .. name .. "']: " .. tostring(err), vim.log.levels.ERROR)
    end
    return
  end
  self:request("workspace/executeCommand", params, function(err, result)
    if handler then
      handler(err, result, ctx)
    elseif err then
      nx.notify("nx.lsp: '" .. name .. "' failed: " .. tostring(err), vim.log.levels.ERROR)
    end
  end, ctx.bufnr)
end

-- The handle's metatable. `offset_encoding` is the one field that is not a plain
-- value: reading it yields the encoding negotiated at `initialize`, and ASSIGNING it
-- re-negotiates the live client (`LspOp::SetOffsetEncoding`) rather than just
-- relabelling the handle.
--
-- The write path exists for one real shape — a config's `on_init` reading an
-- encoding the server reported outside `capabilities.positionEncoding`, which is how
-- clangd answers (a top-level `offsetEncoding` the protocol doesn't define). If the
-- assignment only landed in Lua, the handle would report utf-8 while every column on
-- the wire stayed utf-16: wrong only on lines holding a multi-byte character, and
-- silently so.
local client_mt = {
  __index = function(self, key)
    if key == "offset_encoding" then
      return rawget(self, "_offset_encoding")
    end
    return client_handle[key]
  end,
  __newindex = function(self, key, value)
    if key == "offset_encoding" then
      if type(value) ~= "string" then
        error("client.offset_encoding must be a string, got " .. type(value), 2)
      end
      rawset(self, "_offset_encoding", value)
      nx._lsp_set_offset_encoding(rawget(self, "id"), value)
      return
    end
    rawset(self, key, value)
  end,
}

-- Build the snapshot handle mirrored into `nx.lsp._clients[id]`. It carries the
-- server's resolved capabilities, its negotiated position encoding, and the generic
-- `:request` / `:notify` escape hatch (engine Decision 3: callback-shaped, a
-- generation token, stale replies dropped server-side). `on_attach` / `on_init`
-- receive this same handle.
local function make_client(id, name, capabilities, offset_encoding)
  return setmetatable({
    id = id,
    name = name,
    server_capabilities = capabilities or {},
    -- Read back through `__index` as `offset_encoding`; set directly here so
    -- mirroring a client does NOT queue a re-negotiation of what it already agreed to.
    _offset_encoding = offset_encoding or "utf-16",
  }, client_mt)
end

-- `nx.lsp.client_by_id(id)`: the handle for client `id`, or nil once its server has
-- exited. The lookup behind a handler that is handed a `client_id` (a code-action
-- `ctx`, an `LspAttach` autocmd's `args.data.client_id`) rather than a handle.
function nx.lsp.client_by_id(id)
  return nx.lsp._clients[id]
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

-- `nx.lsp.progress(filter)`: the `$/progress` work language servers are running
-- **right now** — what a statusline renders as "lua_ls: Indexing 43%".
--
-- Servers report long tasks (indexing, loading a workspace, building a crate graph)
-- as a `begin` → `report`* → `end` sequence sharing a token; nxvim folds that stream
-- into settled records, so this returns a flat list of what is in flight, newest
-- task last:
--
-- ```lua
-- for _, p in ipairs(nx.lsp.progress()) do
--   print(("%s: %s %s%s"):format(
--     p.client_name, p.title, p.message or "", p.percentage and (" " .. p.percentage .. "%%") or ""))
-- end
-- ```
--
-- Each item is:
--
-- ```
-- client_id    the reporting client (resolve with `nx.lsp.client_by_id`)
-- client_name  its config name (`"lua_ls"`)
-- token        the task's `$/progress` token, unique within the client
-- title        the `begin` title (`"Indexing"`); `""` if the server never began
-- message      the detail line (`"3/25 files"`), or nil
-- percentage   0-100, or nil for an indeterminate task (show a spinner, not a bar)
-- cancellable  whether the server would honor a cancel for this token
-- ```
--
-- `filter.client_id` narrows to one client; `filter.bufnr` (`0` = current) narrows to
-- the clients attached to that buffer, which is what a per-window statusline wants —
-- a busy server serving some *other* buffer isn't this buffer's status.
--
-- The order is STABLE: clients ascending by id (so the longest-running server's work
-- comes first), and within a client the order the tasks began. A renderer showing only
-- the first task therefore keeps showing the same one across updates.
--
-- The list is EMPTY when nothing is in flight (a finished task is removed, not left
-- at 100%), and a server that exits mid-task takes its entries with it. Reads the
-- mirror — no request is issued. `LspProgress` fires on every update, with the kind
-- (`"begin"` / `"report"` / `"end"`) as the autocmd pattern, so a renderer invalidates
-- on the event and reads through here rather than polling.
function nx.lsp.progress(filter)
  filter = filter or {}
  local want = nil
  if filter.bufnr ~= nil then
    want = nx.lsp._attached[cur_bufnr(filter.bufnr)] or {}
  end
  -- `pairs` over the mirror is UNORDERED — it walks a client-id-keyed table, and once
  -- the ids sit in the hash part (any session where an earlier client has stopped) it
  -- yields whichever client reported first. Walk the ids sorted instead, so the list
  -- doesn't reshuffle under a renderer that shows only `[1]`.
  local ids = {}
  for id in pairs(nx.lsp._progress) do
    ids[#ids + 1] = id
  end
  table.sort(ids)
  local out = {}
  for _, id in ipairs(ids) do
    local tasks = nx.lsp._progress[id]
    local client = nx.lsp._clients[id]
    local keep = client ~= nil
      and (filter.client_id == nil or filter.client_id == id)
      and (want == nil or want[id] == true)
    if keep then
      for _, t in ipairs(tasks) do
        out[#out + 1] = {
          client_id = id,
          client_name = client.name,
          token = t.token,
          title = t.title,
          message = t.message,
          percentage = t.percentage,
          cancellable = t.cancellable,
        }
      end
    end
  end
  return out
end

-- Resolve the client a `nx.lsp.request` / `nx.lsp.notify` goes to. `target` is a
-- bufnr (`0`/nil = current) or a `{ bufnr =, name = }` table — `name` routes to that
-- config's client rather than "whichever attached one comes first", which on a
-- multi-server buffer is not the one a server-specific method belongs to. Returns nil
-- after a loud notify when nothing matches (the caller adds no fallback).
local function request_client(verb, target)
  local filter = { bufnr = 0 }
  if type(target) == "table" then
    filter.bufnr = target.bufnr or 0
    filter.name = target.name
  elseif target ~= nil then
    filter.bufnr = target
  end
  local client = nx.lsp.clients(filter)[1]
  if not client then
    local which = filter.name and (" named '" .. filter.name .. "'") or ""
    nx.notify(verb .. ": no LSP client" .. which .. " attached to the buffer", vim.log.levels.ERROR)
    return nil
  end
  return client
end

-- `nx.lsp.request(method, params, handler, target)`: sugar resolving one of the
-- buffer's clients and issuing `client:request`. `target` is a bufnr (`0`/nil =
-- current) or `{ bufnr =, name = }`, where `name` picks the attached client by config
-- name instead of taking the first. No matching client fails loud.
function nx.lsp.request(method, params, handler, target)
  local client = request_client("nx.lsp.request", target)
  if client then
    client:request(method, params, handler)
  end
end

-- `nx.lsp.notify(method, params, target)`: the fire-and-forget sibling of `request`,
-- with the same `target` routing.
function nx.lsp.notify(method, params, target)
  local client = request_client("nx.lsp.notify", target)
  if client then
    client:notify(method, params)
  end
end

-- ----- hand-built request params ---------------------------------------------
-- Everything nxvim asks a server itself is built engine-side, in Rust, against the
-- rope. These exist for the other direction: a per-server config issuing one of its
-- server's OWN requests (`textDocument/switchSourceHeader`, `textDocument/build`)
-- has to hand over a document reference and a cursor position, and getting the
-- column convention wrong is silent — the request succeeds and answers about the
-- wrong character.

-- `byte -> column in `encoding``, over one line's text. nxvim columns are byte
-- offsets; LSP counts utf-16 code units by default, so on any line holding a
-- multi-byte character the two numbers differ. Malformed utf-8 (a file nxvim
-- transcoded, a buffer mid-edit) falls back to the byte count rather than raising:
-- a slightly-off position beats a config that errors on one bad line.
local function byte_to_encoded_col(line, byte_col, encoding)
  if encoding == "utf-8" then
    return math.min(byte_col, #line)
  end
  local prefix = line:sub(1, byte_col)
  local ok, count = pcall(function()
    local n = 0
    for _, cp in utf8.codes(prefix) do
      -- utf-16 needs a surrogate PAIR for anything past the BMP; utf-32 counts
      -- codepoints, which is what the loop already does.
      n = n + ((encoding == "utf-16" and cp >= 0x10000) and 2 or 1)
    end
    return n
  end)
  return ok and count or #prefix
end

-- The inverse: a `character` counted in `encoding` -> the byte offset into `line`.
-- Used when a *server's* position comes back to be turned into something nxvim
-- addresses by byte (a quickfix column).
local function encoded_col_to_byte(line, character, encoding)
  if encoding == "utf-8" then
    return math.min(character, #line)
  end
  local ok, byte = pcall(function()
    local units, i = 0, 1
    while i <= #line do
      if units >= character then
        return i - 1
      end
      local cp = utf8.codepoint(line, i)
      units = units + ((encoding == "utf-16" and cp >= 0x10000) and 2 or 1)
      i = utf8.offset(line, 2, i) or (#line + 1)
    end
    return #line
  end)
  return ok and byte or math.min(character, #line)
end

-- The position encoding to count a buffer's columns in: the one the buffer's own
-- server negotiated. With several servers attached the first is taken — a caller who
-- means a specific server passes `opts.encoding = client.offset_encoding`, which is
-- what every per-server config does (it already holds the handle).
local function buffer_encoding(bufnr)
  local client = nx.lsp.clients({ bufnr = bufnr })[1]
  return client and client.offset_encoding or "utf-16"
end

-- `nx.lsp.text_document_params([bufnr])` -> `{ uri = … }`, the LSP
-- `TextDocumentIdentifier` for buffer `bufnr` (`0`/nil = current) — the params of
-- every request that names a document and nothing else.
function nx.lsp.text_document_params(bufnr)
  return { uri = nx.utils.uri_from_buf(cur_bufnr(bufnr)) }
end

-- `nx.lsp.position_params([opts])` -> `{ textDocument = { uri }, position = { line,
-- character } }`, the LSP `TextDocumentPositionParams` for the cursor — the params
-- shape most requests take.
--
-- opts:
--   * `win` — the window whose cursor to read (`0`/nil = current).
--   * `bufnr` — the document to name (default: `win`'s buffer).
--   * `encoding` — the position encoding to count `character` in. **Pass the
--     answering client's `offset_encoding`**; the default is whatever the buffer's
--     first attached server negotiated, which is only right by luck when two servers
--     with different encodings share a buffer.
--
-- ```lua
-- local params = nx.lsp.position_params({ encoding = client.offset_encoding })
-- client:request("textDocument/build", params, on_reply)
-- ```
function nx.lsp.position_params(opts)
  opts = opts or {}
  local win = opts.win or 0
  local bufnr = cur_bufnr(opts.bufnr or nx.win.buf(win))
  local pos = nx.cursor.get(win)
  local row, col = pos[1], pos[2]
  local line = nx.buf.lines(bufnr, row - 1, row, false)[1] or ""
  return {
    textDocument = { uri = nx.utils.uri_from_buf(bufnr) },
    position = {
      line = row - 1,
      character = byte_to_encoded_col(line, col, opts.encoding or buffer_encoding(bufnr)),
    },
  }
end

-- The lines of the document a location names, for the `text` of its quickfix entry:
-- from the mirror when the file is already open (which is both cheaper and *correct*
-- for unsaved edits), else read off disk. Never rejects — a location into a file that
-- has since been deleted still deserves its entry, just without the line's text.
local location_lines = nx.async(function(path)
  for _, bufnr in ipairs(nx.buf.list()) do
    if nx.fname.modify(nx.buf.name(bufnr), ":p") == path then
      return nx.buf.lines(bufnr, 0, -1, false)
    end
  end
  local text = nx.await(nx.fs.read_text(path):catch(function()
    return nil
  end))
  return type(text) == "string" and nx.str.split(text, "\n") or {}
end)

-- `nx.lsp.locations_to_items(locations[, opts])` -> a PROMISE of quickfix items,
-- one per location, sorted by file then position — the bridge from a server's
-- `Location[]` / `LocationLink[]` to the shape `nx.qf.setqflist` takes
-- (`{ filename, lnum, col, end_lnum, end_col, text }`, all 1-based).
--
-- ```lua
-- nx.lsp.locations_to_items(refs, { encoding = client.offset_encoding })
--   :next(function(items)
--     nx.qf.setqflist({}, " ", { title = "References", items = items })
--     nx.qf.open()
--   end)
-- ```
--
-- A promise, not a value, and that is not incidental: each item's `text` is the
-- source line it points at, and a location into a file no buffer holds means reading
-- that file. neovim reads them synchronously; nxvim does no blocking I/O anywhere, so
-- the conversion is async and the *editor* keeps running while a 900-reference result
-- resolves.
--
-- `opts.encoding` is the position encoding the server counted its columns in
-- (default `"utf-16"`, the protocol's) — columns come out as nxvim's own bytes.
-- Locations naming a non-`file://` document are dropped: a quickfix entry addresses a
-- path, and a `deno:`/`jdt:` URI has none.
nx.lsp.locations_to_items = nx.async(function(locations, opts)
  opts = opts or {}
  local encoding = opts.encoding or "utf-16"
  local by_path, order = {}, {}
  for _, loc in ipairs(locations or {}) do
    local uri = loc.uri or loc.targetUri
    local path = nx.utils.uri_to_path(uri)
    local range = loc.range or loc.targetSelectionRange or loc.targetRange
    if path and type(range) == "table" then
      path = nx.utils.normalize(path)
      if not by_path[path] then
        by_path[path] = {}
        order[#order + 1] = path
      end
      table.insert(by_path[path], range)
    end
  end
  table.sort(order)
  local items = {}
  for _, path in ipairs(order) do
    local lines = nx.await(location_lines(path))
    local ranges = by_path[path]
    table.sort(ranges, function(a, b)
      if a.start.line ~= b.start.line then
        return a.start.line < b.start.line
      end
      return a.start.character < b.start.character
    end)
    for _, range in ipairs(ranges) do
      local row = range.start.line
      local text = lines[row + 1] or ""
      local end_row = (range["end"] or range.start).line
      items[#items + 1] = {
        filename = path,
        lnum = row + 1,
        col = encoded_col_to_byte(text, range.start.character, encoding) + 1,
        end_lnum = end_row + 1,
        end_col = encoded_col_to_byte(
          lines[end_row + 1] or "",
          (range["end"] or range.start).character,
          encoding
        ) + 1,
        text = text,
      }
    end
  end
  return items
end)

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

-- Run a code action's `command` (called by the engine when an action is applied),
-- in precedence order: the OFFERING config's own `commands` table, then the global
-- `nx.lsp.commands`, then `workspace/executeCommand` on the client that offered it.
--
-- The per-config table wins because a command name is one server's private
-- vocabulary: two servers on the same buffer can both offer `applyFix` meaning
-- different things, and the config that shipped the handler is the one that knows
-- which. A config declares it exactly like the global registry:
--
-- ```lua
-- nx.lsp.config("ts_ls", {
--   commands = {
--     ["editor.action.showReferences"] = function(command, ctx) … end,
--   },
-- })
-- ```
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
  local client = nx.lsp._clients[client_id]
  if client then
    -- One implementation of the precedence, shared with the `client:exec_cmd` a
    -- config's own command calls — two copies would drift, and the difference
    -- (does MY handler run, or does the server?) is invisible until it misfires.
    client:exec_cmd(command)
    return
  end
  -- No client left, so only a client-side handler can still act. A global one is
  -- reachable without the handle; a per-config one is not (the config is found
  -- through the client's name), which is itself worth saying.
  local handler = nx.lsp.commands[name]
  if handler then
    local ok, err = pcall(handler, command, { client_id = client_id })
    if not ok then
      nx.notify("nx.lsp.commands['" .. name .. "']: " .. tostring(err), vim.log.levels.ERROR)
    end
    return
  end
  nx.notify(
    "nx.lsp: no client to execute '" .. name .. "' (its server is gone)",
    vim.log.levels.ERROR
  )
end

-- ----- engine -> Lua mirror hooks (called by nxvim-server) -------------------
-- The server drives these once per client lifecycle event (runtime.rs:
-- set_lsp_client / run_lsp_on_init / run_lsp_on_exit / remove_lsp_client). They
-- keep `nx.lsp._clients` in sync and run the config's lifecycle hooks.

-- A server finished `initialize`: mirror its handle with the translated provider
-- capabilities (the `*Provider` booleans neovim configs probe) and the position
-- encoding the two sides settled on.
--
-- A config that names an `offset_encoding` overrides the negotiated one here, right
-- after the handle exists — the same write an `on_init` would do, applied for the
-- config that would rather state the encoding than probe for it.
function nx.lsp._set_client(id, name, capabilities, offset_encoding)
  local client = make_client(id, name, capabilities, offset_encoding)
  nx.lsp._clients[id] = client
  local forced = config_of_client(name).offset_encoding
  if type(forced) == "string" and forced ~= "" and forced ~= client.offset_encoding then
    client.offset_encoding = forced
  end
end

-- A server exited: forget its handle, drop it from every buffer's attach set, and
-- drop whatever it was in the middle of. A dead server's half-finished "Indexing
-- 40%" would otherwise sit on the statusline forever — its `end` is never coming.
function nx.lsp._remove_client(id)
  nx.lsp._clients[id] = nil
  nx.lsp._progress[id] = nil
  for _, set in pairs(nx.lsp._attached) do
    set[id] = nil
  end
end

-- The server pushed this client's live `$/progress` tasks (the whole list, already
-- folded from the begin/report/end stream). An empty list clears the slot, so
-- `nx.lsp.progress()` lists only what is running right now.
function nx.lsp._set_progress(id, tasks)
  nx.lsp._progress[id] = (tasks and #tasks > 0) and tasks or nil
end

-- Right after the client is mirrored: run the config's `on_init(client, result)`
-- with the raw `initialize` result, so the hook can read `result.capabilities` /
-- `result.offsetEncoding` and tweak the client.
function nx.lsp._run_on_init(id, result)
  local client = nx.lsp._clients[id]
  if not client then
    return
  end
  local cfg = config_of_client(client.name)
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
  local cfg = config_of_client(client.name)
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

-- ----- applying an edit / jumping to a location by hand ----------------------

-- Apply an LSP `WorkspaceEdit` — the same path a rename reply or a server-initiated
-- `workspace/applyEdit` takes, exposed for a plugin (or an `nx.lsp.commands`
-- handler) holding an edit a server handed it as command arguments.
--
-- `edit` is the protocol shape, either form:
--
-- ```lua
-- nx.lsp.apply_workspace_edit({
--   changes = { ["file:///p/a.rs"] = { { range = r, newText = "bar" } } },
-- })
-- nx.lsp.apply_workspace_edit({
--   documentChanges = {
--     { kind = "create", uri = "file:///p/new.rs" },
--     { textDocument = { uri = "file:///p/new.rs", version = 0 },
--       edits = { { range = r, newText = "fn helper() {}\n" } } },
--   },
-- })
-- ```
--
-- `documentChanges` is applied **in the order given**, resource operations
-- (`create` / `rename` / `delete`) included: those move real files, so they run off
-- the editor tick and settle a moment later — identically in a local, daemon or
-- browser session. Text edits land in the buffers (left modified, written by `:w` /
-- `:wa`) exactly as a rename's do; a `create` is written out for you.
--
-- `opts.encoding` is the position encoding the edit's `character` columns are
-- counted in (`"utf-8"` / `"utf-16"` / `"utf-32"`), defaulting to the protocol's
-- `"utf-16"`. Pass the encoding the server that *authored* the edit negotiated —
-- `nx.lsp.clients({ bufnr = 0 })[1].offset_encoding` — when it isn't utf-16, or every
-- column on a line with a multi-byte character lands in the wrong place. Anything
-- that fails is reported loud, never silently skipped.
function nx.lsp.apply_workspace_edit(edit, opts)
  if type(edit) ~= "table" then
    error("nx.lsp.apply_workspace_edit: edit must be a table, got " .. type(edit), 2)
  end
  local encoding = type(opts) == "table" and opts.encoding or "utf-16"
  nx._lsp_apply_workspace_edit(edit, encoding)
end

-- Jump to an LSP `Location` (or `LocationLink`), opening its file if it isn't
-- already in a buffer — for a location a server hands over, e.g. as a command's
-- `arguments`:
--
-- ```lua
-- nx.lsp.commands["rust-analyzer.gotoLocation"] = function(command)
--   local loc = command.arguments and command.arguments[1]
--   if loc then nx.lsp.show_document(loc) end
-- end
-- ```
--
-- `opts.encoding` is the position encoding the location's columns are in
-- (`"utf-8"` / `"utf-16"` / `"utf-32"`), defaulting to the protocol's `"utf-16"`.
-- A location with no usable URI is an error, not a silent no-op.
function nx.lsp.show_document(location, opts)
  if type(location) ~= "table" then
    error("nx.lsp.show_document: location must be a table, got " .. type(location), 2)
  end
  local uri = location.uri or location.targetUri
  if type(uri) ~= "string" then
    error("nx.lsp.show_document: location has no uri", 2)
  end
  -- `Location.range`, or a `LocationLink`'s selection range (whose `targetRange` is
  -- the whole declaration, while the selection range is the name to land on).
  local range = location.range or location.targetSelectionRange or location.targetRange
  local start = type(range) == "table" and range.start or nil
  local line = type(start) == "table" and start.line or 0
  local character = type(start) == "table" and start.character or 0
  local encoding = type(opts) == "table" and opts.encoding or "utf-16"
  nx._lsp_show_document(uri, line, character, encoding)
end

-- ----- change annotations: asking before applying ---------------------------

-- `nx.lsp._confirm_edit(group, groups)`: the engine parks a workspace edit whose
-- server marked some of its changes `needsConfirmation` and calls this to ask. Each
-- entry of `groups` is `{ label, description, ids }` — one per distinct annotation
-- LABEL (nxvim advertises `groupsOnLabel`), carrying the annotation ids it speaks
-- for. Answers with `nx._lsp_edit_decision(group, accepted_ids)`; the changes tagged
-- with an id that isn't accepted never apply.
--
-- Asked one at a time through `nx.ui.confirm` — a plain yes/no per group, `<Esc>`
-- declining — because that is what the protocol's model is: a group is accepted or
-- declined whole. Nothing blocks: the chain is promises, and the answer arrives on a
-- later tick.
--
-- The chain **always** answers, including when it breaks: a server-initiated
-- `workspace/applyEdit` is a request the server is blocked on until the decision
-- lands, so a rejected confirm (or an error in this chain) reports what was accepted
-- so far rather than leaving the edit — and the server — parked forever. The same
-- reason the file operations have a watchdog.
function nx.lsp._confirm_edit(group, groups)
  local accepted = {}
  local i = 0
  local answered = false
  local function answer()
    if answered then
      return
    end
    answered = true
    nx._lsp_edit_decision(group, accepted)
  end
  local function ask()
    i = i + 1
    local g = groups and groups[i]
    if g == nil then
      answer()
      return
    end
    local label = tostring(g.label or "change")
    -- The description is the server's own longer explanation; it goes in the
    -- question rather than being dropped, since it is the reason to say yes or no.
    local question = g.description and (label .. " — " .. tostring(g.description)) or label
    local asked = nx.ui.confirm("Apply: " .. question .. "?"):next(function(yes)
      if yes then
        for _, id in ipairs(g.ids or {}) do
          accepted[#accepted + 1] = id
        end
      end
      ask()
    end)
    -- On the promise `:next` returned, so it covers both a confirm that rejects and
    -- a throw inside the handler above — either way the decision still goes back.
    asked:catch(function(err)
      nx.notify("nx.lsp: could not ask about a workspace edit: " .. tostring(err), "warn")
      answer()
    end)
  end
  ask()
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
-- As with `format`: `opts.name` is **modeled** — it routes the rename to one of the
-- buffer's attached clients by config name, neovim's own meaning for the key. Its
-- `filter` / `bufnr` are not, and are rejected rather than quietly ignored (nxvim
-- renames the symbol under the cursor in the current buffer).
vim.lsp.buf.rename = function(new_name, opts)
  if opts ~= nil then
    if type(opts) ~= "table" then
      error("vim.lsp.buf.rename: opts must be a table, got " .. type(opts), 2)
    end
    for k in pairs(opts) do
      if k ~= "name" then
        error("vim.lsp.buf.rename: unsupported option '" .. tostring(k) .. "'", 2)
      end
    end
  end
  return nx.lsp.rename(new_name, opts)
end
vim.lsp.buf.document_symbol = nx.lsp.document_symbol
vim.lsp.buf.workspace_symbol = function(query, opts)
  return nx.lsp.workspace_symbol(query, opts)
end
vim.lsp.get_clients = function(filter)
  return nx.lsp.clients(filter)
end
vim.lsp.get_client_by_id = function(id)
  return nx.lsp.client_by_id(id)
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
-- `vim.lsp.util` is the spelling a neovim-shaped plugin reaches for; both entries
-- are the nx verbs above, so the two spellings cannot drift.
vim.lsp.util = vim.lsp.util or {}
vim.lsp.util.apply_workspace_edit = function(edit, encoding)
  -- neovim takes the offset encoding as a second positional argument; nxvim takes it
  -- in `opts`, so it is carried across rather than dropped (dropping it silently
  -- misread every column on a line with a multi-byte character).
  return nx.lsp.apply_workspace_edit(edit, encoding and { encoding = encoding } or nil)
end
vim.lsp.util.show_document = function(location, encoding, opts)
  -- neovim's third argument is `{ reuse_win, focus }`. nxvim's jump is always a focused
  -- `'switchbuf'`-aware one (which is `reuse_win` behavior by default), and there is no
  -- "open it but leave me where I am" on this path — so a caller that asked for
  -- `focus = false` is told rather than quietly focused anyway.
  if type(opts) == "table" and opts.focus == false then
    nx.notify("vim.lsp.util.show_document: `focus = false` is not supported — jumping", "warn")
  end
  return nx.lsp.show_document(location, encoding and { encoding = encoding } or nil)
end
