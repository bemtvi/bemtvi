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
--   * `"view"`  — a focus-taking, navigable `nx.view` buffer (dock / split / grabbing float).
--               `render` returns `{ lines, decor }`; `ctx` gains `keymap_set` / `line` /
--               `set_cursor` / `bufnr`. This is what a file tree / picker / modal dialog
--               wants. `nx.view.component(def)` is the sugar for it.
--   * `"float"` — a NON-focus `nx.ui.float` content float (the which-key surface). It never
--               steals focus and binds no keys; `render` returns
--               `{ lines, title?, relative?, border? }` (lines may be styled chunk rows),
--               and an EMPTY render hides the float. Reach it with
--               `nx.component{ surface = "float", … }`.
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

-- The `"view"` backend: a focus-taking, navigable `nx.view` buffer. `render` -> { lines, decor }.
--
-- `opts.persist` (a stable id) + `opts._create_ns` (the resolved owner namespace) opt the
-- backing view into cross-session restore — the component core threads them in for a
-- persistent mount (see `mount_persistent`); absent, the view is ephemeral as before. The
-- surface is NOT shown in the constructor: the core calls `show(mode)` to either mount it
-- fresh (`mode.fresh`) or adopt a reserved restore slot (`mode.place`).
local function view_backend(opts)
  nx.view._component_ns = nx.view._component_ns or nx.ns.create("nx.view.component")
  local v = nx.view.create({
    name = opts.name or "nx-component",
    filetype = opts.filetype,
    persist = opts.persist, -- nil ⇒ ephemeral (the default)
    namespace = opts._create_ns, -- explicit owner ns for the persist case; nil otherwise
  })

  -- Blank the end-of-buffer `~` fillers in the component's window. A component
  -- surface (file tree / dashboard / dialog) is plugin-owned content, not an editable
  -- file, so the empty rows below it should read as blank rather than show a text
  -- buffer's tildes. Window-local (`fillchars` `eob:` → a space); set once the window
  -- exists — its winid arrives a tick after the surface shows via the `nx._view_win`
  -- mirror, so wait for it across ticks. Best-effort (`:catch`): a dock/split surface
  -- could close before its window settles. Opt out with `opts.eob = true`.
  local function blank_eob()
    if opts.eob then
      return
    end
    nx.wait_for(function()
      return v:winid()
    end)
      :next(function(win)
        vim.wo[win].fillchars = "eob: "
      end)
      :catch(function() end)
  end

  -- `show(mode)` — put the view on screen. `mode.place` adopts a reserved restore slot (the
  -- persisted-view path); otherwise mount fresh per the dock / split / float opts.
  local function show(mode)
    if mode and mode.place then
      v:place_in(mode.place)
    elseif opts.dock then
      v:mount({ dock = opts.dock, size = opts.size })
    elseif opts.split then
      v:mount({ split = opts.split })
    else
      v:mount({ float = opts.float or { width = 50, height = 12, grab = true } })
    end
    blank_eob()
  end

  return {
    show = show,
    -- The backing buffer number arrives a tick after create/mount (the `nx._view_buf` mirror).
    ready = function()
      return v:bufnr() ~= nil
    end,
    apply = function(out)
      if type(out) ~= "table" then
        return
      end
      -- Materialize the line list into a plain sequence before it crosses to native. A
      -- `render` that returns reactive state directly (`{ lines = state.list }`) hands us a
      -- reactive proxy whose elements live in a wrapped raw table — native iteration would
      -- see it EMPTY and silently blank the view. An `ipairs` copy (the proxy honours `#` /
      -- `__index`) sidesteps that, so returning reactive state from `render` just works.
      local raw = out.lines or out
      local lines = {}
      for _, l in ipairs(raw) do
        lines[#lines + 1] = l
      end
      v:set_lines(lines)
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
        winid = function()
          return v:winid()
        end,
        -- `ctx.bo` / `ctx.wo` — the view's buffer-local and window-local options, the same
        -- `vim.bo[buf]` / `vim.wo[win]` tables scoped to this view (e.g.
        -- `ctx.bo.shiftwidth = 2`, `ctx.bo.expandtab = true`; `ctx.wo.number = true`,
        -- `ctx.wo.wrap = false`). Use the option's real scope — display options like
        -- `number` / `wrap` are window-local (`wo`), content options like `shiftwidth` /
        -- `expandtab` are buffer-local (`bo`). Valid in `setup` and handlers (the buffer +
        -- window exist by then).
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
        wo = setmetatable({}, {
          __index = function(_, k)
            local w = v:winid()
            return w and vim.wo[w][k] or nil
          end,
          __newindex = function(_, k, val)
            local w = v:winid()
            if w then
              vim.wo[w][k] = val
            end
          end,
        }),
        -- A thin wrapper over `nx.keymap.set(mode, lhs, rhs, opts)` — same signature — that
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

-- `nx.ui.float` is a SINGLE editor-wide content-float slot (the core holds one
-- `content_float`), so two float components displaying at once would clobber each other.
-- Track which live float component currently owns that slot so a second one fails LOUD
-- (CLAUDE.md: no silent clobber) rather than silently stealing it. Ownership is held only
-- while DISPLAYING — a hidden (empty-render) component releases it, so floats that are never
-- visible at the same time coexist fine.
local content_float_owner = nil

-- The `"float"` backend: a non-focus `nx.ui.float` content float. `render` ->
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
  -- A content float has no separate mount step (it appears via `apply` when the render is
  -- non-empty), so `show` is a no-op; floats are transient and never persisted.
  adapter.show = function() end
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

-- ----- persisted view components: the shared restore router -----------------------------
--
-- A persistent view component (`mount{ persist=, … }`) opts its backing view into the
-- session, exactly like a raw `nx.view.create{ persist=}`. On a restore, core reserves the
-- view's slot and dispatches it to that namespace's `nx.view.on_restore` handler. Since
-- on_restore is one-handler-per-namespace, the framework registers a SINGLE router per
-- namespace and routes the reserved `(id, place)` to the matching component's adopt fn — so
-- many persistent components in one namespace coexist.
nx._component_restorers = nx._component_restorers or {} -- ns -> { id -> adopt_fn }
nx._component_router_ns = nx._component_router_ns or {} -- ns -> true (router registered)

local function register_component_restorer(ns, raw_namespace, id, adopt)
  local reg = nx._component_restorers[ns]
  if not reg then
    reg = {}
    nx._component_restorers[ns] = reg
  end
  reg[id] = adopt
  if not nx._component_router_ns[ns] then
    nx._component_router_ns[ns] = true
    -- `raw_namespace` (the user's `opts.namespace`, possibly nil) resolves to `ns` exactly
    -- as create / shada did, so on_restore keys this router under the same ns core dispatches.
    -- Registering the router also drains any slot already reserved for a component already in
    -- `reg` (this one included) — the pull that lets a router registered LATE, from an async
    -- plugin `config`, still claim its slots.
    nx.view.on_restore(function(rid, place)
      local fn = nx._component_restorers[ns] and nx._component_restorers[ns][rid]
      if fn then
        fn(place)
      end
    end, raw_namespace)
  else
    -- The router is already registered (an earlier component in this namespace drained it),
    -- so `on_restore`'s drain won't re-run for this newly-added id — re-attempt just its slot.
    nx._claim_pending_restore(ns, id)
  end
end

-- ----- nx.component: the generic core ---------------------------------------------------

-- `nx.component(def)` -> { mount(opts) } — build a reactive, Vue-shaped UI component for
-- a plugin surface, then `:mount()` one or more instances of it. The reactive core is
-- surface-agnostic: the same component can drive a focus-taking buffer or a passive
-- float, chosen per `def` / `mount`.
--
-- `def` is a table:
--   * `render(state, inst)` — REQUIRED and PURE. Maps the current state to what's on
--     screen and returns the surface's output (see Surfaces). The framework re-runs it
--     automatically whenever reactive state changes, coalesced to ONE render per tick.
--     May be async (call `nx.await(...)` straight inside it).
--   * `setup(ctx, props)` — OPTIONAL; runs ONCE on mount and owns every side effect —
--     it creates reactive state, subscribes to events, binds keys, fetches data — and
--     RETURNS the `state` value handed to `render`. Runs only after the surface is
--     ready, so everything on `ctx` is already valid (no tick-dance). May be async.
--   * `surface` — `"view"` (default) or `"float"`; or pass `backend`
--     (a `function(opts) -> adapter`) to render to a custom surface.
--
-- The `ctx` handed to `setup` carries the reactivity and lifecycle:
--   * `ctx.reactive(tbl)` — a deep reactive proxy; writing any key (`s.x = 1`) schedules
--     a re-render. Iterate with `ipairs` / `#` (NOT `pairs` — PUC 5.4 has no `__pairs`).
--   * `ctx.computed(getter)` — a cached derived value, read as `c()` or `c.value`; it
--     re-evaluates only when a reactive input it read last time has changed.
--   * `ctx.refresh()` — force a re-render. `ctx.props` — the `opts.props` from `mount`.
--   * `ctx.on_close(fn)` / `ctx.close()` — register a teardown hook / close the instance.
--   On the `"view"` surface `ctx` also gains: `ctx.view`, `ctx.bufnr()`, `ctx.winid()`,
--   `ctx.line()`, `ctx.set_cursor(n)`, `ctx.bo` / `ctx.wo` (the view's buffer/window-local
--   option tables), and `ctx.keymap_set(mode, lhs, rhs, opts)` (buffer-scoped + `nowait`
--   by default).
--
-- Surfaces — what `render` returns, and how the surface behaves:
--   * `"view"`  — a focus-taking, navigable `nx.view` buffer (dock / split / grabbing
--     float): the file-tree / list / modal-dialog case. `render` returns
--     `{ lines, decor }` (or a bare line list). `nx.view.component(def)` is the sugar.
--   * `"float"` — a NON-focus `nx.ui.float` content float (the which-key surface): never
--     steals focus, binds no keys. `render` returns `{ lines, title?, relative?, border? }`;
--     an EMPTY render HIDES the float (a later non-empty one re-opens it), so a component
--     shows/hides purely by what it returns. Only one float component may display at once
--     (a second fails loud rather than clobbering the single content-float slot).
--
-- `mount(opts)` instantiates and returns the instance (with `:close()`). `opts.props` is
-- passed to `setup`; the rest configures the surface — view: `name` / `filetype` / `dock`
-- / `split` / `float` (and `eob` to keep end-of-buffer tildes); float: `title` /
-- `relative` / `border`. Render errors and setup errors are caught and surfaced via
-- `nx.notify` rather than crashing the editor.
--
-- Persistence (view surface) — `mount{ persist = "<id>", … }` opts a view component into
-- cross-session restore, the high-level form of `nx.view.create{ persist=}` +
-- `nx.view.on_restore`. The framework resolves the owning namespace ONCE (from the
-- mount call site, or `opts.namespace` for a no-attribution context — the same escape-hatch
-- contract as `nx.shada.plugin()`), records only `(namespace, id)` + the view's slot in the
-- session, and on a restart picks fresh-vs-restore for you: it adopts the reserved slot if
-- the session reopened the view, else mounts fresh — no `on_restore` handler, no `VimEnter`
-- fallback. The content is the component's own: `setup` reads it from `ctx.store` and a
-- mutation saves it back. A persistent component's `ctx` gains:
--   * `ctx.store` — `nx.shada.plugin(ns)` for the resolved owner namespace: an isolated,
--     cross-session key/value slice. Read saved state in `setup`, write it on every change.
--   * `ctx.namespace` — the resolved owner namespace; `ctx.persist_id` — the stable id.
-- (`examples/view-persist/` is a runnable pinned-notes plugin built on exactly this.)
--
-- Example — a live-updating counter in a floating view:
--
-- ```lua
-- local Counter = nx.component({
--   setup = function(ctx)
--     local s = ctx.reactive({ n = 0 })
--     ctx.keymap_set("n", "+", function() s.n = s.n + 1 end)  -- write -> re-render
--     ctx.keymap_set("n", "q", ctx.close)
--     return s
--   end,
--   render = function(s)
--     return { lines = { "count: " .. s.n, "", "+ to increment · q to quit" } }
--   end,
-- })
-- Counter.mount({ float = { width = 30, height = 4, grab = true } })
-- ```
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

  -- `instantiate(opts, show_mode)` — build one live instance: create the surface, show it
  -- (mount fresh via `show_mode.fresh`, or adopt a reserved restore slot via
  -- `show_mode.place`), then run the reactive lifecycle. Returns the instance handle.
  local function instantiate(opts, show_mode)
    local backend = make_backend(opts)
    backend.show(show_mode)

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
      -- `ctx.computed(getter)` -> a cached derived value, read as `c()` (or `c.value`). It
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
    -- Persisted-view extras: a stable per-component cross-session store + identity, keyed by
    -- the resolved owner namespace. `mount_persistent` resolved it once at its attributing
    -- call site and threads it here explicitly, so this `nx.shada.plugin(ns)` never has to
    -- re-attribute off the (deferred / async) stack.
    if opts._resolved_ns then
      ctx.namespace = opts._resolved_ns
      ctx.persist_id = opts.persist
      ctx.store = nx.shada.plugin(opts._resolved_ns)
    end

    -- The whole lifecycle, as one linear async flow: wait for the surface to be ready, run
    -- setup (awaiting it if async), then the first render. Subsequent renders are reactive.
    nx.async(function()
      nx.await(nx.wait_for(backend.ready, { message = "the component surface never became ready" }))
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

  -- `mount_persistent(opts)` — a persistent view mount (`mount{ persist=, … }`). Resolve the
  -- owner namespace ONCE, synchronously, at this (attributing) call site, then thread it
  -- explicitly through every later deferred / async call (create, the store, the restore
  -- router) so none of them re-attribute off the stack. Returns a proxy handle immediately;
  -- the real instance is built when EITHER a session restore claims the reserved slot OR the
  -- fresh fallback fires (whichever wins — `_real` makes `build` idempotent).
  local function mount_persistent(opts)
    local id = opts.persist
    local ns = nx._resolve_namespace(opts.namespace, "nx.view.component: persist")
    opts._resolved_ns = ns -- ctx.store / ctx.namespace key
    opts._create_ns = ns -- explicit owner ns for nx.view.create in off-stack contexts

    local proxy = { _closed = false }
    function proxy:close()
      if self._closed then
        return
      end
      self._closed = true
      if self._real then
        self._real:close()
      end
    end
    local function build(show_mode)
      if proxy._real or proxy._closed then
        return
      end
      proxy._real = instantiate(opts, show_mode)
    end

    -- Restore route: claim the reserved slot if this session reopened the view.
    register_component_restorer(ns, opts.namespace, id, function(place)
      build({ place = place })
    end)
    -- Fresh fallback: `nx.on_next_tick` runs AFTER boot's restore dispatch, so when no
    -- restore claimed us, mount fresh; idempotent against the restore route via `build`.
    nx.on_next_tick(function()
      build({ fresh = true })
    end)

    return proxy
  end

  local M = {}
  function M.mount(opts)
    opts = opts or {}
    -- A persistent mount (a stable `persist` id) is supported on the built-in view backend
    -- only — it's the surface a session can reserve a slot for. Defer showing until a
    -- restore or the fresh fallback claims it.
    if opts.persist and make_backend == view_backend then
      return mount_persistent(opts)
    end
    return instantiate(opts, { fresh = true })
  end

  return M
end

-- `nx.view.component(def)` — the view-backed component (a focus-taking `nx.view` buffer): the
-- common case (file tree, list, modal dialog). Sugar over `nx.component` with the view
-- backend; `mount(opts)` takes the view surface options (name / filetype / dock / split /
-- float), plus `persist = "<id>"` (+ optional `namespace`) to make the view survive a
-- restart — see the Persistence note on `nx.component` above.
function nx.view.component(def)
  assert(type(def) == "table", "nx.view.component: pass a { setup, render } table")
  return nx.component({
    setup = def.setup,
    render = def.render,
    backend = view_backend,
  })
end

return nx
