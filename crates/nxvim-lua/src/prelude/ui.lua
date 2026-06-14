-- nxvim Lua prelude — timers + the async UI surface (nx.ui).
-- nx.timer (alias vim.defer_fn) over the event-loop bridge, plus nx.ui.select —
-- the callback-shaped chooser backed by the server's floating selectable-list
-- widget (docs/specs/2026-06-14-nx-ui-float-widget.md), aliased by vim.ui.select
-- per ADR 0002's whitelist. nx.ui.open and the nx.validate / nx.deprecate no-ops
-- are not part of nxvim's config API and remain intentionally absent.
local vim = vim
nx = nx or {}
nx.ui = nx.ui or {}
vim.ui = vim.ui or {}

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

-- ----- nx.ui.select [alias vim.ui.select] ------------------------------------
-- nx.ui.select(items, opts, on_choice): open a floating selectable list and call
-- on_choice(item, index) with the chosen element and its 1-based index — or
-- on_choice(nil, nil) on cancel. The server owns the widget, its navigation, and
-- the input grab; Lua only renders the display labels (opts.format_item, default
-- tostring) up front and maps the chosen index back to the original item, so an
-- arbitrary item table round-trips even though only strings cross the bridge.
-- Non-blocking and callback-shaped (ADR 0002 rule 3): the call returns at once
-- and on_choice fires on a later tick.
function nx.ui.select(items, opts, on_choice)
  opts = opts or {}
  on_choice = on_choice or function() end
  if type(items) ~= "table" then
    error("nx.ui.select: items must be a list", 2)
  end
  local format_item = opts.format_item or tostring
  local labels = {}
  for i, item in ipairs(items) do
    labels[i] = tostring(format_item(item))
  end
  -- An empty list has nothing to choose: resolve to cancel without a menu.
  if #labels == 0 then
    on_choice(nil, nil)
    return
  end
  local id = nx._next_cb_id()
  nx._cb_fns[id] = function(idx)
    -- idx: the 1-based chosen index, or nil on cancel.
    if idx == nil then
      return on_choice(nil, nil)
    end
    return on_choice(items[idx], idx)
  end
  nx._ui_select(labels, opts.prompt or "", id)
end
vim.ui.select = nx.ui.select
