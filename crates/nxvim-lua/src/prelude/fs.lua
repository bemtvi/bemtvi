-- nx.fs — a promise-always filesystem API (docs/plans/2026-06-16-nx-fs-api.md).
-- Part of the pure-Lua `nx.*` prelude (see runtime.rs); the Lua half over the
-- `nx._fs_*` Rust bridge in install.rs. Loaded AFTER promise.lua — every op builds
-- on nx.promise.
--
-- Shape (matches the nx.run / nx.run_stream split): one-shot ops return a PROMISE
-- of the result; the streaming `watch` (a change async-iterator) is Phase 2 and
-- not here yet. There are NO callbacks. A failed op REJECTS with a
-- `{ code, message }` table — `code` is a libuv/errno-style hint (ENOENT/EACCES/…):
--
--   nx.async(function()
--     local entries = nx.await(nx.fs.readdir(dir))      -- { { name=, type= }, … }
--     local ok, err = pcall(nx.await, nx.fs.remove(p))  -- err.code == "ENOENT" / …
--   end)()
--
-- The bridge runs each op synchronously through the LuaFs seam, so in a daemon
-- session it blocks briefly on the wire exactly like the vim.fn fs builtins; the
-- promise shape is what consumers (the file tree) depend on regardless.

nx.fs = nx.fs or {}

-- Run a `nx._fs_*` bridge (which returns `value, err`) and settle a promise on it:
-- reject with the `{ code, message }` table when `err` is non-nil, else resolve
-- with the value. The op itself runs synchronously inside the executor; the
-- reaction fires as a microtask, so `nx.await` / `:next` see it on the same tick.
local function settle(bridge, ...)
  local n = select("#", ...)
  local args = { ... }
  return nx.promise.new(function(resolve, reject)
    local value, err = bridge(table.unpack(args, 1, n))
    if err ~= nil then
      reject(err)
    else
      resolve(value)
    end
  end)
end

-- ----- metadata --------------------------------------------------------------

-- nx.fs.stat(path)  -> promise of { type, size, mtime, atime, mode }  (follows links)
function nx.fs.stat(path)
  return settle(nx._fs_stat, path)
end

-- nx.fs.lstat(path) -> like stat but does NOT follow a symlink (type may be "link").
function nx.fs.lstat(path)
  return settle(nx._fs_lstat, path)
end

-- nx.fs.exists(path) -> promise of a boolean. The one op that never rejects: a
-- missing path (or any error) resolves `false`. Existence of the entry itself, so
-- a dangling symlink is `true`.
function nx.fs.exists(path)
  return nx.promise.new(function(resolve)
    resolve(nx._fs_exists(path))
  end)
end

-- ----- listing ---------------------------------------------------------------

-- nx.fs.readdir(path) -> promise of { { name=, type="file"|"directory"|"link" }, … },
-- the entries directly under `path` (no "."/".."), each with its dirent kind in the
-- SAME call (no per-entry stat). `type` is lstat-flavoured (a symlink reports "link").
function nx.fs.readdir(path)
  return settle(nx._fs_readdir, path)
end

-- ----- reading / writing -----------------------------------------------------

-- nx.fs.read(path) -> promise of the file's RAW bytes (a Lua byte-string).
function nx.fs.read(path)
  return settle(nx._fs_read, path)
end

-- nx.fs.read_text(path[, { encoding = "utf-8" }]) -> promise of decoded text.
-- Decodes through the encoding seam and REJECTS (EILSEQ) on invalid input — never
-- lossy replacement text. Use nx.fs.read for raw bytes.
function nx.fs.read_text(path, opts)
  return settle(nx._fs_read_text, path, opts and opts.encoding)
end

-- nx.fs.write(path, data) -> promise (resolves nil). Truncates / creates.
function nx.fs.write(path, data)
  return settle(nx._fs_write, path, data)
end

-- nx.fs.append(path, data) -> promise (resolves nil). Creates if absent.
function nx.fs.append(path, data)
  return settle(nx._fs_append, path, data)
end

-- ----- mutation --------------------------------------------------------------

-- nx.fs.mkdir(path[, { recursive = false }]) -> promise (resolves nil).
function nx.fs.mkdir(path, opts)
  return settle(nx._fs_mkdir, path, opts and opts.recursive or false)
end

-- nx.fs.rename(from, to) -> promise (resolves nil).
function nx.fs.rename(from, to)
  return settle(nx._fs_rename, from, to)
end

-- nx.fs.remove(path[, { recursive = false }]) -> promise (resolves nil). A file is
-- unlinked; a directory needs `recursive` unless already empty.
function nx.fs.remove(path, opts)
  return settle(nx._fs_remove, path, opts and opts.recursive or false)
end

-- nx.fs.copy(src, dst[, { recursive = false }]) -> promise (resolves nil). A file
-- copies (overwriting); a directory needs `recursive`.
function nx.fs.copy(src, dst, opts)
  return settle(nx._fs_copy, src, dst, opts and opts.recursive or false)
end

-- nx.fs.realpath(path) -> promise of the canonical absolute path (symlinks resolved).
function nx.fs.realpath(path)
  return settle(nx._fs_realpath, path)
end

-- ----- watch (continuous → async-iterator) -----------------------------------
--
-- nx.fs.watch(path[, { recursive = false }]) -> a Watch you iterate with
-- nx.await_each (the streaming sibling of the one-shot ops, same shape as
-- nx.run_stream). Each step yields a COALESCED change batch:
--
--   { kind = "create"|"modify"|"remove"|"rename", paths = { <abspath>, … } }
--
-- bursts within a 10 ms window are merged in the server (mixed kinds coarsen to
-- "modify", paths deduped). Want a longer settle? Wrap nx.utils.debounce on top.
--
--   nx.async(function()
--     local w = nx.fs.watch(dir, { recursive = true })
--     for ev in nx.await_each(w) do redraw(ev.paths) end   -- until w:stop()
--   end)()
--
-- A watch that can't arm (bad path, watch limit) or a build with no native watcher
-- (browser / serverless) REJECTS the first pull — fail loud, never a dead watch.
-- `:stop()` cancels the native watch and ends the iteration.

-- Persistent per-watch pumps, keyed by stream id (like nx._stdout_fns for streams).
nx._fs_watch_fns = nx._fs_watch_fns or {}

-- The server fires this on each coalesced change (`ev`, nil) or terminal error
-- (nil, `err`). Routes to the registered pump; a no-op once the stream is stopped.
function nx._run_fs_watch(id, ev, err)
  local fn = nx._fs_watch_fns[id]
  if fn then
    fn(ev, err)
  end
end

local Watch = {}
Watch.__index = Watch

-- :next() -> promise of the next change batch, nil at end (after :stop()), or a
-- rejection carrying the watch error. SEQUENTIAL like the process Stream: one
-- outstanding :next() at a time (what a `for` loop does); batches arriving between
-- pulls buffer in `_queue`.
function Watch:next()
  return nx.promise.new(function(resolve, reject)
    if self._err ~= nil then
      reject(self._err)
    elseif #self._queue > 0 then
      resolve(table.remove(self._queue, 1))
    elseif self._done then
      resolve(nil)
    else
      self._waiter = { resolve = resolve, reject = reject }
    end
  end)
end

-- :stop() cancels the native watch and ends iteration (a parked :next resolves nil).
function Watch:stop()
  if self._done then
    return
  end
  self._done = true
  nx._fs_watch_fns[self._id] = nil
  nx._fs_unwatch(self._id)
  local waiter = self._waiter
  self._waiter = nil
  if waiter then
    waiter.resolve(nil)
  end
end

function nx.fs.watch(path, opts)
  local self = setmetatable({ _queue = {}, _done = false, _waiter = nil }, Watch)
  local id = nx._next_cb_id()
  self._id = id
  nx._fs_watch_fns[id] = function(ev, err)
    local waiter = self._waiter
    if err ~= nil then
      -- Terminal: surface the error to the consumer and tear the watch down.
      self._err = err
      self._done = true
      nx._fs_watch_fns[id] = nil
      if waiter then
        self._waiter = nil
        waiter.reject(err)
      end
    elseif waiter then
      self._waiter = nil
      waiter.resolve(ev)
    else
      self._queue[#self._queue + 1] = ev
    end
  end
  nx._fs_watch(id, path, opts and opts.recursive or false)
  return self
end
