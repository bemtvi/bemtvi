-- nxvim Lua prelude — runtime services.
-- The nx._notimpl loud-failure funnel, the deferred-callback registry (nx.schedule / _cb_fns / proc pids), and nx.notify / nx.inspect (with vim.* aliases). (vim.treesitter is wired later, in prelude/treesitter.lua.)
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `nx.*` (with vim.* aliases) layered on the Rust bridge.

local vim = vim

-- ----- misc ------------------------------------------------------------------

-- nx._notimpl(name): the loud-failure funnel for not-yet-implemented surface.
-- Records `name` into nx._notimpl_hits (a set, so a future `:checkhealth` /
-- `nx._report` can enumerate which gaps a real config actually hit) and
-- raises a named error. A stub that quietly returns a fake/empty value makes a
-- broken server look configured; routing every hollow stub through here turns
-- "we think it works" into a concrete, trackable list of what to build (the
-- guiding principle of docs/plans/2026-06-05-lsp-completion.md). `level` (default 2) blames
-- the stub's call site in the error position; the message names the function.
nx._notimpl_hits = nx._notimpl_hits or {}
function nx._notimpl(name, level)
  nx._notimpl_hits[name] = true
  error("nxvim: not implemented: " .. name, level or 2)
end

-- Make a call to an unimplemented `vim.fn.<name>` fail *loud and named* instead of
-- the bare "attempt to call a nil value" a missing field would otherwise give. The
-- Rust bridge creates `vim.fn` as a plain table and the prelude adds the builtins
-- nxvim provides (rawset keys, found before this `__index` ever fires); any name
-- nxvim doesn't have yet resolves to a stub that records and raises through
-- `nx._notimpl` when *called* — never on mere access. That matters two ways:
--   * neovim's `vim.fn` is likewise always-callable (an unknown function raises
--     E117 at call time, and `if vim.fn.foo then` is truthy), so feature-probing
--     configs keep working; returning nil here would diverge.
--   * a gap surfaces as "nxvim: not implemented: vim.fn.<name>" pointing at the
--     call site, so a missing builtin is a one-line diagnosis rather than a buried
--     nil-call error (which `nvim_exec_lua` would swallow to the message line).
-- A plugin that genuinely wants to detect absence can still `vim.fn.has(...)` or
-- pcall the call; it cannot rely on the field being nil (neither can it in neovim).
setmetatable(vim.fn, {
  __index = function(_, name)
    local fn = function()
      return nx._notimpl("vim.fn." .. name)
    end
    return fn
  end,
})

-- ----- the async runtime: the deferred-callback registry ---------------------
-- The spine of nxvim's event loop. A deferred function (nx.schedule, defer_fn,
-- a timer, a system on_exit) is stored by integer id in nx._cb_fns
-- and run *later*, by id, from Rust — the nx._keymap_fns / nx._run_keymap shape
-- applied to async work. nx._next_cb_id() allocates a fresh id; nx._run_cb runs
-- one and (unless `keep`) drops it so the registry can't grow unbounded.
nx._cb_fns = nx._cb_fns or {}
nx._cb_seq = nx._cb_seq or 0
function nx._next_cb_id()
  nx._cb_seq = nx._cb_seq + 1
  return nx._cb_seq
end

-- Run the callback registered under `id`, forwarding any extra args. `keep` is
-- false for one-shots (vim.schedule, defer_fn, a system on_exit) — the entry is
-- dropped *before* the call so a throwing or re-scheduling callback still leaves
-- the registry clean — and true for a repeating timer, whose fn is retained
-- across fires (its :stop()/:close() drops it). A nil id (already stopped) is a
-- silent no-op. The return value is forwarded so an <expr>-like caller could read
-- it; current callers ignore it.
function nx._run_cb(id, keep, ...)
  local fn = nx._cb_fns[id]
  if not keep then
    nx._cb_fns[id] = nil
    -- A spent one-shot timer (defer_fn / uv timer) is no longer active. This is
    -- the only place that transition is observable Lua-side; clearing it here
    -- keeps a handle's :is_active() honest (a no-op for non-timer callbacks,
    -- whose ids are never in this table). See prelude/ui.lua.
    if nx._timer_active then
      nx._timer_active[id] = nil
    end
  end
  if fn then
    return fn(...)
  end
end

-- nx.schedule(fn): defer `fn` to the end of the current convergence — it runs
-- after the work that scheduled it settles, no longer nested in the caller's
-- stack frame (the strict improvement over the old inline `fn()`), but still
-- within the same input tick (not a later wall-clock turn; that is defer_fn).
-- This is exactly what the colorscheme's "defer to avoid reentrancy" wants.
function nx.schedule(fn)
  local id = nx._next_cb_id()
  nx._cb_fns[id] = fn
  nx._schedule(id) -- Rust bridge: push LoopOp::Schedule{id} onto Shared.loop_ops
end
vim.schedule = nx.schedule

-- nx.schedule_wrap [alias vim.schedule_wrap] (fn): return a function that, when
-- called, schedules `fn` with whatever arguments it was given — a common plugin
-- idiom for "run this callback safely on the loop". The captured args ride into
-- the deferred call via a closure.
function nx.schedule_wrap(fn)
  return function(...)
    local args = { ... }
    local n = select("#", ...)
    nx.schedule(function()
      fn(table.unpack(args, 1, n))
    end)
  end
end
vim.schedule_wrap = nx.schedule_wrap

-- pid registry for async vim.system handles. The event-loop actor reports a
-- spawned child's OS pid back to the server, which records it here keyed by the
-- handle's callback id; the handle's `.pid` reads through this table (nil until
-- the spawn lands, since it can't be known synchronously on a single thread).
nx._proc_pids = nx._proc_pids or {}
function nx._set_proc_pid(id, pid)
  nx._proc_pids[id] = pid
end

-- Streaming-stdout registry for nx.spawn handles. Unlike nx._cb_fns (one-shot),
-- an on_stdout fires repeatedly — once per newline-delimited batch the child
-- emits — so its function persists here, keyed by the spawn's callback id, and is
-- dropped only when the child exits (the exit dispatcher clears it). The server
-- calls nx._run_stdout(id, lines) per ProcessStdout event; a nil entry (no
-- on_stdout, or already exited) is a silent no-op.
nx._stdout_fns = nx._stdout_fns or {}
function nx._run_stdout(id, lines)
  local fn = nx._stdout_fns[id]
  if fn then
    return fn(lines)
  end
end

-- nx.spawn { cmd, args, cwd, env, on_stdout, on_exit }: spawn a child and STREAM
-- its stdout. Each newline-delimited batch fires `on_stdout(lines)` (a list of
-- strings) as it arrives; `on_exit(result)` fires once when the child exits
-- (`result = { code, stdout = "", stderr }` — stdout already streamed). Returns a
-- handle with `:kill()`. The streaming twin of `vim.system` (which delivers stdout
-- once, on exit). `cmd` is a string or argv list; `args` is appended.
function nx.spawn(opts)
  local cmd = opts.cmd
  if type(cmd) == "string" then
    cmd = { cmd }
  end
  local argv = {}
  for _, c in ipairs(cmd) do
    argv[#argv + 1] = c
  end
  for _, a in ipairs(opts.args or {}) do
    argv[#argv + 1] = a
  end
  local id = nx._next_cb_id()
  if opts.on_stdout then
    nx._stdout_fns[id] = opts.on_stdout
  end
  -- One-shot exit dispatcher: clear the persistent on_stdout, then fire on_exit.
  nx._cb_fns[id] = function(result)
    nx._stdout_fns[id] = nil
    if opts.on_exit then
      opts.on_exit(result)
    end
  end
  nx._spawn_stream(id, argv, opts.cwd, opts.env)
  return {
    _id = id,
    kill = function()
      nx._system_kill(id)
    end,
  }
end

-- ----- vim.on_key: the keystroke observer ------------------------------------
-- Plugins (popup helpers, debugging tools) register a function to watch every key the
-- server processes. The registry lives Lua-side keyed by namespace id; the server
-- checks nx._has_on_key once per key (cheap) and, when there are observers, calls
-- nx._run_on_key(key, typed) for each. nxvim passes the key's vim notation for
-- both arguments (it has no separate terminal-byte form).
nx._on_key_fns = nx._on_key_fns or {}
nx._on_key_seq = nx._on_key_seq or 0

-- nx.on_key [alias vim.on_key] (fn[, ns_id[, opts]]): register `fn` as a keystroke
-- observer and return its namespace id. `nx.on_key(nil, ns)` removes that observer;
-- `nx.on_key(nil)` clears them all. Re-registering an existing ns replaces it.
function nx.on_key(fn, ns_id, _opts)
  if fn == nil then
    if ns_id == nil then
      nx._on_key_fns = {}
      return 0
    end
    nx._on_key_fns[ns_id] = nil
    return ns_id
  end
  if ns_id == nil then
    nx._on_key_seq = nx._on_key_seq + 1
    ns_id = nx._on_key_seq
  end
  nx._on_key_fns[ns_id] = fn
  return ns_id
end
vim.on_key = nx.on_key

-- Whether any observer is registered (the server's per-key fast-path guard).
function nx._has_on_key()
  return next(nx._on_key_fns) ~= nil
end

-- Run every observer with (key, typed). A throwing observer is DETACHED (matching
-- neovim, which removes an on_key callback that errors) and the error reported,
-- so one bad observer can't break input handling or silence the others.
function nx._run_on_key(key, typed)
  for ns, fn in pairs(nx._on_key_fns) do
    local ok, err = pcall(fn, key, typed)
    if not ok then
      nx._on_key_fns[ns] = nil
      vim.notify("nxvim: on_key callback errored and was removed: " .. tostring(err))
    end
  end
end

function nx.notify(msg, _level, _opts)
  if type(msg) == "table" then
    msg = table.concat(msg, "\n")
  end
  print(msg)
end
vim.notify = nx.notify

-- nx.notify_once [alias vim.notify_once]: in neovim this dedups by message; we have
-- no message history to dedup against during a one-shot colorscheme load, so route
-- to notify.
function nx.notify_once(msg, level, opts)
  return nx.notify(msg, level, opts)
end
vim.notify_once = nx.notify_once

-- nx.inspect [alias vim.inspect]: pretty-print a value (tables recursively).
function nx.inspect(value)
  local function ins(v, indent)
    if type(v) ~= "table" then
      if type(v) == "string" then
        return string.format("%q", v)
      end
      return tostring(v)
    end
    local parts = {}
    for k, val in pairs(v) do
      parts[#parts + 1] = indent .. "  " .. tostring(k) .. " = " .. ins(val, indent .. "  ")
    end
    return "{\n" .. table.concat(parts, ",\n") .. "\n" .. indent .. "}"
  end
  return ins(value, "")
end
vim.inspect = nx.inspect
