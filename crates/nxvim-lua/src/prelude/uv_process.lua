-- nxvim Lua prelude — the libuv PROCESS surface on vim.uv / vim.loop.
-- Loaded as a sequential prelude chunk by `LuaRuntime::new` (runtime.rs), after
-- `install_runtime_api` has built the vim.uv table.
--
-- uv.spawn + uv.new_pipe + uv.new_check + the handle lifecycle
-- (is_closing/is_active/close) are what plenary.job (and plenary.curl, built on
-- it) bind directly to run subprocesses — a separate API from the batch
-- vim.system. nxvim already spawns children in its event-loop actor for
-- vim.system (LoopOp::Spawn → run_process, delivering { code, stdout, stderr }
-- when the child exits). This models libuv's process/pipe/check OBJECTS in Lua
-- over that one batch primitive, the same "Lua objects over a Rust primitive"
-- shape the timer handles use.
--
-- DOCUMENTED APPROXIMATIONS (all invisible to a job that runs a command and reads
-- its output, which is the dominant use — telescope pickers, plenary.curl):
--   * Output is delivered to a pipe's read_start callback in ONE chunk at exit
--     (then EOF), not streamed incrementally as it is produced. The accumulated
--     final result is identical; live/interactive streaming is not modelled.
--   * `signal` in on_exit is always 0 (the actor reports an exit code, not the
--     terminating signal).
--   * The pid is not available synchronously (the spawn is async): uv.spawn's
--     second return is nil. A handle's :kill() still works (it routes through the
--     callback id, like vim.system's async handle).
--   * A spawn failure (e.g. missing command) surfaces as a later on_exit with
--     code -1 and the message on stderr, rather than uv.spawn returning nil — the
--     same approximation async vim.system makes.

local vim = vim
local uv = vim.uv -- == vim.loop (aliased in install.rs)

-- vim.in_fast_event(): true while running inside a libuv callback where most of
-- the API is unavailable. nxvim's Lua is synchronous on the one editor thread and
-- never runs in such a context, so this is truthfully always false (plenary.job's
-- path `expand` branches on it). Defined here because the process surface is the
-- first consumer; it is a general vim.* function.
if vim.in_fast_event == nil then
  function vim.in_fast_event() return false end
end

-- ----- handle helpers --------------------------------------------------------

-- uv.is_closing(handle) / uv.is_active(handle): the module-function forms plenary
-- uses (alongside the method forms). Every handle this module hands out carries
-- `_closing` / `_active` booleans; a nil handle reads as closed / inactive.
function uv.is_closing(handle) return handle == nil or handle._closing == true end

function uv.is_active(handle) return handle ~= nil and handle._active == true end

-- ----- pipes -----------------------------------------------------------------
-- A libuv pipe stream. stdout/stderr pipes carry a read callback (read_start);
-- stdin pipes accumulate writes that are flushed to the child at spawn.

local Pipe = {}
Pipe.__index = Pipe

function uv.new_pipe(_ipc)
  return setmetatable({
    _kind = "pipe",
    _closing = false,
    _read_cb = nil,
    _write_buf = {},
    _write_cbs = {},
  }, Pipe)
end

function Pipe:read_start(cb)
  self._read_cb = cb
  return 0
end

function Pipe:read_stop()
  self._read_cb = nil
  return 0
end

-- Buffer a write; the bytes reach the child's stdin at spawn (see uv.spawn). A
-- completion callback is recorded and fired once the buffer is handed off (the
-- common idiom is `stdin:write(data, function() stdin:close() end)`).
function Pipe:write(data, cb)
  self._write_buf[#self._write_buf + 1] = data
  if cb then self._write_cbs[#self._write_cbs + 1] = cb end
  return 0
end

function Pipe:close(cb)
  self._closing = true
  self._read_cb = nil
  if cb then vim.schedule(cb) end
end

function Pipe:is_closing() return self._closing end

-- ----- check handles ---------------------------------------------------------
-- libuv's "check" phase handle: a callback run on each loop iteration until
-- stopped. plenary uses one to poll for "all pipes closed" before finishing a
-- job; nvim-cmp's async Scheduler uses one as its executor — `:start(step)` drives
-- the coroutine queue that runs the completion filter→menu pipeline, so the handle
-- MUST expose both the function forms (uv.check_start/stop) and the *method* forms
-- (handle:start/stop) luv offers, exactly like the timer handle. Modelled by
-- re-scheduling the callback through vim.schedule until stopped — which the polling
-- callback calls as soon as its condition holds.

local Check = {}
Check.__index = Check

function uv.new_check()
  return setmetatable({ _kind = "check", _closing = false, _active = false, _cb = nil }, Check)
end

function Check:is_active() return self._active end

function Check:is_closing() return self._closing end

function Check:close()
  self._closing = true
  self._active = false
  self._cb = nil
end

-- Arm the check: run `cb` once per convergence (modelled via vim.schedule
-- re-arming) until :stop()/:close(). Starting an already-active check just swaps
-- the callback (no second poll loop), matching luv's single-armed handle.
function Check:start(cb)
  local was_active = self._active
  self._active = true
  self._cb = cb
  if was_active then return 0 end
  local function poll()
    if not self._active or not self._cb then return end
    self._cb()
    -- Re-arm only if the callback did not stop the check (its condition not yet
    -- met). The convergence fixpoint (run_pending) runs each re-schedule.
    if self._active then vim.schedule(poll) end
  end
  vim.schedule(poll)
  return 0
end

function Check:stop()
  self._active = false
  self._cb = nil
  return 0
end

-- luv's function forms: uv.check_start(handle, cb) / uv.check_stop(handle), the
-- table-level twins of the handle methods (some callers use each). Delegate so the
-- single-armed / re-schedule semantics stay identical.
function uv.check_start(check, cb) return check:start(cb) end

function uv.check_stop(check) return check:stop() end

-- ----- spawn -----------------------------------------------------------------

local Handle = {}
Handle.__index = Handle

function Handle:is_closing() return self._closing end

function Handle:close(cb)
  self._closing = true
  if cb then vim.schedule(cb) end
end

function Handle:kill(signal) vim._system_kill(self._cbid, signal) end

-- luv's options.env is an array of "NAME=VALUE" strings; the Rust spawn bridge
-- (vim._system_async → env_pairs) wants a { NAME = "VALUE" } map. Convert a list;
-- pass a map (or nil) through unchanged.
local function env_to_map(env)
  if env == nil or env[1] == nil then return env end
  local map = {}
  for _, kv in ipairs(env) do
    local key, value = string.match(kv, "^([^=]+)=(.*)$")
    if key then map[key] = value end
  end
  return map
end

-- uv.spawn(cmd, options, on_exit): run `cmd` with options.args, wired to the
-- options.stdio = { stdin, stdout, stderr } pipes, calling on_exit(code, signal)
-- when it exits. Returns (handle, pid) — pid is nil (the spawn is async).
function uv.spawn(cmd, options, on_exit)
  options = options or {}
  local stdio = options.stdio or {}
  local stdin_pipe, stdout_pipe, stderr_pipe = stdio[1], stdio[2], stdio[3]

  local argv = { cmd }
  for _, arg in ipairs(options.args or {}) do
    argv[#argv + 1] = arg
  end

  local handle = setmetatable({ _kind = "process", _closing = false, pid = nil }, Handle)
  local id = vim._next_cb_id()
  handle._cbid = id

  -- Deliver one stream's captured output to its read callback (one chunk, then
  -- EOF) and mark the pipe closing, so plenary's _pipes_are_closed poll passes.
  local function deliver(pipe, data)
    if pipe then
      if pipe._read_cb then
        if data and #data > 0 then pipe._read_cb(nil, data) end
        pipe._read_cb(nil, nil) -- EOF
      end
      pipe._closing = true
    end
  end

  -- The exit dispatcher: the vim.system on_exit shape ({ code, stdout, stderr })
  -- delivered by the actor when the child exits.
  vim._cb_fns[id] = function(result)
    deliver(stdout_pipe, result.stdout)
    deliver(stderr_pipe, result.stderr)
    if stdin_pipe then stdin_pipe._closing = true end
    handle._closing = true
    if on_exit then on_exit(result.code, 0) end
  end

  -- Defer the real launch to convergence so the synchronous stdin writes that
  -- follow this call (plenary writes its `writer` right after uv.spawn returns)
  -- have populated the stdin pipe before the child reads it.
  vim.schedule(function()
    local stdin_data = nil
    if stdin_pipe then
      stdin_data = table.concat(stdin_pipe._write_buf)
      for _, cb in ipairs(stdin_pipe._write_cbs) do
        cb()
      end
      stdin_pipe._write_cbs = {}
    end
    vim._system_async(id, argv, options.cwd, env_to_map(options.env), stdin_data)
  end)

  return handle, handle.pid
end
