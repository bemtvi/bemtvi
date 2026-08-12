-- btv.fs — a promise-always filesystem API (docs/plans/2026-06-16-btv-fs-api.md).
-- Part of the pure-Lua `btv.*` prelude (see runtime.rs); the Lua half over the
-- `btv._fs_*` Rust bridge in install.rs. Loaded AFTER promise.lua — every op builds
-- on `btv.promise`.
--
-- Shape (matches the `btv.run` / `btv.run_stream` split): one-shot ops return a PROMISE
-- of the result; the streaming `watch` (a change async-iterator) is Phase 2 and
-- not here yet. There are NO callbacks. A failed op REJECTS with a
-- `{ code, message }` table — `code` is a libuv/errno-style hint (ENOENT/EACCES/…):
--
--   btv.async(function()
--     local entries = btv.await(btv.fs.readdir(dir))      -- { { name=, type= }, … }
--     local ok, err = pcall(btv.await, btv.fs.remove(p))  -- err.code == "ENOENT" / …
--   end)()
--
-- The bridge runs each op OFF the editor tick: `btv._fs_op` queues a LoopOp::Fs and
-- returns immediately, so the promise stays pending and SETTLES ON A LATER TICK (the
-- event-loop actor runs the op on its blocking pool natively; the daemon luafs leg
-- runs it on the browser). This is non-blocking on native and the only way to reach
-- the daemon on wasm. The promise shape is unchanged — only the timing moved from
-- "resolved-already" to "resolved next tick".

btv.fs = btv.fs or {}

-- Queue an off-tick fs op described by `job` ({ op = "<name>", … }) and return a
-- pending promise. A callback id is registered in `btv._cb_fns`; the server fires
-- `btv._run_cb(id, false, err, value)` when the op settles — `err` the { code, message }
-- table (then `value` is nil) on failure, else `err` nil with the resolved value.
local function run_fs(job)
  return btv.promise.new(function(resolve, reject)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(err, value)
      if err ~= nil then
        reject(err)
      else
        resolve(value)
      end
    end
    btv._fs_op(job, id)
  end)
end

-- ----- metadata --------------------------------------------------------------

-- `btv.fs.stat(path)`  -> promise of { type, size, mtime, atime, mode }  (follows links)
function btv.fs.stat(path)
  return run_fs({ op = "stat", path = path })
end

-- `btv.fs.lstat(path)` -> like stat but does NOT follow a symlink (type may be `"link"`).
function btv.fs.lstat(path)
  return run_fs({ op = "lstat", path = path })
end

-- `btv.fs.exists(path)` -> promise of a boolean. The one op that never rejects: a
-- missing path (or any error) resolves `false`. Existence of the entry itself, so
-- a dangling symlink is `true`. (The `exists` job resolves a bool rather than
-- rejecting, so no reject-to-false mapping is needed in the wrapper.)
function btv.fs.exists(path)
  return run_fs({ op = "exists", path = path })
end

-- ----- listing ---------------------------------------------------------------

-- `btv.fs.readdir(path)` -> promise of { { name=, type=`"file"`|`"directory"`|`"link"` }, … },
-- the entries directly under `path` (no `"."`/`".."`), each with its dirent kind in the
-- SAME call (no per-entry stat). `type` is lstat-flavoured (a symlink reports `"link"`).
function btv.fs.readdir(path)
  return run_fs({ op = "readdir", path = path })
end

-- `btv.fs.walk(dir[, opts])` -> promise of a LIST of file paths relative to `dir`, a
-- recursive directory listing built from readdir. The transport-agnostic file
-- enumeration the codebase reaches for when `rg`/`fd` aren't available (the pure web
-- client, where a spawn has no real shell) — it rides the same off-tick fs seam as
-- every other `btv.fs` op, so it works against local disk, a daemon, and OPFS alike.
-- opts:
--
-- ```
-- max     cap on files returned (default 50000) — a runaway guard on huge trees
-- hidden  include dotfiles / dotdirs (default false)
-- skip    set of directory basenames to prune (default { [".git"] = true })
-- ```
--
-- An unreadable subdirectory is skipped (not fatal). MUST be awaited inside `btv.async`.
function btv.fs.walk(dir, opts)
  opts = opts or {}
  local max = opts.max or 50000
  local hidden = opts.hidden or false
  local skip = opts.skip or { [".git"] = true }
  return btv.async(function()
    local out = {}
    -- Relative subdirs still to visit; "" is `dir` itself.
    local stack = { "" }
    while #stack > 0 and #out < max do
      local rel = table.remove(stack)
      local abs = rel == "" and dir or (dir .. "/" .. rel)
      -- Skip a directory we can't read (permissions / vanished mid-walk) rather
      -- than aborting the whole enumeration.
      local ok, entries = pcall(btv.await, btv.fs.readdir(abs))
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

-- `btv.fs.grep(dir, query[, opts])` -> promise of a LIST of matches, each
-- { path = <rel>, row = <1-based lnum>, col = <1-based>, text = <line> }: a recursive,
-- transport-agnostic plain-substring search. The fallback the grep picker reaches for
-- when `rg`/`grep` aren't available (the pure web client) — it rides the same off-tick
-- fs seam as every other `btv.fs` op, so it works against local disk, a daemon, and OPFS.
-- Walks `dir` (`btv.fs.walk`), reads each file (`btv.fs.read_text`), and matches `query` as a
-- LITERAL substring per line. Binary / unreadable files (read_text rejects) are skipped.
-- `opts` pass through to `btv.fs.walk` (max / hidden / skip). MUST be awaited inside `btv.async`.
function btv.fs.grep(dir, query, opts)
  return btv.async(function()
    local out = {}
    if not query or query == "" then
      return out
    end
    local files = btv.await(btv.fs.walk(dir, opts))
    local base = dir:gsub("/$", "")
    for _, rel in ipairs(files) do
      -- read_text rejects on invalid UTF-8 (a binary file); skip it rather than abort.
      local ok, text = pcall(btv.await, btv.fs.read_text(base .. "/" .. rel))
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

-- `btv.fs.read(path)` -> promise of the file's RAW bytes (a Lua byte-string).
function btv.fs.read(path)
  return run_fs({ op = "read", path = path })
end

-- `btv.fs.read_text(path[, { encoding = "utf-8" }])` -> promise of decoded text.
-- Decodes through the encoding seam and REJECTS (EILSEQ) on invalid input — never
-- lossy replacement text. Use `btv.fs.read` for raw bytes.
function btv.fs.read_text(path, opts)
  return run_fs({ op = "read_text", path = path, encoding = opts and opts.encoding })
end

-- `btv.fs.write(path, data)` -> promise (resolves nil). Truncates / creates.
function btv.fs.write(path, data)
  return run_fs({ op = "write", path = path, data = data })
end

-- `btv.fs.append(path, data)` -> promise (resolves nil). Creates if absent.
function btv.fs.append(path, data)
  return run_fs({ op = "append", path = path, data = data })
end

-- ----- mutation --------------------------------------------------------------

-- `btv.fs.mkdir(path[, { recursive = false, mode = 0o755 }])` -> promise (resolves
-- nil). `mode` is the Unix permission bits applied to every directory created
-- (defaults to 0o755 in Rust when omitted; ignored off Unix) — pass it to keep a
-- private data/state dir from being created world-readable.
function btv.fs.mkdir(path, opts)
  return run_fs({
    op = "mkdir",
    path = path,
    recursive = opts and opts.recursive or false,
    mode = opts and opts.mode or nil,
  })
end

-- `btv.fs.rename(from, to)` -> promise (resolves nil).
function btv.fs.rename(from, to)
  return run_fs({ op = "rename", from = from, to = to })
end

-- `btv.fs.remove(path[, { recursive = false }])` -> promise (resolves nil). A file is
-- unlinked; a directory needs `recursive` unless already empty.
function btv.fs.remove(path, opts)
  return run_fs({ op = "remove", path = path, recursive = opts and opts.recursive or false })
end

-- `btv.fs.copy(src, dst[, { recursive = false }])` -> promise (resolves nil). A file
-- copies (overwriting); a directory needs `recursive`.
function btv.fs.copy(src, dst, opts)
  return run_fs({ op = "copy", src = src, dst = dst, recursive = opts and opts.recursive or false })
end

-- `btv.fs.realpath(path)` -> promise of the canonical absolute path (symlinks resolved).
function btv.fs.realpath(path)
  return run_fs({ op = "realpath", path = path })
end

-- `btv.fs.which(name)` -> promise of the absolute path of the executable `name`, or
-- **nil** when nothing matches. A bare name is searched across `$PATH`; a `name` that
-- already contains a `/` is taken as an explicit path and accepted only when it *is*
-- an executable file. The async, transport-agnostic replacement for vim's blocking
-- `executable()` / `exepath()` — a build/tool lookup is I/O, so it rides the same
-- off-tick `btv.fs` seam as every other op and works unchanged against local disk, a
-- daemon, and a browser session.
--
-- It resolves `nil` rather than rejecting when the program is absent: "not installed"
-- is a true answer, and only a transport failure is an error. That makes the two
-- common shapes read naturally —
--
-- ```lua
-- -- prefer a project-local binary, fall back to the one on $PATH
-- local local_bin = btv.utils.joinpath(root, "node_modules/.bin", "eslint")
-- local cmd = btv.await(btv.fs.which(local_bin)) or "eslint"
--
-- -- gate a feature on a tool being present
-- btv.fs.which("rg"):next(function(path)
--   if path then use_ripgrep(path) end
-- end)
-- ```
--
-- In a **serverless** browser session there are no executables at all, so every
-- lookup resolves nil; with a daemon attached the daemon's own `$PATH` answers — which
-- is the right one, since that is where the language servers and tools actually run.
function btv.fs.which(name)
  return run_fs({ op = "which", name = name })
end

btv.hash = btv.hash or {}

-- `btv.hash.file(path[, algo])` -> promise of the file's lowercase-hex digest. The
-- streaming member of the `btv.hash.*` family (the in-memory string hashers and the
-- incremental `btv.hash.new` live in hash.lua): hashing a file is I/O, so it routes
-- through the fs machinery rather than reading the file into Lua first. The server
-- streams the file in fixed 64 KiB chunks and folds each into the hasher, so a 300 MB
-- file costs 64 KiB of memory — not 300 MB, as `btv.hash.sha256(btv.await(btv.fs.read(path)))`
-- would. On a remote/browser build the hashing runs entirely on the daemon; only the
-- short digest crosses the wire, never the file's bytes. `algo` is one of `"sha1"` /
-- `"sha256"` / `"sha512"` / `"md5"` (default `"sha256"`); an unknown algorithm rejects (EINVAL).
function btv.hash.file(path, algo)
  return run_fs({ op = "hash_file", path = path, algo = algo or "sha256" })
end

-- ----- watch (continuous → async-iterator) -----------------------------------
--
-- `btv.fs.watch(path[, { recursive = false }])` -> a Watch you iterate with
-- `btv.await_each` (the streaming sibling of the one-shot ops, same shape as
-- `btv.run_stream`). Each step yields a COALESCED change batch:
--
--   { kind = "create"|"modify"|"remove"|"rename", paths = { <abspath>, … } }
--
-- bursts within a 10 ms window are merged in the server, **one batch per kind**
-- (paths deduped within each). A burst that mixes kinds therefore yields one batch
-- each rather than a single coarsened one: writing a new file is a `"create"` plus a
-- `"modify"`, and folding them together would lose the only signal that says the file
-- is new. Want a longer settle? Wrap `btv.utils.debounce` on top.
--
--   btv.async(function()
--     local w = btv.fs.watch(dir, { recursive = true })
--     for ev in btv.await_each(w) do redraw(ev.paths) end   -- until w:stop()
--   end)()
--
-- The watch is armed where the session's files are: locally in a local session, and on
-- the **daemon** in a daemon session (native or browser) — so a remote project reports
-- its own changes rather than this machine's. A native daemon link re-arms its watches
-- after a reconnect, so a live iterator survives an outage.
--
-- A watch that can't arm (bad path, watch limit) or a session with no change source at
-- all (serverless browser) REJECTS the first pull — fail loud, never a dead watch.
-- `:stop()` cancels the watch and ends the iteration.

-- Persistent per-watch pumps, keyed by stream id (like `btv._stdout_fns` for streams).
btv._fs_watch_fns = btv._fs_watch_fns or {}

-- The server fires this on each coalesced change (`ev`, nil) or terminal error
-- (nil, `err`). Routes to the registered pump; a no-op once the stream is stopped.
function btv._run_fs_watch(id, ev, err)
  local fn = btv._fs_watch_fns[id]
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
  return btv.promise.new(function(resolve, reject)
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
  btv._fs_watch_fns[self._id] = nil
  btv._fs_unwatch(self._id)
  local waiter = self._waiter
  self._waiter = nil
  if waiter then
    waiter.resolve(nil)
  end
end

function btv.fs.watch(path, opts)
  local self = setmetatable({ _queue = {}, _done = false, _waiter = nil }, Watch)
  local id = btv._next_cb_id()
  self._id = id
  btv._fs_watch_fns[id] = function(ev, err)
    local waiter = self._waiter
    if err ~= nil then
      -- Terminal: surface the error to the consumer and tear the watch down.
      self._err = err
      self._done = true
      btv._fs_watch_fns[id] = nil
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
  btv._fs_watch(id, path, opts and opts.recursive or false)
  return self
end
