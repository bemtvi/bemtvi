-- nxvim Lua prelude — nx.promise, a Promises/A+ surface shaped like the browser's.
--
-- `nx` async is PROMISE-ONLY (ADR 0002): a one-shot async API returns a promise
-- (nx.run, nx.fs, …) and streaming is an async-iterator over them (nx.run_stream +
-- nx.await_each). `nx.promise` is the foundation: the exact object model the
-- browser exposes — `nx.promise.new(executor)`, `:next`/`:catch`/`:finally`, and
-- the `nx.promise.all/all_settled/race/any/resolve/reject/try` combinators — plus
-- `nx.async`/`nx.await` coroutine sugar so a chain of awaits reads like
-- straight-line code. (`nx.promise.try` folds a function that may throw
-- SYNCHRONOUSLY or reject ASYNCHRONOUSLY into one chain — see its definition.)
--
-- This is pure Lua layered on the existing async runtime: a promise's reactions
-- run as MICROTASKS via `nx.schedule` (prelude/runtime.lua) — deferred to the end
-- of the current convergence, never inline, exactly like the browser runs `.then`
-- callbacks off the current stack. So `:next` is *always* asynchronous even for an
-- already-settled promise, matching Promises/A+ §2.2.4.
--
-- It is an nxvim-native surface (no `vim.*` twin — neovim core has no Promise), so
-- it lives entirely on `nx`. As the async FOUNDATION every later surface builds on
-- (process / picker / complete / nx.ui), it loads early — right after the runtime
-- services it needs: `nx.schedule` (the microtask primitive the reactions run on)
-- and `nx.timer` (the wall-clock deferral `nx.promise.delay` uses), both installed
-- in prelude/runtime.lua just above.

local vim = vim
nx = nx or {}

-- Run `fn` as a microtask — off the current stack, at convergence, same tick.
-- The single scheduling primitive the whole module is built on.
local function microtask(fn)
  nx.schedule(fn)
end

-- A value is "callable" if it's a function or a table with a __call metamethod —
-- the test Promises/A+ uses before treating an onFulfilled/onRejected as a handler.
local function is_callable(v)
  if type(v) == "function" then
    return true
  end
  if type(v) == "table" then
    local mt = getmetatable(v)
    return mt ~= nil and type(mt.__call) == "function"
  end
  return false
end

-- ----- the Promise object ----------------------------------------------------

local Promise = {}
Promise.__index = Promise

local PENDING, FULFILLED, REJECTED = "pending", "fulfilled", "rejected"

-- True for any of our own promises (the cheap, exact thenable check).
local function is_promise(v)
  return type(v) == "table" and getmetatable(v) == Promise
end

local function new_pending()
  return setmetatable({
    _state = PENDING,
    _value = nil,
    _queue = nil, -- list of reaction thunks while pending; nil once settled
    _handled = false, -- did anything subscribe (for unhandled-rejection reporting)
  }, Promise)
end

-- Forward declaration: the Promises/A+ resolution procedure (§2.3).
local resolve_promise

-- A rejected promise that nothing ever handles is a silent swallowed error — the
-- one thing this project refuses to ship (CLAUDE.md: no silent failures). The
-- browser reports "Uncaught (in promise)"; we do the same. After a promise
-- rejects we schedule a check one microtask later: if STILL nothing has
-- subscribed (`:next`/`:catch`/`:finally` flips `_handled`), surface it. The
-- delay gives synchronous `p:catch(...)` right after creation time to attach.
local function report_unhandled(p)
  microtask(function()
    if p._state == REJECTED and not p._handled then
      vim.notify("nxvim: unhandled promise rejection: " .. tostring(p._value), 4)
    end
  end)
end

-- Move a pending promise to its final state and flush queued reactions as
-- microtasks. A no-op if already settled (a promise settles exactly once).
local function settle(p, state, value)
  if p._state ~= PENDING then
    return
  end
  p._state = state
  p._value = value
  local queue = p._queue
  p._queue = nil
  if queue then
    for _, run in ipairs(queue) do
      microtask(run)
    end
  end
  if state == REJECTED then
    report_unhandled(p)
  end
end

-- Register reactions on `p`. If still pending they queue; if already settled the
-- matching one runs on a fresh microtask. Subscribing marks the promise handled,
-- which is what suppresses the unhandled-rejection warning.
function Promise:_subscribe(on_fulfilled, on_rejected)
  self._handled = true
  local function run()
    if self._state == FULFILLED then
      on_fulfilled(self._value)
    else
      on_rejected(self._value)
    end
  end
  if self._state == PENDING then
    self._queue = self._queue or {}
    self._queue[#self._queue + 1] = run
  else
    microtask(run)
  end
end

-- The resolution procedure: settle `p` with `x`, adopting `x`'s state if it is a
-- promise/thenable (so `resolve(anotherPromise)` waits on it) rather than
-- fulfilling with the promise object itself.
resolve_promise = function(p, x)
  if x == p then
    -- Resolving a promise with itself would deadlock — reject per §2.3.1.
    return settle(p, REJECTED, "TypeError: promise resolved with itself (chaining cycle)")
  end
  if is_promise(x) then
    return x:_subscribe(function(v)
      resolve_promise(p, v)
    end, function(e)
      settle(p, REJECTED, e)
    end)
  end
  -- Foreign thenable: anything exposing a callable `next` (our convention). Adopt
  -- it through its own `next`, guarding the access/call the way §2.3.3 requires.
  if type(x) == "table" then
    local ok, next_fn = pcall(function()
      return x.next
    end)
    if not ok then
      return settle(p, REJECTED, next_fn)
    end
    if is_callable(next_fn) then
      local called = false
      local ok2, err = pcall(next_fn, x, function(v)
        if not called then
          called = true
          resolve_promise(p, v)
        end
      end, function(e)
        if not called then
          called = true
          settle(p, REJECTED, e)
        end
      end)
      if not ok2 and not called then
        settle(p, REJECTED, err)
      end
      return
    end
  end
  settle(p, FULFILLED, x)
end

-- :next(on_fulfilled, on_rejected) — the spine. Returns a NEW promise resolved
-- with the handler's return (adopting it if it's itself a promise), or rejected
-- if the handler throws. A missing handler passes the value/reason straight
-- through to the returned promise, which is what makes a bare `:catch` at the end
-- of a chain catch errors from anywhere earlier in it.
--
-- Named `:next` (not `:then`) because `then` is a Lua keyword — `p:then(...)`
-- won't parse. `:next` is the idiomatic Lua spelling; `:catch`/`:finally` keep
-- their browser names verbatim.
function Promise:next(on_fulfilled, on_rejected)
  local result = new_pending()
  self:_subscribe(function(value)
    if is_callable(on_fulfilled) then
      local ok, r = pcall(on_fulfilled, value)
      if ok then
        resolve_promise(result, r)
      else
        settle(result, REJECTED, r)
      end
    else
      settle(result, FULFILLED, value)
    end
  end, function(reason)
    if is_callable(on_rejected) then
      local ok, r = pcall(on_rejected, reason)
      if ok then
        resolve_promise(result, r)
      else
        settle(result, REJECTED, r)
      end
    else
      settle(result, REJECTED, reason)
    end
  end)
  return result
end

-- :catch(on_rejected) — sugar for :next(nil, on_rejected), verbatim browser.
function Promise:catch(on_rejected)
  return self:next(nil, on_rejected)
end

-- :finally(on_finally) — run `on_finally` whichever way the promise settles, then
-- pass the original value/reason through untouched (so `finally` can't swallow a
-- result or a rejection). Matches the browser's pass-through semantics.
function Promise:finally(on_finally)
  return self:next(function(value)
    if is_callable(on_finally) then
      on_finally()
    end
    return value
  end, function(reason)
    if is_callable(on_finally) then
      on_finally()
    end
    error(reason, 0) -- rethrow so the rejection propagates past finally
  end)
end

-- ----- nx.promise: constructors + combinators --------------------------------

local M = {}

-- nx.promise.new(executor): the browser constructor. `executor(resolve, reject)`
-- runs SYNCHRONOUSLY now (as in the browser); a throw inside it rejects the
-- promise. `nx.promise(executor)` is the same thing via __call sugar.
function M.new(executor)
  local p = new_pending()
  if executor ~= nil then
    if not is_callable(executor) then
      error("nx.promise.new: executor must be a function", 2)
    end
    local ok, err = pcall(executor, function(value)
      resolve_promise(p, value)
    end, function(reason)
      settle(p, REJECTED, reason)
    end)
    if not ok then
      settle(p, REJECTED, err)
    end
  end
  return p
end

-- nx.promise.resolve(value): a promise already fulfilled with `value` (or, if
-- `value` is itself a promise/thenable, the very same/adopted promise).
function M.resolve(value)
  if is_promise(value) then
    return value
  end
  local p = new_pending()
  resolve_promise(p, value)
  return p
end

-- nx.promise.reject(reason): a promise already rejected with `reason`.
function M.reject(reason)
  local p = new_pending()
  settle(p, REJECTED, reason)
  return p
end

-- nx.promise.try(fn, ...): run `fn(...)` INSIDE a promise — a synchronous throw
-- becomes a rejection, and a returned promise (or plain value) is adopted. So a
-- function that may fail either way (sync error before it returns, or async
-- rejection of what it returns) folds into ONE chain: no `pcall` + branch at the
-- call site, just `nx.promise.try(fn, ...):next(...):catch(...)`. Mirrors the
-- browser's `Promise.try`.
function M.try(fn, ...)
  local args = { ... }
  local argc = select("#", ...)
  return M.new(function(resolve)
    -- A throw in `fn` propagates out of this executor, which M.new turns into a
    -- rejection; a returned promise is adopted by `resolve` (the resolution proc).
    resolve(fn(table.unpack(args, 1, argc)))
  end)
end

-- Count a 1..n list (combinators take array-like tables of promises/values).
local function list_len(t)
  return #t
end

-- nx.promise.all(list): fulfils with the array of every value once ALL fulfil, in
-- input order; rejects as soon as ANY rejects (with that reason). An empty list
-- fulfils immediately with {}.
function M.all(list)
  return M.new(function(resolve, reject)
    local n = list_len(list)
    if n == 0 then
      return resolve({})
    end
    local results = {}
    local remaining = n
    for i = 1, n do
      M.resolve(list[i]):next(function(value)
        results[i] = value
        remaining = remaining - 1
        if remaining == 0 then
          resolve(results)
        end
      end, reject)
    end
  end)
end

-- nx.promise.all_settled(list) [alias allSettled]: fulfils once every promise
-- settles, with an array of outcome tables: { status = "fulfilled", value = v }
-- or { status = "rejected", reason = e }. Never rejects.
function M.all_settled(list)
  return M.new(function(resolve)
    local n = list_len(list)
    if n == 0 then
      return resolve({})
    end
    local results = {}
    local remaining = n
    local function record(i, outcome)
      results[i] = outcome
      remaining = remaining - 1
      if remaining == 0 then
        resolve(results)
      end
    end
    for i = 1, n do
      M.resolve(list[i]):next(function(value)
        record(i, { status = "fulfilled", value = value })
      end, function(reason)
        record(i, { status = "rejected", reason = reason })
      end)
    end
  end)
end

-- nx.promise.race(list): settles the moment the FIRST input settles, adopting its
-- fulfilment or rejection. An empty list stays pending forever (as in the
-- browser).
function M.race(list)
  return M.new(function(resolve, reject)
    for i = 1, list_len(list) do
      M.resolve(list[i]):next(resolve, reject)
    end
  end)
end

-- nx.promise.any(list): fulfils with the first value to fulfil; rejects only if
-- ALL reject, with an aggregate { errors = {...} }. An empty list rejects at once.
function M.any(list)
  return M.new(function(resolve, reject)
    local n = list_len(list)
    if n == 0 then
      return reject({ message = "All promises were rejected", errors = {} })
    end
    local errors = {}
    local remaining = n
    for i = 1, n do
      M.resolve(list[i]):next(resolve, function(reason)
        errors[i] = reason
        remaining = remaining - 1
        if remaining == 0 then
          reject({ message = "All promises were rejected", errors = errors })
        end
      end)
    end
  end)
end

-- ----- nx-native conveniences (still promise-shaped) -------------------------

-- nx.promise.delay(ms[, value]): a promise that fulfils with `value` after `ms`
-- wall-clock milliseconds, on the loop. The promise-flavoured vim.defer_fn — the
-- await-able sleep that makes retry/debounce chains read linearly.
function M.delay(ms, value)
  return M.new(function(resolve)
    nx.timer(function()
      resolve(value)
    end, ms or 0)
  end)
end

-- nx.wait_for(predicate[, opts]) -> promise: poll `predicate` BETWEEN ticks until it
-- returns a truthy value, then fulfil with that value. The await-able form of the
-- bounded "spin until a cross-tick condition holds" loop that recurs across the
-- codebase — a freshly-mounted window's id, a server-repopulated mirror, a view
-- buffer that exists next tick. It yields the tick (nx.on_next_tick) between checks, so
-- those mirrors actually refresh, instead of spinning within one convergence the way
-- a bare nx.schedule re-arm does. `predicate` is checked once immediately, then once
-- per following tick.
--
--   opts.tries     max checks before giving up (default 200 — a few seconds of ticks)
--   opts.interval  ms between checks (default: the next tick); set for slower polling
--   opts.message   the rejection message used on timeout
--
-- REJECTS (so an `nx.await` fails loud and a chain can `:catch`) if the condition
-- never holds within `tries`, or if `predicate` throws. RESOLVES with the predicate's
-- truthy value, so `:next(function(win) … end)` receives it directly. A best-effort
-- caller should `:catch` the timeout.
function M.wait_for(predicate, opts)
  opts = opts or {}
  local tries = opts.tries or 200
  local interval = opts.interval
  return M.new(function(resolve, reject)
    local n = 0
    local function step()
      local ok, val = pcall(predicate)
      if not ok then
        return reject({ message = "nx.wait_for: predicate errored: " .. tostring(val) })
      end
      if val then
        return resolve(val)
      end
      n = n + 1
      if n >= tries then
        return reject({
          message = opts.message or ("nx.wait_for: condition not met after " .. tries .. " ticks"),
        })
      end
      if interval then
        nx.timer(step, interval)
      else
        nx.on_next_tick(step)
      end
    end
    step() -- check immediately (tick 0), then poll the next ticks
  end)
end
nx.wait_for = M.wait_for

-- nx.promise.wrap(fn): lift a single-callback async function into a
-- promise-returning one. The wrapped function appends a resolver as the LAST
-- argument and resolves with whatever that callback receives (its single arg, or
-- all of them as a table when there's more than one) — the shape nxvim's own
-- callback APIs use (nx.ui.select's on_choice, an on_exit, …). Use this to turn
-- "pass me a callback" surfaces into await-ables; reach for nx.promise.new
-- directly when the callback uses a node-style (err, value) convention.
function M.wrap(fn)
  return function(...)
    local args = { ... }
    local argc = select("#", ...)
    return M.new(function(resolve)
      args[argc + 1] = function(...)
        local nres = select("#", ...)
        if nres <= 1 then
          resolve((...))
        else
          resolve({ ... })
        end
      end
      fn(table.unpack(args, 1, argc + 1))
    end)
  end
end

setmetatable(M, {
  __call = function(_, executor)
    return M.new(executor)
  end,
})

-- Browser-name aliases so muscle memory works (the canonical names are snake_case
-- to match the rest of nx.*).
M.allSettled = M.all_settled

nx.promise = M

-- ----- nx.async / nx.await: coroutine sugar over the same promises -----------
--
-- `nx.async(fn)` returns a function that, when called, runs `fn` as a coroutine
-- and returns a promise for its result. Inside, `nx.await(p)` suspends until `p`
-- settles and evaluates to its value (or re-raises its rejection as a Lua error),
-- so a sequence of awaits reads top-to-bottom with no nesting:
--
--     local load = nx.async(function(path)
--       local stat = nx.await(fs.stat(path))
--       local data = nx.await(fs.read(path))
--       return parse(data, stat)
--     end)
--     load("init.lua"):next(use):catch(report)
--
-- A rejected await raises inside the coroutine, so you can handle it either way:
-- wrap the await in `pcall` to catch it locally (PUC 5.2+ yields across a `pcall`
-- boundary, so this works on nxvim's 5.4 backend), or let it propagate to the
-- coroutine edge (the returned promise rejects, caught by `:catch` on the result)
-- or attach `:catch` to the awaited promise itself.

function nx.async(fn)
  if not is_callable(fn) then
    error("nx.async: argument must be a function", 2)
  end
  return function(...)
    local args = { ... }
    local argc = select("#", ...)
    return M.new(function(resolve, reject)
      local co = coroutine.create(fn)
      -- Drive the coroutine one resume at a time. Each yield hands us the awaited
      -- value; we resolve it to a promise and re-enter when it settles, passing
      -- (ok, value) back so nx.await can return the value or raise the reason.
      local function step(...)
        local ok, yielded = coroutine.resume(co, ...)
        if not ok then
          -- The coroutine body threw (or an awaited rejection was re-raised).
          return reject(yielded)
        end
        if coroutine.status(co) == "dead" then
          -- `fn` returned `yielded` — that's the async function's result.
          return resolve(yielded)
        end
        -- Suspended on an await: `yielded` is the awaited value/promise.
        M.resolve(yielded):next(function(value)
          step(true, value)
        end, function(reason)
          step(false, reason)
        end)
      end
      step(table.unpack(args, 1, argc))
    end)
  end
end

-- nx.await(awaitable): suspend the enclosing nx.async coroutine until `awaitable`
-- settles. Returns the fulfilment value, or raises the rejection reason as an
-- error (which, uncaught, rejects the async function's promise). Errors loudly if
-- called outside an nx.async coroutine — there is nothing to suspend.
function nx.await(awaitable)
  -- `coroutine.isyieldable()` is false on the main thread and true inside a
  -- coroutine — exactly "is there an nx.async frame to suspend?". (The 5.1
  -- spelling `coroutine.running() == nil` broke in 5.2+, where `running()`
  -- returns the main thread itself rather than nil.)
  if not coroutine.isyieldable() then
    error("nx.await must be called inside an nx.async function", 2)
  end
  local ok, value = coroutine.yield(awaitable)
  if not ok then
    error(value, 0)
  end
  return value
end

return nx
