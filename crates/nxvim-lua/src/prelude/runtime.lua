-- nxvim Lua prelude — runtime services.
-- The vim._notimpl loud-failure funnel, the deferred-callback registry (vim.schedule / _cb_fns / proc pids), and vim.notify / vim.inspect. (vim.treesitter is wired later, in prelude/treesitter.lua.)
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- misc ------------------------------------------------------------------

-- vim._notimpl(name): the loud-failure funnel for not-yet-implemented surface.
-- Records `name` into vim._notimpl_hits (a set, so a future `:checkhealth` /
-- `vim._report` can enumerate which gaps a real config actually hit) and
-- raises a named error. A stub that quietly returns a fake/empty value makes a
-- broken server look configured; routing every hollow stub through here turns
-- "we think it works" into a concrete, trackable list of what to build (the
-- guiding principle of docs/plans/2026-06-05-lsp-completion.md). `level` (default 2) blames
-- the stub's call site in the error position; the message names the function.
vim._notimpl_hits = vim._notimpl_hits or {}
function vim._notimpl(name, level)
  vim._notimpl_hits[name] = true
  error("nxvim: not implemented: " .. name, level or 2)
end

-- Make a call to an unimplemented `vim.fn.<name>` fail *loud and named* instead of
-- the bare "attempt to call a nil value" a missing field would otherwise give. The
-- Rust bridge creates `vim.fn` as a plain table and the prelude adds the builtins
-- nxvim provides (rawset keys, found before this `__index` ever fires); any name
-- nxvim doesn't have yet resolves to a stub that records and raises through
-- `vim._notimpl` when *called* — never on mere access. That matters two ways:
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
    local fn = function() return vim._notimpl("vim.fn." .. name) end
    return fn
  end,
})

-- ----- the async runtime: the deferred-callback registry ---------------------
-- The spine of nxvim's event loop. A deferred function (vim.schedule, defer_fn,
-- a vim.uv timer, a vim.system on_exit) is stored by integer id in vim._cb_fns
-- and run *later*, by id, from Rust — the vim._keymap_fns / vim._run_keymap shape
-- applied to async work. vim._next_cb_id() allocates a fresh id; vim._run_cb runs
-- one and (unless `keep`) drops it so the registry can't grow unbounded.
vim._cb_fns = vim._cb_fns or {}
vim._cb_seq = vim._cb_seq or 0
function vim._next_cb_id()
  vim._cb_seq = vim._cb_seq + 1
  return vim._cb_seq
end

-- Run the callback registered under `id`, forwarding any extra args. `keep` is
-- false for one-shots (vim.schedule, defer_fn, a system on_exit) — the entry is
-- dropped *before* the call so a throwing or re-scheduling callback still leaves
-- the registry clean — and true for a repeating timer, whose fn is retained
-- across fires (its :stop()/:close() drops it). A nil id (already stopped) is a
-- silent no-op. The return value is forwarded so an <expr>-like caller could read
-- it; current callers ignore it.
function vim._run_cb(id, keep, ...)
  local fn = vim._cb_fns[id]
  if not keep then
    vim._cb_fns[id] = nil
    -- A spent one-shot timer (defer_fn / uv timer) is no longer active. This is
    -- the only place that transition is observable Lua-side; clearing it here
    -- keeps a handle's :is_active() honest (a no-op for non-timer callbacks,
    -- whose ids are never in this table). See prelude/timer.lua.
    if vim._timer_active then vim._timer_active[id] = nil end
  end
  if fn then return fn(...) end
end

-- vim._pump(fn, ...): run fn(...) inside a coroutine so a SYNCHRONOUS prompt
-- (vim.fn.input / vim.fn.confirm) called within it can `coroutine.yield` to park
-- the chunk on the command line while it waits for the answer, then resume inline
-- with it. The pumped entry points (a :lua chunk, a keymap RHS, a user command)
-- run their Lua through this; a bare callback (timer / schedule / autocmd) does
-- not, so a blocking prompt there fails loud rather than hanging.
--
-- Returns (true, fn's-first-return) when fn ran to completion, or (false) when it
-- parked on a prompt — the prompt-result callback the prompt registered resumes
-- the coroutine later (see vim.fn.input). A fn error is re-raised (level 0, so no
-- extra position is prepended) for the server to surface as E5108.
function vim._pump(fn, ...)
  local co = coroutine.create(fn)
  local ok, a = coroutine.resume(co, ...)
  if not ok then error(a, 0) end
  if coroutine.status(co) == "dead" then
    return true, a -- completed; `a` is fn's first return value
  end
  return false -- suspended: parked on a prompt
end

-- vim._source_init(fn): run the user's init.lua chunk through the same coroutine
-- pump as _pump, but RETAIN the coroutine (in vim._init_co) so the server can poll
-- whether it has finished (vim._init_done). Like _pump it lets a blocking vim.wait
-- / vim.fn.input in init.lua PARK on the loop instead of erroring "outside a
-- coroutine"; the server then nested-drives the event loop (timers firing, child
-- processes exiting — e.g. lazy.nvim's git clones) until the coroutine is dead,
-- matching neovim sourcing init.lua to completion before serving the UI.
--
-- Returns true when the chunk ran straight through (no park), false when it parked.
-- A chunk error is re-raised (level 0) for the server to surface as E5113.
function vim._source_init(fn)
  local co = coroutine.create(fn)
  vim._init_co = co
  local ok, a = coroutine.resume(co)
  if not ok then
    vim._init_co = nil
    error(a, 0)
  end
  return coroutine.status(co) == "dead"
end

-- vim._init_done(): has the parked init.lua coroutine finished? True when there is
-- no init coroutine (none sourced, or it ran straight through) or it is dead (it
-- completed, or errored on a resumed continuation — either way nothing left to
-- drive). The server polls this between nested loop drives during startup.
function vim._init_done()
  local co = vim._init_co
  return co == nil or coroutine.status(co) == "dead"
end

-- vim.schedule(fn): defer `fn` to the end of the current convergence — it runs
-- after the work that scheduled it settles, no longer nested in the caller's
-- stack frame (the strict improvement over the old inline `fn()`), but still
-- within the same input tick (not a later wall-clock turn; that is defer_fn).
-- This is exactly what the colorscheme's "defer to avoid reentrancy" wants.
function vim.schedule(fn)
  local id = vim._next_cb_id()
  vim._cb_fns[id] = fn
  vim._schedule(id) -- Rust bridge: push LoopOp::Schedule{id} onto Shared.loop_ops
end

-- vim.schedule_wrap(fn): return a function that, when called, schedules `fn` with
-- whatever arguments it was given — a common plugin idiom for "run this callback
-- safely on the loop". The captured args ride into the deferred call via a closure.
function vim.schedule_wrap(fn)
  return function(...)
    local args = { ... }
    local n = select("#", ...)
    vim.schedule(function() fn(table.unpack and table.unpack(args, 1, n) or unpack(args, 1, n)) end)
  end
end

-- pid registry for async vim.system handles. The event-loop actor reports a
-- spawned child's OS pid back to the server, which records it here keyed by the
-- handle's callback id; the handle's `.pid` reads through this table (nil until
-- the spawn lands, since it can't be known synchronously on a single thread).
vim._proc_pids = vim._proc_pids or {}
function vim._set_proc_pid(id, pid) vim._proc_pids[id] = pid end

-- ----- vim.on_key: the keystroke observer ------------------------------------
-- Plugins (which-key, debugging tools) register a function to watch every key the
-- server processes. The registry lives Lua-side keyed by namespace id; the server
-- checks vim._has_on_key once per key (cheap) and, when there are observers, calls
-- vim._run_on_key(key, typed) for each. nxvim passes the key's vim notation for
-- both arguments (it has no separate terminal-byte form).
vim._on_key_fns = vim._on_key_fns or {}
vim._on_key_seq = vim._on_key_seq or 0

-- vim.on_key(fn[, ns_id[, opts]]): register `fn` as a keystroke observer and
-- return its namespace id. `vim.on_key(nil, ns)` removes that observer;
-- `vim.on_key(nil)` clears them all. Re-registering an existing ns replaces it.
function vim.on_key(fn, ns_id, _opts)
  if fn == nil then
    if ns_id == nil then
      vim._on_key_fns = {}
      return 0
    end
    vim._on_key_fns[ns_id] = nil
    return ns_id
  end
  if ns_id == nil then
    vim._on_key_seq = vim._on_key_seq + 1
    ns_id = vim._on_key_seq
  end
  vim._on_key_fns[ns_id] = fn
  return ns_id
end

-- Whether any observer is registered (the server's per-key fast-path guard).
function vim._has_on_key() return next(vim._on_key_fns) ~= nil end

-- Run every observer with (key, typed). A throwing observer is DETACHED (matching
-- neovim, which removes an on_key callback that errors) and the error reported,
-- so one bad observer can't break input handling or silence the others.
function vim._run_on_key(key, typed)
  for ns, fn in pairs(vim._on_key_fns) do
    local ok, err = pcall(fn, key, typed)
    if not ok then
      vim._on_key_fns[ns] = nil
      vim.notify("nxvim: on_key callback errored and was removed: " .. tostring(err))
    end
  end
end

function vim.notify(msg, _level, _opts)
  if type(msg) == "table" then msg = table.concat(msg, "\n") end
  print(msg)
end

-- vim.notify_once: in neovim this dedups by message; we have no message history
-- to dedup against during a one-shot colorscheme load, so route to notify.
function vim.notify_once(msg, level, opts) return vim.notify(msg, level, opts) end

-- vim.health: the checkhealth reporting API plugins call from their `check()`
-- functions (and bind into locals at load time — lazy.nvim does
-- `local start = vim.health.start or vim.health.report_start` when its health
-- module is required, so the table must exist with callable members or the
-- require errors). nxvim has no :checkhealth report buffer yet, so each call
-- accumulates into vim._health_report (observable / inspectable) instead of
-- rendering. The deprecated `report_*` names alias the current ones.
vim._health_report = vim._health_report or {}
local function health_push(level, msg)
  vim._health_report[#vim._health_report + 1] = { level = level, msg = tostring(msg) }
end
vim.health = {
  start = function(name) health_push("start", name) end,
  ok = function(msg) health_push("ok", msg) end,
  info = function(msg) health_push("info", msg) end,
  warn = function(msg, _adv) health_push("warn", msg) end,
  error = function(msg, _adv) health_push("error", msg) end,
}
vim.health.report_start = vim.health.start
vim.health.report_ok = vim.health.ok
vim.health.report_info = vim.health.info
vim.health.report_warn = vim.health.warn
vim.health.report_error = vim.health.error

function vim.inspect(value)
  local function ins(v, indent)
    if type(v) ~= "table" then
      if type(v) == "string" then return string.format("%q", v) end
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
