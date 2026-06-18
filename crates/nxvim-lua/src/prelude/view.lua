-- nx.view — plugin-owned, read-only content surfaces.
--
-- A view is the dockable, mountable generalization of the bottom panel: an
-- ordinary editor buffer whose lines a plugin replaces wholesale, whose `<CR>`
-- dispatches to an `on_select` callback, and which the editing grammar treats as
-- inert (navigation works, text-mutating keys don't). It is the content surface a
-- pure-Lua file tree / symbol list / any line-oriented widget mounts in a dock or a
-- split.
--
-- `nx.view.create{...}` returns a handle whose methods queue the native ops (the
-- `nx.view._*` Rust bridges) and whose Lua-side state — the per-line `userdata` and
-- the `on_select` callback — lives in the handle. The backing buffer number and the
-- view's cursor line arrive each tick via the `nx._view_buf` / `nx._view_line`
-- mirror, so `:set_decor` / `:bufnr` / `:line` read live editor state with no
-- round-trip. Navigation is plain normal-mode motion on the nomodifiable view
-- buffer; the one special key, `<CR>` → `confirm`, is a buffer-local default map
-- installed at create (`nx._install_view_keymaps`) and lands here through
-- `nx._view_select`.

nx.view = nx.view or {}
nx._views = nx._views or {} -- id -> handle
nx._view_next_id = nx._view_next_id or 0

-- The view's one activation action: `<CR>` → confirm → the handle's `on_select`. It
-- fires the native bridge (nx._view_action -> Editor::apply_view_action). Navigation
-- is plain normal-mode motion on the nomodifiable view buffer, so this is the only
-- view action.
nx.view.actions = nx.view.actions or {}
nx.view.actions.confirm = function()
  nx._view_action("confirm")
end

-- nx._install_view_keymaps(buf) — install the view's buffer-local default activation
-- map. Called by the server right after the view's backing buffer is created (the
-- bufnr is known synchronously in core, ahead of the next-tick `nx._view_buf`
-- mirror), so the `<CR>` → `on_select` map exists immediately. `default = true` lets a
-- plugin override `<CR>` with its own `{ buffer = buf }` map. A view is an ordinary
-- `nomodifiable` buffer otherwise, so this is its only special key. (The explorer /
-- quickfix install their maps off a `FileType` autocmd instead — see
-- prelude/keymap.lua — but a view's filetype is content-semantic and it may never be
-- the current buffer when `FileType` would fire, so it installs at create time.)
function nx._install_view_keymaps(buf)
  nx.keymap.set(
    "n",
    "<CR>",
    nx.view.actions.confirm,
    { buffer = buf, default = true, desc = "Select entry" }
  )
end

local View = {}
View.__index = View

-- nx.view.create{ name?, filetype? } -> handle. Mints the backing read-only buffer
-- (off-screen until mounted) and returns the handle. `filetype` drives treesitter /
-- decoration on the view buffer.
function nx.view.create(opts)
  opts = opts or {}
  nx._view_next_id = nx._view_next_id + 1
  local id = nx._view_next_id
  local self = setmetatable({
    id = id,
    name = opts.name or "",
    filetype = opts.filetype or "",
    _userdata = {},
    _on_select = nil,
  }, View)
  nx._views[id] = self
  nx.view._create(id, self.name, self.filetype)
  return self
end

-- :set_lines(lines) — replace the view's content wholesale.
function View:set_lines(lines)
  nx.view._set_lines(self.id, lines or {})
  return self
end

-- :set_userdata(list) — opaque per-line data, parallel to the lines (1-based). The
-- entry for the selected line is handed to `on_select`. Pure Lua state.
function View:set_userdata(list)
  self._userdata = list or {}
  return self
end

-- :on_select(fn) — `fn(line, userdata)` fires on `<CR>` / confirm, with the 1-based
-- cursor line and that line's userdata entry. Pure Lua state.
function View:on_select(fn)
  self._on_select = fn
  return self
end

-- :bufnr() — the backing buffer number (from the mirror), or nil before the view's
-- buffer exists (i.e. before the create op has drained). The target for extmarks.
function View:bufnr()
  return nx._view_buf and nx._view_buf[self.id]
end

-- :set_decor(ns, marks) — replace namespace `ns`'s decoration on the view buffer
-- with `marks`. Each mark is `{ line, col, <extmark opts> }` (0-based `line`/`col`,
-- then any `nvim_buf_set_extmark` opt: `hl_group`, `end_col`, `virt_text`,
-- `sign_text`, `priority`, …). A no-op (warned) before the buffer exists.
function View:set_decor(ns, marks)
  local buf = self:bufnr()
  if not buf then
    return nx.notify("nx.view:set_decor: the view buffer does not exist yet", 3)
  end
  nx.buf.clear_namespace(buf, ns, 0, -1)
  for _, m in ipairs(marks or {}) do
    local o = {}
    for k, v in pairs(m) do
      if k ~= "line" and k ~= "col" then
        o[k] = v
      end
    end
    nx.buf.set_extmark(buf, ns, m.line, m.col, o)
  end
  return self
end

-- The float-mount config keywords, validated here so the native bridge can trust the
-- strings (the same closed sets `nx._open_win` enforces). `relative` is what the float
-- positions against; `anchor` is its pinned corner; `border` is the frame style.
local FLOAT_RELATIVE = { editor = true, win = true, cursor = true }
local FLOAT_ANCHOR = { NW = true, NE = true, SW = true, SE = true }
local FLOAT_BORDER =
  { none = true, single = true, double = true, rounded = true, solid = true, shadow = true }

-- :mount(opts) — show the view. `opts.dock = "left"|"right"|"top"|"bottom"` mounts it in
-- that dock (`opts.size` columns/rows); `opts.split = "vsplit"|"split"` mounts it in a
-- split of the main editor area; `opts.float = { … }` mounts it in a floating window.
-- Mounting focuses the view.
--
-- The float table takes `width` / `height` (inner size; required), `relative`
-- ("editor"|"win"|"cursor", default "editor"), `anchor` ("NW"|"NE"|"SW"|"SE", default
-- "NW"), `row` / `col` (offset, default 0), `border` (default "rounded"), `title`,
-- `zindex`, `focusable`, and `grab`. `grab` (default true) hard-locks focus to the float
-- like the bottom panel — `<C-w>` can't leave it and unmount restores the prior window —
-- which is what a modal dialog (a checkbox list, a confirm) wants; pass `grab = false` for
-- a non-modal floating panel that focus can leave.
function View:mount(opts)
  opts = opts or {}
  if opts.dock then
    nx.view._mount_dock(self.id, opts.dock, opts.size)
  elseif opts.split then
    nx.view._mount_split(self.id, opts.split ~= "split")
  elseif opts.float then
    local f = opts.float
    local relative = f.relative or "editor"
    local anchor = f.anchor or "NW"
    local border = f.border or "rounded"
    -- Fail loud on a bad enum / missing size rather than silently mispositioning.
    if not FLOAT_RELATIVE[relative] then
      return nx.notify("nx.view:mount{ float }: invalid relative '" .. tostring(relative) .. "'", 4)
    end
    if not FLOAT_ANCHOR[anchor] then
      return nx.notify("nx.view:mount{ float }: invalid anchor '" .. tostring(anchor) .. "'", 4)
    end
    if not FLOAT_BORDER[border] then
      return nx.notify("nx.view:mount{ float }: invalid border '" .. tostring(border) .. "'", 4)
    end
    if type(f.width) ~= "number" or type(f.height) ~= "number" then
      return nx.notify("nx.view:mount{ float }: width and height are required", 4)
    end
    nx.view._mount_float(self.id, {
      relative = relative,
      win = f.win or 0,
      anchor = anchor,
      row = f.row or 0,
      col = f.col or 0,
      width = f.width,
      height = f.height,
      zindex = f.zindex or 50,
      focusable = f.focusable ~= false,
      border = border,
      title = f.title,
      grab = f.grab ~= false,
    })
  else
    nx.notify("nx.view:mount: pass one of { dock = … } / { split = … } / { float = … }", 4)
  end
  return self
end

-- :unmount() — remove the view from view, keeping it (and its content) alive for a
-- later :mount.
function View:unmount()
  nx.view._unmount(self.id)
  return self
end

-- :focus() — move focus to the window showing the view.
function View:focus()
  nx.view._focus(self.id)
  return self
end

-- :line() — the view's 1-based cursor line (from the mirror), valid while the view
-- is focused. nil before the buffer exists.
function View:line()
  return nx._view_line and nx._view_line[self.id]
end

-- :cursor() — the view's cursor as `(line, col)`. `col` is always 0 in v1 (a view's
-- cursor rests at column 0); the line is `:line()`.
function View:cursor()
  return self:line(), 0
end

-- :set_cursor(line) — focus the view and move its cursor to 1-based `line` (clamped
-- to the content; column 0). The reveal / find-file primitive — the one sanctioned
-- cursor write; ordinary navigation is plain normal-mode motion.
function View:set_cursor(line)
  nx.view._set_cursor(self.id, line)
  return self
end

-- :redraw() — request a repaint. The editor already repaints at the end of every
-- input batch / drained chunk, so this is a no-op kept for API completeness (and so
-- a plugin can express intent at the call site).
function View:redraw()
  return self
end

-- :close() — unmount the view and drop its backing buffer and registry entry.
function View:close()
  nx.view._destroy(self.id)
  nx._views[self.id] = nil
end

-- nx._view_select(id, line) — dispatch a `<CR>`/confirm on view `id` to its handle's
-- `on_select(line, userdata[line])`. Called from the server after the core records
-- the select. A no-op when the view has no handler.
function nx._view_select(id, line)
  local v = nx._views[id]
  if not v or not v._on_select then
    return
  end
  local ud = v._userdata and v._userdata[line]
  v._on_select(line, ud)
end

-- ===========================================================================
-- nx.view.component — a Vue-shaped component model over the raw view handle.
-- ===========================================================================
--
-- A component is `{ setup, render }`. `setup(ctx, props)` runs ONCE when the view is
-- mounted — it owns side effects: it creates reactive state, binds keys, fetches data.
-- `render(state)` is PURE: it maps state to what's on screen (`{ lines, decor }`), and
-- the framework re-runs it automatically whenever the reactive state changes. **Both may
-- be async** — write `nx.await(...)` straight inside them (the framework runs each in an
-- `nx.async` coroutine), so a `setup` that loads from `nx.fs`, or a `render` that has to
-- fetch to display, just reads top-to-bottom.
--
-- The payoff over the raw handle is that the framework owns the lifecycle the raw API
-- leaks: it waits for the backing buffer to materialize before running `setup`, so
-- `ctx.keymap_set` / `ctx.bufnr()` are valid immediately (no `nx.schedule` tick-dance), and it
-- batches state writes into one re-render per tick (no manual `render()` calls). It is the
-- nx.view analogue of a Vue single-file component: `setup` = `<script setup>`, `render` =
-- the template, the reactive state = `ref`/`reactive`.

-- A deep reactive proxy: reading a nested table returns a nested proxy (lazily, cached),
-- and writing ANY key at ANY depth calls `on_change` — coarse-grained (no per-key dep
-- tracking: a write re-runs the whole `render`, which is cheap for a line list and keeps
-- the model tiny). Iterate a reactive table with `ipairs` / `#` (both honour the
-- metamethods on PUC 5.4); `pairs` does NOT (5.4 dropped `__pairs`), so a render that
-- needs unordered iteration should key off an array it builds in `setup`.
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
        return wrap(t[k])
      end,
      __newindex = function(_, k, v)
        t[k] = v
        on_change()
      end,
      __len = function()
        return #t
      end,
    })
    cache[t] = p
    return p
  end
  return wrap(root)
end

-- nx.view.component(def) -> { mount(opts) }. `def.render` is required; `def.setup` is
-- optional. `mount(opts)` instantiates: `opts.float` / `opts.dock` / `opts.split` choose
-- the surface (default: a centered grabbing float), `opts.props` is handed to `setup`,
-- `opts.name` / `opts.filetype` name the view. Returns the instance (with `:close()`).
function nx.view.component(def)
  assert(type(def) == "table", "nx.view.component: pass a { setup, render } table")
  assert(type(def.render) == "function", "nx.view.component: a render function is required")

  local M = {}
  function M.mount(opts)
    opts = opts or {}
    nx.view._component_ns = nx.view._component_ns or nx.ns.create("nx.view.component")

    local v = nx.view.create({ name = opts.name or "nx-component", filetype = opts.filetype })
    if opts.dock then
      v:mount({ dock = opts.dock, size = opts.size })
    elseif opts.split then
      v:mount({ split = opts.split })
    else
      v:mount({ float = opts.float or { width = 50, height = 12, grab = true } })
    end

    local inst = { view = v, _closed = false, _on_close = {} }
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
          if mine ~= gen or inst._closed or type(out) ~= "table" then
            return
          end
          v:set_lines(out.lines or out) -- accept { lines=, decor= } or a bare line list
          if out.decor then
            v:set_decor(nx.view._component_ns, out.decor)
          end
        end)
        :catch(function(e)
          nx.notify("nx.view.component: render error: " .. tostring(e), 4)
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
      v:close()
    end

    -- The context handed to `setup`: reactive state, the live cursor, buffer-local key
    -- binding, and lifecycle. Everything here is valid immediately because the framework
    -- only runs `setup` once the backing buffer exists.
    local ctx = {
      view = v,
      props = opts.props or {},
      reactive = function(tbl)
        return make_reactive(tbl or {}, schedule_render)
      end,
      line = function()
        return v:line()
      end,
      set_cursor = function(n)
        v:set_cursor(n)
      end,
      bufnr = function()
        return v:bufnr()
      end,
      refresh = schedule_render,
      on_close = function(fn)
        inst._on_close[#inst._on_close + 1] = fn
      end,
      close = function()
        inst:close()
      end,
      -- A thin wrapper over the real `nx.keymap.set(mode, lhs, rhs, opts)` — same
      -- signature — that defaults `buffer` to this view and `nowait` on (so a single
      -- dialog key fires without waiting on a longer mapping). Any field the caller
      -- passes in `opts` overrides the defaults, including `buffer` / `nowait`.
      keymap_set = function(mode, lhs, rhs, user_opts)
        local merged = { buffer = v:bufnr(), nowait = true }
        for k, val in pairs(user_opts or {}) do
          merged[k] = val
        end
        nx.keymap.set(mode, lhs, rhs, merged)
      end,
    }

    -- The whole lifecycle, as one linear async flow: wait for the buffer, run setup
    -- (awaiting it if async), then the first render. Subsequent renders are reactive.
    nx.async(function()
      -- The backing buffer number arrives a tick after `create`/`mount` (via the
      -- `nx._view_buf` mirror). Yield to the loop until it's live — bounded, fail loud.
      local tries = 0
      while not v:bufnr() do
        if tries > 200 then
          error("the backing buffer never materialized")
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
      nx.notify("nx.view.component: setup error: " .. tostring(e), 4)
    end)

    return inst
  end

  return M
end
