-- bemtvi Lua prelude — the **local-always** seam: `btv.run_local` / `btv.fs_local`, the twins
-- of `btv.run` / `btv.fs` that always act on THIS machine (the client) even when the session's
-- `btv.run` / `btv.fs` route to a `--daemon`. Together with the pre-existing
-- `btv.http.fetch_local` (prelude/http.lua) they are the surface a plugin uses for work that
-- must happen locally regardless of session routing:
--
--   * the **plugin manager** (`btv.plugins`) — clones/discovers/sources on the local disk,
--     since a plugin loads into THIS Lua VM via the local runtimepath, never the daemon;
--   * a **remote connector** (§E of the remote-connectors plan) — provisions the remote
--     (`docker exec … uname`, `scp`, an ssh control-master) from the local machine, since it
--     is what DIALS the daemon.
--
-- Runtime plugin code that edits the remote's files still uses the session-routed
-- `btv.run` / `btv.fs` — the local seam is only for client-side machinery. Same spec shapes and
-- promise contracts as `btv.run` / `btv.fs`; only the routing differs (the `btv._local_*`
-- bridges push a LOCAL loop op). Loads after promise.lua (every op returns a promise) and
-- before plugins.lua (which builds on it). See docs/plans/2026-07-03-remote-aware-plugin-manager.md
-- and docs/plans/2026-07-05-remote-connectors-and-system-plugins.md → §E.

local vim = vim
btv = btv or {}

-- Argv normalization is the same shared `btv.utils.argv` `btv.run` uses
-- (prelude/utils.lua, loaded above) — the local twin takes identical specs.
local build_argv = btv.utils.argv

-- `btv.run_local { cmd, args, cwd, env, stdin }` -> promise of `{ code, stdout, stderr }`.
-- The local-always twin of `btv.run`: runs a one-shot child to completion on THIS machine,
-- even when the session's processes route to a daemon. Same spec + resolution as `btv.run`
-- (RESOLVES, never rejects; a spawn failure is `code = -1`). Reach for it for client-side
-- work — provisioning a remote, an ssh control-master, a `docker cp` — that must not run on
-- the daemon. In a bare/local session it is exactly `btv.run`.
function btv.run_local(spec)
  if type(spec) ~= "table" then
    error("btv.run_local: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  return btv.promise.new(function(resolve)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(result)
      resolve({
        code = result.code,
        stdout = result.stdout or "",
        stderr = result.stderr or "",
      })
    end
    btv._local_system_async(id, build_argv(spec), spec.cwd, spec.env, spec.stdin)
  end)
end

-- `btv.fs_local` — the local-always twin of `btv.fs`: the subset of filesystem ops that a
-- client-side plugin needs, forced onto THIS machine's disk. Same op shapes + promise
-- contract as `btv.fs.*` (see prelude/fs.lua); only the routing differs. In a bare/local
-- session it is exactly `btv.fs`.
local function local_fs_op(job)
  return btv.promise.new(function(resolve, reject)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(err, value)
      if err ~= nil then
        reject(err)
      else
        resolve(value)
      end
    end
    btv._local_fs_op(job, id)
  end)
end

btv.fs_local = {
  -- `btv.fs_local.exists(path)` -> promise of boolean.
  exists = function(path)
    return local_fs_op({ op = "exists", path = path })
  end,
  -- `btv.fs_local.readdir(path)` -> promise of `{ { name, type }, … }`.
  readdir = function(path)
    return local_fs_op({ op = "readdir", path = path })
  end,
  -- `btv.fs_local.read_text(path[, { encoding = "utf-8" }])` -> promise of the decoded
  -- text. `encoding` selects the decoder exactly as in `btv.fs.read_text` (which
  -- REJECTS on invalid input rather than substituting replacement characters).
  read_text = function(path, opts)
    return local_fs_op({ op = "read_text", path = path, encoding = opts and opts.encoding })
  end,
  -- `btv.fs_local.write(path, data)` -> promise; writes `data`, replacing the file.
  write = function(path, data)
    return local_fs_op({ op = "write", path = path, data = data })
  end,
  -- `btv.fs_local.append(path, data)` -> promise; appends `data` to the file.
  append = function(path, data)
    return local_fs_op({ op = "append", path = path, data = data })
  end,
  -- `btv.fs_local.mkdir(path[, { recursive, mode }])` -> promise; creates the directory.
  -- `mode` is the Unix permission bits, exactly as in `btv.fs.mkdir` — pass it to keep a
  -- private local store (a trust/credential dir) from being created world-readable.
  mkdir = function(path, opts)
    return local_fs_op({
      op = "mkdir",
      path = path,
      recursive = opts and opts.recursive or false,
      mode = opts and opts.mode or nil,
    })
  end,
  -- `btv.fs_local.rename(from, to)` -> promise (resolves nil). Atomic on a POSIX
  -- filesystem, so a write-temp-then-rename gives a torn-read-free local update.
  rename = function(from, to)
    return local_fs_op({ op = "rename", from = from, to = to })
  end,
  -- `btv.fs_local.remove(path[, { recursive }])` -> promise; removes the file/dir.
  remove = function(path, opts)
    return local_fs_op({ op = "remove", path = path, recursive = opts and opts.recursive or false })
  end,
}

return btv
