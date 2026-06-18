-- nx.component — a Vue-shaped component model for plugin UIs, surface-agnostic.
--
-- A component is `{ setup, render }`. `setup(ctx, props)` runs ONCE when mounted — it owns
-- side effects: reactive state (`ctx.reactive`), derived state (`ctx.computed`), data
-- fetch, event subscriptions, and (on surfaces that take focus) key binds. `render(state)`
-- is PURE: it maps state to what's on screen, and the framework re-runs it automatically
-- whenever the reactive state changes (coalesced to one render per tick). **Both may be
-- async** — write `nx.await(...)` straight inside them (each runs in an `nx.async`
-- coroutine), so a `setup` that loads from `nx.fs`, or a `render` that fetches to display,
-- reads top-to-bottom.
--
-- What the component owns — and what makes it worth more than driving a surface by hand —
-- is the LIFECYCLE: it waits for the surface to become ready before running `setup` (so
-- everything in `ctx` is valid immediately, no tick-dance), batches state writes into one
-- re-render, gen-gates async renders, and tears down on close.
--
-- WHERE it renders is a pluggable BACKEND, so the same reactive core drives different
-- surfaces. Two ship here:
--   * "view"  — a focus-taking, navigable `nx.view` buffer (dock / split / grabbing float).
--               `render` returns `{ lines, decor }`; `ctx` gains `keymap_set` / `line` /
--               `set_cursor` / `bufnr`. This is what a file tree / picker / modal dialog
--               wants. `nx.view.component(def)` is the sugar for it.
--   * "float" — a NON-focus `nx.ui.float` content float (the which-key surface). It never
--               steals focus and binds no keys; `render` returns `{ lines, title?,
--               relative?, border? }` (lines may be styled chunk rows), and an EMPTY render
--               hides the float. Reach it with `nx.component{ surface = "float", … }`.
-- A third party can pass `def.backend` (a `function(opts) -> adapter`) to render anywhere.

local vim = vim
nx = nx or {}

-- ----- reactive runtime: dependency tracking shared by reactive + computed --------------
--
-- A small Vue-3-shaped reactivity core. Reading a key inside a `computed` getter RECORDS a
-- dependency (key -> the computeds that read it); writing that key INVALIDATES exactly those
-- computeds (and, transitively, the computeds that read them). So a `computed` re-evaluates
-- only when one of ITS inputs changed — a `render` that reads it gets the cached value
-- otherwise. A component's `render` is deliberately NOT an effect: a write re-runs the whole
-- render (coarse, cheap for a line list), and the render reads computeds from cache.

local active_computed = nil -- the computed currently (re)evaluating, for dependency capture
local target_deps = setmetatable({}, { __mode = "k" }) -- raw table -> key -> { [computed]=true }
local LEN_KEY = {} -- sentinel dependency key standing for a table's length / iteration

-- Record that `active_computed` (if any) read `target[key]`.
local function track(target, key)
  local c = active_computed
  if not c then
    return
  end
  local keys = target_deps[target]
  if not keys then
    keys = {}
    target_deps[target] = keys
  end
  local dep = keys[key]
  if not dep then
    dep = setmetatable({}, { __mode = "k" })
    keys[key] = dep
  end
  if not dep[c] then
    dep[c] = true
    c.deps[#c.deps + 1] = dep -- remembered so the next recompute can drop stale deps
  end
end

-- Mark a computed (and everything that read it) stale; recompute is lazy, on next read.
local function invalidate(c)
  if c.dirty then
    return
  end
  c.dirty = true
  local subs = {}
  for sub in pairs(c.subs) do
    subs[#subs + 1] = sub
  end
  for _, sub in ipairs(subs) do
    invalidate(sub)
  end
end

-- A write to `target[key]` happened: invalidate the computeds that depend on it.
local function trigger(target, key)
  local keys = target_deps[target]
  if not keys then
    return
  end
  local dep = keys[key]
  if not dep then
    return
  end
  local cs = {}
  for c in pairs(dep) do
    cs[#cs + 1] = c
  end
  for _, c in ipairs(cs) do
    invalidate(c)
  end
end

-- A deep reactive proxy: reading a nested table returns a nested proxy (lazily, cached),
-- reads `track` dependencies (for any enclosing computed) and writes `trigger` the dependent
-- computeds AND call `on_change` (the component's coarse re-render). Iterate a reactive table
-- with `ipairs` / `#` (both honour the metamethods on PUC 5.4); `pairs` does NOT (5.4 dropped
-- `__pairs`), so a render that needs unordered iteration should key off an array.
local function make_reactive(root, on_change)
  local cache = setmetatable({}, { __mode = "k" }) -- raw table -> its proxy
  local function wrap(t)
    if type(t) ~= "table" then
      return t
    end
    if cache[t] then
      return cache[t]
    end
    local p = setmetatable({}, {
      __index = function(_, k)
        track(t, k)
        return wrap(t[k])
      end,
      __newindex = function(_, k, v)
        t[k] = v
        trigger(t, k)
        if type(k) == "number" then
          trigger(t, LEN_KEY) -- a numeric write may change length / iteration
        end
        on_change()
      end,
      __len = function()
        track(t, LEN_KEY)
        return #t
      end,
    })
    cache[t] = p
    return p
  end
  return wrap(root)
end

-- make_computed(getter) -> a cached derived value. Call it (`c()`, or read `c.value`) to get
-- the value; it re-evaluates `getter` only when a reactive key it read last time has since
-- changed, otherwise returns the memoized value. Reading a computed inside another computed
-- chains the dependency, so derived-of-derived stays correct. `getter` must be pure.
local function make_computed(getter)
  local c = {
    dirty = true,
    value = nil,
    deps = {}, -- dependency sets this computed is currently registered in (for cleanup)
    subs = setmetatable({}, { __mode = "k" }), -- computeds that read THIS one
  }
  local function get()
    if c.dirty then
      for _, dep in ipairs(c.deps) do
        dep[c] = nil -- drop stale dependencies before re-tracking
      end
      c.deps = {}
      local prev = active_computed
      active_computed = c
      local ok, val = pcall(getter)
      active_computed = prev
      if not ok then
        error(val, 0)
      end
      c.value = val
      c.dirty = false
    end
    -- If read while another computed is evaluating, that computed depends on this one.
    if active_computed and active_computed ~= c and not c.subs[active_computed] then
      c.subs[active_computed] = true
      active_computed.deps[#active_computed.deps + 1] = c.subs
    end
    return c.value
  end
  return setmetatable({}, {
    __call = function()
      return get()
    end,
    __index = function(_, k)
      if k == "value" then
        return get()
      end
    end,
  })
end

-- ----- backends: how a component's render output reaches a surface ----------------------

-- The "view" backend: a focus-taking, navigable nx.view buffer. `render` -> { lines, decor }.
local function view_backend(opts)
  nx.view._component_ns = nx.view._component_ns or nx.ns.create("nx.view.component")
  local v = nx.view.create({ name = opts.name or "nx-component", filetype = opts.filetype })
  if opts.dock then
    v:mount({ dock = opts.dock, size = opts.size })
  elseif opts.split then
    v:mount({ split = opts.split })
  else
    v:mount({ float = opts.float or { width = 50, height = 12, grab = true } })
  end
  return {
    -- The backing buffer number arrives a tick after create/mount (the nx._view_buf mirror).
    ready = function()
      return v:bufnr() ~= nil
    end,
    apply = function(out)
      if type(out) ~= "table" then
        return
      end
      v:set_lines(out.lines or out) -- accept { lines=, decor= } or a bare line list
      if out.decor then
        v:set_decor(nx.view._component_ns, out.decor)
      end
    end,
    close = function()
      v:close()
    end,
    -- Surface-specific ctx: the live cursor + buffer-local key binding.
    ctx = function()
      return {
        view = v,
        line = function()
          return v:line()
        end,
        set_cursor = function(n)
          v:set_cursor(n)
        end,
        bufnr = function()
          return v:bufnr()
        end,
        -- ctx.bo — the view buffer's local options, the same `vim.bo[buf]` table scoped to
        -- this view (e.g. `ctx.bo.commentstring = "# %s"`, `ctx.bo.swapfile = false`). Valid
        -- in `setup` and handlers (the buffer exists by then). Window-local options (`wo`:
        -- cursorline / wrap / …) need a view→window handle nxvim doesn't expose yet.
        bo = setmetatable({}, {
          __index = function(_, k)
            local b = v:bufnr()
            return b and vim.bo[b][k] or nil
          end,
          __newindex = function(_, k, val)
            local b = v:bufnr()
            if b then
              vim.bo[b][k] = val
            end
          end,
        }),
        -- A thin wrapper over nx.keymap.set(mode, lhs, rhs, opts) — same signature — that
        -- defaults `buffer` to this view and `nowait` on; any field the caller passes wins.
        keymap_set = function(mode, lhs, rhs, user_opts)
          local merged = { buffer = v:bufnr(), nowait = true }
          for k, val in pairs(user_opts or {}) do
            merged[k] = val
          end
          nx.keymap.set(mode, lhs, rhs, merged)
        end,
      }
    end,
  }
end

-- nx.ui.float is a SINGLE editor-wide content-float slot (the core holds one
-- `content_float`), so two float components displaying at once would clobber each other.
-- Track which live float component currently owns that slot so a second one fails LOUD
-- (CLAUDE.md: no silent clobber) rather than silently stealing it. Ownership is held only
-- while DISPLAYING — a hidden (empty-render) component releases it, so floats that are never
-- visible at the same time coexist fine.
local content_float_owner = nil

-- The "float" backend: a non-focus nx.ui.float content float. `render` ->
-- { lines, title?, relative?, border? }. An empty render hides the float (and a later
-- non-empty one re-opens it), so a component can show/hide by what it returns — which is
-- exactly the which-key shape. Takes no focus and binds no keys (no ctx extras).
local function float_backend(opts)
  local handle = nil
  local base = {
    border = opts.border or "rounded",
    relative = opts.relative or "cursor",
    title = opts.title,
  }
  local adapter = {} -- identity token for content-float ownership
  adapter.ready = function()
    return true -- no backing buffer to wait on
  end
  adapter.apply = function(out)
    local lines = type(out) == "table" and (out.lines or out) or nil
    if type(lines) ~= "table" or #lines == 0 then
      -- Hidden: release the slot so another float component may use it.
      if content_float_owner == adapter then
        content_float_owner = nil
      end
      if handle and handle:is_open() then
        handle:close()
      end
      handle = nil
      return
    end
    local fopts = {
      title = (type(out) == "table" and out.title) or base.title,
      relative = (type(out) == "table" and out.relative) or base.relative,
      border = (type(out) == "table" and out.border) or base.border,
    }
    if handle and handle:is_open() then
      handle:update(lines, fopts) -- already ours
    else
      -- Claiming the single content-float slot: refuse loudly if another live float
      -- component is already displaying, rather than silently clobbering its float.
      if content_float_owner ~= nil and content_float_owner ~= adapter then
        return nx.notify(
          "nx.component: a float component is already displaying; only one content float can be open at a time",
          4
        )
      end
      content_float_owner = adapter
      handle = nx.ui.float(lines, {
        persist = true,
        title = fopts.title,
        relative = fopts.relative,
        border = fopts.border,
      })
    end
  end
  adapter.close = function()
    if content_float_owner == adapter then
      content_float_owner = nil
    end
    if handle then
      handle:close()
      handle = nil
    end
  end
  return adapter
end

local BACKENDS = { view = view_backend, float = float_backend }

-- ----- nx.component: the generic core ---------------------------------------------------

-- nx.component(def) -> { mount(opts) }. `def.render` is required; `def.setup` optional.
-- The surface is `def.backend` (a `function(opts) -> adapter`), or `def.surface`
-- ("view" default | "float") to pick a built-in. `mount(opts)` instantiates: `opts.props`
-- is handed to `setup`, and the rest of `opts` configures the surface (view: name /
-- filetype / dock / split / float; float: title / relative / border). Returns the instance
-- (with `:close()`).
function nx.component(def)
  assert(type(def) == "table", "nx.component: pass a { setup, render } table")
  assert(type(def.render) == "function", "nx.component: a render function is required")
  local make_backend = def.backend
  if not make_backend then
    make_backend = BACKENDS[def.surface or "view"]
    if not make_backend then
      error("nx.component: unknown surface '" .. tostring(def.surface) .. "'", 2)
    end
  end

  local M = {}
  function M.mount(opts)
    opts = opts or {}
    local backend = make_backend(opts)

    local inst = { _closed = false, _on_close = {} }
    local state -- whatever setup returns; the render input
    local gen = 0 -- render generation, so a slow async render can't clobber a newer one
    local dirty = false
    local do_render

    -- Coalesce a burst of state writes into ONE render on the next microtask.
    local function schedule_render()
      if dirty or inst._closed then
        return
      end
      dirty = true
      nx.schedule(function()
        dirty = false
        do_render()
      end)
    end

    do_render = function()
      if inst._closed then
        return
      end
      gen = gen + 1
      local mine = gen
      -- `render` may be sync or async; fold both into one promise chain. A stale result
      -- (a newer render started meanwhile) is dropped.
      nx.promise
        .try(def.render, state, inst)
        :next(function(out)
          if mine == gen and not inst._closed then
            backend.apply(out)
          end
        end)
        :catch(function(e)
          nx.notify("nx.component: render error: " .. tostring(e), 4)
        end)
    end

    function inst:close()
      if self._closed then
        return
      end
      self._closed = true
      for _, fn in ipairs(self._on_close) do
        pcall(fn)
      end
      backend.close()
    end

    -- The context handed to `setup`: surface-agnostic reactivity + lifecycle, plus whatever
    -- the backend contributes (the view backend adds keymap_set / line / set_cursor / bufnr).
    local ctx = {
      props = opts.props or {},
      reactive = function(tbl)
        return make_reactive(tbl or {}, schedule_render)
      end,
      -- ctx.computed(getter) -> a cached derived value, read as `c()` (or `c.value`). It
      -- re-evaluates only when a reactive value it read has changed.
      computed = function(getter)
        return make_computed(getter)
      end,
      refresh = schedule_render,
      on_close = function(fn)
        inst._on_close[#inst._on_close + 1] = fn
      end,
      close = function()
        inst:close()
      end,
    }
    if backend.ctx then
      for k, val in pairs(backend.ctx()) do
        ctx[k] = val
      end
    end

    -- The whole lifecycle, as one linear async flow: wait for the surface to be ready, run
    -- setup (awaiting it if async), then the first render. Subsequent renders are reactive.
    nx.async(function()
      local tries = 0
      while not backend.ready() do
        if tries > 200 then
          error("the component surface never became ready")
        end
        tries = tries + 1
        nx.await(nx.promise.delay(0))
      end
      -- `nx.promise.resolve` makes this uniform whether `setup` returns a plain value or a
      -- promise; and because we're inside an `nx.async` coroutine, a `setup` that calls
      -- `nx.await(...)` directly suspends here too. Either async style works.
      if def.setup then
        state = nx.await(nx.promise.resolve(def.setup(ctx, ctx.props)))
      end
      do_render()
    end)():catch(function(e)
      nx.notify("nx.component: setup error: " .. tostring(e), 4)
    end)

    return inst
  end

  return M
end

-- nx.view.component(def) — the view-backed component (a focus-taking nx.view buffer): the
-- common case (file tree, list, modal dialog). Sugar over `nx.component` with the view
-- backend; `mount(opts)` takes the view surface options (name / filetype / dock / split /
-- float).
function nx.view.component(def)
  assert(type(def) == "table", "nx.view.component: pass a { setup, render } table")
  return nx.component({
    setup = def.setup,
    render = def.render,
    backend = view_backend,
  })
end

return nx
