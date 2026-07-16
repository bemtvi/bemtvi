-- nxvim Lua prelude — the **local-always** seam: `nx.run_local` / `nx.fs_local`, the twins
-- of `nx.run` / `nx.fs` that always act on THIS machine (the client) even when the session's
-- `nx.run` / `nx.fs` route to a `--daemon`. Together with the pre-existing
-- `nx.http.fetch_local` (prelude/http.lua) they are the surface a plugin uses for work that
-- must happen locally regardless of session routing:
--
--   * the **plugin manager** (`nx.plugins`) — clones/discovers/sources on the local disk,
--     since a plugin loads into THIS Lua VM via the local runtimepath, never the daemon;
--   * a **remote connector** (§E of the remote-connectors plan) — provisions the remote
--     (`docker exec … uname`, `scp`, an ssh control-master) from the local machine, since it
--     is what DIALS the daemon.
--
-- Runtime plugin code that edits the remote's files still uses the session-routed
-- `nx.run` / `nx.fs` — the local seam is only for client-side machinery. Same spec shapes and
-- promise contracts as `nx.run` / `nx.fs`; only the routing differs (the `nx._local_*`
-- bridges push a LOCAL loop op). Loads after promise.lua (every op returns a promise) and
-- before plugins.lua (which builds on it). See docs/plans/2026-07-03-remote-aware-plugin-manager.md
-- and docs/plans/2026-07-05-remote-connectors-and-system-plugins.md → §E.

local vim = vim
nx = nx or {}

-- Build an argv list from `{ cmd = string|list, args = list }` — `cmd` is a string or an
-- argv list, `args` is appended. Mirrors `nx.run`'s builder (kept in step with prelude/process.lua).
local function build_argv(spec)
  local cmd = spec.cmd
  if type(cmd) == "string" then
    cmd = { cmd }
  end
  local argv = {}
  for _, c in ipairs(cmd) do
    argv[#argv + 1] = c
  end
  for _, a in ipairs(spec.args or {}) do
    argv[#argv + 1] = a
  end
  return argv
end

-- `nx.run_local { cmd, args, cwd, env, stdin }` -> promise of `{ code, stdout, stderr }`.
-- The local-always twin of `nx.run`: runs a one-shot child to completion on THIS machine,
-- even when the session's processes route to a daemon. Same spec + resolution as `nx.run`
-- (RESOLVES, never rejects; a spawn failure is `code = -1`). Reach for it for client-side
-- work — provisioning a remote, an ssh control-master, a `docker cp` — that must not run on
-- the daemon. In a bare/local session it is exactly `nx.run`.
function nx.run_local(spec)
  if type(spec) ~= "table" then
    error("nx.run_local: expected a table { cmd, args, ... }, got " .. type(spec), 2)
  end
  return nx.promise.new(function(resolve)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(result)
      resolve({
        code = result.code,
        stdout = result.stdout or "",
        stderr = result.stderr or "",
      })
    end
    nx._local_system_async(id, build_argv(spec), spec.cwd, spec.env, spec.stdin)
  end)
end

-- `nx.fs_local` — the local-always twin of `nx.fs`: the subset of filesystem ops that a
-- client-side plugin needs, forced onto THIS machine's disk. Same op shapes + promise
-- contract as `nx.fs.*` (see prelude/fs.lua); only the routing differs. In a bare/local
-- session it is exactly `nx.fs`.
local function local_fs_op(job)
  return nx.promise.new(function(resolve, reject)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(err, value)
      if err ~= nil then
        reject(err)
      else
        resolve(value)
      end
    end
    nx._local_fs_op(job, id)
  end)
end

nx.fs_local = {
  -- `nx.fs_local.exists(path)` -> promise of boolean.
  exists = function(path)
    return local_fs_op({ op = "exists", path = path })
  end,
  -- `nx.fs_local.readdir(path)` -> promise of `{ { name, type }, … }`.
  readdir = function(path)
    return local_fs_op({ op = "readdir", path = path })
  end,
  -- `nx.fs_local.read_text(path)` -> promise of the file's contents as a string.
  read_text = function(path)
    return local_fs_op({ op = "read_text", path = path })
  end,
  -- `nx.fs_local.write(path, data)` -> promise; writes `data`, replacing the file.
  write = function(path, data)
    return local_fs_op({ op = "write", path = path, data = data })
  end,
  -- `nx.fs_local.append(path, data)` -> promise; appends `data` to the file.
  append = function(path, data)
    return local_fs_op({ op = "append", path = path, data = data })
  end,
  -- `nx.fs_local.mkdir(path[, { recursive }])` -> promise; creates the directory.
  mkdir = function(path, opts)
    return local_fs_op({ op = "mkdir", path = path, recursive = opts and opts.recursive or false })
  end,
  -- `nx.fs_local.remove(path[, { recursive }])` -> promise; removes the file/dir.
  remove = function(path, opts)
    return local_fs_op({ op = "remove", path = path, recursive = opts and opts.recursive or false })
  end,
}

return nx
