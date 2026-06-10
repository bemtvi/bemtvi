-- nxvim Lua prelude — opt-in yieldable protected calls (the vim.co_pcall family).
-- vim.co_pcall / vim.co_xpcall / vim.co_wrap: pcall/xpcall/coroutine.wrap analogs
-- whose protected function may `coroutine.yield` — so a blocking read
-- (vim.fn.getcharstr / input / confirm) survives being wrapped in a protected
-- call on PUC Lua 5.1.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new` (see
-- runtime.rs); placed right after runtime.lua so `vim._co_driver` exists before
-- fs.lua's `await_prompt` (the blocking funnel) can read it.
--
-- ----- why this exists -------------------------------------------------------
--
-- PUC Lua 5.1 (nxvim's `lua51` backend, and the *only* backend in the browser —
-- LuaJIT can't compile to wasm) cannot `coroutine.yield` across a C-call frame,
-- and the built-in `pcall` is a C function. So `pcall(vim.fn.getcharstr)` — the
-- shape which-key's live-popup loop uses — raises *"attempt to yield across a
-- C-call boundary"* instead of blocking for a key. LuaJIT (the default native
-- backend) has a yieldable `pcall`, so this only bites `lua51` / the browser.
--
-- The fix is to run the protected function in its OWN coroutine and relay its
-- yields up through a pure-Lua path (no C frame in the way). We expose that as a
-- named, opt-in primitive rather than replacing the global `pcall`: a global swap
-- would impose a per-call coroutine allocation on *every* protected call and risk
-- fidelity regressions across unrelated code, for the benefit of the handful of
-- plugins that block-read inside `pcall`. A plugin author targeting nxvim calls
-- `vim.co_pcall` explicitly where they need a yieldable protected call.
-- (docs/plans/2026-06-09-edit-host-and-browser-lua.md → Phase 2.)
--
-- ----- how it composes with nxvim's blocking model ---------------------------
--
-- nxvim's blocking reads don't "bubble a yield up to the pump"; instead
-- fs.lua's `await_prompt` registers a server callback that *resumes the running
-- coroutine directly* when the key arrives, then yields. Under a `co_pcall` the
-- running coroutine is the inner one — so a direct resume would bypass this relay
-- and the relay's coroutine (which holds the continuation of the protected call)
-- would never wake. To bridge the two models, `await_prompt` resumes the
-- OUTERMOST driver instead, and the relay chain forwards the resume value back
-- down to the blocked inner coroutine. `vim._co_driver` records, per coroutine,
-- the coroutine that drives it, so `await_prompt` can walk to that outermost root.
-- Without any `co_pcall` on the stack the map is empty, the root *is* the running
-- coroutine, and `await_prompt` behaves exactly as before.

local vim = vim

-- coroutine -> the coroutine that resumes it (its driver). Weak keys so a
-- finished inner coroutine's entry is collected rather than leaked.
vim._co_driver = vim._co_driver or setmetatable({}, { __mode = "k" })

-- vim.co_pcall(f, ...): pcall, but `f` may yield (e.g. block on getcharstr).
-- Returns `true, f's-returns…` on success or `false, error` on error, exactly
-- like pcall. `f` runs in a fresh coroutine; the relay catches its yields (no C
-- frame between, unlike the global pcall) and re-emits them up this coroutine so
-- a pumped entry can park, then forwards the resume value back down to `f`.
function vim.co_pcall(f, ...)
  local co = coroutine.create(f)
  vim._co_driver[co] = coroutine.running()
  local function step(ok, ...)
    if not ok then
      vim._co_driver[co] = nil
      return false, ...
    end
    if coroutine.status(co) == "dead" then
      vim._co_driver[co] = nil
      return true, ...
    end
    -- `co` yielded (it parked on a blocking read): relay the yield up through
    -- THIS coroutine — a pure-Lua frame, so no C boundary to cross — and, once
    -- resumed, forward the resume value back down into `co`.
    return step(coroutine.resume(co, coroutine.yield(...)))
  end
  return step(coroutine.resume(co, ...))
end

-- vim.co_xpcall(f, msgh, ...): xpcall, but yieldable. On error, `msgh` is called
-- with the error value and `co_xpcall` returns `false, msgh's-returns…`; on
-- success it returns `true, f's-returns…`. Like neovim/LuaJIT's xpcall, extra
-- args are forwarded to `f`. LIMITATION: because the protected call runs in its
-- own coroutine, the stack has already unwound by the time `msgh` runs, so a
-- `debug.traceback` inside `msgh` won't capture `f`'s original frames.
function vim.co_xpcall(f, msgh, ...)
  local function finish(ok, ...)
    if ok then return true, ... end
    return false, msgh(...)
  end
  return finish(vim.co_pcall(f, ...))
end

-- vim.co_wrap(f): coroutine.wrap-flavored — returns a function that runs `f`
-- through the yieldable relay and RE-RAISES any error (no leading ok flag),
-- returning `f`'s results on success. Unlike coroutine.wrap it does not reuse one
-- coroutine across calls (it is a protected *call* helper, not a generator): each
-- invocation runs `f` afresh. Use it to call a yielding `f` from a spot that would
-- otherwise interpose a C frame.
function vim.co_wrap(f)
  return function(...)
    local function finish(ok, ...)
      if not ok then error((...), 0) end
      return ...
    end
    return finish(vim.co_pcall(f, ...))
  end
end
