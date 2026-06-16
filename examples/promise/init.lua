-- ~~~ nxvim nx.promise playground: Promises/A+, shaped like the browser ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/promise \
--       cargo run -p nxvim -- examples/promise/sample.txt
--
-- nx async is PROMISE-ONLY: a one-shot async API returns a promise (nx.run, fs)
-- and streaming is an async-iterator over them (nx.run_stream + nx.await_each).
-- `nx.promise` is the foundation — the exact browser object model: nx.promise.new
-- / :next / :catch / :finally and the all / all_settled / race / any / resolve /
-- reject / try combinators — plus nx.async/nx.await coroutine sugar so a chain of
-- awaits reads top-to-bottom. (nx.run and the nx.ui.* chooser/prompt surfaces are
-- already promise-only; the remaining callback APIs — LSP — follow on the same
-- principle.)
--
-- Everything below runs at startup with NO keypresses. Watch the message line
-- (`:messages` for the full history); each demo also records into `_G.promise_demo`,
-- so you can inspect at any moment with:
--     :lua print(vim.inspect(_G.promise_demo))
_G.promise_demo = {}

--------------------------------------------------------------------------------
-- 1. The basics: resolve → :next → :next. Reactions run as MICROTASKS (off the
--    current stack, at convergence) — exactly like the browser runs `.then`, so
--    `:next` is always async even for an already-resolved promise.
--    Note it's `:next`, not `:then` — `then` is a Lua keyword and won't parse.
--------------------------------------------------------------------------------
nx.promise
  .resolve(20)
  :next(function(v) return v + 1 end)
  :next(function(v)
    _G.promise_demo.basic = v
    print("[1] basic chain → " .. v) -- 21
  end)

--------------------------------------------------------------------------------
-- 2. Errors: a throw anywhere in a chain skips later :next handlers and lands in
--    the trailing :catch. One terminal catch covers the whole chain.
--------------------------------------------------------------------------------
nx.promise
  .resolve("config")
  :next(function() error("disk on fire") end)
  :next(function() print("[2] this NEVER runs") end)
  :catch(function(err)
    _G.promise_demo.caught = err
    print("[2] caught: " .. tostring(err))
  end)

--------------------------------------------------------------------------------
-- 3. nx.promise.delay — the promise-flavoured vim.defer_fn: an await-able sleep
--    on the loop (off the input tick). Chain it for retry/debounce that reads
--    linearly instead of nesting timers.
--------------------------------------------------------------------------------
nx.promise.delay(200, "woke up"):next(function(msg)
  _G.promise_demo.delayed = msg
  vim.notify("[3] " .. msg .. " ~200ms after startup (off the input tick)")
end)

--------------------------------------------------------------------------------
-- 4. Combinators. all() waits for every input (in order); race() takes the first
--    to settle. Mix promises and plain values freely.
--------------------------------------------------------------------------------
nx.promise
  .all({
    nx.promise.resolve(1),
    nx.promise.delay(120, 2),
    3, -- a plain value passes straight through
  })
  :next(function(vals)
    _G.promise_demo.all = vals
    print("[4] all → " .. vals[1] .. "," .. vals[2] .. "," .. vals[3])
  end)

nx.promise
  .race({ nx.promise.delay(300, "slow"), nx.promise.delay(60, "fast") })
  :next(function(winner) print("[4] race winner → " .. winner) end)

--------------------------------------------------------------------------------
-- 5. nx.async / nx.await — the real cure for callback hell. Inside an nx.async
--    function, nx.await(p) suspends until `p` settles and evaluates to its value,
--    so a sequence of async steps reads like straight-line code. The function
--    returns a promise for its result.
--
--    A rejected await raises inside the coroutine: catch it with a pcall around
--    the await (PUC 5.4 yields across pcall), with :catch on the RESULT (as
--    below), or by attaching :catch to the awaited promise.
--------------------------------------------------------------------------------
local load_settings = nx.async(function(name)
  print("[5] loading '" .. name .. "' …")
  local base = nx.await(nx.promise.delay(80, 10)) -- pretend: read a file
  local extra = nx.await(nx.promise.delay(80, 5)) -- pretend: read another
  return base + extra
end)

load_settings("init")
  :next(function(total)
    _G.promise_demo.async = total
    vim.notify("[5] async/await done → " .. total .. " (two awaits, no nesting)")
  end)
  :catch(function(err) vim.notify("[5] failed: " .. tostring(err)) end)

print("[*] config sourced — promises settle on later microtasks/timers above")
