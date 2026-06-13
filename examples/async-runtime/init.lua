-- ~~~ nxvim async Lua runtime playground: the event loop ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/async-runtime \
--       cargo run -p nxvim -- examples/async-runtime/sample.txt
--
-- Before this feature, all Lua ran synchronously on the input tick: vim.schedule
-- ran its callback inline and vim.defer_fn raised "not implemented". Now a
-- background actor (the event loop) owns timers and wakes the editor when they
-- complete — so deferred work and timers fire OFF the input tick without ever
-- stalling the editor.
--
-- Everything below runs at startup. Watch the MESSAGE LINE (and `:messages` for
-- the full history): lines appear on wall-clock time with no keypresses, and the
-- editor stays fully responsive the entire time — type, move, :q whenever.
--
-- Each demo also records its progress in `_G.async_demo`, so you can inspect the
-- state at any moment with:   :lua print(vim.inspect(_G.async_demo))
_G.async_demo = { schedule = "pending", defer = "pending", timer_ticks = 0 }

--------------------------------------------------------------------------------
-- 1. vim.schedule — defer to the END of the current convergence (not inline).
--    The print order proves it: "direct" is emitted first even though the
--    schedule call comes first, because the scheduled fn runs AFTER this chunk
--    settles rather than nested in it.
--------------------------------------------------------------------------------
vim.schedule(function()
  _G.async_demo.schedule = "ran"
  print("[schedule] ran after the config finished sourcing (deferred, not inline)")
end)
print("[schedule] this 'direct' line is printed BEFORE the scheduled one")

--------------------------------------------------------------------------------
-- 2. vim.defer_fn — run once, after a wall-clock delay, on the loop.
--    ~300ms after startup the message line shows this. The editor did not block
--    waiting for it.
--------------------------------------------------------------------------------
vim.defer_fn(function()
  _G.async_demo.defer = "fired"
  vim.notify("[defer_fn] fired ~300ms after startup, off the input tick")
end, 300)

--------------------------------------------------------------------------------
-- 3. A repeating timer via vim.defer_fn — it self-reschedules each tick, prints
--    on wall-clock time with no keypresses, and stops itself after 4 ticks.
--------------------------------------------------------------------------------
do
  local function tick()
    _G.async_demo.timer_ticks = _G.async_demo.timer_ticks + 1
    print("[timer] tick " .. _G.async_demo.timer_ticks .. " of 4")
    if _G.async_demo.timer_ticks < 4 then
      vim.defer_fn(tick, 250)
    else
      vim.notify("[timer] done — stopped after 4 ticks")
    end
  end
  vim.defer_fn(tick, 250)
end
