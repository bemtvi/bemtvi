-- btv.view — plugin-owned, read-only content surfaces.
--
-- A view is the dockable, mountable generalization of the bottom panel: an
-- ordinary editor buffer whose lines a plugin replaces wholesale, whose `<CR>`
-- dispatches to an `on_select` callback, and which the editing grammar treats as
-- inert (navigation works, text-mutating keys don't). It is the content surface a
-- pure-Lua file tree / symbol list / any line-oriented widget mounts in a dock or a
-- split.
--
-- `btv.view.create{...}` returns a handle whose methods queue the native ops (the
-- `btv.view._*` Rust bridges) and whose Lua-side state — the per-line `userdata` and
-- the `on_select` callback — lives in the handle. The backing buffer number and the
-- view's cursor line arrive each tick via the `btv._view_buf` / `btv._view_line`
-- mirror, so `:set_decor` / `:bufnr` / `:line` read live editor state with no
-- round-trip. Navigation is plain normal-mode motion on the nomodifiable view
-- buffer; the one special key, `<CR>` → `confirm`, is a buffer-local default map
-- installed at create (`btv._install_view_keymaps`) and lands here through
-- `btv._view_select`.

btv.view = btv.view or {}
btv._views = btv._views or {} -- id -> handle
btv._view_next_id = btv._view_next_id or 0

-- The view's one activation action: `<CR>` → confirm → the handle's `on_select`. It
-- fires the native bridge (`btv._view_action` -> `Editor::apply_view_action`). Navigation
-- is plain normal-mode motion on the nomodifiable view buffer, so this is the only
-- view action.
btv.view.actions = btv.view.actions or {}
btv.view.actions.confirm = function()
  btv._view_action("confirm")
end

-- `btv._install_view_keymaps(buf)` — install the view's buffer-local default activation
-- maps. Called by the server right after the view's backing buffer is created (the
-- bufnr is known synchronously in core, ahead of the next-tick `btv._view_buf`
-- mirror), so the `<CR>` → `on_select` map exists immediately. `default = true` lets a
-- plugin override either map with its own `{ buffer = buf }` map. A view is an ordinary
-- `nomodifiable` buffer otherwise, so these are its only special keys. (The explorer /
-- quickfix install their maps off a `FileType` autocmd instead — see
-- prelude/keymap.lua — but a view's filetype is content-semantic and it may never be
-- the current buffer when `FileType` would fire, so it installs at create time.)
--
-- `<2-LeftMouse>` is the mouse form of `<CR>`: a single left-click positions the cursor
-- on a row (core dock/window mouse handling) and the double-click confirms it, so every
-- view is clickable for free — a plugin needn't wire the mouse itself.
function btv._install_view_keymaps(buf)
  btv.keymap.set(
    "n",
    "<CR>",
    btv.view.actions.confirm,
    { buffer = buf, default = true, desc = "Select entry" }
  )
  btv.keymap.set(
    "n",
    "<2-LeftMouse>",
    btv.view.actions.confirm,
    { buffer = buf, default = true, desc = "Select entry (double-click)" }
  )
end

local View = {}
View.__index = View

-- `btv.view.create{ name?, filetype?, persist?, namespace? }` -> handle. Mints the backing
-- read-only buffer (off-screen until mounted) and returns the handle. `filetype` drives
-- treesitter / decoration on the view buffer.
--
-- `persist` opts the view into cross-session restore: it is a stable, plugin-chosen string
-- id (instance-unique within the plugin) that core round-trips through the workspace
-- session — recording only `(namespace, id)` + the view's slot, never its content. On
-- restore core reserves the slot and hands the id back via `btv.view.on_restore`, and the
-- plugin (which keyed its own `btv.shada.plugin()` store by the same id) rebuilds the
-- content. Absent ⇒ the view is ephemeral (today's behavior — not persisted). The owning
-- `namespace` is auto-derived from the calling plugin's location (same resolver as
-- `btv.shada.plugin()`); `opts.namespace` is the escape hatch for a context that attributes
-- to no runtimepath entry (a bare `:lua` / RPC / test) and is an error from a real plugin
-- file — exactly the `btv.shada.plugin(dev_namespace)` contract.
function btv.view.create(opts)
  opts = opts or {}
  btv._view_next_id = btv._view_next_id + 1
  local id = btv._view_next_id
  local persist = opts.persist
  local namespace
  if persist ~= nil then
    if type(persist) ~= "string" or persist == "" then
      error("btv.view.create: persist must be a non-empty string id", 2)
    end
    namespace = btv._resolve_namespace(opts.namespace, "btv.view.create")
  end
  local self = setmetatable({
    id = id,
    name = opts.name or "",
    filetype = opts.filetype or "",
    persist = persist, -- plugin-chosen stable id (nil ⇒ ephemeral)
    namespace = namespace, -- core-resolved owner namespace (nil when not persisted)
    _userdata = {},
    _on_select = nil,
    _on_close = nil,
  }, View)
  btv._views[id] = self
  btv.view._create(id, self.name, self.filetype, namespace or "", persist or "")
  return self
end

-- `btv.view.pending_restores()` -> the views a session restore reserved a slot for but that
-- no plugin has adopted yet, each `{ namespace=, id=, win= }` (`win` is the reserved
-- window). The pull primitive behind `btv.view.on_restore` and the black-box test hook.
-- Refreshed each tick from core's `btv._view_pending` mirror.
function btv.view.pending_restores()
  return btv._view_pending or {}
end

btv._view_restorers = btv._view_restorers or {} -- namespace -> fn
-- Slots already handed to a handler this session, keyed "<ns>\0<id>", so the boot push
-- dispatch and a late `on_restore` drain-now never adopt the same slot twice (core drops an
-- adopted slot from the pending list, but the two dispatch paths can run within one tick,
-- before the `btv._view_pending` mirror refreshes).
btv._view_claimed = btv._view_claimed or {}

-- Hand one pending restore entry `e` to handler `fn`. The slot is marked claimed only when
-- the handler actually calls `place(view)` — a handler that declines this id (the component
-- framework's per-namespace router is a no-op for ids it hasn't registered yet) or errors
-- before placing leaves the slot UNCLAIMED, so a later attempt (a sibling component that
-- mounts after the router's first drain, or a reloaded plugin) can still adopt it. The
-- top-of-function guard makes a re-dispatch of an already-placed slot a no-op.
local function dispatch_restore(e, fn)
  local key = tostring(e.namespace) .. "\0" .. tostring(e.id)
  if btv._view_claimed[key] then
    return
  end
  local win = e.win
  local ok, err = pcall(fn, e.id, function(view)
    btv._view_claimed[key] = true
    return view:place_in(win)
  end)
  if not ok then
    btv.notify("btv.view.on_restore[" .. tostring(e.namespace) .. "] failed: " .. tostring(err), 4)
  end
end

-- `btv.view.on_restore(fn[, namespace])` — register THIS plugin's restorer for persisted
-- views, AND immediately claim any slot already reserved for it. After a session restore,
-- core reserves a slot (a placeholder window) for every persisted view the plugin had open;
-- each reserved slot whose owning namespace matches is dispatched to `fn(id, place)`:
--   * `id` is the plugin-chosen persist string (the same one passed to `create{ persist=}`),
--   * `place(view)` drops a freshly-created view into the reserved window.
-- The plugin keyed its own `btv.shada.plugin()` store by `id`, so it reads its saved state
-- back and rebuilds the view's content before calling `place`. The owning namespace is
-- resolved from the caller's location; the optional `namespace` arg is the escape hatch for
-- a no-attribution context (a bare `:lua` / RPC / test), the same contract as create's.
--
-- Call it whenever the plugin has finished loading — it is safe to register LATE. A plugin
-- loaded via `btv.plugins({ config = … })` runs its `config` (and this call) on a tick after
-- the server's boot restore dispatch, so that pass never sees its handler; the drain-now loop
-- below adopts the still-reserved slot at registration time instead. (Slots are reserved at
-- `shada_load`, before any plugin code runs, so they are already present in the live
-- `btv._view_pending` mirror whenever this is called.) Orphan slots are not reaped until every
-- eager plugin load has settled (see `btv._maybe_collapse_view_restores`), so a late handler
-- reliably gets its chance.
function btv.view.on_restore(fn, namespace)
  if type(fn) ~= "function" then
    error("btv.view.on_restore: expected a function", 2)
  end
  local ns = btv._resolve_namespace(namespace, "btv.view.on_restore")
  btv._view_restorers[ns] = fn
  for _, e in ipairs(btv._view_pending or {}) do
    if e.namespace == ns then
      dispatch_restore(e, fn)
    end
  end
  return fn
end

-- `btv._claim_pending_restore(ns, id)` — re-attempt the reserved slot for a SINGLE `(ns, id)`
-- against `ns`'s registered handler. `on_restore`'s drain covers a namespace's slots at
-- registration time, but the component framework registers one router per namespace and adds
-- components to it incrementally: a component that mounts after the router's first drain needs
-- its own slot re-attempted, which this does (idempotent via the claimed guard). A no-op when
-- the namespace has no handler or the slot is gone / already adopted.
function btv._claim_pending_restore(ns, id)
  local fn = btv._view_restorers[ns]
  if not fn then
    return
  end
  for _, e in ipairs(btv._view_pending or {}) do
    if e.namespace == ns and tostring(e.id) == tostring(id) then
      dispatch_restore(e, fn)
    end
  end
end

-- `btv._run_view_restores()` — the BOOT push dispatch, run ONCE by the server after the config
-- and boot-sourced plugins are in place (`restore_persisted_views`), with `btv._view_pending`
-- freshly mirrored. Delivers each reserved slot to its namespace's already-registered handler
-- (a synchronous `init.lua` / `pack/start` plugin — an async `btv.plugins` handler registers
-- later and self-claims via `on_restore`'s drain-now). Then decides orphan collapse.
function btv._run_view_restores()
  -- FIRST, wake the lazy plugins the reserved slots belong to. A slot whose namespace names
  -- a `cmd`/`keys`/`event`-lazy plugin has no handler here and never will — a restore
  -- presses none of those triggers — so without this its dock collapses as an orphan and the
  -- sidebar you quit with does not come back. The manager counts each wake-up load in flight
  -- (`btv._view_restore_pending_loads`), so the collapse decision below waits for the load's
  -- `config` to register its handler and claim the slot.
  if btv.plugins and btv.plugins._wake_for_view_restore then
    local want = {}
    for _, e in ipairs(btv._view_pending or {}) do
      -- Skip a namespace already registered: its handler is dispatched below (and its
      -- plugin, if managed, is loaded by definition).
      if not btv._view_restorers[e.namespace] then
        want[#want + 1] = e.namespace
      end
    end
    btv.plugins._wake_for_view_restore(want)
  end
  for _, e in ipairs(btv._view_pending or {}) do
    local fn = btv._view_restorers[e.namespace]
    if fn then
      dispatch_restore(e, fn)
    end
  end
  btv._view_restore_boot_ran = true
  btv._maybe_collapse_view_restores()
end

-- Orphan collapse coordinator. A reserved slot no plugin adopts must eventually collapse (its
-- placeholder window lingers empty otherwise), but only once no plugin can still claim it.
-- Two facts gate that: the boot push dispatch has run (synchronous handlers had their turn —
-- and it only runs when a restore actually reserved slots, so this also stays a no-op when
-- there were none), and no eager `btv.plugins` load is in flight (an async `config` that would
-- register a handler on a later tick). The plugin manager maintains the in-flight count in
-- `btv._view_restore_pending_loads` and calls this as each load settles. When both hold,
-- enqueue the collapse — harmless to call repeatedly, since core reaps the pending list once
-- and any later call finds it empty.
btv._view_restore_pending_loads = btv._view_restore_pending_loads or 0
function btv._maybe_collapse_view_restores()
  if not btv._view_restore_boot_ran then
    return
  end
  if (btv._view_restore_pending_loads or 0) > 0 then
    return
  end
  btv.view._collapse_orphans()
end

-- `:set_lines(lines)` — replace the view's content wholesale.
function View:set_lines(lines)
  btv.view._set_lines(self.id, lines or {})
  return self
end

-- `:set_userdata(list)` — opaque per-line data, parallel to the lines (1-based). The
-- entry for the selected line is handed to `on_select`. Pure Lua state.
function View:set_userdata(list)
  self._userdata = list or {}
  return self
end

-- `:on_select(fn)` — `fn(line, userdata)` fires on `<CR>` / confirm, with the 1-based
-- cursor line and that line's userdata entry. Pure Lua state.
function View:on_select(fn)
  self._on_select = fn
  return self
end

-- `:on_close(fn)` — `fn()` fires when the USER closes the view's window (`:q` / `:close` /
-- `<C-w>c` on the view buffer), letting the owner tear down a group of related views
-- (e.g. close every diff pane when one is `:q`'d). It does NOT fire on a programmatic
-- `:unmount()` / `:close()` — only the user close path records it — so the handler can
-- freely close other views without recursion. Pure Lua state.
function View:on_close(fn)
  self._on_close = fn
  return self
end

-- `:bufnr()` — the backing buffer number (from the mirror), or nil before the view's
-- buffer exists (i.e. before the create op has drained). The target for extmarks.
function View:bufnr()
  return btv._view_buf and btv._view_buf[self.id]
end

-- `:winid()` — the window currently showing the view (from the mirror), or nil while the
-- view is unmounted. The target for window-local options (`vim.wo[winid]`).
function View:winid()
  return btv._view_win and btv._view_win[self.id]
end

-- `:set_decor(ns, marks)` — replace namespace `ns`'s decoration on the view buffer
-- with `marks`. Each mark is `{ line, col, <extmark opts> }` (0-based `line`/`col`,
-- then any `nvim_buf_set_extmark` opt: `hl_group`, `end_col`, `virt_text`,
-- `sign_text`, `priority`, …). A no-op (warned) before the buffer exists.
function View:set_decor(ns, marks)
  local buf = self:bufnr()
  if not buf then
    return btv.notify("btv.view:set_decor: the view buffer does not exist yet", 3)
  end
  btv.buf.clear_namespace(buf, ns, 0, -1)
  for _, m in ipairs(marks or {}) do
    local o = {}
    for k, v in pairs(m) do
      if k ~= "line" and k ~= "col" then
        o[k] = v
      end
    end
    btv.buf.set_extmark(buf, ns, m.line, m.col, o)
  end
  return self
end

-- The float-mount config keywords, validated here so the native bridge can trust the
-- strings (the same closed sets `btv._open_win` enforces). `relative` is what the float
-- positions against; `anchor` is its pinned corner; `border` is the frame style.
local FLOAT_RELATIVE = { editor = true, win = true, cursor = true }
local FLOAT_ANCHOR = { NW = true, NE = true, SW = true, SE = true }
local FLOAT_BORDER =
  { none = true, single = true, double = true, rounded = true, solid = true, shadow = true }

-- `:mount(opts)` — show the view. `opts.dock = "left"|"right"|"top"|"bottom"` mounts it in
-- that dock (`opts.size` columns/rows); `opts.tab = true` mounts it as the sole window of
-- a fresh tab page (no split — the view fills the tab; closing it closes the tab);
-- `opts.split = "vsplit"|"split"` mounts it in a split of the main editor area;
-- `opts.float = { … }` mounts it in a floating window. Mounting focuses the view.
--
-- The float table takes `width` / `height` (inner size; required) — cells (a
-- number) or a viewport fraction (`"50vw"` / `"30vh"` / `"50%"`), which reflows on
-- resize — `relative` (`"editor"`|`"win"`|`"cursor"`, default `"editor"`; `"editor"` is
-- the WHOLE screen, dock bands included, so a centered float centers on the screen
-- rather than on whatever region happens to be focused), and either the
-- high-level `align` (`"top-left"`|`"top"`|`"top-right"`|`"left"`|`"center"`|`"right"`|
-- `"bottom-left"`|`"bottom"`|`"bottom-right"`) + `margin` (a gap from the edges: a number
-- — the vertical gap, the horizontal sides getting 2x to look even since cells are
-- ~2x taller than wide — or an explicit {vertical, horizontal} / {top, right,
-- bottom, left} / {top=, …}), or the
-- low-level `anchor` (`"NW"`|`"NE"`|`"SW"`|`"SE"`, default `"NW"`) + `row` / `col` (offset,
-- default 0). Plus `border` (default `"rounded"`), `title`, `zindex`, `focusable`, and
-- `grab`. `grab` (default true) hard-locks focus to the float
-- like the bottom panel — `<C-w>` can't leave it and unmount restores the prior window —
-- which is what a modal dialog (a checkbox list, a confirm) wants; pass `grab = false` for
-- a non-modal floating panel that focus can leave.
function View:mount(opts)
  opts = opts or {}
  if opts.dock then
    btv.view._mount_dock(self.id, opts.dock, opts.size)
  elseif opts.tab then
    btv.view._mount_tab(self.id)
  elseif opts.split then
    btv.view._mount_split(self.id, opts.split ~= "split")
  elseif opts.float then
    local f = opts.float
    local relative = f.relative or "editor"
    local anchor = f.anchor or "NW"
    local border = f.border or "rounded"
    -- Fail loud on a bad enum / missing size rather than silently mispositioning.
    if not FLOAT_RELATIVE[relative] then
      return btv.notify(
        "btv.view:mount{ float }: invalid relative '" .. tostring(relative) .. "'",
        4
      )
    end
    if not FLOAT_ANCHOR[anchor] then
      return btv.notify("btv.view:mount{ float }: invalid anchor '" .. tostring(anchor) .. "'", 4)
    end
    if not FLOAT_BORDER[border] then
      return btv.notify("btv.view:mount{ float }: invalid border '" .. tostring(border) .. "'", 4)
    end
    if f.width == nil or f.height == nil then
      return btv.notify("btv.view:mount{ float }: width and height are required", 4)
    end
    -- `width`/`height` accept cells or a viewport fraction ("50vw" / "30vh" /
    -- "50%"); `align` is the high-level placement word (default the low-level
    -- anchor/offset form); `margin` insets an aligned float from the edges. The
    -- shared `btv._geom` normalizer validates and emits the wire shape.
    local ok, cfg = pcall(function()
      return {
        relative = relative,
        win = f.win or 0,
        anchor = anchor,
        row = f.row or 0,
        col = f.col or 0,
        width = btv._geom.size(f.width),
        height = btv._geom.size(f.height),
        align = btv._geom.align(f.align),
        margin = btv._geom.margin(f.margin),
        zindex = f.zindex or 50,
        focusable = f.focusable ~= false,
        border = border,
        title = f.title,
        grab = f.grab ~= false,
      }
    end)
    if not ok then
      return btv.notify("btv.view:mount{ float }: " .. tostring(cfg), 4)
    end
    btv.view._mount_float(self.id, cfg)
  else
    btv.notify("btv.view:mount: pass one of { dock = … } / { split = … } / { float = … }", 4)
  end
  return self
end

-- `:place_in(win)` — adopt the reserved restore slot `win` for this view: retarget that
-- placeholder window (minted by a session restore for a persisted view) to this view's
-- backing buffer, instead of opening a fresh window like `:mount`. This is what the `place`
-- argument handed to an `btv.view.on_restore` handler calls
-- (`place = function(view) view:place_in(win) end`), so a plugin rarely calls it directly.
function View:place_in(win)
  btv.view._adopt(self.id, win)
  return self
end

-- `:unmount()` — remove the view from view, keeping it (and its content) alive for a
-- later `:mount`.
function View:unmount()
  btv.view._unmount(self.id)
  return self
end

-- `:focus()` — move focus to the window showing the view.
function View:focus()
  btv.view._focus(self.id)
  return self
end

-- `:line()` — the view's 1-based cursor line (from the mirror), valid while the view
-- is focused. nil before the buffer exists.
function View:line()
  return btv._view_line and btv._view_line[self.id]
end

-- `:cursor()` — the view's cursor as `(line, col)`. `col` is always 0 in v1 (a view's
-- cursor rests at column 0); the line is `:line()`.
function View:cursor()
  return self:line(), 0
end

-- `:set_cursor(line)` — focus the view and move its cursor to 1-based `line` (clamped
-- to the content; column 0). The reveal / find-file primitive — the one sanctioned
-- cursor write; ordinary navigation is plain normal-mode motion.
function View:set_cursor(line)
  btv.view._set_cursor(self.id, line)
  return self
end

-- `:redraw()` — request a repaint. The editor already repaints at the end of every
-- input batch / drained chunk, so this is a no-op kept for API completeness (and so
-- a plugin can express intent at the call site).
function View:redraw()
  return self
end

-- `:close()` — unmount the view and drop its backing buffer and registry entry.
function View:close()
  btv.view._destroy(self.id)
  btv._views[self.id] = nil
end

-- `btv._view_select(id, line)` — dispatch a `<CR>`/confirm on view `id` to its handle's
-- `on_select(line, userdata[line])`. Called from the server after the core records
-- the select. A no-op when the view has no handler.
function btv._view_select(id, line)
  local v = btv._views[id]
  if not v or not v._on_select then
    return
  end
  local ud = v._userdata and v._userdata[line]
  v._on_select(line, ud)
end

-- `btv._view_closed(id)` — dispatch a USER window-close on view `id` to its handle's
-- `on_close()`. Called from the server when the user `:q`s / `:close`s a view window. A
-- no-op when the view is gone or has no handler.
function btv._view_closed(id)
  local v = btv._views[id]
  if v and v._on_close then
    v._on_close()
  end
end
