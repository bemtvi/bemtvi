-- nxvim Lua prelude — the `nx.*` namespace, nxvim's own config/plugin API.
--
-- This chunk loads LAST (see PRELUDE_MODULES in runtime.rs). Per ADR 0002 the
-- break is: `nx.*` is the canonical editor API, and the bounded `vim.*` whitelist
-- is *aliases onto it* — the same objects, the same semantics, two names. The
-- variable / option / dispatch / keymap surfaces are now *authored as `nx.*`* in
-- their home prelude chunks (stdlib / timer / nvim_api / keymap, plus `nx.cmd`
-- seeded by the Rust bridge), each setting the matching `vim.*` name to the same
-- object right after. So those nouns are already on `nx` by the time this chunk
-- runs — it does not re-bind them. What lives here is the rest of the config
-- surface a typical `init.lua` targets that has no `vim.*` twin or needs an
-- nxvim-native shape: event/command registration and the callback-shaped async.

nx = nx or {}

-- Events — structured autocmd subscriptions. `nx.on(event, opts, fn)`: the
-- canonical verb. `fn` (when given) is the handler; otherwise `opts.callback` /
-- `opts.command` apply, exactly as the underlying registry expects. Returns the
-- subscription id (droppable with `nx.off`).
function nx.on(event, opts, fn)
  opts = opts or {}
  if fn ~= nil then
    -- Don't mutate the caller's table; layer the handler on a shallow copy.
    local merged = {}
    for k, v in pairs(opts) do
      merged[k] = v
    end
    merged.callback = fn
    opts = merged
  end
  return nx.autocmd.create(event, opts)
end

-- Drop a subscription created by `nx.on`.
function nx.off(id)
  return nx.autocmd.del(id)
end

-- User commands — `nx.command(name, fn, opts)` defines `:Name`; `fn` is a
-- function or an ex-command string.
function nx.command(name, fn, opts)
  return nx.user_command.create(name, fn, opts)
end

-- Dock-scoped options (the dock scope, alongside nx.bo/nx.wo/nx.o). Set via
-- `nx.dock.opt(side).<name> = <value>` or inline in `nx.dock.open{...}`; read back
-- through the same proxy. `nx._dock_opts` is a write-through cache keyed by side,
-- and `nx.dock._set_opt` (Rust) queues the change to the core. Known options:
-- `showtabline` (0/1/2), `size`, `title`, `winhighlight`.
nx._dock_opts = nx._dock_opts or {}
local DOCK_OPT_DEFAULT = { showtabline = nil, size = 0, title = "", winhighlight = "" }

-- Apply one dock option: write-through the cache, then queue it to the core.
local function dock_set_opt(side, name, value)
  if DOCK_OPT_DEFAULT[name] == nil and name ~= "showtabline" then
    return nx.notify("nx.dock.opt: unknown option '" .. tostring(name) .. "'", 4)
  end
  nx._dock_opts[side] = nx._dock_opts[side] or {}
  nx._dock_opts[side][name] = value
  nx.dock._set_opt(side, name, value)
end

-- `nx.dock.opt(side)` — an options proxy for one dock, mirroring nx.wo/nx.bo:
-- reads return the cached value (or the default), writes queue the change.
nx.dock.opt = function(side)
  return setmetatable({}, {
    __index = function(_, k)
      local cached = nx._dock_opts[side]
      if cached and cached[k] ~= nil then
        return cached[k]
      end
      return DOCK_OPT_DEFAULT[k]
    end,
    __newindex = function(_, k, v)
      dock_set_opt(side, k, v)
    end,
  })
end

-- Wrap `nx.dock.open` so it accepts the dock options inline (`showtabline`,
-- `title`, `winhighlight`) alongside `side`/`size`/`buf`, applying them through the
-- same path so the read cache stays in sync.
local _dock_open_raw = nx.dock.open
nx.dock.open = function(o)
  _dock_open_raw({ side = o.side, size = o.size, buf = o.buf })
  if o.size ~= nil then
    nx._dock_opts[o.side] = nx._dock_opts[o.side] or {}
    nx._dock_opts[o.side].size = o.size
  end
  for _, name in ipairs({ "showtabline", "title", "winhighlight" }) do
    if o[name] ~= nil then
      dock_set_opt(o.side, name, o[name])
    end
  end
end

-- Dock ex-commands — thin wrappers over the Rust-backed `nx.dock.*` surface
-- (installed before the prelude), dogfooding the nx API. `:DockOpen {side} [size]`
-- opens/focuses a permanent edge panel; `:DockClose`/`:DockFocus {side}` address it.
nx.command("DockOpen", function(o)
  local side = o.fargs[1]
  if not side then
    return nx.notify("usage: :DockOpen {left|right|top|bottom} [size]", 4)
  end
  nx.dock.open({ side = side, size = tonumber(o.fargs[2]) })
end)
nx.command("DockClose", function(o)
  if o.fargs[1] then
    nx.dock.close(o.fargs[1])
  end
end)
nx.command("DockFocus", function(o)
  if o.fargs[1] then
    nx.dock.focus(o.fargs[1])
  end
end)

-- (`nx.notify` / `nx.schedule` — the callback-shaped async — are authored as
-- `nx.*` in prelude/runtime.lua, with `vim.*` aliased onto them there.)
--
-- Treesitter highlighting is controlled declaratively through buffer options
-- (nx.bo.filetype + nx.bo.ts_highlight), part of the options surface in
-- prelude/state.lua — there is no separate nx.treesitter verb API.

return nx
