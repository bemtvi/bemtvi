-- nx process API — promise-only async over the event-loop spawn transports.
--
-- `nx` is promise-only (ADR 0002): no callback-shaped one-shot async. So instead
-- of the old `nx.spawn{ on_stdout, on_exit }`, running a child is either:
--
--   * nx.run{ ... }        -> a PROMISE of { code, stdout, stderr }   (one-shot)
--   * nx.run_stream{ ... } -> a STREAM you iterate with nx.await_each (streaming)
--
-- Two transports back these (both in install.rs): `nx._system_async` collects all
-- stdout and fires once on exit (nx.run); `nx._spawn_stream` streams stdout in
-- newline-delimited batches (nx.run_stream). Loaded after promise.lua — nx.run*
-- build on nx.promise / nx.async / nx.await.

-- Build an argv list from { cmd = string|list, args = list } — `cmd` is a string
-- or an argv list, `args` is appended.
local function build_argv(spec)
  local cmd = spec.cmd
  if type(cmd) == "string" then
    cmd = { cmd }
  end
  local argv = {}
  for _, c in ipairs(cmd) do
    argv[#argv + 1] = c
  end
  for _, a in ipairs(spec.args or {}) do
    argv[#argv + 1] = a
  end
  return argv
end

-- nx.run { cmd, args, cwd, env, stdin } -> promise of { code, stdout, stderr }.
-- Runs a child to completion off the input tick, collecting all of stdout. It
-- RESOLVES (never rejects) with the exit result: a non-zero `code` is the caller's
-- to act on, and a spawn failure (e.g. binary not found) surfaces as `code = -1`
-- with empty output — exactly like vim.system. The one-shot promise twin of
-- nx.run_stream.
function nx.run(spec)
  if type(spec) ~= "table" then
    error("nx.run: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  return nx.promise.new(function(resolve)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(result)
      resolve({
        code = result.code,
        stdout = result.stdout or "",
        stderr = result.stderr or "",
      })
    end
    nx._system_async(id, build_argv(spec), spec.cwd, spec.env, spec.stdin)
  end)
end

-- A Stream over a streaming child's stdout. `:next()` returns a promise of the
-- next batch (a list of lines) or `nil` at end-of-stream; `:kill()` reaps the
-- child early. Consume it with nx.await_each inside an nx.async function.
--
-- SEQUENTIAL contract: at most one outstanding `:next()` at a time (which is what
-- a `for` loop does). Batches arriving between `:next()` calls buffer in `_queue`;
-- a `:next()` that finds the queue empty parks a single `_waiter` that the next
-- batch — or the exit — wakes.
local Stream = {}
Stream.__index = Stream

function Stream:next()
  return nx.promise.new(function(resolve)
    if #self._queue > 0 then
      resolve(table.remove(self._queue, 1))
    elseif self._done then
      resolve(nil)
    else
      self._waiter = resolve
    end
  end)
end

function Stream:kill()
  nx._system_kill(self._id)
end

-- nx.run_stream { cmd, args, cwd, env } -> Stream. Spawns a child and streams its
-- stdout in newline-delimited batches. The streaming twin of nx.run; the picker /
-- completion sources consume it to feed results as they arrive.
function nx.run_stream(spec)
  if type(spec) ~= "table" then
    error("nx.run_stream: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  local self = setmetatable({ _queue = {}, _done = false, _waiter = nil }, Stream)
  local id = nx._next_cb_id()
  self._id = id
  -- Persistent stdout: hand each batch to a parked waiter, else buffer it.
  nx._stdout_fns[id] = function(lines)
    local waiter = self._waiter
    if waiter then
      self._waiter = nil
      waiter(lines)
    else
      self._queue[#self._queue + 1] = lines
    end
  end
  -- One-shot exit: mark done and wake any parked waiter with nil (end-of-stream).
  nx._cb_fns[id] = function(result)
    nx._stdout_fns[id] = nil
    self._done = true
    self._exit = result
    local waiter = self._waiter
    if waiter then
      self._waiter = nil
      waiter(nil)
    end
  end
  nx._spawn_stream(id, build_argv(spec), spec.cwd, spec.env)
  return self
end

-- nx.await_each(stream): a `for`-loop iterator over a Stream's batches. Each step
-- awaits the next batch; the loop ends when the stream is exhausted (`:next()`
-- resolves nil). MUST run inside an nx.async function (nx.await suspends the
-- enclosing coroutine).
--
--   for batch in nx.await_each(stream) do
--     for _, line in ipairs(batch) do ... end
--   end
function nx.await_each(stream)
  return function()
    return nx.await(stream:next())
  end
end
