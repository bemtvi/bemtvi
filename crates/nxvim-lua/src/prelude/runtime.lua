-- nxvim Lua prelude — runtime services.
-- The vim._notimpl loud-failure funnel, the deferred-callback registry (vim.schedule / _cb_fns / proc pids), and vim.notify / vim.inspect / the vim.treesitter version-probe shell.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `vim.*` layered on the Rust bridge.

local vim = vim

-- ----- misc ------------------------------------------------------------------

-- vim._notimpl(name): the loud-failure funnel for not-yet-implemented surface.
-- Records `name` into vim._notimpl_hits (a set, so a future `:checkhealth` /
-- `vim.lsp._report` can enumerate which gaps a real config actually hit) and
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
  if not keep then vim._cb_fns[id] = nil end
  if fn then return fn(...) end
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

function vim.notify(msg, _level, _opts)
  if type(msg) == "table" then msg = table.concat(msg, "\n") end
  print(msg)
end

-- vim.notify_once: in neovim this dedups by message; we have no message history
-- to dedup against during a one-shot colorscheme load, so route to notify.
function vim.notify_once(msg, level, opts) return vim.notify(msg, level, opts) end

-- vim.treesitter: nxvim runs its own out-of-process treesitter highlighter, not
-- neovim's in-VM one, so this namespace is otherwise absent. catppuccin probes
-- `vim.treesitter.highlighter.hl_map` purely to detect ancient neovim 0.7; an
-- empty `highlighter` makes that field nil, so the modern path is taken.
-- INCOMPLETE: the namespace is a near-empty shell — only the version-probe shape
-- exists. Every real API (vim.treesitter.get_parser, .query.*, .start, language
-- registration, etc.) is absent, so a config calling one hits a nil-index rather
-- than a named failure. A real impl would either bridge to nxvim's out-of-process
-- highlighter or route the unimplemented entry points through vim._notimpl.
vim.treesitter = vim.treesitter or { highlighter = {} }

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

