-- btv process API — promise-only async over the event-loop spawn transports.
--
-- `btv` is promise-only (ADR 0002): no callback-shaped one-shot async. So instead
-- of the old `btv.spawn{ on_stdout, on_exit }`, running a child is either:
--
--   * `btv.run{ ... }`        -> a PROMISE of `{ code, stdout, stderr }`   (one-shot)
--   * `btv.run_stream{ ... }` -> a STREAM you iterate with `btv.await_each` (streaming)
--
-- Two transports back these (both in install.rs): `btv._system_async` collects all
-- stdout and fires once on exit (`btv.run`); `btv._spawn_stream` streams stdout in
-- newline-delimited batches (`btv.run_stream`). Loaded after promise.lua — `btv.run*`
-- build on `btv.promise` / `btv.async` / `btv.await`.

-- Argv normalization (`{ cmd = string|list, args = list }` → flat argv) is the
-- shared `btv.utils.argv` (prelude/utils.lua, loaded above).
local build_argv = btv.utils.argv

-- `btv.run { cmd, args, cwd, env, stdin }` -> promise of `{ code, stdout, stderr }`.
-- Runs a child to completion off the input tick, buffering all of its stdout and
-- stderr and resolving once, on exit.
--
-- Spec fields:
--   * `cmd`   — the program. A string, or an argv list whose first element is the
--             program. Spawned directly, with NO shell: nothing is word-split,
--             quoted, or glob-expanded, so pass each argument as its own element.
--   * `args`  — optional list appended after `cmd`, so `{ cmd = "git", args = { "log" } }`
--             and `{ cmd = { "git", "log" } }` are equivalent.
--   * `cwd`   — optional working directory for the child.
--   * `env`   — optional `{ NAME = value }` map of environment overrides.
--   * `stdin` — optional string piped to the child; its stdin then closes (EOF).
--
-- RESOLVES (never rejects) with the exit result: a non-zero `code` is the caller's
-- to act on, and a spawn failure (e.g. binary not found) surfaces as `code = -1`
-- with empty output — exactly like `vim.system`. Await it inside `btv.async`, or chain
-- with `:next` / `:catch`:
--
-- ```lua
-- btv.async(function()
--   local r = btv.await(btv.run({ cmd = "git", args = { "rev-parse", "HEAD" } }))
--   if r.code == 0 then btv.print(r.stdout) end
-- end)()
-- ```
--
-- The one-shot promise twin of `btv.run_stream` (stream stdout as it arrives). For a
-- duplex child whose stdin stays open for a framed protocol (LSP/DAP) use
-- `btv.process` instead.
function btv.run(spec)
  if type(spec) ~= "table" then
    error("btv.run: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  return btv.promise.new(function(resolve)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(result)
      btv._proc_pids[id] = nil -- the pid registry entry dies with the child
      resolve({
        code = result.code,
        stdout = result.stdout or "",
        stderr = result.stderr or "",
      })
    end
    btv._bridge(id, function()
      btv._system_async(id, build_argv(spec), spec.cwd, spec.env, spec.stdin)
    end)
  end)
end

-- A Stream over a streaming child's stdout. `:next()` returns a promise of the
-- next batch (a list of lines) or `nil` at end-of-stream; `:kill()` reaps the
-- child early. Consume it with `btv.await_each` inside an `btv.async` function.
--
-- SEQUENTIAL contract: at most one outstanding `:next()` at a time (which is what
-- a `for` loop does). Batches arriving between `:next()` calls buffer in `_queue`;
-- a `:next()` that finds the queue empty parks a single `_waiter` that the next
-- batch — or the exit — wakes.
local Stream = {}
Stream.__index = Stream

function Stream:next()
  return btv.promise.new(function(resolve)
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
  btv._system_kill(self._id)
end

-- `stream:exit()`: how the child ENDED — a table `{ code, stderr }` once it has
-- exited, or `nil` while it is still running (so it is meaningful only after the
-- `btv.await_each` loop has finished). `code` is the child's real exit status;
-- **`-1` means the child never ran** — the binary wasn't found, or `:kill()` reaped
-- it. `stdout` is empty here: it was already delivered through the stream.
--
-- The point of the accessor is to tell "the tool ran and its answer was nothing"
-- apart from "the tool isn't installed", which output alone cannot distinguish. A
-- fallback chain must branch on the status, not on emptiness — `rg` exits `1` on
-- zero matches, and re-searching the tree with the next tool for that is pure waste:
--
-- ```lua
-- local stream = btv.run_stream({ cmd = "rg", args = { "--vimgrep", "--", q } })
-- for batch in btv.await_each(stream) do ... end
-- local exit = stream:exit()
-- if exit and exit.code == -1 then ... end  -- no rg here; try the next tool
-- ```
function Stream:exit()
  return self._exit
end

-- `stream:pid()`: the running child's OS pid, or `nil`. The pid is reported
-- asynchronously by the event-loop actor (it can't be known synchronously on the
-- single-threaded runtime) into the `btv._proc_pids` registry this reads — so `nil`
-- before the spawn lands, on a failed spawn, and again after the child exits (the
-- registry entry dies with the child). Handy for signalling the child out-of-band
-- (e.g. `kill -USR1`) where `:kill()`'s reap is too blunt.
function Stream:pid()
  return btv._proc_pids[self._id]
end

-- `btv.run_stream { cmd, args, cwd, env }` -> Stream. Spawns a child and streams its
-- stdout as it arrives, in newline-delimited batches — each batch a list of lines
-- with the trailing newline stripped. Takes the same spec as `btv.run` minus `stdin`
-- (the child's stdin is closed at spawn). Only stdout streams; the exit status and
-- stderr land together at the end, on `stream:exit()`.
--
-- The streaming twin of `btv.run` — reach for it when output is large or long-lived
-- and you want to act on lines as they come (the picker / completion sources feed
-- results this way) rather than waiting for the child to exit. Iterate it with
-- `btv.await_each` inside an `btv.async` function; call `:kill()` to reap the child early
-- (e.g. a superseded query):
--
-- ```lua
-- btv.async(function()
--   local stream = btv.run_stream({ cmd = "rg", args = { "TODO" } })
--   for batch in btv.await_each(stream) do
--     for _, line in ipairs(batch) do btv.print(line) end
--   end
-- end)()
-- ```
function btv.run_stream(spec)
  if type(spec) ~= "table" then
    error("btv.run_stream: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  local self = setmetatable({ _queue = {}, _done = false, _waiter = nil }, Stream)
  local id = btv._next_cb_id()
  self._id = id
  -- Persistent stdout: hand each batch to a parked waiter, else buffer it.
  btv._stdout_fns[id] = function(lines)
    local waiter = self._waiter
    if waiter then
      self._waiter = nil
      waiter(lines)
    else
      self._queue[#self._queue + 1] = lines
    end
  end
  -- One-shot exit: mark done and wake any parked waiter with nil (end-of-stream).
  btv._cb_fns[id] = function(result)
    btv._stdout_fns[id] = nil
    btv._proc_pids[id] = nil -- the pid registry entry dies with the child
    self._done = true
    self._exit = result
    local waiter = self._waiter
    if waiter then
      self._waiter = nil
      waiter(nil)
    end
  end
  -- A bad spec (non-string cmd element / cwd, unencodable env) throws during the
  -- bridge conversion after both entries were registered — drop both on the way
  -- out so a retrying consumer doesn't leak a pump per attempt.
  btv._bridge(id, function()
    btv._spawn_stream(id, build_argv(spec), spec.cwd, spec.env)
  end, function(cb_id)
    btv._stdout_fns[cb_id] = nil
  end)
  return self
end

-- `btv.await_each(stream)`: a `for`-loop iterator over a Stream's batches. Each step
-- awaits the next batch; the loop ends when the stream is exhausted (`:next()`
-- resolves nil). MUST run inside an `btv.async` function (`btv.await` suspends the
-- enclosing coroutine).
--
-- ```lua
-- for batch in btv.await_each(stream) do
--   for _, line in ipairs(batch) do ... end
-- end
-- ```
function btv.await_each(stream)
  return function()
    return btv.await(stream:next())
  end
end

-- ----- btv.process: a duplex (bidirectional) child --------------------------------
--
-- `btv.run` / `btv.run_stream` are read-only and one-shot: they close the child's stdin at
-- spawn and (`run_stream`) newline-split its stdout. A framed wire protocol — a Debug
-- Adapter (DAP) or a language server speaking Content-Length JSON — needs the
-- opposite: stdin stays OPEN for incremental writes, and stdout arrives as raw,
-- un-split byte chunks the caller frames itself. `btv.process.open` is that transport
-- (the keystone for bemtvi-dap), backed by the native `_proc_open`/`_proc_write`/
-- `_proc_kill` ops.
--
-- It is deliberately handler-shaped, not promise-shaped: a long-lived process is a
-- bidirectional event source (stdout chunks, stderr chunks, one exit), not a value
-- that resolves once. (`btv` is promise-only for ONE-SHOT async; a persistent stream
-- of events stays handler-based — same as autocmds / `btv.fs.watch`.)

btv.process = btv.process or {}

-- id -> `{ on_stdout, on_stderr, on_exit }` for every live duplex child.
btv._proc_handlers = btv._proc_handlers or {}

-- Native callback: a raw output chunk arrived (`data` is a binary-safe string;
-- `stderr` true selects the error stream). Dispatched to the registered handler.
function btv._proc_recv(id, data, stderr)
  local h = btv._proc_handlers[id]
  if not h then
    return
  end
  local cb = stderr and h.on_stderr or h.on_stdout
  if cb then
    cb(data)
  end
end

-- Native callback: the child exited. Fire `on_exit(code)` once, then forget it.
function btv._proc_exit(id, code)
  local h = btv._proc_handlers[id]
  if not h then
    return
  end
  btv._proc_handlers[id] = nil
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
  btv._proc_write(self.id, data)
end

-- Terminate the child (its `on_exit` still fires with code -1).
function Process:kill()
  if not self._alive then
    return
  end
  self._alive = false
  btv._proc_kill(self.id)
end

-- `btv.process.open { cmd, args, cwd, env, on_stdout, on_stderr, on_exit }` -> handle.
-- Spawns a duplex child and returns a handle with `:write(data)` and `:kill()`. The
-- callbacks fire on the editor thread (they may queue effects — extmarks, view
-- renders — like any btv callback): `on_stdout(chunk)` / `on_stderr(chunk)` per raw
-- batch, `on_exit(code)` exactly once.
function btv.process.open(spec)
  if type(spec) ~= "table" then
    error("btv.process.open: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  local id = btv._next_cb_id()
  local handle = setmetatable({ id = id, _alive = true }, Process)
  btv._proc_handlers[id] = {
    on_stdout = spec.on_stdout,
    on_stderr = spec.on_stderr,
    on_exit = function(code)
      -- Mark dead before the user's `on_exit` so a `:write` from inside it no-ops.
      handle._alive = false
      if spec.on_exit then
        spec.on_exit(code)
      end
    end,
  }
  -- A bad spec throws during the bridge conversion after the handler was
  -- registered — drop it so a throwing call doesn't leak the handler table.
  btv._bridge(id, function()
    btv._proc_open(id, build_argv(spec), spec.cwd, spec.env)
  end, function(cb_id)
    btv._proc_handlers[cb_id] = nil
  end)
  return handle
end

-- ----- btv.socket: a duplex TCP client connection ---------------------------------
--
-- The socket sibling of `btv.process`, for a framed-protocol peer that listens on a TCP
-- port instead of speaking over stdio — a DAP adapter run in `"server"` mode
-- (`type = "server"`), which the debuggee or a launcher starts and the client connects to.
-- Same handler-shaped surface (`on_connect` / `on_data` / `on_close`), same duplex
-- contract: `handle:write(bytes)` sends, inbound bytes arrive raw on `on_data`.

btv.socket = btv.socket or {}

-- id -> `{ on_connect, on_data, on_close, connected }` for every live connection.
btv._sock_handlers = btv._sock_handlers or {}

-- Native callback: the TCP connection is established.
function btv._sock_connected(id)
  local h = btv._sock_handlers[id]
  if not h then
    return
  end
  h.connected = true
  if h.on_connect then
    h.on_connect()
  end
end

-- Native callback: a raw inbound chunk arrived (`data` is a binary-safe string).
function btv._sock_data(id, data)
  local h = btv._sock_handlers[id]
  if h and h.on_data then
    h.on_data(data)
  end
end

-- Native callback: the connection closed (`err` a string on failure, nil on a clean
-- close). Fire `on_close(err)` once, then forget it.
function btv._sock_closed(id, err)
  local h = btv._sock_handlers[id]
  if not h then
    return
  end
  btv._sock_handlers[id] = nil
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
  btv._sock_write(self.id, data)
end

-- Close the connection (its `on_close` still fires).
function Socket:close()
  if not self._alive then
    return
  end
  self._alive = false
  btv._sock_close(self.id)
end

-- `btv.socket.connect { host, port, on_connect, on_data, on_close }` -> handle. Opens a
-- TCP client connection and returns a handle with `:write(data)` and `:close()`. The
-- callbacks fire on the editor thread: `on_connect()` once connected, `on_data(chunk)`
-- per raw inbound batch, `on_close(err)` exactly once (`err` set on a connect/I-O
-- failure).
function btv.socket.connect(spec)
  if type(spec) ~= "table" then
    error("btv.socket.connect: expected a table { host, port, ... }, got " .. type(spec), 2)
  end
  if type(spec.host) ~= "string" or type(spec.port) ~= "number" then
    error("btv.socket.connect: needs a string `host` and a number `port`", 2)
  end
  local id = btv._next_cb_id()
  local handle = setmetatable({ id = id, _alive = true }, Socket)
  btv._sock_handlers[id] = {
    on_connect = spec.on_connect,
    on_data = spec.on_data,
    on_close = function(err)
      handle._alive = false
      if spec.on_close then
        spec.on_close(err)
      end
    end,
  }
  -- A port that fails the u16 conversion throws after the handler was
  -- registered — drop it so a throwing call doesn't leak the handler table.
  btv._bridge(id, function()
    btv._sock_connect(id, spec.host, spec.port)
  end, function(cb_id)
    btv._sock_handlers[cb_id] = nil
  end)
  return handle
end
