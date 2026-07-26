-- nx.plugins — nxvim's native package / plugin manager.
--
-- The extensibility half of ADR 0002 / docs/specs/2026-06-11-native-plugin-api.md:
-- there is no third-party plugin-manager layer because the manager is BUILT IN.
-- You DECLARE a set of plugins in init.lua; the manager clones/updates them over
-- the async runtime (`nx.git_local.*` — first-party gix, no `git` binary) and LOADS
-- each one — adds its
-- directory to the runtimepath (`nx._add_rtp`, so `require` and the plugin's
-- `colors/` / `queries/` / `lsp/` resolve without a restart), sources its
-- `plugin/` + `after/plugin/` scripts, and runs its `config` — either EAGERLY at
-- startup or LAZILY on a trigger (`cmd` / `event` / `ft` / `keys`).
--
-- Nothing blocks (ADR 0002 rule 3): every install/source step is a promise, so the
-- UI paints before plugins finish loading. Loaded LAST in the prelude — it builds
-- on nx.git / nx.fs / nx.promise / nx.async / nx.command / nx.keymap / nx.on /
-- nx.notify, all installed above.
--
-- Management runs on the LOCAL disk even in a daemon / remote-config session (plugins
-- load into the local Lua VM via the local runtimepath): the clone / discover / source
-- ops go through the local-always seams (`lfs` / `lgit` below), NOT the session's
-- `nx.fs` / `nx.git`. See docs/plans/2026-07-03-remote-aware-plugin-manager.md.

nx.plugins = nx.plugins or {}
local M = nx.plugins

-- ----- state -----------------------------------------------------------------

M._specs = M._specs or {} -- name -> normalized spec
M._order = M._order or {} -- declaration order (names), for deterministic sync
M._loaded = M._loaded or {} -- name -> true once fully loaded (config ran)
M._loading = M._loading or {} -- name -> true while a load is in flight (cycle guard)

-- The startup "all my non-lazy plugins are ready" signal — see `maybe_fire_plugins_loaded`.
M._eager_pending = M._eager_pending or 0 -- eager (non-lazy) loads currently in flight
M._plugins_loaded_fired = M._plugins_loaded_fired or false -- `PluginsLoaded` fired once
M._vim_entered = M._vim_entered or false -- the startup point (VimEnter) has passed

-- The SYSTEM-PLUGIN TIER: `name -> { name = , dir = }` for plugins the client seeds into
-- every session (before `init.lua`), plus any promoted at runtime via `M.system` / `M.promote`.
-- Kept SEPARATE from `M._specs`/`M._order` so the managed verbs (sync/update/clean) never
-- touch a system plugin — a system plugin is never a dangling managed clone. The server
-- reads `M._system_dirs()` to skip these when it sources the runtimepath (they are sourced
-- in the dedicated phase before `init.lua` / by `M.system`), so each loads exactly once. See
-- docs/plans/2026-07-05-remote-connectors-and-system-plugins.md → §A.
M._system = M._system or {} -- name -> { name = , dir = }

-- Per-plugin operation state, so a UI can render LIVE progress (a spinner while a
-- clone/pull runs, a ✓/✗ when it finishes).
-- `name -> { op = "install"|"update", state = "running"|"done"|"error", msg = <human text> }`.
-- The git verbs below set entries here; the manager UI (prelude/plugins_ui.lua) reads
-- them and subscribes via
-- `M.on_change`. Survives a re-source (`or {}`) so an open UI keeps its history.
M._tasks = M._tasks or {}
M._watchers = M._watchers or {} -- on_change callbacks, fired on any state transition

-- ----- local-always fs + proc (plugin management is a LOCAL concern) ----------
-- Plugin management runs on the LOCAL disk even in a daemon / remote-config session:
-- clones land under `stdpath("data")/plugins` and load into THIS (local) Lua VM via the
-- local runtimepath (`nx._add_rtp` + `require`, both local). So the manager clones,
-- discovers, and sources through the local-always seams — `nx._local_fs_op` /
-- `nx._local_system_async` — never the session's `nx.fs` / `nx.run`, which in an edit-host
-- session route to the *remote* daemon (where the plugin would be cloned but never loaded).
-- Runtime plugin code is untouched: a loaded plugin's own `nx.fs` / `nx.run` still route to
-- the session (remote), because it edits the remote's files. See
-- docs/plans/2026-07-03-remote-aware-plugin-manager.md.
-- The manager's fs + git use the public LOCAL-ALWAYS seams: `nx.fs_local` and
-- `nx.git_local`, the twins of `nx.fs` / `nx.git` forced onto the client disk. A plugin
-- loads into THIS (local) Lua VM via the local runtimepath, so the manager must clone /
-- discover / source locally even in a daemon session (the session's `nx.fs` / `nx.git`
-- route to the *remote*, where the plugin would be cloned but never loaded). Runtime
-- plugin code that edits the remote's files still uses the session-routed `nx.fs`.
local lfs = nx.fs_local
local lgit = nx.git_local

-- Fire every change watcher (a load completing, a task transitioning). Wrapped in
-- pcall so one bad observer can't break the manager. The UI uses this to re-render.
local function notify_change()
  for _, fn in ipairs(M._watchers) do
    pcall(fn)
  end
end

-- M.on_change(fn) — subscribe to manager state changes (task transitions, plugin
-- loads). Returns an unsubscribe function. The manager UI re-renders on each call.
function M.on_change(fn)
  M._watchers[#M._watchers + 1] = fn
  return function()
    for i, f in ipairs(M._watchers) do
      if f == fn then
        table.remove(M._watchers, i)
        return
      end
    end
  end
end

-- Record an operation's state for `name` and notify watchers. `state == "done"`
-- finalizes with a result word (the spinner stops); "error" keeps the captured
-- message so the UI can show why.
local function set_task(name, op, state, msg)
  M._tasks[name] = { op = op, state = state, msg = msg }
  notify_change()
end
M._opts = M._opts
  or {
    -- Where clones land. One dir per plugin under here; the manager owns this
    -- tree (`:PluginClean` prunes it), so keep it OUT of the user's config repo.
    root = nil, -- resolved lazily (stdpath("data")/plugins) on first use
    -- "owner/repo" shorthand expands through this. `%s` is the shorthand.
    github = "https://github.com/%s.git",
  }

-- The install root, resolved lazily so a test can override it via setup_manager{}
-- before first use without depending on the host's data dir.
local function root()
  if not M._opts.root then
    M._opts.root = vim.fn.stdpath("data") .. "/plugins"
  end
  return M._opts.root
end

-- The user's config directory — where the first-run setup writes the managed
-- `lua/plugins.lua` and points `init.lua` at it. Overridable via setup_manager{}
-- (a test points it at a temp dir; a user could relocate it).
local function config_dir()
  if not M._opts.config then
    M._opts.config = vim.fn.stdpath("config")
  end
  return M._opts.config
end

-- The LOCKFILE path: `<config>/nxvim-lock.json`, beside the `lua/plugins.lua` the
-- first-run setup writes. It lives in the CONFIG dir, not the manager-owned install
-- root, because it is part of the user's config — the file they commit alongside
-- `init.lua` so another machine reproduces the same plugin commits. (The install root is
-- a cache `:PluginClean` prunes; a lockfile there would be lost with it.) Overridable via
-- setup_manager{ lockfile = }, mirroring `root` / `config` / `system`.
local function lockfile_path()
  return M._opts.lockfile or (config_dir() .. "/nxvim-lock.json")
end

-- The client-owned SYSTEM-PLUGIN DIR: `stdpath("data")/system`, one plugin repo per
-- subdir. `M.system` / `M.promote` clone/copy a plugin here so the client re-seeds it
-- into every future session; the native client scans it at startup
-- (`nxvim_server::discover_system_plugins`). Overridable via setup_manager{ system = }
-- (a test points it at a temp dir), mirroring `root` / `config`.
local function system_dir()
  if not M._opts.system then
    M._opts.system = vim.fn.stdpath("data") .. "/system"
  end
  return M._opts.system
end

-- ----- spec normalization ----------------------------------------------------

-- Forward declarations — these helpers are defined further down but referenced by
-- the functions just below, so they must be in lexical scope as locals here.
local aslist, aslist_triggers

-- The basename of a git source or local dir, sans a trailing ".git" — the plugin's
-- default `name` (its directory under the install root, and its require key).
-- Exported as `M._source_name` so the dashboard (plugins_ui.lua) labels raw specs
-- with exactly the name normalize() would install them under.
local function basename(s)
  return nx.utils.basename((s:gsub("%.git$", "")))
end
M._source_name = basename

-- True for a string that already names a transport (a full URL or scp-form
-- `git@host:owner/repo`), as opposed to the "owner/repo" GitHub shorthand.
local function is_full_url(s)
  return s:match("^%a[%w+.-]*://") ~= nil or s:match("^[^/]+@[^/]+:") ~= nil
end

-- A hook (config/init) may be given as a function OR as a STRING of Lua source.
-- The string form exists so the recommended set can be written back out to
-- `plugins.lua` (a function can't be serialized); here we compile it to a function
-- so the live manager only ever deals with functions. Fails loud on bad source.
local function as_fn(v, what)
  if type(v) == "string" then
    local f, err = loadstring(v, "@nx.plugins/" .. what)
    if not f then
      error("nx.plugins: invalid " .. what .. " source: " .. tostring(err), 0)
    end
    return f
  end
  return v
end

-- Normalize one declared spec (a string shorthand or a table) into the internal
-- record every later step reads. Fails loud on a spec naming neither a source nor
-- a local `dir` — a silent skip would make a typo look like a working install.
local function normalize(spec)
  if type(spec) == "string" then
    spec = { spec }
  elseif type(spec) ~= "table" then
    error("nx.plugins: a spec must be a string or table, got " .. type(spec), 0)
  end

  local src = spec.src or spec.url or spec[1]
  if not src and not spec.dir then
    error('nx.plugins: a spec needs a source ("owner/repo"/url) or a local `dir`', 0)
  end

  -- A local-dev `dir` may use `~`/`~/` for the home directory; expand it once here
  -- so every later step (the require key, the rtp entry) sees an absolute path.
  local dir = spec.dir and nx.utils.expanduser(spec.dir) or nil

  local name = spec.name or (src and basename(src)) or basename(dir)
  local url = src and (is_full_url(src) and src or M._opts.github:format(src)) or nil

  -- `commit`/`tag`/`version` all pin; `commit` wins. A pin is never auto-updated.
  local commit = spec.commit
  local tag = spec.tag or spec.version

  -- Init/update the plugin's git submodules on install & update. DEFAULT ON —
  -- submodule-bearing plugins are common and an un-recursed clone silently ships a
  -- broken plugin (its vendored deps missing). Opt out with `submodules = false`
  -- for a plugin you know has none, to skip the extra git call. (A dev `dir` plugin
  -- is never cloned, so this only affects managed clones.)
  local submodules = spec.submodules ~= false

  -- Dependencies: declare each (so it is installable) and remember its name. A
  -- dependency is loaded before its dependent.
  local deps = {}
  for _, d in ipairs(spec.dependencies or spec.deps or {}) do
    deps[#deps + 1] = M.add({ d }) -- add() returns the (single) registered name
  end

  -- Lazy when explicitly asked, or implied by any trigger (unless lazy=false).
  local triggers = {
    cmd = aslist(spec.cmd),
    event = aslist(spec.event),
    ft = aslist(spec.ft),
    keys = aslist(spec.keys),
  }
  local has_trigger = #triggers.cmd + #triggers.event + #triggers.ft + #triggers.keys > 0
  local lazy = spec.lazy
  if lazy == nil then
    lazy = has_trigger
  end

  return {
    name = name,
    url = url,
    -- The author's own one-line description, kept verbatim: the manager never reads
    -- it, but the dashboard shows it when a row is expanded (and the recommended-set
    -- checklist labels its entries with it), so it must survive normalization.
    desc = spec.desc,
    branch = spec.branch,
    commit = commit,
    tag = tag,
    submodules = submodules,
    -- Resolved install directory: an explicit `dir` (a local/dev checkout, never
    -- cloned; `~` already expanded) or root()/name.
    dir = dir, -- local-dev marker (nil for a managed clone)
    _dir = dir or (root() .. "/" .. name),
    enabled = spec.enabled,
    config = as_fn(spec.config, "config"),
    init = as_fn(spec.init, "init"),
    lazy = lazy,
    _triggers = triggers,
    _deps = deps,
  }
end

-- Coerce nil / a scalar / a list into a list (a shallow copy, so we never alias
-- the caller's table).
aslist = function(v)
  if v == nil then
    return {}
  elseif type(v) == "table" and not v.lhs then
    -- A plain list (not a single { lhs=, mode= } keys entry).
    local out = {}
    for i = 1, #v do
      out[i] = v[i]
    end
    return out
  else
    return { v }
  end
end

-- enabled may be a boolean or a predicate; default on.
local function enabled(spec)
  local e = spec.enabled
  if type(e) == "function" then
    return e() ~= false
  end
  return e ~= false
end

-- ----- loading (rtp + plugin/ scripts + config) ------------------------------

-- Every `*.lua` under `dir`, recursing, sorted by path — a deterministic,
-- neovim-like `plugin/` load order. Returns a promise of the path list.
local function collect_lua(dir)
  return nx.async(function()
    local out = {}
    local function walk(d)
      if not nx.await(lfs.exists(d)) then
        return
      end
      local entries = nx.await(lfs.readdir(d))
      table.sort(entries, function(a, b)
        return a.name < b.name
      end)
      for _, e in ipairs(entries) do
        local p = d .. "/" .. e.name
        if e.type == "directory" then
          walk(p)
        elseif e.name:sub(-4) == ".lua" then
          out[#out + 1] = p
        end
      end
    end
    walk(dir)
    return out
  end)()
end

-- Source a plugin's `plugin/` then `after/plugin/` Lua scripts, in path order —
-- the package-load step that lets a classic plugin wire its commands / autocmds
-- without an explicit `require`. Reads each file off the tick (`nx.fs.read_text`)
-- and runs it on the editor thread (`loadstring`), so no blocking IO. A script
-- that fails to compile or errors is reported LOUD (never a silent skip) and does
-- not abort the rest.
local function source_runtime(dir)
  return nx.async(function()
    for _, sub in ipairs({ "plugin", "after/plugin" }) do
      local files = nx.await(collect_lua(dir .. "/" .. sub))
      for _, f in ipairs(files) do
        local content = nx.await(lfs.read_text(f))
        local chunk, lerr = loadstring(content, "@" .. f)
        if not chunk then
          nx.notify("nx.plugins: cannot load " .. f .. ": " .. tostring(lerr), 4)
        else
          local ok, rerr = pcall(chunk)
          if not ok then
            nx.notify("nx.plugins: error sourcing " .. f .. ": " .. tostring(rerr), 4)
          end
        end
      end
    end
  end)()
end

-- Run a user hook (`config` / `init`) that may be a PLAIN or an ASYNC function.
-- Wrapping it in `nx.async` makes both shapes work uniformly: a plain body runs to
-- completion immediately (the promise resolves at once), while a body that
-- `nx.await`s — reads a file, shells out — runs in its own coroutine and
-- suspends/resumes. Returns a promise of the hook's completion (rejected if it
-- errors), so a caller can await it or just report a failure. (Without this, an
-- async `init` armed synchronously at declaration would error — there is no
-- coroutine to suspend.)
local function run_hook(fn)
  return nx.async(fn)()
end

-- The shada namespace the manager assigns to the plugin installed at `dir` (a
-- runtimepath entry): the registered `name` of the spec whose `_dir` is `dir`, or
-- `nil` when no managed plugin lives there. `nx.shada.plugin` consults this so a
-- manager-loaded plugin keys its store on its canonical `name` — which a `name = …`
-- spec can set apart from the directory basename — while a plugin loaded outside the
-- manager falls back to its directory name. Trailing separators are trimmed so
-- `…/foo` and `…/foo/` match.
function M._namespace_for(dir)
  local trim = function(p)
    return (p:gsub("[/\\]+$", ""))
  end
  local target = trim(dir)
  for name, spec in pairs(M._specs) do
    if spec._dir and trim(spec._dir) == target then
      return name
    end
  end
  return nil
end

-- Load a plugin by name: dependencies first, then put it on the runtimepath, source
-- its `plugin/` scripts, and run its `config`. Idempotent (a second call is a
-- no-op) and cycle-safe (a `_loading` guard breaks dependency loops). Returns a
-- promise resolving `true` when newly loaded, `false` if already loaded / in
-- flight; REJECTS (loud) if the plugin is not installed.
function M.load(name)
  if M._loaded[name] or M._loading[name] then
    return nx.promise.resolve(false)
  end
  local spec = M._specs[name]
  if not spec then
    return nx.promise.reject({ message = "nx.plugins: unknown plugin '" .. tostring(name) .. "'" })
  end
  M._loading[name] = true
  local loading = nx.async(function()
    for _, dep in ipairs(spec._deps) do
      nx.await(M.load(dep))
    end
    local present = spec.dir ~= nil or nx.await(lfs.exists(spec._dir))
    if not present then
      error("nx.plugins: '" .. name .. "' is not installed — run :PluginSync", 0)
    end
    nx._add_rtp(spec._dir)
    nx.await(source_runtime(spec._dir))
    if spec.config then
      -- Await the hook so "loaded" means config finished — an async config that
      -- loads data is not ready until then. A failure is reported, not fatal: the
      -- `:catch` recovers the rejection so the load still completes.
      nx.await(run_hook(spec.config):catch(function(err)
        nx.notify("nx.plugins[" .. name .. "].config: " .. tostring(err and err.message or err), 4)
      end))
    end
    M._loaded[name] = true
    notify_change() -- a UI watching load state repaints this plugin as loaded
    -- Per-plugin "this one finished loading" hook: fires for eager AND lazy plugins
    -- (both route through here), the moment the plugin — its `plugin/` scripts and
    -- `config`, async config awaited above — is fully ready. The event's `pattern`
    -- is the plugin name (so `nx.on("PluginLoaded", { pattern = name }, …)` targets
    -- one), and `args.data.name` carries it too.
    nx.autocmd.exec("PluginLoaded", { pattern = name, data = { name = name } })
    return true
  end)()
  -- The in-flight guard must drop on EVERY exit — success, "not installed", a
  -- dependency's rejection re-raised by the await, a runtime-source failure.
  -- Clearing it only on the happy paths wedged the plugin forever: each later
  -- trigger saw `_loading` truthy and silently resolved `false`, so a load could
  -- never be retried (e.g. after installing the missing dependency).
  return loading:finally(function()
    M._loading[name] = false
  end)
end

-- Load `name` and report a rejection (uninstalled / load error) on the message
-- line — the fire-and-forget entry the lazy triggers and eager activation use.
-- Returns the load promise (already `:catch`ed, so it resolves rather than rejects on a
-- failed load). An event/filetype trigger hands it back to the autocmd dispatcher so
-- the settle protocol waits for the load — including an *async* `config` — and then
-- replays the event to the handlers that config registered. Without returning it, the
-- fire sees no pending promise, never arms a replay, and the plugin's own `FileType`
-- handler misses the very buffer that woke it.
local function load_reporting(name)
  return M.load(name):catch(function(err)
    nx.notify(tostring(err and err.message or err), 4)
  end)
end

-- ----- lazy triggers ---------------------------------------------------------

-- Arm a lazy plugin's triggers: the first matching command / event / filetype /
-- keypress loads it, and the trigger then makes sure the plugin still sees what woke
-- it. Each trigger kind does that differently:
--
--   * `cmd` re-dispatches the original invocation, with its args, against the real
--     command the plugin just defined (it replaced this stub);
--   * `keys` feeds the key back through the typeahead so the plugin's own mapping
--     handles it;
--   * `event` / `ft` **return the load promise**, so the autocmd dispatcher's settle
--     protocol waits for the load — including an *async* `config` — and then replays
--     the event to the handlers that config registered. A hot-path `event` is the one
--     exception (returning a promise from one raises), so there the plugin's handler
--     catches the next occurrence instead of this one.
--
-- `init` (the always-run hook) fires here, at startup, regardless of when the body
-- loads.
local function arm_lazy(spec)
  local name = spec.name
  if spec.init then
    -- init runs at startup regardless of when the body loads; fire-and-forget
    -- (we don't block declaration on it), accepting a plain or async function,
    -- with errors reported.
    run_hook(spec.init):catch(function(err)
      nx.notify("nx.plugins[" .. name .. "].init: " .. tostring(err and err.message or err), 4)
    end)
  end

  for _, c in ipairs(spec._triggers.cmd) do
    nx.command(c, function(o)
      M.load(name)
        :next(function()
          -- Re-dispatch the original invocation against the real command the
          -- plugin just defined (it replaced this stub).
          nx.cmd(c .. (o.bang and "!" or "") .. (o.args ~= "" and (" " .. o.args) or ""))
        end)
        :catch(function(err)
          nx.notify(tostring(err and err.message or err), 4)
        end)
    end, { nargs = "*", desc = "Lazy-load " .. name .. ", then run :" .. c })
  end

  for _, ev in ipairs(spec._triggers.event) do
    nx.on(ev, {}, function()
      -- A hot-path event (`BufEnter`, `CursorMoved`, …) must not be handed a promise —
      -- it fires per keypress and the dispatcher raises on one — so there the load is
      -- fire-and-forget and the plugin's own handler catches the *next* occurrence
      -- rather than this one. Every other event returns the promise and gets the replay.
      if nx._hot_events[ev] then
        load_reporting(name)
        return
      end
      return load_reporting(name)
    end)
  end

  if #spec._triggers.ft > 0 then
    nx.on("FileType", { pattern = spec._triggers.ft }, function()
      -- `FileType` is non-hot, so returning the promise is what makes an ft-lazy plugin
      -- with an async `config` actually receive the event it was woken by.
      return load_reporting(name)
    end)
  end

  for _, k in ipairs(spec._triggers.keys) do
    local mode = "n"
    local lhs = k
    if type(k) == "table" then
      lhs = k.lhs or k[1]
      mode = k.mode or k[2] or "n"
    end
    nx.keymap.set(mode, lhs, function()
      M.load(name)
        :next(function()
          nx._feedkeys(lhs, true, false) -- remap=true so the plugin's mapping fires
        end)
        :catch(function(err)
          nx.notify(tostring(err and err.message or err), 4)
        end)
    end, { desc = "Lazy-load " .. name })
  end
end

-- Fire `PluginsLoaded` exactly once, when every eager (non-lazy) plugin that began
-- loading at startup has settled AND the startup point (`VimEnter`) has passed. This
-- is the "all my plugins are ready" hook a config taps to run *after* every eager
-- plugin's `config` — an async config is awaited before its load counts as settled
-- (see `M.load`), so this waits on those too. Gated on `VimEnter` so the transient
-- moments the pending count touches zero *between* successive `nx.plugins{…}` calls
-- during config sourcing don't fire it early; by `VimEnter` every config-declared
-- spec is registered. It fires once — an eager plugin a later `:PluginSync` installs
-- still emits its own per-plugin `PluginLoaded`, but does not re-fire this.
local function maybe_fire_plugins_loaded()
  if M._plugins_loaded_fired or not M._vim_entered or M._eager_pending > 0 then
    return
  end
  M._plugins_loaded_fired = true
  nx.autocmd.exec("PluginsLoaded", {})
end
nx._maybe_fire_plugins_loaded = maybe_fire_plugins_loaded

-- Load every enabled, eager (non-lazy), already-installed plugin that is not yet
-- loaded — at startup and again after a sync brings new ones onto disk.
local function activate_eager()
  for _, name in ipairs(M._order) do
    local spec = M._specs[name]
    if enabled(spec) and not spec.lazy and not M._loaded[name] then
      -- Count this load as in flight for the persisted-view restore coordinator: an eager
      -- plugin's `config` may register an `nx.view.on_restore` handler on a later tick, so a
      -- reserved view slot it owns must not be reaped as an orphan until this load settles.
      -- (Balanced by the `:finally` below, whichever way the load resolves.) See
      -- `nx._maybe_collapse_view_restores`.
      nx._view_restore_pending_loads = (nx._view_restore_pending_loads or 0) + 1
      -- Same in-flight accounting for the `PluginsLoaded` signal (fired when this hits 0).
      M._eager_pending = M._eager_pending + 1
      nx.async(function()
        if spec.dir ~= nil or nx.await(lfs.exists(spec._dir)) then
          nx.await(M.load(name))
        end
      end)()
        :catch(function(err)
          nx.notify(tostring(err and err.message or err), 4)
        end)
        :finally(function()
          nx._view_restore_pending_loads = math.max(0, (nx._view_restore_pending_loads or 1) - 1)
          if nx._maybe_collapse_view_restores then
            nx._maybe_collapse_view_restores()
          end
          M._eager_pending = math.max(0, M._eager_pending - 1)
          maybe_fire_plugins_loaded()
        end)
    end
  end
end

-- ----- declaration ------------------------------------------------------------

-- Register one or more plugin specs. Accepts a single spec (string or table) or a
-- list of them. Each is normalized and stored; a lazy spec's triggers are armed
-- (and its `init` run) immediately, while an enabled eager spec that is already on
-- disk begins loading. Returns the name of the LAST spec added (so a single-spec
-- `add` — as dependency registration uses — yields that dependency's name).
function M.add(specs)
  -- A bare spec (string, or a table that is itself a spec rather than a list of
  -- them) is wrapped into a one-element list. A list of specs has a [1] that is
  -- itself a string/spec-table without our marker keys.
  local list = specs
  if type(specs) == "string" then
    list = { specs }
  elseif
    type(specs) == "table"
    and (specs.src or specs.url or specs.dir or specs.name or type(specs[1]) == "string")
  then
    -- A single table spec, not a list. The canonical form carries its source at
    -- [1] (e.g. `{ "owner/repo", cmd = "X" }`), so a string [1] marks a lone spec;
    -- a list of specs has spec *tables* at its numeric indices instead. Without
    -- this, a single positional spec was iterated as a list and its named fields
    -- (cmd/config/keys/dependencies) were silently dropped.
    list = { specs }
  end

  local last
  for _, raw in ipairs(list) do
    local spec = normalize(raw)
    if not M._specs[spec.name] then
      M._order[#M._order + 1] = spec.name
    end
    M._specs[spec.name] = spec
    last = spec.name
    if enabled(spec) then
      if spec.lazy then
        arm_lazy(spec)
      else
        arm_lazy({ name = spec.name, init = spec.init, _triggers = aslist_triggers() }) -- run init only
      end
    end
  end
  -- Kick eager activation for anything already installed (no-op for the not-yet-
  -- cloned, which :PluginSync will pick up).
  activate_eager()
  return last
end

-- The empty trigger set an eager spec's init-only arm passes (no cmd/event/ft/keys).
aslist_triggers = function()
  return { cmd = {}, event = {}, ft = {}, keys = {} }
end

-- nx.plugins(specs) == nx.plugins.add(specs): a callable namespace, so init.lua
-- reads `nx.plugins { {"owner/repo"}, ... }`.
setmetatable(M, {
  __call = function(_, specs)
    return M.add(specs)
  end,
})

-- nx.plugins.setup_manager{ root=, config=, system=, lockfile=, github= }: override the
-- install root (a test points it at a temp dir), the config dir, the system-plugin dir,
-- the lockfile path (default `<config>/nxvim-lock.json`), or the shorthand URL template.
-- Merges onto the defaults. (Named `setup_manager`, not `setup`, so it reads distinctly from a
-- plugin's own `require("plugin").setup{}` and never looks like the call that
-- declares plugins — that is `nx.plugins{...}` / `nx.plugins.add`.)
function M.setup_manager(opts)
  opts = opts or {}
  for k, v in pairs(opts) do
    M._opts[k] = v
  end
end

-- ----- git operations ---------------------------------------------------------

-- Forward declarations — the lockfile helpers below are defined further down (with the
-- rest of the lockfile machinery, which reads better as one block) but are referenced by
-- the install / update verbs just below, so they must be in lexical scope as locals here.
local lock_applies, checkout_deep

-- Await one `nx.git_local.*` promise for plugin `name`'s `op` task (`verb` names the
-- git action for the message). On a reject — every git verb rejects with a `{ code,
-- message }` — mark the task errored and re-raise loud (a failed step must never look
-- like a half-installed success). Returns the resolved value on success.
local function git_step(name, op, verb, promise)
  local ok, res = pcall(nx.await, promise)
  if not ok then
    local msg = (type(res) == "table" and res.message) or tostring(res)
    set_task(name, op, "error", msg)
    error("nx.plugins: git " .. verb .. " failed for " .. name .. ": " .. msg, 0)
  end
  return res
end

-- Init + recursively check out `name`'s submodules to the commits its tree pins (like
-- `git submodule update --init --recursive`). gix's clone does NOT recurse submodules,
-- so this is a distinct step — a no-op for a plugin with none.
local function update_submodules(name, op, dir)
  git_step(
    name,
    op,
    "submodule update",
    lgit.submodule_update(dir, { init = true, recursive = true })
  )
end

-- Clone `name` if missing. Resolves a status string: "local" (a `dir` dev plugin,
-- never cloned), "exists" (already on disk), or "installed". REJECTS on a git
-- failure (loud — a failed clone must not look like a success).
function M._install(name)
  local spec = M._specs[name]
  return nx.async(function()
    if spec.dir ~= nil then
      return "local"
    end
    if nx.await(lfs.exists(spec._dir)) then
      return "exists"
    end
    set_task(name, "install", "running", "cloning")
    nx.await(lfs.mkdir(root(), { recursive = true }))

    -- Which exact commit to land on, if any. Precedence, highest first:
    --
    --   1. `spec.commit`  — a hand-written pin is an INSTRUCTION.
    --   2. the lockfile   — a recorded commit, which outranks the floating refs below
    --                       precisely because pinning a floating ref is what it is for —
    --                       but only while it still records what the spec ASKS for, see
    --                       `lock_applies`.
    --   3. `spec.tag` / `spec.version`, then `spec.branch` — checked out as a ref by the
    --      clone itself (`opts.branch`), not resolved to a commit here.
    --   4. the remote's default branch.
    --
    -- Reading the lockfile here (rather than once per verb) costs one small off-tick read
    -- on the path that is about to do a network clone, and keeps a direct `_install` call
    -- as correct as one driven by `install()` — no "did someone load the lock first?"
    -- ordering to get wrong.
    local entry = nx.await(M._read_lock())[name]
    local locked = lock_applies(spec, entry) and entry.commit or nil
    local pin = spec.commit or locked

    -- Shallow (depth 1) unless we must reach an arbitrary pinned commit, which a
    -- shallow clone may not contain; a branch/tag pins the ref to check out. (git's
    -- `--filter=blob:none` has no gix analog — the shallow depth gives the same win.)
    local opts = { branch = spec.branch or spec.tag }
    if not pin then
      opts.depth = 1
    end
    git_step(name, "install", "clone", lgit.clone(spec.url, spec._dir, opts))
    if pin then
      -- Detach onto the pinned commit (the full clone above guarantees it's present).
      -- A commit the remote no longer has (force-pushed away) fails LOUD rather than
      -- quietly leaving the clone at the tip: silently installing something other than
      -- what the lockfile names would defeat the point of having one. When the pin came
      -- from the lock, say so and name the remedy — the message is the only place the
      -- user can learn which file to fix.
      local ok, err = pcall(
        git_step,
        name,
        "install",
        "checkout " .. pin,
        lgit.checkout(spec._dir, pin, { detach = true })
      )
      if not ok then
        local msg = (type(err) == "table" and err.message) or tostring(err)
        if spec.commit == nil then
          local e = {
            code = "ELOCK",
            message = "nx.plugins: "
              .. name
              .. " is locked at "
              .. pin
              .. " but that commit is unreachable ("
              .. msg
              .. "). Remove its entry from "
              .. lockfile_path()
              .. " (or re-lock) to install the current revision.",
          }
          error(e, 0)
        end
        error(err, 0)
      end
    end
    if spec.submodules then
      update_submodules(name, "install", spec._dir)
    end
    set_task(name, "install", "done", "installed")
    return "installed"
  end)()
end

-- Realize a spec's explicit `commit` / `tag` pin — check the plugin out at it unless the
-- checkout is already there. A pin is an INSTRUCTION, so it has to reach the tree even
-- when the declaration changed under an existing install: a `tag = "v2"` bump that never
-- moved the checkout leaves the config saying one thing and the editor running another,
-- silently and with nothing to show it (head and lock agree, so not even `drifted`).
--
-- A `commit` is compared against the checked-out sha directly. A `tag` cannot be — there
-- is no rev-parse verb, and resolving one means moving the checkout — so the lock entry's
-- recorded `tag` stands in as the record of which pin THIS checkout was made under, the
-- same record `lock_applies` invalidates a stale entry against.
local function converge_pin(name, spec, entry)
  return nx.async(function()
    local target, at_it
    if spec.commit then
      local head = git_step(name, "update", "head", lgit.head(spec._dir))
      target, at_it = spec.commit, head.sha == spec.commit
    else
      target, at_it = spec.tag, entry ~= nil and entry.tag == spec.tag
    end
    if at_it then
      return "pinned"
    end
    set_task(name, "update", "running", "checking out " .. target)
    if not nx.await(checkout_deep(name, "update", spec._dir, target)) then
      set_task(name, "update", "error", "cannot reach " .. target)
      local e = {
        code = "EPIN",
        message = "nx.plugins: "
          .. name
          .. " is pinned at "
          .. target
          .. " but that revision is unreachable (gone from the remote?).",
      }
      error(e, 0)
    end
    if spec.submodules then
      update_submodules(name, "update", spec._dir)
    end
    set_task(name, "update", "done", "repinned")
    return "repinned"
  end)()
end

-- Realize the lockfile's commit for `name` — the `cargo build` half. An install that is
-- already there is left alone; one that has drifted (a colleague's newer lockfile, a local
-- `:PluginUpdate` being rolled back by re-syncing) is moved onto it, deepening a shallow
-- clone if the commit isn't present. Reproducing the lock only for clones that happen to
-- be MISSING would make `:PluginSync` honour the file by accident of what is on disk.
local function converge_lock(name, spec, entry)
  return nx.async(function()
    local head = git_step(name, "update", "head", lgit.head(spec._dir))
    if head.sha == entry.commit then
      return "locked"
    end
    set_task(name, "update", "running", "restoring " .. entry.commit:sub(1, 7))
    if not nx.await(checkout_deep(name, "update", spec._dir, entry.commit)) then
      set_task(name, "update", "error", "cannot reach " .. entry.commit:sub(1, 7))
      local e = {
        code = "ELOCK",
        message = "nx.plugins: "
          .. name
          .. " is locked at "
          .. entry.commit
          .. " but that commit is unreachable (gone from the remote?). Remove its entry from "
          .. lockfile_path()
          .. " (or re-lock) to move it.",
      }
      error(e, 0)
    end
    if spec.submodules then
      update_submodules(name, "update", spec._dir)
    end
    set_task(name, "update", "done", "relocked")
    return "relocked"
  end)()
end

-- Bring `name` in line with what its spec and the lockfile ask for. Resolves a status word:
--
-- ```
-- "local"     a dev `dir` plugin — never touched
-- "missing"   not installed
-- "pinned"    already at the spec's commit/tag
-- "repinned"  MOVED onto the spec's commit/tag (the declaration changed under it)
-- "locked"    already at the commit the lockfile records
-- "relocked"  MOVED onto the commit the lockfile records
-- "updated"   fast-forwarded
-- "uptodate"  already at its branch tip
-- ```
--
-- The three MOVED words are the ones the verbs count.
--
-- `opts.advance` decides what a LOCKFILE entry means, which is the whole install-vs-update
-- split (the `cargo build` / `cargo update` distinction):
--
--   * without it, a locked plugin is put (or left) exactly where the lockfile says — this
--     is what `sync` uses, so realizing the declared state never silently moves a plugin
--     past the revision the lockfile records;
--   * with it (the explicit `:PluginUpdate`), the lock is deliberately advanced past.
--
-- Otherwise a lock would be a permanent freeze with no in-editor way out. A spec `commit`
-- or `tag` pin outranks both modes — neither verb moves a plugin past what it names.
function M._update(name, opts)
  local spec = M._specs[name]
  local advance = opts ~= nil and opts.advance == true
  return nx.async(function()
    if spec.dir ~= nil then
      return "local"
    end
    if not nx.await(lfs.exists(spec._dir)) then
      return "missing"
    end
    local entry = nx.await(M._read_lock())[name]
    if spec.commit or spec.tag then
      return nx.await(converge_pin(name, spec, entry))
    end
    if not advance and lock_applies(spec, entry) then
      return nx.await(converge_lock(name, spec, entry))
    end
    -- A plugin installed FROM THE LOCKFILE sits on a detached commit, and `pull`
    -- fast-forwards the current *branch* — on a detached HEAD it rejects outright. So
    -- re-attach first: `update` is the deliberate "move forward and re-record" verb (the
    -- `cargo update` half of the split — `install`/`sync` REPRODUCE the lock, `update`
    -- ADVANCES it), so a lock pin must not become a permanent freeze.
    --
    -- The branch to return to is the one the plugin tracks: the spec's if declared, else
    -- what the lockfile recorded (carried across the detaching install by `resolve_lock`).
    -- With neither we cannot know, so fail loud naming the fix rather than guessing a
    -- branch name and silently moving the plugin somewhere unintended.
    local head = git_step(name, "update", "head", lgit.head(spec._dir))
    local before = head.sha
    if head.detached then
      local branch = spec.branch or (entry or {}).branch
      if not branch then
        set_task(name, "update", "error", "detached, no branch to reattach to")
        local e = {
          code = "EDETACHED",
          message = "nx.plugins: "
            .. name
            .. " is on a detached commit and no branch is recorded for it — set `branch = "
            .. '"…"` in its spec (or remove its entry from '
            .. lockfile_path()
            .. ") to update it.",
        }
        error(e, 0)
      end
      set_task(name, "update", "running", "reattaching to " .. branch)
      git_step(name, "update", "checkout " .. branch, lgit.checkout(spec._dir, branch))
    end
    set_task(name, "update", "running", "pulling")
    -- `pull` is fast-forward-only (rejects `ENOTFF` on a divergence) and resolves
    -- `{ updated, sha }` — `updated` is false when the branch was already current.
    local res = git_step(name, "update", "pull", lgit.pull(spec._dir))
    -- What makes this an update is whether the CHECKOUT moved, not whether the pull did.
    -- Advancing past a lock happens in the re-attach above — which puts the worktree on the
    -- branch tip — leaving the pull that follows a no-op. Trusting `res.updated` therefore
    -- reported "updated 0 plugin(s)" for a run that moved the plugin off its locked commit
    -- and rewrote the lockfile, and told the dashboard the plugin was "up to date".
    local moved = res.sha ~= before
    if spec.submodules and moved then
      -- Pick up any submodule bumps the move brought in; skipped when nothing moved (the
      -- gitlinks then can't have changed).
      update_submodules(name, "update", spec._dir)
    end
    set_task(name, "update", "done", moved and "updated" or "up to date")
    return moved and "updated" or "uptodate"
  end)()
end

-- ----- the lockfile -----------------------------------------------------------
--
-- `<config>/nxvim-lock.json` records the commit every MANAGED plugin resolved to, so a
-- config plus its lockfile reproduces the same plugin tree on another machine (and an
-- update that broke something can be undone). Without it an unpinned install is whatever
-- `HEAD` was that day and the previous revision is written down nowhere.
--
-- The file is GENERATED and committable:
--
-- ```json
-- {
--   "catppuccin": { "branch": "main", "commit": "0b0a9a1…" },
--   "nxvim-line": { "commit": "ada94b5…", "tag": "v2.1.0" }
-- }
-- ```
--
-- An entry records the commit AND the declaration it resolved — the `branch` the plugin
-- tracks, the `tag` it was pinned to. That second half is what makes the record
-- invalidatable (`lock_applies`): a lockfile describes a past resolution, so like
-- `Cargo.lock` against its manifest it stops applying the moment the spec that produced it
-- changes. Without it, bumping `tag = "v1"` to `"v2"` would resolve to v1 forever.
--
-- A flat map keyed by plugin name, encoded via `nx.json` with `pretty = true` — which
-- emits 2-space indentation AND sorted object keys, so the committed file's diffs stay
-- one line per changed plugin regardless of declaration order. JSON rather than Lua
-- because a lockfile is pure data that travels between machines: no code-exec vector, the
-- same reasoning behind project-local `.nxvim/workspace.json`.
--
-- Read/written through the LOCAL-ALWAYS `lfs` seam like every other manager op — plugins
-- load into THIS Lua VM off the local runtimepath, so in a daemon / wasm session the
-- lockfile belongs on the client disk beside the clones it describes, not on the remote.

-- The lockfile contents as last read or written (`name -> { commit, branch? }`). Kept in
-- memory so the UI and `list()` can consult it synchronously; `{}` before the first read.
M._lock = M._lock or {}

-- `nx.plugins.locked()` — a synchronous snapshot of the last read/written lockfile.
-- Cheap (no disk); call `_read_lock()` first if the file may have changed underneath.
function M.locked()
  local out = {}
  for name, entry in pairs(M._lock) do
    out[name] = { commit = entry.commit, branch = entry.branch, tag = entry.tag }
  end
  return out
end

-- Whether lock `entry` still records what `spec` ASKS for — the invalidation rule that
-- keeps a record from outranking the declaration it was derived from. A lockfile pins the
-- resolution of a floating ref; the moment the spec names a different ref, the entry
-- describes a question nobody is asking any more and must be re-resolved (`Cargo.lock`
-- against a changed `Cargo.toml`). Without this a `tag = "v2"` bump installs v1 forever:
-- silently, with no drift to see (head and lock agree) and no verb able to move it.
--
-- A `tag` must match exactly — including "the spec has one and the entry doesn't", which
-- is how an entry written before the pin was added gets re-resolved. A `branch` only
-- invalidates when BOTH sides name one and they differ: an entry with no branch (a
-- hand-written pin) says nothing about which branch produced it, so it is not evidence of
-- a change.
lock_applies = function(spec, entry)
  if not entry or not entry.commit then
    return false
  end
  if (spec.tag or false) ~= (entry.tag or false) then
    return false
  end
  if spec.branch and entry.branch and spec.branch ~= entry.branch then
    return false
  end
  return true
end

-- `nx.plugins._read_lock()` — load the lockfile into `M._lock`, resolving it. A missing
-- file resolves `{}` (no lockfile yet is the normal pre-first-sync state). A file that is
-- present but MALFORMED rejects loud, naming the path: silently treating a corrupt lock as
-- "nothing pinned" would quietly drop every pin the user committed, exactly the
-- broken-looks-like-working failure the no-silent-stub rule forbids.
function M._read_lock()
  return nx.async(function()
    local path = lockfile_path()
    if not nx.await(lfs.exists(path)) then
      M._lock = {}
      return M._lock
    end
    local text = nx.await(lfs.read_text(path))
    local ok, decoded = pcall(nx.json.decode, text)
    if not ok or type(decoded) ~= "table" then
      -- Built as a variable (not an `error({…})` literal) so static linters that type
      -- `error`'s argument as a string don't flag it — the prelude's idiom for rejecting
      -- with a `{ code, message }` table (see test.lua's `fail`).
      local e = {
        code = "ELOCK",
        message = "nx.plugins: malformed lockfile " .. path .. ": " .. tostring(
          not ok and decoded or "not a JSON object"
        ),
      }
      error(e, 0)
    end
    -- Keep only the fields we understand: the file is generated, so an entry from a newer
    -- nxvim is read for the fields this version knows and rewritten in this version's
    -- shape. An entry with no usable COMMIT, though, is corruption rather than a version
    -- difference — the commit is the whole content of an entry — and dropping it quietly
    -- would reinstall that plugin at the remote tip, which is exactly the unreproducible
    -- install the file exists to prevent. So it fails loud, naming the entry.
    local lock = {}
    for name, entry in pairs(decoded) do
      if type(entry) ~= "table" or type(entry.commit) ~= "string" or entry.commit == "" then
        local e = {
          code = "ELOCK",
          message = "nx.plugins: malformed lockfile "
            .. path
            .. ": entry '"
            .. tostring(name)
            .. "' has no commit",
        }
        error(e, 0)
      end
      lock[name] = {
        commit = entry.commit,
        branch = type(entry.branch) == "string" and entry.branch or nil,
        tag = type(entry.tag) == "string" and entry.tag or nil,
      }
    end
    M._lock = lock
    return M._lock
  end)()
end

-- Resolve the currently-checked-out commit of every managed, installed plugin into a fresh
-- lock table, over the entries `prev` already records.
--
-- What is NOT recorded is as load-bearing as what is. A dev `dir` plugin never contributes
-- a sha (a working checkout is not a reproducible artifact — recording it would pin
-- something no other machine can fetch), and neither does a plugin whose clone is missing
-- or whose HEAD is unborn (nothing resolved yet; a blank pin is worse than none).
--
-- But "not recorded" must not mean "deleted". A plugin that is DECLARED and simply not
-- installed *here* — `enabled = false`, a platform-conditional `enabled` predicate, a
-- local `dir` override, a clone not yet synced — keeps whatever the lockfile already says
-- about it. The lockfile is the file the user COMMITS, so rebuilding it from only what
-- happens to be on this machine's disk quietly strips the pins the other machines need,
-- and the stripped copy is what travels back to them. Only a name nothing declares any
-- more is dropped: that is a real answer to "is this entry still wanted?", where an
-- uninstalled-here plugin is no answer at all.
--
-- `realized` (a name set, or nil for "all") is the other half of that: it names the
-- plugins this run actually put where the spec and lockfile say they belong. A plugin
-- OUTSIDE it is carried over untouched rather than re-recorded from its checkout, because
-- its checkout is not yet evidence of anything — the classic case being `sync`'s install
-- pass, which touches nothing but would otherwise overwrite an incoming lockfile with the
-- commits the tree happens to sit on, one step before the update pass was going to realize
-- it. `nil` (an explicit `:PluginLock`) means "make the file describe the tree", and then
-- the `tag` is carried instead: HEAD is whatever the user left there, so we cannot attest
-- that the spec's current tag produced it.
local function resolve_lock(prev, realized)
  prev = prev or {}
  return nx.async(function()
    local lock = {}
    for _, name in ipairs(M._order) do
      local spec = M._specs[name]
      local resolved = nil
      local record = realized == nil or realized[name] == true
      if record and spec.dir == nil and nx.await(lfs.exists(spec._dir)) then
        local ok, head = pcall(nx.await, lgit.head(spec._dir))
        if ok and type(head.sha) == "string" and head.sha ~= "" then
          -- Which pin produced this commit. Attested from the spec when this run realized
          -- it (`realized`), carried otherwise. Spelled as a statement rather than an
          -- `and`/`or` chain because the attested value is legitimately nil — a spec that
          -- DROPPED its tag must clear the record, not fall through to the old one.
          local tag
          if realized ~= nil then
            tag = spec.tag
          else
            tag = (prev[name] or {}).tag
          end
          -- A plugin installed FROM the lock sits on a detached HEAD, so `head.branch` is
          -- nil. Carry over the branch the plugin TRACKS (the spec's, else whatever the
          -- lockfile already recorded) instead of erasing it: the branch is a property of
          -- the spec, not of the commit currently checked out, and the first locked
          -- install would otherwise drop the only record of it.
          resolved = {
            commit = head.sha,
            branch = head.branch or spec.branch or (prev[name] or {}).branch,
            tag = tag,
          }
        end
      end
      lock[name] = resolved or prev[name]
    end
    return lock
  end)()
end

-- Whether two lock tables record the same commits (so an unchanged sync doesn't rewrite
-- the file and dirty the user's git working tree for nothing).
local function lock_eq(a, b)
  for name, entry in pairs(a) do
    local other = b[name]
    if
      not other
      or other.commit ~= entry.commit
      or other.branch ~= entry.branch
      or other.tag ~= entry.tag
    then
      return false
    end
  end
  for name in pairs(b) do
    if not a[name] then
      return false
    end
  end
  return true
end

-- `nx.plugins.lock()` — record every managed, installed plugin's current commit to the
-- lockfile. Resolves the written table. Writes only when the resolved set differs from
-- what is already on disk, so a no-op sync leaves the file (and the user's git status)
-- untouched.
--
-- `opts.realized` (internal) narrows the re-record to the plugins a verb just put in
-- place, carrying every other entry over verbatim — see `resolve_lock`. The public call
-- takes no options and re-records everything installed, which is `:PluginLock`'s job.
function M.lock(opts)
  local realized = opts ~= nil and opts.realized or nil
  return nx.async(function()
    -- Read the FILE first, not `M._lock`, so an externally-edited lockfile is reconciled
    -- rather than assumed to match our last write — and so `resolve_lock` can carry over
    -- the tracked branch of a plugin now sitting on a detached (lock-installed) HEAD.
    local on_disk = nx.await(M._read_lock())
    -- Nothing declared: the declared set is the only thing that says which entries are
    -- still wanted, and an empty one says nothing — so it must never be read as "drop
    -- everything". (A session that declares no plugins is ordinary: a `--clean`-ish start,
    -- a config whose plugin file failed to load, `:PluginLock` typed early.)
    if #M._order == 0 and next(on_disk) ~= nil then
      return on_disk
    end
    local resolved = nx.await(resolve_lock(on_disk, realized))
    if lock_eq(resolved, on_disk) then
      M._lock = resolved
      return resolved
    end
    local path = lockfile_path()
    local dir = path:match("^(.*)/[^/]*$")
    if dir and dir ~= "" then
      nx.await(lfs.mkdir(dir, { recursive = true }))
    end
    -- An empty Lua table is ambiguous in JSON and `nx.json` picks the ARRAY form, so an
    -- emptied lockfile would be written as `[]` — not the name-keyed object this file is
    -- documented as and re-read as. Spell the empty object out.
    local body = next(resolved) == nil and "{}" or nx.json.encode(resolved, { pretty = true })
    nx.await(lfs.write(path, body .. "\n"))
    M._lock = resolved
    notify_change()
    return resolved
  end)()
end

-- Check out `name` at `commit`, deepening the clone if that commit is not present.
-- An unpinned install is `depth = 1`, so a commit the lockfile recorded earlier is often
-- simply absent locally — the checkout then fails with nothing wrong except missing
-- objects. `fetch{ unshallow = true }` drops the shallow boundary and the retry succeeds.
-- Only ever ONE unshallow attempt: if the commit is still unreachable it is genuinely gone
-- from the remote (force-pushed away), and no amount of fetching will conjure it.
checkout_deep = function(name, op, dir, commit)
  return nx.async(function()
    local ok = pcall(nx.await, lgit.checkout(dir, commit, { detach = true }))
    if ok then
      return true
    end
    set_task(name, op, "running", "deepening history")
    local fetched = pcall(nx.await, lgit.fetch(dir, { unshallow = true }))
    if not fetched then
      return false
    end
    return (pcall(nx.await, lgit.checkout(dir, commit, { detach = true })))
  end)()
end

-- ----- restore ----------------------------------------------------------------

-- `nx.plugins.restore()` — move every managed plugin to the commit the lockfile records:
-- the "undo the update that broke my editor" verb, and the reason recording commits is
-- worth anything. Resolves
--
-- ```
-- { restored = { name, … }, current = { name, … }, failed = { { name, message }, … } }
-- ```
--
-- so a caller can tell the three outcomes apart. A plugin already at its locked commit is
-- `current` (never re-checked-out); a dev `dir` plugin, an uninstalled one, and one with no
-- lock entry are skipped silently (there is nothing recorded to restore them to).
--
-- A commit that cannot be reached even after unshallowing lands in `failed` and the
-- checkout is left exactly as it was — a partial rollback that CLAIMED success would be
-- worse than none, since the user would stop looking for the real problem. The promise
-- still resolves (so the other plugins' outcomes survive) and `:PluginRestore` reports
-- the failures loud.
function M.restore()
  return nx.async(function()
    local lock = nx.await(M._read_lock())
    local out = { restored = {}, current = {}, failed = {} }
    for _, name in ipairs(M._order) do
      local spec = M._specs[name]
      local want = (lock[name] or {}).commit
      if want and spec.dir == nil and nx.await(lfs.exists(spec._dir)) then
        local ok_head, head = pcall(nx.await, lgit.head(spec._dir))
        if ok_head and head.sha == want then
          out.current[#out.current + 1] = name
        else
          set_task(name, "restore", "running", "restoring " .. want:sub(1, 7))
          if nx.await(checkout_deep(name, "restore", spec._dir, want)) then
            set_task(name, "restore", "done", "restored")
            out.restored[#out.restored + 1] = name
          else
            local msg = "commit " .. want .. " is unreachable (gone from the remote?)"
            set_task(name, "restore", "error", "restore failed")
            out.failed[#out.failed + 1] = { name = name, message = msg }
          end
        end
      end
    end
    notify_change()
    return out
  end)()
end

-- Record the lock after a verb that may have moved a checkout. Never fails the verb: a
-- lockfile that couldn't be written is reported (loud on the message line) but must not
-- make a successful install/update look like a failure.
local function lock_after(op, realized)
  return M.lock({ realized = realized }):catch(function(err)
    nx.notify(
      "nx.plugins: could not write lockfile ("
        .. op
        .. "): "
        .. tostring(err and err.message or err),
      4
    )
  end)
end

-- ----- the verbs --------------------------------------------------------------

-- The `M._update` statuses that mean the CHECKOUT MOVED — what the verbs below count, and
-- what makes recording the lock afterwards necessary rather than merely tidy.
local MOVED = { updated = true, relocked = true, repinned = true }

-- Install every declared, enabled plugin that is missing. Returns a promise of the
-- count installed; activates any newly-present eager plugins as it finishes.
--
-- A failure aborts the run LOUD (a half-installed tree must never look complete), but the
-- lockfile is recorded either way: the plugins that did land are on disk, and a lockfile
-- that no longer describes the tree is precisely the silent disagreement it exists to
-- prevent. The rejection is re-raised after the record is written.
--
-- Only the FRESH clones are recorded (they land exactly on their pin / lock entry, so
-- their HEAD is evidence). A plugin that was already present is left as the lockfile has
-- it: install does not check where it sits, so overwriting its entry from the checkout
-- would throw away a lockfile the user just pulled in — the very thing the next `sync`
-- pass is about to realize.
function M.install()
  return nx.async(function()
    local n = 0
    local installed = {}
    local ok, failure = pcall(function()
      for _, name in ipairs(M._order) do
        if enabled(M._specs[name]) then
          if nx.await(M._install(name)) == "installed" then
            n = n + 1
            installed[name] = true
          end
        end
      end
    end)
    activate_eager()
    nx.await(lock_after("install", installed))
    if not ok then
      error(failure, 0)
    end
    nx.notify("nx.plugins: installed " .. n .. " plugin(s)", n > 0 and 2 or 3)
    return n
  end)()
end

-- Fast-forward every installed, unpinned plugin, ADVANCING past the lockfile (and
-- re-recording it) — the `cargo update` half. `opts.advance = false` instead reproduces
-- the lock, moving a locked plugin onto (or leaving it at) the recorded commit; that is
-- what `sync` passes. Returns a promise of the count of plugins whose checkout MOVED.
-- Records the lock even when a plugin fails, for the reason `install` does.
function M.update(opts)
  local advance = not (opts ~= nil and opts.advance == false)
  return nx.async(function()
    local n = 0
    -- Every plugin this pass settles is realized: it was moved onto its pin / lock / branch
    -- tip, or found already there. Only "missing" and "local" (a dev `dir`) are not, and
    -- neither is recordable anyway.
    local realized = {}
    local ok, failure = pcall(function()
      for _, name in ipairs(M._order) do
        if enabled(M._specs[name]) then
          local status = nx.await(M._update(name, { advance = advance }))
          realized[name] = status ~= "missing" and status ~= "local"
          if MOVED[status] then
            n = n + 1
          end
        end
      end
    end)
    nx.await(lock_after("update", realized))
    if not ok then
      error(failure, 0)
    end
    nx.notify("nx.plugins: updated " .. n .. " plugin(s)", 2)
    return n
  end)()
end

-- Install the missing, then update the rest — the one-shot "make the world match
-- my declarations". Returns a promise of the count NEWLY INSTALLED (so a UI can tell
-- whether a fresh clone landed and prompt to restart).
-- A lockfile makes this the "realize my declared state" verb, NOT an update: it installs
-- what is missing at the locked commits, MOVES an existing checkout onto them when it has
-- drifted (pull a colleague's newer lockfile and sync, and you get the tree it names —
-- reproducing the lock only for clones that happen to be missing would honour the file by
-- accident of what is on disk), realizes any `commit`/`tag` pin the spec has changed, and
-- fast-forwards only what nothing pins. Advancing PAST the lock is `:PluginUpdate`'s job —
-- if sync advanced, the lockfile would be honoured for the instant between the install and
-- the update pass and then thrown away, which is no lockfile at all.
-- (Locking is inherited from the two halves — `install` records the fresh clones and
-- `update` records the rest, after convergence. No third write here.)
function M.sync()
  return nx.async(function()
    local installed = nx.await(M.install())
    nx.await(M.update({ advance = false }))
    nx.notify("nx.plugins: sync complete", 2)
    return installed
  end)()
end

-- Remove cloned plugin directories under the install root that no plugin declares
-- (a dev `dir` plugin lives outside the root and is never touched). Returns a
-- promise of the removed names.
function M.clean()
  return nx.async(function()
    local declared = {}
    for _, name in ipairs(M._order) do
      if M._specs[name].dir == nil then
        declared[name] = true
      end
    end
    local r = root()
    if not nx.await(lfs.exists(r)) then
      return {}
    end
    local removed = {}
    for _, e in ipairs(nx.await(lfs.readdir(r))) do
      if e.type == "directory" and not declared[e.name] then
        nx.await(lfs.remove(r .. "/" .. e.name, { recursive = true }))
        removed[#removed + 1] = e.name
        -- Forget the plugin's shada namespace too, so an uninstalled plugin doesn't
        -- leave its cross-session data orphaned in the store forever. A managed
        -- plugin lives at `root()/<name>` and keys its `nx.shada.plugin` store on that
        -- same `<name>`, so the directory name IS the namespace to drop.
        nx.shada.forget(e.name)
      end
    end
    nx.notify("nx.plugins: removed " .. #removed .. " plugin(s)", #removed > 0 and 2 or 3)
    return removed
  end)()
end

-- ----- system-plugin tier ----------------------------------------------------

-- Record the boot system-plugin set the server seeds from the client's system dir.
-- Called from the server's pre-`init.lua` system-load phase with `{ { name=, dir= }, … }`;
-- the dirs are already on the runtimepath (spliced at boot) and their `plugin/` scripts
-- are sourced by the server right after this, so this only records the tier so
-- `M._system_dirs()` (the server's source-skip set) and `M.list_system()` see them, and a
-- config can introspect the tier. Never re-sources — that is the server's job here.
function M._register_system(list)
  for _, s in ipairs(list or {}) do
    if s.name and s.dir then
      M._system[s.name] = { name = s.name, dir = s.dir }
    end
  end
end

-- The live system-plugin tier dirs, as a plain array — the set the server's
-- `source_plugins` pass skips so a system plugin is never sourced twice. Covers both the
-- boot set (`_register_system`) and any runtime promotion (`M.system` / `M.promote`).
function M._system_dirs()
  local dirs = {}
  for _, s in pairs(M._system) do
    dirs[#dirs + 1] = s.dir
  end
  return dirs
end

-- Every dir the manager ITSELF sources (via `source_runtime` in `M.load`) — the system
-- tier PLUS every managed plugin spec. The server's post-`init.lua` `source_plugins` pass
-- skips these so a manager-owned plugin's `plugin/` scripts never run twice: once by the
-- manager, once by that native runtimepath pass. This matters for an EAGER local-`dir`
-- plugin, whose runtimepath entry is added SYNCHRONOUSLY in `init.lua` (before the pass),
-- so both would otherwise source it. A git-cloned plugin's rtp entry is added after an
-- async `await`, so the pass usually misses it — but skipping it here is correct either way
-- (the manager owns its sourcing). An unloaded lazy plugin isn't on the runtimepath yet, so
-- skipping its dir is a harmless no-op until it loads (and the manager sources it then).
function M._manager_owned_dirs()
  local seen, out = {}, {}
  local function add(d)
    if d and not seen[d] then
      seen[d] = true
      out[#out + 1] = d
    end
  end
  for _, s in pairs(M._system) do
    add(s.dir)
  end
  for _, spec in pairs(M._specs) do
    add(spec._dir)
  end
  return out
end

-- A synchronous snapshot of the system tier: `{ { name=, dir= }, … }`, sorted by name.
function M.list_system()
  local out = {}
  for _, s in pairs(M._system) do
    out[#out + 1] = { name = s.name, dir = s.dir }
  end
  table.sort(out, function(a, b)
    return a.name < b.name
  end)
  return out
end

-- Clone `src` (an "owner/repo" / url) or a local `dir` checkout into the system dir at
-- `target`, unless already present. Fails loud on a git error. Local `dir` checkouts are
-- cloned (not referenced) so the client re-seeds the plugin into every future session —
-- the system dir is the only place the client scan looks. Runs on the LOCAL disk (the
-- local-always seam), like all plugin management.
local function clone_into_system(name, url, dir, target)
  return nx.async(function()
    nx.await(lfs.mkdir(system_dir(), { recursive = true }))
    if nx.await(lfs.exists(target)) then
      return
    end
    -- A dev `dir` checkout is captured in full (a plain local clone of its current
    -- committed state); a managed `url` is a shallow clone (depth 1).
    local source = dir or url
    local opts = dir ~= nil and {} or { depth = 1 }
    local ok, err = pcall(nx.await, lgit.clone(source, target, opts))
    if not ok then
      local msg = (type(err) == "table" and err.message) or tostring(err)
      error("nx.plugins.system: git clone failed for " .. name .. ": " .. msg, 0)
    end
  end)()
end

-- Register `target` (a dir under the system dir) into the tier and LOAD it into the
-- current session — put it on the runtimepath, source its `plugin/` scripts, run `config`.
-- So a runtime promotion takes effect NOW as well as for every future session/swap.
local function activate_system(name, target, config)
  return nx.async(function()
    M._system[name] = { name = name, dir = target }
    nx._add_rtp(target)
    nx.await(source_runtime(target))
    if config then
      nx.await(run_hook(config):catch(function(err)
        nx.notify(
          "nx.plugins.system[" .. name .. "].config: " .. tostring(err and err.message or err),
          4
        )
      end))
    end
    notify_change()
  end)()
end

-- `nx.plugins.system(spec)` — inject a plugin into the system tier: clone/copy it into the
-- system dir (via the local-always seam) and load it into the current session, so it takes
-- effect now AND is re-seeded by the client into every future session (the VS Code
-- "install a connector" move). `spec` is a normal plugin spec (string shorthand / table).
-- Returns a promise of the plugin's name. Callable form for `init.lua`.
function M.system(spec)
  local s = normalize(spec)
  local target = system_dir() .. "/" .. s.name
  return nx.async(function()
    nx.await(clone_into_system(s.name, s.url, s.dir, target))
    nx.await(activate_system(s.name, target, s.config))
    return s.name
  end)()
end

-- `nx.plugins.promote(name)` — promote an already-declared managed plugin into the system
-- tier: clone its on-disk checkout into the system dir and register it, so it persists into
-- every future session. Loads the system copy now unless the plugin is already loaded this
-- session. REJECTS (loud) for an unknown plugin. Returns a promise of the name.
function M.promote(name)
  local spec = M._specs[name]
  if not spec then
    return nx.promise.reject({
      message = "nx.plugins.promote: unknown plugin '" .. tostring(name) .. "'",
    })
  end
  local target = system_dir() .. "/" .. name
  return nx.async(function()
    -- Clone from the plugin's on-disk checkout (a dev `dir` or the managed clone) so the
    -- exact installed state moves into the tier, offline.
    nx.await(clone_into_system(name, nil, spec.dir or spec._dir, target))
    if M._loaded[name] then
      -- Already loaded this session; just register the system copy for future sessions.
      M._system[name] = { name = name, dir = target }
      notify_change()
    else
      nx.await(activate_system(name, target, spec.config))
    end
    return name
  end)()
end

-- ----- recommended set + first-run bootstrap ---------------------------------

-- The curated default set offered on a fresh install. EMPTY by default — nxvim (or
-- a distribution) fills it via `nx.plugins.recommend{...}`; with nothing here the
-- first-run prompt simply never appears. Specs are DATA + string-form hooks only
-- (see recommend()), so the set can be written back out to the user's config.
M._recommended = M._recommended or {}

-- Register the recommended set (replacing any prior). A recommended spec may carry
-- every normal field EXCEPT a `config`/`init` *function* — those must be a STRING
-- of Lua source, because the set is serialized into the user's `plugins.lua` and a
-- function cannot be written out. Fails loud on a function hook rather than
-- silently dropping it.
function M.recommend(specs)
  for _, s in ipairs(specs) do
    if type(s) == "table" then
      for _, k in ipairs({ "config", "init" }) do
        if s[k] ~= nil and type(s[k]) ~= "string" then
          error(
            "nx.plugins.recommend: a recommended spec's '"
              .. k
              .. "' must be a STRING of Lua (it gets written to plugins.lua), not a "
              .. type(s[k]),
            0
          )
        end
      end
    end
  end
  M._recommended = specs
  return specs
end

-- nxvim's BUILT-IN default recommended set — a small, nx.*-native starting point
-- offered on a brand-new setup when the user's config registers no set of its own.
-- It is NOT active by default: the interactive binary opts in by calling
-- `M._use_default_recommended()` at boot (ServerInit.offer_default_recommended) before
-- `init.lua` runs, so a config's own `recommend{...}` still overrides it and declaring
-- any plugin skips the welcome. Tests never opt in, so it can't disrupt the headless
-- suites. String-form hooks only (the chosen subset is serialized into plugins.lua).
-- (A plugin spec is intentionally a mixed table — `[1]` source + named fields — which
-- is the manager's declared format; suppress selene's mixed_table for this literal.)
-- selene: allow(mixed_table)
M._default_recommended = {
  {
    "nxvim/catppuccin-nxvim",
    name = "catppuccin",
    desc = "Soothing pastel colorscheme",
    config = [[ vim.cmd("colorscheme catppuccin") ]],
  },
  {
    "nxvim/nxvim-keys-helper",
    desc = "Popup of available keybindings as you type (which-key)",
    config = [[ require("nxvim-keys-helper").setup() ]],
  },
  {
    "nxvim/nxvim-tree",
    desc = "File explorer sidebar (<leader>e)",
    keys = { "<leader>e" },
    config = [[ require("nxvim-tree").setup() ]],
  },
  {
    "nxvim/nxvim-lspconfig",
    desc = "Quickstart configs for the built-in LSP client",
    config = [[ require("nxvim-lspconfig").setup() ]],
  },
  {
    "nxvim/nxvim-line",
    desc = "Configurable statusline (lualine)",
    config = [[ require("nxvim-line").setup() ]],
  },
  {
    "nxvim/nxvim-diff",
    desc = "Diff & merge-conflict visualizer",
    config = [[ require("nxvim-diff").setup() ]],
  },
  {
    "nxvim/nxvim-dap",
    desc = "Debugger front end — breakpoints, stepping, REPL (<F5>, <leader>db)",
    keys = { "<F5>", "<leader>db" },
    cmd = { "DapContinue", "DapToggleBreakpoint" },
    config = [[ require("nxvim-dap").setup({}) ]],
  },
}

-- Activate the built-in default set as the recommended set, unless one is already
-- registered. The interactive binary calls this before sourcing `init.lua`; the test
-- harness never does (so the headless suites keep an empty set and never offer it).
function M._use_default_recommended()
  if #M._recommended == 0 then
    M.recommend(M._default_recommended)
  end
end

-- Quote a string as a Lua literal (handles quotes / backslashes / newlines).
local function qstr(s)
  return string.format("%q", s)
end

-- Serialize a list of source strings as a Lua list literal: { "a", "b" }.
local function qlist(list)
  local parts = {}
  for _, v in ipairs(list) do
    parts[#parts + 1] = qstr(v)
  end
  return "{ " .. table.concat(parts, ", ") .. " }"
end

-- Serialize ONE raw recommended spec back to a Lua table literal for plugins.lua.
-- Forward-declared so it can recurse into `dependencies`.
local serialize_spec
serialize_spec = function(s)
  if type(s) == "string" then
    return "{ " .. qstr(s) .. " }"
  end
  local fields = {}
  local src = s.src or s.url or s[1]
  if src then
    fields[#fields + 1] = qstr(src)
  end
  for _, k in ipairs({ "name", "desc", "branch", "tag", "version", "commit", "dir" }) do
    if s[k] ~= nil then
      fields[#fields + 1] = k .. " = " .. qstr(s[k])
    end
  end
  for _, k in ipairs({ "cmd", "event", "ft", "keys" }) do
    if s[k] ~= nil then
      fields[#fields + 1] = k .. " = " .. qlist(type(s[k]) == "table" and s[k] or { s[k] })
    end
  end
  local deps = s.dependencies or s.deps
  if deps then
    local parts = {}
    for _, d in ipairs(deps) do
      parts[#parts + 1] = serialize_spec(d)
    end
    fields[#fields + 1] = "dependencies = { " .. table.concat(parts, ", ") .. " }"
  end
  if s.enabled ~= nil and type(s.enabled) ~= "function" then
    fields[#fields + 1] = "enabled = " .. tostring(s.enabled)
  end
  if s.submodules ~= nil then
    fields[#fields + 1] = "submodules = " .. tostring(s.submodules)
  end
  -- Hooks are strings here (recommend() enforced it); emit as real functions.
  for _, k in ipairs({ "config", "init" }) do
    if s[k] ~= nil then
      fields[#fields + 1] = k .. " = function() " .. s[k] .. " end"
    end
  end
  return "{ " .. table.concat(fields, ", ") .. " }"
end

-- The full `plugins.lua` text for `specs` (the chosen subset of the recommended set).
local function serialize_recommended(specs)
  local lines = {
    "-- nxvim recommended plugins, added by first-run setup.",
    "-- This file is yours now: edit it to add, remove, or configure plugins.",
    "nx.plugins({",
  }
  for _, s in ipairs(specs) do
    lines[#lines + 1] = "  " .. serialize_spec(s) .. ","
  end
  lines[#lines + 1] = "})"
  return table.concat(lines, "\n") .. "\n"
end

-- Write `specs` (the chosen subset) to `<config>/lua/plugins.lua` and make `init.lua`
-- `require("plugins")` (creating it if absent, appending if it doesn't already).
-- Leaves any hand-written init.lua otherwise untouched. Returns a promise.
function M._persist_recommended(specs)
  specs = specs or M._recommended
  return nx.async(function()
    local cfg = config_dir()
    nx.await(lfs.mkdir(cfg .. "/lua", { recursive = true }))
    nx.await(lfs.write(cfg .. "/lua/plugins.lua", serialize_recommended(specs)))
    local init = cfg .. "/init.lua"
    local existing = ""
    if nx.await(lfs.exists(init)) then
      existing = nx.await(lfs.read_text(init))
    end
    if not existing:find("require%(%s*['\"]plugins['\"]%s*%)") then
      local lead = (existing ~= "" and existing:sub(-1) ~= "\n") and "\n" or ""
      nx.await(lfs.append(init, lead .. 'require("plugins")\n'))
    end
  end)()
end

-- The marker recording that we've already offered the recommended set, so we ask
-- AT MOST ONCE ever — kept under the manager's own root (overridable, hermetic).
local function prompted_marker()
  return root() .. "/.recommended-prompted"
end

-- First-run flow: on a fresh setup (the user has declared no plugins of their own,
-- a recommended set exists, and we have not asked before), open the WELCOME view — a
-- floating checklist (prelude/plugins_ui.lua → `M.ui.welcome`) explaining that nxvim
-- ships minimal and offering the recommended set pre-ticked, each item untickable.
-- The chosen subset is written to the user's config and installed; an empty / skipped
-- choice does nothing. Returns a promise. Wired to `VimEnter`, and callable
-- directly. Re-entrant-safe (`_prompting` guard) and asks only once (the marker,
-- written before showing the view so a cancel still never nags again).
function M.bootstrap()
  return nx.async(function()
    if M._prompting or #M._order > 0 or #M._recommended == 0 then
      return
    end
    if nx.await(lfs.exists(prompted_marker())) then
      return
    end
    M._prompting = true
    -- Record that we asked BEFORE showing the view, so we never nag again whatever
    -- the user does with it (chooses, skips, or just quits).
    nx.await(lfs.mkdir(root(), { recursive = true }))
    nx.await(lfs.write(prompted_marker(), "1\n"))

    -- The welcome checklist resolves to the list of chosen raw specs ({} on skip).
    -- (Fallback to a plain confirm if the UI module somehow isn't present — e.g. a
    -- headless build that stubbed it out — so first-run still works.)
    local chosen
    if M.ui and M.ui.welcome then
      chosen = nx.await(M.ui.welcome(M._recommended))
    else
      local yes =
        nx.await(nx.ui.confirm("Install the " .. #M._recommended .. " recommended plugins?"))
      chosen = yes and M._recommended or {}
    end
    M._prompting = false
    if not chosen or #chosen == 0 then
      return
    end
    nx.await(M._persist_recommended(chosen))
    M.add(chosen)
    -- Open the manager dashboard and let IT run the install, so first-run shows the
    -- live per-plugin progress (spinner → ✓/✗ + the restart notice) instead of
    -- installing silently in the background. Fall back to a plain sync in a headless
    -- build that stubbed the UI out.
    if M.ui and M.ui.open then
      M.ui.open({ sync_on_open = true })
    else
      nx.await(M.sync())
    end
  end)()
end

-- ----- introspection ----------------------------------------------------------

-- A synchronous snapshot of the declared set (declaration order): each
-- `{ name, url, dir, lazy, pinned, loaded, locked }`. Cheap (no disk) — `loaded` reflects
-- the in-memory load state and `locked` the commit the LAST-READ lockfile records (nil
-- when unlocked). Use `status()` for the disk-checked `installed` / `head` / `drifted`.
function M.list()
  local out = {}
  for _, name in ipairs(M._order) do
    local s = M._specs[name]
    out[#out + 1] = {
      name = name,
      url = s.url,
      dir = s._dir,
      lazy = s.lazy or false,
      pinned = (s.commit or s.tag) ~= nil,
      loaded = M._loaded[name] == true,
      locked = (M._lock[name] or {}).commit,
    }
  end
  return out
end

-- Like list() but disk-checked — a promise, since the reads are off-tick. Adds
-- `installed`, the checked-out `head` commit, and `drifted` (the checkout is at a different
-- commit than the lockfile records). Drift is the signal that the tree no longer matches
-- what the config's lockfile promises, so `:PluginList` and the dashboard can say so
-- instead of leaving the user to compare shas by hand.
--
-- Reads the lockfile from disk first, so drift is computed against the CURRENT file (the
-- user may have just checked out a different one) rather than whatever was last read.
function M.status()
  return nx.async(function()
    local lock = nx.await(M._read_lock())
    local out = M.list()
    for _, row in ipairs(out) do
      local spec = M._specs[row.name]
      row.installed = spec.dir ~= nil or nx.await(lfs.exists(spec._dir))
      row.locked = (lock[row.name] or {}).commit
      if row.installed and spec.dir == nil then
        local ok, head = pcall(nx.await, lgit.head(spec._dir))
        row.head = ok and head.sha or nil
      end
      -- Only a plugin that IS locked can drift; an unlocked one has nothing to differ from.
      row.drifted = row.locked ~= nil and row.head ~= nil and row.head ~= row.locked
    end
    return out
  end)()
end

-- ----- commands ---------------------------------------------------------------

-- Report a rejected manager op (a failed git, a bad path) on the message line —
-- the captured stderr is in err.message, never on the user's terminal.
local function report(promise)
  promise:catch(function(err)
    nx.notify(tostring(err and err.message or err), 4)
  end)
end

nx.command("PluginSync", function()
  report(M.sync())
end, {
  desc = "Realize the declared + locked state: install missing, check out what the lockfile pins, fast-forward the rest (nx.plugins.sync).",
})
nx.command("PluginInstall", function()
  report(M.install())
end, { desc = "Clone any declared plugin not yet on disk (nx.plugins.install)." })
nx.command("PluginUpdate", function()
  report(M.update())
end, {
  desc = "Fast-forward every unpinned plugin, advancing past the lockfile (nx.plugins.update).",
})
nx.command("PluginClean", function()
  report(M.clean())
end, { desc = "Remove cloned plugin dirs no spec declares (nx.plugins.clean)." })
-- Report a `restore()` outcome on the message line. Shared by `:PluginRestore` and the
-- dashboard's `R` verb so both say the same thing. The failures go out LOUD (level 4) and
-- separately from the summary: a rollback that silently skipped a plugin is the one case
-- the user must not miss, because they would stop looking for the real problem.
function M._restore_notify(r)
  if #r.failed > 0 then
    local lines = { "nx.plugins.restore: " .. #r.failed .. " plugin(s) could NOT be restored:" }
    for _, f in ipairs(r.failed) do
      lines[#lines + 1] = "  " .. f.name .. ": " .. f.message
    end
    nx.notify(table.concat(lines, "\n"), 4)
  end
  nx.notify(
    "nx.plugins: restored " .. #r.restored .. ", already current " .. #r.current,
    #r.restored > 0 and 2 or 3
  )
  return r
end

nx.command("PluginRestore", function()
  report(M.restore():next(M._restore_notify))
end, { desc = "Check out every plugin at the commit the lockfile records (nx.plugins.restore)." })
nx.command("PluginLock", function()
  report(M.lock():next(function(lock)
    local n = 0
    for _ in pairs(lock) do
      n = n + 1
    end
    nx.notify("nx.plugins: locked " .. n .. " plugin(s)", 2)
  end))
end, { desc = "Record every installed plugin's commit to the lockfile (nx.plugins.lock)." })
nx.command("PluginList", function()
  M.status():next(function(rows)
    local lines = { "nx.plugins — " .. #rows .. " declared:" }
    for _, r in ipairs(rows) do
      local flags = {}
      flags[#flags + 1] = r.installed and "installed" or "MISSING"
      if r.loaded then
        flags[#flags + 1] = "loaded"
      end
      if r.lazy then
        flags[#flags + 1] = "lazy"
      end
      if r.pinned then
        flags[#flags + 1] = "pinned"
      end
      if r.drifted then
        flags[#flags + 1] = "DRIFTED"
      end
      lines[#lines + 1] = "  " .. r.name .. "  [" .. table.concat(flags, " ") .. "]"
    end
    nx.notify(table.concat(lines, "\n"), 2)
  end)
end, { desc = "List declared plugins and their install/load state (nx.plugins.status)." })

-- First-run offer: once the editor has finished starting (VimEnter, fired by the
-- server after init.lua + plugin scripts), run the bootstrap — it self-gates to a
-- truly fresh setup with a recommended set and only ever asks once.
nx.on("VimEnter", {}, function()
  -- The startup point has passed: every eager plugin declared by the config is now
  -- registered, so `PluginsLoaded` may fire as soon as their loads settle (now, if
  -- they already have).
  M._vim_entered = true
  maybe_fire_plugins_loaded()
  M.bootstrap()
end)

return nx.plugins
