-- nx.fs — a promise-always filesystem API (docs/plans/2026-06-16-nx-fs-api.md).
-- Part of the pure-Lua `nx.*` prelude (see runtime.rs); the Lua half over the
-- `nx._fs_*` Rust bridge in install.rs. Loaded AFTER promise.lua — every op builds
-- on `nx.promise`.
--
-- Shape (matches the `nx.run` / `nx.run_stream` split): one-shot ops return a PROMISE
-- of the result; the streaming `watch` (a change async-iterator) is Phase 2 and
-- not here yet. There are NO callbacks. A failed op REJECTS with a
-- `{ code, message }` table — `code` is a libuv/errno-style hint (ENOENT/EACCES/…):
--
--   nx.async(function()
--     local entries = nx.await(nx.fs.readdir(dir))      -- { { name=, type= }, … }
--     local ok, err = pcall(nx.await, nx.fs.remove(p))  -- err.code == "ENOENT" / …
--   end)()
--
-- The bridge runs each op OFF the editor tick: `nx._fs_op` queues a LoopOp::Fs and
-- returns immediately, so the promise stays pending and SETTLES ON A LATER TICK (the
-- event-loop actor runs the op on its blocking pool natively; the daemon luafs leg
-- runs it on the browser). This is non-blocking on native and the only way to reach
-- the daemon on wasm. The promise shape is unchanged — only the timing moved from
-- "resolved-already" to "resolved next tick".

nx.fs = nx.fs or {}

-- Queue an off-tick fs op described by `job` ({ op = "<name>", … }) and return a
-- pending promise. A callback id is registered in `nx._cb_fns`; the server fires
-- `nx._run_cb(id, false, err, value)` when the op settles — `err` the { code, message }
-- table (then `value` is nil) on failure, else `err` nil with the resolved value.
local function run_fs(job)
  return nx.promise.new(function(resolve, reject)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(err, value)
      if err ~= nil then
        reject(err)
      else
        resolve(value)
      end
    end
    nx._fs_op(job, id)
  end)
end

-- ----- metadata --------------------------------------------------------------

-- `nx.fs.stat(path)`  -> promise of { type, size, mtime, atime, mode }  (follows links)
function nx.fs.stat(path)
  return run_fs({ op = "stat", path = path })
end

-- `nx.fs.lstat(path)` -> like stat but does NOT follow a symlink (type may be `"link"`).
function nx.fs.lstat(path)
  return run_fs({ op = "lstat", path = path })
end

-- `nx.fs.exists(path)` -> promise of a boolean. The one op that never rejects: a
-- missing path (or any error) resolves `false`. Existence of the entry itself, so
-- a dangling symlink is `true`. (The `exists` job resolves a bool rather than
-- rejecting, so no reject-to-false mapping is needed in the wrapper.)
function nx.fs.exists(path)
  return run_fs({ op = "exists", path = path })
end

-- ----- listing ---------------------------------------------------------------

-- `nx.fs.readdir(path)` -> promise of { { name=, type=`"file"`|`"directory"`|`"link"` }, … },
-- the entries directly under `path` (no `"."`/`".."`), each with its dirent kind in the
-- SAME call (no per-entry stat). `type` is lstat-flavoured (a symlink reports `"link"`).
function nx.fs.readdir(path)
  return run_fs({ op = "readdir", path = path })
end

-- `nx.fs.walk(dir[, opts])` -> promise of a LIST of file paths relative to `dir`, a
-- recursive directory listing built from readdir. The transport-agnostic file
-- enumeration the codebase reaches for when `rg`/`fd` aren't available (the pure web
-- client, where a spawn has no real shell) — it rides the same off-tick fs seam as
-- every other `nx.fs` op, so it works against local disk, a daemon, and OPFS alike.
-- opts:
--
-- ```
-- max     cap on files returned (default 50000) — a runaway guard on huge trees
-- hidden  include dotfiles / dotdirs (default false)
-- skip    set of directory basenames to prune (default { [".git"] = true })
-- ```
--
-- An unreadable subdirectory is skipped (not fatal). MUST be awaited inside `nx.async`.
function nx.fs.walk(dir, opts)
  opts = opts or {}
  local max = opts.max or 50000
  local hidden = opts.hidden or false
  local skip = opts.skip or { [".git"] = true }
  return nx.async(function()
    local out = {}
    -- Relative subdirs still to visit; "" is `dir` itself.
    local stack = { "" }
    while #stack > 0 and #out < max do
      local rel = table.remove(stack)
      local abs = rel == "" and dir or (dir .. "/" .. rel)
      -- Skip a directory we can't read (permissions / vanished mid-walk) rather
      -- than aborting the whole enumeration.
      local ok, entries = pcall(nx.await, nx.fs.readdir(abs))
      if ok then
        for _, e in ipairs(entries) do
          if hidden or e.name:sub(1, 1) ~= "." then
            local child = rel == "" and e.name or (rel .. "/" .. e.name)
            if e.type == "directory" then
              if not skip[e.name] then
                stack[#stack + 1] = child
              end
            elseif e.type == "file" and #out < max then
              out[#out + 1] = child
            end
          end
        end
      end
    end
    return out
  end)()
end

-- `nx.fs.grep(dir, query[, opts])` -> promise of a LIST of matches, each
-- { path = <rel>, row = <1-based lnum>, col = <1-based>, text = <line> }: a recursive,
-- transport-agnostic plain-substring search. The fallback the grep picker reaches for
-- when `rg`/`grep` aren't available (the pure web client) — it rides the same off-tick
-- fs seam as every other `nx.fs` op, so it works against local disk, a daemon, and OPFS.
-- Walks `dir` (`nx.fs.walk`), reads each file (`nx.fs.read_text`), and matches `query` as a
-- LITERAL substring per line. Binary / unreadable files (read_text rejects) are skipped.
-- `opts` pass through to `nx.fs.walk` (max / hidden / skip). MUST be awaited inside `nx.async`.
function nx.fs.grep(dir, query, opts)
  return nx.async(function()
    local out = {}
    if not query or query == "" then
      return out
    end
    local files = nx.await(nx.fs.walk(dir, opts))
    local base = dir:gsub("/$", "")
    for _, rel in ipairs(files) do
      -- read_text rejects on invalid UTF-8 (a binary file); skip it rather than abort.
      local ok, text = pcall(nx.await, nx.fs.read_text(base .. "/" .. rel))
      if ok then
        local lnum = 0
        for line in (text .. "\n"):gmatch("(.-)\n") do
          lnum = lnum + 1
          local col = line:find(query, 1, true) -- plain (non-pattern) substring
          if col then
            out[#out + 1] = { path = rel, row = lnum, col = col, text = line }
          end
        end
      end
    end
    return out
  end)()
end

-- ----- reading / writing -----------------------------------------------------

-- `nx.fs.read(path)` -> promise of the file's RAW bytes (a Lua byte-string).
function nx.fs.read(path)
  return run_fs({ op = "read", path = path })
end

-- `nx.fs.read_text(path[, { encoding = "utf-8" }])` -> promise of decoded text.
-- Decodes through the encoding seam and REJECTS (EILSEQ) on invalid input — never
-- lossy replacement text. Use `nx.fs.read` for raw bytes.
function nx.fs.read_text(path, opts)
  return run_fs({ op = "read_text", path = path, encoding = opts and opts.encoding })
end

-- `nx.fs.write(path, data)` -> promise (resolves nil). Truncates / creates.
function nx.fs.write(path, data)
  return run_fs({ op = "write", path = path, data = data })
end

-- `nx.fs.append(path, data)` -> promise (resolves nil). Creates if absent.
function nx.fs.append(path, data)
  return run_fs({ op = "append", path = path, data = data })
end

-- ----- mutation --------------------------------------------------------------

-- `nx.fs.mkdir(path[, { recursive = false, mode = 0o755 }])` -> promise (resolves
-- nil). `mode` is the Unix permission bits applied to every directory created
-- (defaults to 0o755 in Rust when omitted; ignored off Unix) — pass it to keep a
-- private data/state dir from being created world-readable.
function nx.fs.mkdir(path, opts)
  return run_fs({
    op = "mkdir",
    path = path,
    recursive = opts and opts.recursive or false,
    mode = opts and opts.mode or nil,
  })
end

-- `nx.fs.rename(from, to)` -> promise (resolves nil).
function nx.fs.rename(from, to)
  return run_fs({ op = "rename", from = from, to = to })
end

-- `nx.fs.remove(path[, { recursive = false }])` -> promise (resolves nil). A file is
-- unlinked; a directory needs `recursive` unless already empty.
function nx.fs.remove(path, opts)
  return run_fs({ op = "remove", path = path, recursive = opts and opts.recursive or false })
end

-- `nx.fs.copy(src, dst[, { recursive = false }])` -> promise (resolves nil). A file
-- copies (overwriting); a directory needs `recursive`.
function nx.fs.copy(src, dst, opts)
  return run_fs({ op = "copy", src = src, dst = dst, recursive = opts and opts.recursive or false })
end

-- `nx.fs.realpath(path)` -> promise of the canonical absolute path (symlinks resolved).
function nx.fs.realpath(path)
  return run_fs({ op = "realpath", path = path })
end

nx.hash = nx.hash or {}

-- `nx.hash.file(path[, algo])` -> promise of the file's lowercase-hex digest. The
-- streaming member of the `nx.hash.*` family (the in-memory string hashers and the
-- incremental `nx.hash.new` live in hash.lua): hashing a file is I/O, so it routes
-- through the fs machinery rather than reading the file into Lua first. The server
-- streams the file in fixed 64 KiB chunks and folds each into the hasher, so a 300 MB
-- file costs 64 KiB of memory — not 300 MB, as `nx.hash.sha256(nx.await(nx.fs.read(path)))`
-- would. On a remote/browser build the hashing runs entirely on the daemon; only the
-- short digest crosses the wire, never the file's bytes. `algo` is one of `"sha1"` /
-- `"sha256"` / `"sha512"` / `"md5"` (default `"sha256"`); an unknown algorithm rejects (EINVAL).
function nx.hash.file(path, algo)
  return run_fs({ op = "hash_file", path = path, algo = algo or "sha256" })
end

-- ----- watch (continuous → async-iterator) -----------------------------------
--
-- `nx.fs.watch(path[, { recursive = false }])` -> a Watch you iterate with
-- `nx.await_each` (the streaming sibling of the one-shot ops, same shape as
-- `nx.run_stream`). Each step yields a COALESCED change batch:
--
--   { kind = "create"|"modify"|"remove"|"rename", paths = { <abspath>, … } }
--
-- bursts within a 10 ms window are merged in the server (mixed kinds coarsen to
-- `"modify"`, paths deduped). Want a longer settle? Wrap `nx.utils.debounce` on top.
--
--   nx.async(function()
--     local w = nx.fs.watch(dir, { recursive = true })
--     for ev in nx.await_each(w) do redraw(ev.paths) end   -- until w:stop()
--   end)()
--
-- A watch that can't arm (bad path, watch limit) or a build with no native watcher
-- (browser / serverless) REJECTS the first pull — fail loud, never a dead watch.
-- `:stop()` cancels the native watch and ends the iteration.

-- Persistent per-watch pumps, keyed by stream id (like `nx._stdout_fns` for streams).
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

-- `:next()` -> promise of the next change batch, nil at end (after `:stop()`), or a
-- rejection carrying the watch error. SEQUENTIAL like the process Stream: one
-- outstanding `:next()` at a time (what a `for` loop does); batches arriving between
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

-- `:stop()` cancels the native watch and ends iteration (a parked `:next` resolves nil).
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
