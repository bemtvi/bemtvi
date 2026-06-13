-- nxvim Lua prelude — timers.
-- nx.timer (alias vim.defer_fn) over the event-loop bridge. The interactive UI
-- surface (nx.ui.select / input / open) and the nx.validate / nx.deprecate no-ops
-- are not part of nxvim's config API (autocmds, diagnostics, keymaps, options) and
-- are intentionally absent.
local vim = vim
nx = nx or {}

-- ----- nx.timer [alias vim.defer_fn] -----------------------------------------
-- Wall-clock deferral rides the event-loop actor through the nx._timer_start /
-- nx._timer_stop bridge: a callback id is registered in nx._cb_fns, the actor
-- sleeps and fires LoopEvent::Timer, and the server runs the callback by id on its
-- thread. This is the same registry the keymap/schedule paths use.
nx._timer_active = nx._timer_active or {}

-- A minimal timer handle returned by nx.timer, so a caller can :stop() the
-- deferral before it fires (neovim returns a uv timer; nxvim returns this). It is
-- NOT the libuv handle API — the `nx` timer surface is the supported one.
local defer_handle = {}
defer_handle.__index = defer_handle
function defer_handle:stop()
  nx._timer_active[self._id] = nil
  nx._timer_stop(self._id)
  nx._cb_fns[self._id] = nil
  return 0
end
function defer_handle:is_active()
  return nx._timer_active[self._id] == true
end

-- nx.timer(fn, timeout): the canonical timer / defer primitive (aliased by
-- vim.defer_fn) — run `fn` once, `timeout` ms from now, on the loop — the
-- off-tick deferral configs use for retry patterns. Returns a handle so the
-- caller can :stop() it before it fires.
function nx.timer(fn, timeout)
  local id = nx._next_cb_id()
  nx._cb_fns[id] = fn
  nx._timer_active[id] = true -- armed; the returned handle's :is_active() reads this
  nx._timer_start(id, timeout or 0, 0) -- one-shot
  return setmetatable({ _id = id }, defer_handle)
end
vim.defer_fn = nx.timer
