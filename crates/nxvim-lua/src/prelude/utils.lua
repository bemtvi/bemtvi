-- nxvim Lua prelude — nx.utils, the general-purpose helper namespace.
--
-- The home for broadly-useful utilities that aren't data helpers (those are
-- nx.tbl / nx.list / nx.str / nx.iter in prelude/stdlib.lua) and aren't a feature
-- API — control-flow / timing glue plugin authors reach for. nxvim-native (no
-- vim.* twin). Loaded after prelude/runtime.lua (nx.timer / nx.schedule) and
-- prelude/promise.lua, so a util may build on timers AND the promise/async surface.
local vim = vim
nx = nx or {}
nx.utils = nx.utils or {}

-- ----- nx.utils.debounce -----------------------------------------------------
-- nx.utils.debounce(fn, ms): wrap `fn` into a trailing-edge debounce over
-- nx.timer — the returned value runs `fn` once, `ms` after the LAST call, so a
-- burst of rapid calls collapses to a single invocation with the most recent
-- arguments. A timing/control-flow helper (which-key's show-delay, on-change
-- handlers, resize / scroll reactions); it runs nothing on the input path.
--
-- It is callback-shaped, NOT promise-shaped: debounce coalesces a stream of many
-- calls, whereas a promise models one eventual value — different jobs. They
-- compose, though: pass an nx.async function as `fn` to kick awaitable work after
-- the quiet period, and reach for nx.promise.delay when you want an *await-able*
-- one-shot sleep instead.
--
-- The result is callable AND carries:
--   :cancel()  drop a pending invocation (the next call re-arms)
--   :flush()   run a pending invocation now (no-op when idle)
-- Each call (re)arms the timer; nothing fires until the calls stop for `ms`.
function nx.utils.debounce(fn, ms)
  if type(fn) ~= "function" then
    error("nx.utils.debounce: fn must be a function", 2)
  end
  ms = ms or 0
  local timer -- the armed nx.timer handle while a call is pending, else nil
  -- The most recent call's arguments, captured `{ ... }` + count (the prelude's
  -- vararg idiom, e.g. schedule_wrap — PUC has no whitelisted table.pack); nil
  -- when idle.
  local args, argc
  local function fire()
    timer = nil
    local a, n = args, argc
    args, argc = nil, nil
    fn(table.unpack(a, 1, n))
  end
  local debounced = setmetatable({}, {
    __call = function(_, ...)
      args, argc = { ... }, select("#", ...)
      if timer then
        timer:stop()
      end
      timer = nx.timer(fire, ms)
    end,
  })
  function debounced:cancel()
    if timer then
      timer:stop()
      timer = nil
    end
    args, argc = nil, nil
  end
  function debounced:flush()
    if timer then
      timer:stop()
      fire()
    end
  end
  return debounced
end
