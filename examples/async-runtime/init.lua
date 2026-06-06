-- ~~~ nxvim async Lua runtime playground: the event loop ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/async-runtime \
--       cargo run -p nxvim -- examples/async-runtime/sample.txt
--
-- Before this feature, all Lua ran synchronously on the input tick: vim.schedule
-- ran its callback inline, vim.defer_fn raised "not implemented", and vim.system
-- blocked the whole editor until the child exited. Now a background actor (the
-- event loop) owns timers and child processes and wakes the editor when they
-- complete — so deferred work, timers, and vim.system's on_exit all fire OFF the
-- input tick without ever stalling the editor.
--
-- Everything below runs at startup. Watch the MESSAGE LINE (and `:messages` for
-- the full history): lines appear on wall-clock time with no keypresses, and the
-- editor stays fully responsive the entire time — type, move, :q whenever.
--
-- Each demo also records its progress in `_G.async_demo`, so you can inspect the
-- state at any moment with:   :lua print(vim.inspect(_G.async_demo))
_G.async_demo = { schedule = "pending", defer = "pending", uv_ticks = 0, system = "pending" }

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
-- 3. vim.uv timer — a repeating timer that ticks a few times then stops itself.
--    Each tick prints on wall-clock time with no keypresses; it self-stops after
--    4 ticks (proof a repeating timer both repeats AND is stoppable).
--------------------------------------------------------------------------------
do
  local timer = vim.uv.new_timer()
  timer:start(250, 250, function()
    _G.async_demo.uv_ticks = _G.async_demo.uv_ticks + 1
    print("[uv timer] tick " .. _G.async_demo.uv_ticks .. " of 4")
    if _G.async_demo.uv_ticks >= 4 then
      timer:stop()
      vim.notify("[uv timer] done — stopped after 4 ticks")
    end
  end)
end

--------------------------------------------------------------------------------
-- 4. vim.system — run a child process ASYNCHRONOUSLY. on_exit fires off-tick
--    with { code, stdout, stderr }; the editor never blocks on the child.
--    (The synchronous form — vim.system(cmd):wait() with NO on_exit — still
--    exists for the short shell-outs an lsp/<server>.lua root_dir performs.)
--------------------------------------------------------------------------------
vim.system({ "sh", "-c", "sleep 0.5; echo hello-from-async" }, {}, function(result)
  local out = (result.stdout or ""):gsub("%s+$", "") -- trim echo's trailing newline
  _G.async_demo.system = "code=" .. tostring(result.code) .. " stdout=" .. out
  vim.notify("[vim.system] async on_exit: " .. _G.async_demo.system)
end)
print("[vim.system] spawned async; the editor kept running while it slept")
