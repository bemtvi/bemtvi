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

-- ----- nx.process: a duplex (bidirectional) child --------------------------------
--
-- nx.run / nx.run_stream are read-only and one-shot: they close the child's stdin at
-- spawn and (run_stream) newline-split its stdout. A framed wire protocol — a Debug
-- Adapter (DAP) or a language server speaking Content-Length JSON — needs the
-- opposite: stdin stays OPEN for incremental writes, and stdout arrives as raw,
-- un-split byte chunks the caller frames itself. `nx.process.open` is that transport
-- (the keystone for nxvim-dap), backed by the native `_proc_open`/`_proc_write`/
-- `_proc_kill` ops.
--
-- It is deliberately handler-shaped, not promise-shaped: a long-lived process is a
-- bidirectional event source (stdout chunks, stderr chunks, one exit), not a value
-- that resolves once. (`nx` is promise-only for ONE-SHOT async; a persistent stream
-- of events stays handler-based — same as autocmds / nx.fs.watch.)

nx.process = nx.process or {}

-- id -> { on_stdout, on_stderr, on_exit } for every live duplex child.
nx._proc_handlers = nx._proc_handlers or {}

-- Native callback: a raw output chunk arrived (`data` is a binary-safe string;
-- `stderr` true selects the error stream). Dispatched to the registered handler.
function nx._proc_recv(id, data, stderr)
  local h = nx._proc_handlers[id]
  if not h then
    return
  end
  local cb = stderr and h.on_stderr or h.on_stdout
  if cb then
    cb(data)
  end
end

-- Native callback: the child exited. Fire `on_exit(code)` once, then forget it.
function nx._proc_exit(id, code)
  local h = nx._proc_handlers[id]
  if not h then
    return
  end
  nx._proc_handlers[id] = nil
  if h.on_exit then
    h.on_exit(code)
  end
end

local Process = {}
Process.__index = Process

-- Write raw bytes to the child's still-open stdin. Accepts a string or a list of
-- strings (concatenated). A no-op once the child has exited.
function Process:write(data)
  if not self._alive then
    return
  end
  if type(data) == "table" then
    data = table.concat(data)
  end
  nx._proc_write(self.id, data)
end

-- Terminate the child (its `on_exit` still fires with code -1).
function Process:kill()
  if not self._alive then
    return
  end
  self._alive = false
  nx._proc_kill(self.id)
end

-- nx.process.open { cmd, args, cwd, env, on_stdout, on_stderr, on_exit } -> handle.
-- Spawns a duplex child and returns a handle with `:write(data)` and `:kill()`. The
-- callbacks fire on the editor thread (they may queue effects — extmarks, view
-- renders — like any nx callback): `on_stdout(chunk)` / `on_stderr(chunk)` per raw
-- batch, `on_exit(code)` exactly once.
function nx.process.open(spec)
  if type(spec) ~= "table" then
    error("nx.process.open: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  local id = nx._next_cb_id()
  local handle = setmetatable({ id = id, _alive = true }, Process)
  nx._proc_handlers[id] = {
    on_stdout = spec.on_stdout,
    on_stderr = spec.on_stderr,
    on_exit = function(code)
      -- Mark dead before the user's on_exit so a `:write` from inside it no-ops.
      handle._alive = false
      if spec.on_exit then
        spec.on_exit(code)
      end
    end,
  }
  nx._proc_open(id, build_argv(spec), spec.cwd, spec.env)
  return handle
end

-- ----- nx.socket: a duplex TCP client connection ---------------------------------
--
-- The socket sibling of nx.process, for a framed-protocol peer that listens on a TCP
-- port instead of speaking over stdio — a DAP adapter run in "server" mode (`type =
-- "server"`), which the debuggee or a launcher starts and the client connects to.
-- Same handler-shaped surface (`on_connect` / `on_data` / `on_close`), same duplex
-- contract: `handle:write(bytes)` sends, inbound bytes arrive raw on `on_data`.

nx.socket = nx.socket or {}

-- id -> { on_connect, on_data, on_close, connected } for every live connection.
nx._sock_handlers = nx._sock_handlers or {}

-- Native callback: the TCP connection is established.
function nx._sock_connected(id)
  local h = nx._sock_handlers[id]
  if not h then
    return
  end
  h.connected = true
  if h.on_connect then
    h.on_connect()
  end
end

-- Native callback: a raw inbound chunk arrived (`data` is a binary-safe string).
function nx._sock_data(id, data)
  local h = nx._sock_handlers[id]
  if h and h.on_data then
    h.on_data(data)
  end
end

-- Native callback: the connection closed (`err` a string on failure, nil on a clean
-- close). Fire `on_close(err)` once, then forget it.
function nx._sock_closed(id, err)
  local h = nx._sock_handlers[id]
  if not h then
    return
  end
  nx._sock_handlers[id] = nil
  if h.on_close then
    h.on_close(err)
  end
end

local Socket = {}
Socket.__index = Socket

-- Send raw bytes over the connection (a string, or a list of strings concatenated).
-- A no-op once the connection has closed.
function Socket:write(data)
  if not self._alive then
    return
  end
  if type(data) == "table" then
    data = table.concat(data)
  end
  nx._sock_write(self.id, data)
end

-- Close the connection (its `on_close` still fires).
function Socket:close()
  if not self._alive then
    return
  end
  self._alive = false
  nx._sock_close(self.id)
end

-- nx.socket.connect { host, port, on_connect, on_data, on_close } -> handle. Opens a
-- TCP client connection and returns a handle with `:write(data)` and `:close()`. The
-- callbacks fire on the editor thread: `on_connect()` once connected, `on_data(chunk)`
-- per raw inbound batch, `on_close(err)` exactly once (err set on a connect/I-O
-- failure).
function nx.socket.connect(spec)
  if type(spec) ~= "table" then
    error("nx.socket.connect: expected a table { host, port, ... }, got " .. type(spec), 2)
  end
  if type(spec.host) ~= "string" or type(spec.port) ~= "number" then
    error("nx.socket.connect: needs a string `host` and a number `port`", 2)
  end
  local id = nx._next_cb_id()
  local handle = setmetatable({ id = id, _alive = true }, Socket)
  nx._sock_handlers[id] = {
    on_connect = spec.on_connect,
    on_data = spec.on_data,
    on_close = function(err)
      handle._alive = false
      if spec.on_close then
        spec.on_close(err)
      end
    end,
  }
  nx._sock_connect(id, spec.host, spec.port)
  return handle
end
