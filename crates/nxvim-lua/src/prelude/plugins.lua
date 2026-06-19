-- nx.plugins — nxvim's native package / plugin manager.
--
-- The extensibility half of ADR 0002 / docs/specs/2026-06-11-native-plugin-api.md:
-- there is no third-party plugin-manager layer because the manager is BUILT IN.
-- You DECLARE a set of plugins in init.lua; the manager clones/updates them over
-- the async runtime (`nx.run` driving real `git`) and LOADS each one — adds its
-- directory to the runtimepath (`nx._add_rtp`, so `require` and the plugin's
-- `colors/` / `queries/` / `lsp/` resolve without a restart), sources its
-- `plugin/` + `after/plugin/` scripts, and runs its `config` — either EAGERLY at
-- startup or LAZILY on a trigger (`cmd` / `event` / `ft` / `keys`).
--
-- Nothing blocks (ADR 0002 rule 3): every install/source step is a promise, so the
-- UI paints before plugins finish loading. Loaded LAST in the prelude — it builds
-- on nx.run / nx.fs / nx.promise / nx.async / nx.command / nx.keymap / nx.on /
-- nx.notify, all installed above.

nx.plugins = nx.plugins or {}
local M = nx.plugins

-- ----- state -----------------------------------------------------------------

M._specs = M._specs or {} -- name -> normalized spec
M._order = M._order or {} -- declaration order (names), for deterministic sync
M._loaded = M._loaded or {} -- name -> true once fully loaded (config ran)
M._loading = M._loading or {} -- name -> true while a load is in flight (cycle guard)

-- Per-plugin operation state, so a UI can render LIVE progress (a spinner while a
-- clone/pull runs, a ✓/✗ when it finishes). `name -> { op = "install"|"update",
-- state = "running"|"done"|"error", msg = <human text> }`. The git verbs below set
-- entries here; the manager UI (prelude/plugins_ui.lua) reads them and subscribes via
-- `M.on_change`. Survives a re-source (`or {}`) so an open UI keeps its history.
M._tasks = M._tasks or {}
M._watchers = M._watchers or {} -- on_change callbacks, fired on any state transition

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
    -- The git executable. Configurable so a test can point it at a fake, and a
    -- user can pin a specific git.
    git = "git",
  }

-- The environment EVERY git invocation runs with — forced non-interactive so git
-- can never block the editor or scribble its credential prompt onto the terminal.
--
-- A child process shares the editor's controlling terminal, so a git that wants a
-- password would open `/dev/tty` directly (bypassing the stdout/stderr pipes we
-- capture), corrupting the TUI and hanging on a read that never comes. These knobs
-- make git fail FAST with a message on stderr — which we capture and surface
-- through `nx.notify` — instead:
--   * GIT_TERMINAL_PROMPT=0  — never prompt for credentials on the terminal.
--   * GIT_SSH_COMMAND=…BatchMode=yes — ssh never prompts for a password/passphrase
--     or an unknown-host confirmation; it errors instead.
--   * GCM_INTERACTIVE=never  — the Git Credential Manager stays silent.
-- (Env is MERGED onto the inherited environment by the spawn seam, so PATH etc.
-- are untouched.)
local GIT_ENV = {
  GIT_TERMINAL_PROMPT = "0",
  GIT_SSH_COMMAND = "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new -o ConnectTimeout=10",
  GCM_INTERACTIVE = "never",
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

-- ----- spec normalization ----------------------------------------------------

-- Forward declarations — these helpers are defined further down but referenced by
-- the functions just below, so they must be in lexical scope as locals here.
local aslist, aslist_triggers

-- The basename of a git source or local dir, sans a trailing ".git" — the plugin's
-- default `name` (its directory under the install root, and its require key).
local function basename(s)
  return (s:gsub("%.git$", ""):gsub("[/\\]+$", ""):match("[^/\\]+$"))
end

-- Expand a leading `~` / `~/` to $HOME so a spec can point `dir` at a dev checkout
-- under the home directory (`dir = "~/work/foo"`). Only the leading tilde is
-- touched — a mid-path `~` is a literal path component — and with no $HOME the
-- path is returned unchanged rather than mangled.
local function expanduser(p)
  local home = os.getenv("HOME")
  if not home or home == "" then
    return p
  elseif p == "~" then
    return home
  elseif p:sub(1, 2) == "~/" then
    return home .. p:sub(2)
  end
  return p
end

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
  local dir = spec.dir and expanduser(spec.dir) or nil

  local name = spec.name or (src and basename(src)) or basename(dir)
  local url = src and (is_full_url(src) and src or M._opts.github:format(src)) or nil

  -- `commit`/`tag`/`version` all pin; `commit` wins. A pin is never auto-updated.
  local commit = spec.commit
  local tag = spec.tag or spec.version

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
    branch = spec.branch,
    commit = commit,
    tag = tag,
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
      if not nx.await(nx.fs.exists(d)) then
        return
      end
      local entries = nx.await(nx.fs.readdir(d))
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
        local content = nx.await(nx.fs.read_text(f))
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
  return nx.async(function()
    for _, dep in ipairs(spec._deps) do
      nx.await(M.load(dep))
    end
    local present = spec.dir ~= nil or nx.await(nx.fs.exists(spec._dir))
    if not present then
      M._loading[name] = false
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
    M._loading[name] = false
    notify_change() -- a UI watching load state repaints this plugin as loaded
    return true
  end)()
end

-- Load `name` and report a rejection (uninstalled / load error) on the message
-- line — the fire-and-forget entry the lazy triggers and eager activation use.
local function load_reporting(name)
  M.load(name):catch(function(err)
    nx.notify(tostring(err and err.message or err), 4)
  end)
end

-- ----- lazy triggers ---------------------------------------------------------

-- Arm a lazy plugin's triggers: the first matching command / event / filetype /
-- keypress loads it, then the trigger re-fires against the now-loaded plugin (a
-- command re-dispatches with its original args; a key is fed back through the
-- typeahead so the plugin's own mapping handles it). `init` (the always-run hook)
-- fires here, at startup, regardless of when the body loads.
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
      load_reporting(name)
    end)
  end

  if #spec._triggers.ft > 0 then
    nx.on("FileType", { pattern = spec._triggers.ft }, function()
      load_reporting(name)
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

-- Load every enabled, eager (non-lazy), already-installed plugin that is not yet
-- loaded — at startup and again after a sync brings new ones onto disk.
local function activate_eager()
  for _, name in ipairs(M._order) do
    local spec = M._specs[name]
    if enabled(spec) and not spec.lazy and not M._loaded[name] then
      nx.async(function()
        if spec.dir ~= nil or nx.await(nx.fs.exists(spec._dir)) then
          nx.await(M.load(name))
        end
      end)():catch(function(err)
        nx.notify(tostring(err and err.message or err), 4)
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
  elseif type(specs) == "table" and (specs.src or specs.url or specs.dir or specs.name) then
    list = { specs } -- a single table spec, not a list
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

-- nx.plugins.setup_manager{ root=, github= }: override the install root (a test
-- points it at a temp dir) or the shorthand URL template. Merges onto the
-- defaults. (Named `setup_manager`, not `setup`, so it reads distinctly from a
-- plugin's own `require("plugin").setup{}` and never looks like the call that
-- declares plugins — that is `nx.plugins{...}` / `nx.plugins.add`.)
function M.setup_manager(opts)
  opts = opts or {}
  for k, v in pairs(opts) do
    M._opts[k] = v
  end
end

-- ----- git operations ---------------------------------------------------------

local function git(args, cwd)
  return nx.run({ cmd = M._opts.git, args = args, cwd = cwd, env = GIT_ENV })
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
    if nx.await(nx.fs.exists(spec._dir)) then
      return "exists"
    end
    set_task(name, "install", "running", "cloning")
    nx.await(nx.fs.mkdir(root(), { recursive = true }))
    local args = { "clone", "--filter=blob:none" }
    local ref = spec.branch or spec.tag
    if ref then
      args[#args + 1] = "--branch"
      args[#args + 1] = ref
    end
    if not spec.commit then
      -- Shallow unless we must reach an arbitrary commit (which a shallow clone
      -- may not contain).
      args[#args + 1] = "--depth"
      args[#args + 1] = "1"
    end
    args[#args + 1] = spec.url
    args[#args + 1] = spec._dir
    local res = nx.await(git(args))
    if res.code ~= 0 then
      set_task(name, "install", "error", res.stderr)
      error("nx.plugins: git clone failed for " .. name .. ": " .. res.stderr, 0)
    end
    if spec.commit then
      local co = nx.await(git({ "checkout", "--detach", spec.commit }, spec._dir))
      if co.code ~= 0 then
        set_task(name, "install", "error", co.stderr)
        error(
          "nx.plugins: git checkout " .. spec.commit .. " failed for " .. name .. ": " .. co.stderr,
          0
        )
      end
    end
    set_task(name, "install", "done", "installed")
    return "installed"
  end)()
end

-- Fast-forward `name` to its remote. Resolves "local"/"pinned"/"missing"/"updated".
-- A pinned (commit/tag) or dev (`dir`) plugin is never moved.
function M._update(name)
  local spec = M._specs[name]
  return nx.async(function()
    if spec.dir ~= nil then
      return "local"
    end
    if spec.commit or spec.tag then
      return "pinned"
    end
    if not nx.await(nx.fs.exists(spec._dir)) then
      return "missing"
    end
    set_task(name, "update", "running", "pulling")
    local res = nx.await(git({ "pull", "--ff-only" }, spec._dir))
    if res.code ~= 0 then
      set_task(name, "update", "error", res.stderr)
      error("nx.plugins: git pull failed for " .. name .. ": " .. res.stderr, 0)
    end
    -- "Already up to date." means no movement; show the calmer word so the UI does
    -- not imply a change that did not happen.
    local moved = not res.stdout:find("up to date", 1, true)
    set_task(name, "update", "done", moved and "updated" or "up to date")
    return "updated"
  end)()
end

-- ----- the verbs --------------------------------------------------------------

-- Install every declared, enabled plugin that is missing. Returns a promise of the
-- count installed; activates any newly-present eager plugins as it finishes.
function M.install()
  return nx.async(function()
    local n = 0
    for _, name in ipairs(M._order) do
      if enabled(M._specs[name]) then
        if nx.await(M._install(name)) == "installed" then
          n = n + 1
        end
      end
    end
    activate_eager()
    nx.notify("nx.plugins: installed " .. n .. " plugin(s)", n > 0 and 2 or 3)
    return n
  end)()
end

-- Fast-forward every installed, unpinned plugin. Returns a promise of the count
-- actually updated.
function M.update()
  return nx.async(function()
    local n = 0
    for _, name in ipairs(M._order) do
      if enabled(M._specs[name]) and nx.await(M._update(name)) == "updated" then
        n = n + 1
      end
    end
    nx.notify("nx.plugins: updated " .. n .. " plugin(s)", 2)
    return n
  end)()
end

-- Install the missing, then update the rest — the one-shot "make the world match
-- my declarations". Returns a promise of the count NEWLY INSTALLED (so a UI can tell
-- whether a fresh clone landed and prompt to restart).
function M.sync()
  return nx.async(function()
    local installed = nx.await(M.install())
    nx.await(M.update())
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
    if not nx.await(nx.fs.exists(r)) then
      return {}
    end
    local removed = {}
    for _, e in ipairs(nx.await(nx.fs.readdir(r))) do
      if e.type == "directory" and not declared[e.name] then
        nx.await(nx.fs.remove(r .. "/" .. e.name, { recursive = true }))
        removed[#removed + 1] = e.name
      end
    end
    nx.notify("nx.plugins: removed " .. #removed .. " plugin(s)", #removed > 0 and 2 or 3)
    return removed
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
    "davidrios/catppuccin-nxvim",
    name = "catppuccin",
    desc = "Soothing pastel colorscheme",
    config = [[ vim.cmd("colorscheme catppuccin") ]],
  },
  {
    "davidrios/nx-files",
    desc = "Fuzzy finder for files & buffers (<leader>ff)",
    keys = { "<leader>ff" },
    dependencies = { "davidrios/nx-async-utils" },
  },
  {
    "davidrios/nx-emoji",
    desc = "Insert emoji by name (:Emoji)",
    cmd = "Emoji",
  },
  {
    "davidrios/nx-statusline-extras",
    desc = "Extra statusline segments (git, diagnostics, …)",
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
    nx.await(nx.fs.mkdir(cfg .. "/lua", { recursive = true }))
    nx.await(nx.fs.write(cfg .. "/lua/plugins.lua", serialize_recommended(specs)))
    local init = cfg .. "/init.lua"
    local existing = ""
    if nx.await(nx.fs.exists(init)) then
      existing = nx.await(nx.fs.read_text(init))
    end
    if not existing:find("require%(%s*['\"]plugins['\"]%s*%)") then
      local lead = (existing ~= "" and existing:sub(-1) ~= "\n") and "\n" or ""
      nx.await(nx.fs.append(init, lead .. 'require("plugins")\n'))
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
-- choice does nothing. Returns a promise. Wired to `VimEnter` below, and callable
-- directly. Re-entrant-safe (`_prompting` guard) and asks only once (the marker,
-- written before showing the view so a cancel still never nags again).
function M.bootstrap()
  return nx.async(function()
    if M._prompting or #M._order > 0 or #M._recommended == 0 then
      return
    end
    if nx.await(nx.fs.exists(prompted_marker())) then
      return
    end
    M._prompting = true
    -- Record that we asked BEFORE showing the view, so we never nag again whatever
    -- the user does with it (chooses, skips, or just quits).
    nx.await(nx.fs.mkdir(root(), { recursive = true }))
    nx.await(nx.fs.write(prompted_marker(), "1\n"))

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
    nx.await(M.sync())
  end)()
end

-- ----- introspection ----------------------------------------------------------

-- A synchronous snapshot of the declared set (declaration order): each
-- `{ name, url, dir, lazy, pinned, loaded }`. Cheap (no disk) — `loaded` reflects
-- the in-memory load state. Use `status()` for the disk-checked `installed` flag.
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
    }
  end
  return out
end

-- Like list() but with the disk-checked `installed` flag — a promise, since the
-- existence check is off-tick.
function M.status()
  return nx.async(function()
    local out = M.list()
    for _, row in ipairs(out) do
      local spec = M._specs[row.name]
      row.installed = spec.dir ~= nil or nx.await(nx.fs.exists(spec._dir))
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
end, { desc = "Install missing and update existing declared plugins (nx.plugins.sync)." })
nx.command("PluginInstall", function()
  report(M.install())
end, { desc = "Clone any declared plugin not yet on disk (nx.plugins.install)." })
nx.command("PluginUpdate", function()
  report(M.update())
end, { desc = "Fast-forward every installed, unpinned plugin (nx.plugins.update)." })
nx.command("PluginClean", function()
  report(M.clean())
end, { desc = "Remove cloned plugin dirs no spec declares (nx.plugins.clean)." })
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
      lines[#lines + 1] = "  " .. r.name .. "  [" .. table.concat(flags, " ") .. "]"
    end
    nx.notify(table.concat(lines, "\n"), 2)
  end)
end, { desc = "List declared plugins and their install/load state (nx.plugins.status)." })

-- First-run offer: once the editor has finished starting (VimEnter, fired by the
-- server after init.lua + plugin scripts), run the bootstrap — it self-gates to a
-- truly fresh setup with a recommended set and only ever asks once.
nx.on("VimEnter", {}, function()
  M.bootstrap()
end)

return nx.plugins
