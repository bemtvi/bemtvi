-- nxvim Lua prelude — the async (callback) form of the vim.uv filesystem family.
-- Loaded as one of the sequential prelude chunks by `LuaRuntime::new` (runtime.rs),
-- after `install_runtime_api` has built the `vim.uv` table (src/uvfs.rs).
--
-- In luv every fs_* function is dual-mode: SYNCHRONOUS when called without a
-- trailing callback (it returns `value`, or `nil, err` on failure), and
-- ASYNCHRONOUS when the last argument is a function — it returns immediately and
-- the result is delivered to `callback(err, value)` on a later loop iteration.
-- The Rust primitives in src/uvfs.rs implement only the synchronous form; this
-- wraps each so that passing a trailing function dispatches the async form.
--
-- The deferral rides nxvim's existing event loop: vim.schedule pushes the
-- callback onto the convergence queue (vim._cb_fns + LoopOp::Schedule), so it
-- fires off the calling frame, after the current work settles — exactly the
-- contract plenary.path's coroutine-wrapped async readers (Path:_read_async) and
-- plenary.async depend on. Chained async ops (open → fstat → read → close, each
-- re-scheduling from inside the previous callback) converge inside one
-- `run_pending` fixpoint.
--
-- DOCUMENTED APPROXIMATION: the I/O itself still runs synchronously, on the
-- editor thread, at call time — only the *callback* is deferred. This is forced
-- by nxvim's design (the fd table that fs_open populates is thread-local to the
-- single Lua thread; the off-thread event-loop actor can't touch it), and it is
-- invisible to callers, who only observe `callback(err, value)` arriving later.
-- A true libuv-threadpool overlap of the I/O is not modelled; the observable
-- callback contract is faithful.

local vim = vim
local uv = vim.uv -- same table as vim.loop (aliased in install.rs), so wrapping
-- here updates both surfaces plugins reach through.

local unpack = table.unpack or unpack

-- The fs_* primitives that take a trailing async callback in luv. (os_homedir,
-- cwd, hrtime, fs_realpath, os_uname and the timer handles are not async fs ops
-- and are left untouched. fs_scandir_next is the synchronous iterator pull — it
-- has no async form — so it is also excluded.)
local ASYNC_FS = {
  "fs_open",
  "fs_close",
  "fs_read",
  "fs_write",
  "fs_stat",
  "fs_lstat",
  "fs_fstat",
  "fs_mkdir",
  "fs_rmdir",
  "fs_unlink",
  "fs_rename",
  "fs_copyfile",
  "fs_utime",
  "fs_access",
  "fs_scandir",
}

for _, name in ipairs(ASYNC_FS) do
  local sync = uv[name]
  -- Guard: only wrap what the Rust side actually provides, so a future change to
  -- the primitive set can't silently leave a wrapper pointing at nil.
  if type(sync) == "function" then
    uv[name] = function(...)
      local n = select("#", ...)
      local last = n > 0 and select(n, ...) or nil
      if type(last) == "function" then
        -- Async form: run the sync primitive with the callback stripped, then
        -- deliver its (value, err) to the callback as luv's (err, value).
        local args = { ... }
        local value, err = sync(unpack(args, 1, n - 1))
        vim.schedule(function() last(err, value) end)
        return -- luv returns a uv_fs_t request handle here; callers ignore it.
      end
      -- Sync form: tail-call so both return values (value, err) propagate, e.g.
      -- `assert(uv.fs_open(...))` and `local fd, err = uv.fs_open(...)`.
      return sync(...)
    end
  end
end
