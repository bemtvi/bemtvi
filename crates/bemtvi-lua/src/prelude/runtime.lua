-- bemtvi Lua prelude — runtime services.
-- The btv._notimpl loud-failure funnel, the deferred-callback registry (btv.schedule / _cb_fns / proc pids), and btv.notify / btv.inspect (with vim.* aliases).
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new`
-- (see runtime.rs); the pure-Lua half of `btv.*` (with vim.* aliases) layered on the Rust bridge.

local vim = vim

-- ----- misc ------------------------------------------------------------------

-- `btv._notimpl(name)`: the loud-failure funnel for not-yet-implemented surface.
-- Records `name` into `btv._notimpl_hits` (a set, so a future `:checkhealth` /
-- `btv._report` can enumerate which gaps a real config actually hit) and
-- raises a named error. A stub that quietly returns a fake/empty value makes a
-- broken server look configured; routing every hollow stub through here turns
-- "we think it works" into a concrete, trackable list of what to build (the
-- guiding principle of docs/plans/2026-06-05-lsp-completion.md). `level` (default 2) blames
-- the stub's call site in the error position; the message names the function.
btv._notimpl_hits = btv._notimpl_hits or {}
function btv._notimpl(name, level)
  btv._notimpl_hits[name] = true
  error("bemtvi: not implemented: " .. name, level or 2)
end

-- Make a call to an unimplemented `vim.fn.<name>` fail *loud and named* instead of
-- the bare "attempt to call a nil value" a missing field would otherwise give. The
-- Rust bridge creates `vim.fn` as a plain table and the prelude adds the builtins
-- bemtvi provides (rawset keys, found before this `__index` ever fires); any name
-- bemtvi doesn't have yet resolves to a stub that records and raises through
-- `btv._notimpl` when *called* — never on mere access. That matters two ways:
--   * neovim's `vim.fn` is likewise always-callable (an unknown function raises
--     `E117` at call time, and `if vim.fn.foo then` is truthy), so feature-probing
--     configs keep working; returning nil here would diverge.
--   * a gap surfaces as "bemtvi: not implemented: vim.fn.<name>" pointing at the
--     call site, so a missing builtin is a one-line diagnosis rather than a buried
--     nil-call error (which `nvim_exec_lua` would swallow to the message line).
-- A plugin that genuinely wants to detect absence can still `vim.fn.has(...)` or
-- pcall the call; it cannot rely on the field being nil (neither can it in neovim).
setmetatable(vim.fn, {
  __index = function(_, name)
    local fn = function()
      return btv._notimpl("vim.fn." .. name)
    end
    return fn
  end,
})

-- ----- the async runtime: the deferred-callback registry ---------------------
-- The spine of bemtvi's event loop. A deferred function (`btv.schedule`, `defer_fn`,
-- a timer, a system on_exit) is stored by integer id in `btv._cb_fns`
-- and run *later*, by id, from Rust — the `btv._keymap_fns` / `btv._run_keymap` shape
-- applied to async work. `btv._next_cb_id()` allocates a fresh id; `btv._run_cb` runs
-- one and (unless `keep`) drops it so the registry can't grow unbounded.
btv._cb_fns = btv._cb_fns or {}
btv._cb_seq = btv._cb_seq or 0
function btv._next_cb_id()
  btv._cb_seq = btv._cb_seq + 1
  return btv._cb_seq
end

-- `btv._bridge(id, call, cleanup)`: run the Rust bridge `call` (a closure) with the
-- promise's callback already registered under `id`. The op surfaces call this right
-- after registering — if the bridge itself throws (an arg-conversion error, a nil
-- bridge in a session that does not route the op), the entry would otherwise sit in
-- `btv._cb_fns` forever: the callback only ever fires through `btv._run_cb`, which the
-- error path never reaches. Drop the dead entry (plus any *other* registry the op
-- wrote before the call, via the optional `cleanup(id)`), then rethrow so the
-- enclosing promise executor (`btv.promise.new` pcalls it) still turns the throw
-- into a rejection. Streams that register a pump alongside the one-shot callback
-- (`btv.fs.watch`'s `_fs_watch_fns`, `btv.run_stream`'s `_stdout_fns`,
-- `btv.process.open`'s / `btv.socket.connect`'s `_proc_handlers`/`_sock_handlers`)
-- pass `cleanup` so a conversion throw leaks neither registry.
function btv._bridge(id, call, cleanup)
  local ok, err = pcall(call)
  if not ok then
    btv._cb_fns[id] = nil
    if cleanup then
      cleanup(id)
    end
    error(err, 0)
  end
end

-- Run the callback registered under `id`, forwarding any extra args. `keep` is
-- false for one-shots (`vim.schedule`, `defer_fn`, a system on_exit) — the entry is
-- dropped *before* the call so a throwing or re-scheduling callback still leaves
-- the registry clean — and true for a repeating timer, whose fn is retained
-- across fires (its `:stop()`/`:close()` drops it). A nil id (already stopped) is a
-- silent no-op. The return value is forwarded so an `<expr>`-like caller could read
-- it; current callers ignore it.
function btv._run_cb(id, keep, ...)
  local fn = btv._cb_fns[id]
  if not keep then
    btv._cb_fns[id] = nil
    -- A spent one-shot timer (defer_fn / uv timer) is no longer active. This is
    -- the only place that transition is observable Lua-side; clearing it here
    -- keeps a handle's :is_active() honest (a no-op for non-timer callbacks,
    -- whose ids are never in this table). btv._timer_active / btv.timer are defined
    -- just below.
    if btv._timer_active then
      btv._timer_active[id] = nil
    end
  end
  if fn then
    return fn(...)
  end
end

-- `btv.schedule(fn)`: defer `fn` to the end of the current convergence — it runs
-- after the work that scheduled it settles, no longer nested in the caller's
-- stack frame (the strict improvement over the old inline `fn()`), but still
-- within the same input tick (not a later wall-clock turn; that is `defer_fn`).
-- This is exactly what the colorscheme's "defer to avoid reentrancy" wants.
function btv.schedule(fn)
  local id = btv._next_cb_id()
  btv._cb_fns[id] = fn
  btv._schedule(id) -- Rust bridge: push LoopOp::Schedule{id} onto Shared.loop_ops
end
vim.schedule = btv.schedule

-- `btv.schedule_wrap` [alias `vim.schedule_wrap`] (fn): return a function that, when
-- called, schedules `fn` with whatever arguments it was given — a common plugin
-- idiom for "run this callback safely on the loop". The captured args ride into
-- the deferred call via a closure.
function btv.schedule_wrap(fn)
  return function(...)
    local args = { ... }
    local n = select("#", ...)
    btv.schedule(function()
      fn(table.unpack(args, 1, n))
    end)
  end
end
vim.schedule_wrap = btv.schedule_wrap

-- ----- btv.timer [alias vim.defer_fn] -----------------------------------------
-- The wall-clock sibling of `btv.schedule`: where `btv.schedule` runs `fn` at the end of
-- the current convergence (same tick, a microtask), `btv.timer` runs it `timeout` ms
-- from now on a LATER tick. It rides the event-loop actor through the
-- `btv._timer_start` / `btv._timer_stop` bridge: a callback id is registered in
-- `btv._cb_fns`, the actor sleeps and fires `LoopEvent::Timer`, and the server runs the
-- callback by id on its thread. Both deferral primitives live here, next to the
-- `btv._cb_fns` registry and the `btv._run_cb` cleanup above (which clears
-- `btv._timer_active` for a spent one-shot). `btv.promise.delay` builds on this.
btv._timer_active = btv._timer_active or {}

-- A minimal timer handle returned by `btv.timer`, so a caller can `:stop()` the
-- deferral before it fires (neovim returns a uv timer; bemtvi returns this). It is
-- NOT the libuv handle API — the `btv` timer surface is the supported one.
local defer_handle = {}
defer_handle.__index = defer_handle
function defer_handle:stop()
  btv._timer_active[self._id] = nil
  btv._timer_stop(self._id)
  btv._cb_fns[self._id] = nil
  return 0
end
function defer_handle:is_active()
  return btv._timer_active[self._id] == true
end

-- `btv.timer(fn, timeout)`: the canonical timer / defer primitive (aliased by
-- `vim.defer_fn`) — run `fn` once, `timeout` ms from now, on the loop — the
-- off-tick deferral configs use for retry patterns. Returns a handle so the
-- caller can `:stop()` it before it fires.
function btv.timer(fn, timeout)
  local id = btv._next_cb_id()
  btv._cb_fns[id] = fn
  btv._timer_active[id] = true -- armed; the returned handle's :is_active() reads this
  btv._timer_start(id, timeout or 0, 0) -- one-shot
  return setmetatable({ _id = id }, defer_handle)
end
vim.defer_fn = btv.timer

-- `btv.on_next_tick(fn)`: run `fn` on the NEXT event-loop tick — the turn after the
-- current one finishes. The cross-tick sibling of `btv.schedule`: where `btv.schedule`
-- fires at the end of THIS convergence (a same-tick microtask, so it cannot observe
-- state that only refreshes between ticks — a freshly-mounted window's id, a mirror
-- the server repopulates each turn), `btv.on_next_tick` yields the tick entirely and runs
-- on the next one, when those mirrors have been refreshed. A zero-delay one-shot
-- timer is exactly that. Returns the timer handle, so a caller can `:stop()` it before
-- it fires. (Poll across several ticks by calling it again from within `fn`.)
function btv.on_next_tick(fn)
  return btv.timer(fn, 0)
end

-- pid registry for spawned children (`btv.run` / `btv.run_stream`). The event-loop
-- actor reports a spawned child's OS pid back to the server, which records it here
-- keyed by the spawn's callback id; `stream:pid()` (prelude/process.lua) reads
-- through this table (nil until the spawn lands, since it can't be known
-- synchronously on a single thread).
btv._proc_pids = btv._proc_pids or {}
function btv._set_proc_pid(id, pid)
  btv._proc_pids[id] = pid
end

-- The **bounded compute sandbox** surfaces, by their public dotted name
-- (`"fold.text"`, `"qf.parse"`, …) → the Lua source last installed on each, or nil.
--
-- Every `btv.*` setter that hands source to the sandbox writes here through
-- `btv._sandbox_set` rather than into a private field of its own, so the set of
-- installed surfaces is enumerable in one place. That is what `btv.test`'s per-test
-- baseline snapshots and restores: keyed by the setter's own path, a surface added
-- later is isolated the moment it uses the helper, with no second list to keep in
-- step. (It stopped being kept in step once already — `complete.scorer`,
-- `decor.expr` and both `qf` surfaces leaked between tests until this replaced the
-- hand-written list.)
btv._sandbox_srcs = btv._sandbox_srcs or {}

-- `btv._sandbox_set(name, native, src)`: the shared body of every sandbox-source
-- setter. Validates that `src` is a string (or nil, to clear), records it under
-- `name` in `btv._sandbox_srcs`, and hands it to the native setter that compiles it.
--
-- `name` is the setter's own dotted path under `btv.` — `"qf.text"` for
-- `btv.qf.text` — because the restore path resolves the setter back from the key.
-- An error blames the caller's caller (level 3): the user called `btv.qf.text`, not
-- this.
function btv._sandbox_set(name, native, src)
  if src ~= nil and type(src) ~= "string" then
    error("btv." .. name .. ": expected a string of Lua source (or nil), got " .. type(src), 3)
  end
  btv._sandbox_srcs[name] = src
  native(src)
end

-- `btv._sandbox_setter(name)`: resolve a `btv._sandbox_srcs` key back to the public
-- setter it names, or nil when the surface's module was not loaded.
function btv._sandbox_setter(name)
  local fn = btv
  for part in name:gmatch("[^.]+") do
    if type(fn) ~= "table" then
      return nil
    end
    fn = fn[part]
  end
  return type(fn) == "function" and fn or nil
end

-- Streaming-stdout registry for streaming-child handles (`btv.run_stream`, defined
-- in prelude/process.lua). Unlike `btv._cb_fns` (one-shot), an on_stdout fires
-- repeatedly — once per newline-delimited batch the child emits — so its function
-- persists here, keyed by the spawn's callback id, and is dropped only when the
-- child exits (the exit dispatcher clears it). The server calls
-- `btv._run_stdout(id, lines)` per `ProcessStdout` event; a nil entry (no handler, or
-- already exited) is a silent no-op.
btv._stdout_fns = btv._stdout_fns or {}
function btv._run_stdout(id, lines)
  local fn = btv._stdout_fns[id]
  if fn then
    return fn(lines)
  end
end

function btv.notify(msg, level, _opts)
  if type(msg) == "table" then
    msg = table.concat(msg, "\n")
  end
  -- Honour the severity: an ERROR-level notification lands on the message line
  -- painted red, matching a genuine error (the core `echo_err` path). The message
  -- line has only two states — error (red) or plain — so WARN and below funnel
  -- through `print` like any other message; neovim's distinct WarningMsg colour
  -- has no analogue on our line yet. (`btv.log.levels.ERROR == 4`; guarded so a
  -- bare-prelude call before `state.lua` loads still works.) A string severity
  -- ("error", "warn", …, the vim.log.levels key spelling) counts too — several
  -- surfaces pass `"error"`, and degrading those to a plain print would hide them.
  local ERROR = (btv.log and btv.log.levels and btv.log.levels.ERROR) or 4
  local is_error = (type(level) == "number" and level >= ERROR)
    or (type(level) == "string" and level:lower() == "error")
  if is_error and btv._echo_err then
    btv._echo_err(tostring(msg))
  else
    print(msg)
  end
end
vim.notify = btv.notify

-- `btv.notify_once` [alias `vim.notify_once`]: in neovim this dedups by message; we have
-- no message history to dedup against during a one-shot colorscheme load, so route
-- to notify.
function btv.notify_once(msg, level, opts)
  return btv.notify(msg, level, opts)
end
vim.notify_once = btv.notify_once

-- `btv.inspect` [alias `vim.inspect`]: pretty-print a value (tables recursively). A
-- table reached again on the current descent renders as `"<cycle>"` — a plugin
-- inspects arbitrary state (parent-linked trees, self-referencing registries),
-- and unguarded recursion would blow the C stack instead of printing.
function btv.inspect(value)
  local seen = {}
  local function ins(v, indent)
    if type(v) ~= "table" then
      if type(v) == "string" then
        return string.format("%q", v)
      end
      return tostring(v)
    end
    if seen[v] then
      return "<cycle>"
    end
    seen[v] = true
    local parts = {}
    for k, val in pairs(v) do
      parts[#parts + 1] = indent .. "  " .. tostring(k) .. " = " .. ins(val, indent .. "  ")
    end
    seen[v] = nil
    return "{\n" .. table.concat(parts, ",\n") .. "\n" .. indent .. "}"
  end
  return ins(value, "")
end
vim.inspect = btv.inspect
