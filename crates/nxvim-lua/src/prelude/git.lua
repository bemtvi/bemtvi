-- nx.git — a promise-always git API (docs/plans/2026-07-24-native-git-gix.md).
-- Part of the pure-Lua `nx.*` prelude (see runtime.rs); the Lua half over the
-- `nx._git_op` Rust bridge in install.rs, backed natively by the `gix` engine in the
-- `nxvim-git` crate. Loaded AFTER promise.lua — every op builds on `nx.promise`.
--
-- Shape mirrors `nx.fs`: one-shot ops return a PROMISE of the result; there are NO
-- callbacks. A failed op REJECTS with a `{ code, message }` table — `code` a short
-- stable hint (`ENOREPO` / `ENOENT` / `EGIT`):
--
--   nx.async(function()
--     local h = nx.await(nx.git.head(nx.buf.name(0)))         -- { branch, detached, sha }
--     local ok, err = pcall(nx.await, nx.git.discover(dir))   -- err.code == "ENOREPO" / …
--   end)()
--
-- The bridge runs each op OFF the editor tick (`nx._git_op` queues a LoopOp::Git and
-- returns immediately, so the promise stays pending and SETTLES ON A LATER TICK): the
-- event-loop actor runs it on its blocking pool natively, the daemon `git_op` leg runs
-- it daemon-side (a daemon / web session — git runs where the files are). A serverless
-- web session with no daemon REJECTS loud (there is no in-browser git engine).
--
-- `nx.git_local.*` is the LOCAL-always twin (the git sibling of `nx.fs_local` /
-- `nx.run_local`): its ops act on the client machine's disk even in a daemon session.
-- The plugin manager (`nx.plugins`) uses it — its repos live on the local disk that
-- plugins load from.

nx.git = nx.git or {}
nx.git_local = nx.git_local or {}

-- Queue an off-tick git op described by `job` ({ op = "<name>", … }) via `bridge`
-- (`nx._git_op` or `nx._local_git_op`) and return a pending promise. A callback id is
-- registered in `nx._cb_fns`; the server fires `nx._run_cb(id, false, err, value)` when
-- the op settles — `err` the { code, message } table (then `value` nil) on failure,
-- else `err` nil with the resolved value.
local function run_git(bridge, job)
  return nx.promise.new(function(resolve, reject)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(err, value)
      if err ~= nil then
        reject(err)
      else
        resolve(value)
      end
    end
    bridge(job, id)
  end)
end

-- Build the two parallel surfaces (`nx.git` over `nx._git_op`, `nx.git_local` over
-- `nx._local_git_op`) from one verb table, so the twins never drift.
local function define(surface, bridge)
  -- `discover(path)` -> promise of { root, git_dir, prefix }. `path` is any path
  -- inside the repo. Replaces `rev-parse --show-toplevel / --absolute-git-dir /
  -- --show-prefix`. Rejects (`ENOREPO`) when `path` is not inside a repository.
  function surface.discover(path)
    return run_git(bridge, { op = "discover", path = path })
  end

  -- `head(path)` -> promise of { branch, detached, sha }. `branch` is nil on a
  -- detached HEAD (`detached` then true); `sha` is the full resolved commit oid (empty
  -- on an unborn HEAD). Replaces `rev-parse --abbrev-ref HEAD`.
  function surface.head(path)
    return run_git(bridge, { op = "head", path = path })
  end

  -- `show(file, rev)` -> promise of the RAW bytes of `file` (an absolute path) as it
  -- was at `rev` (e.g. `"HEAD"`). Replaces `git show HEAD:<rel>`, but you pass the
  -- plain file path — the repo-relative path is computed for you (symlink-safe).
  -- Rejects (`ENOENT`) when the file has no version at `rev` (new / untracked file).
  function surface.show(file, rev)
    return run_git(bridge, { op = "show", file = file, rev = rev or "HEAD" })
  end

  -- `diff_file(path, file)` -> promise of { added, changed, removed, hunks } for
  -- `file`'s working tree vs its HEAD blob (each hunk `{ old_start, old_count,
  -- new_start, new_count }`). Replaces `git diff -U0 -- <file>` plus hunk parsing.
  -- `path` locates the repo (any path inside it); `file` is the file to diff.
  function surface.diff_file(path, file)
    return run_git(bridge, { op = "diff_file", path = path, file = file })
  end

  -- `status(path)` -> promise of { dirty, entries }, each entry `{ path, index,
  -- worktree }` in `git status --porcelain` XY terms (`index` the staged column,
  -- `worktree` the unstaged column; each a single letter, `" "` when unmodified).
  function surface.status(path)
    return run_git(bridge, { op = "status", path = path })
  end

  -- ----- mutation / network verbs (plugin-manager backing) -----

  -- `clone(url, dir, opts)` -> promise of the created `dir`. `opts.depth` makes it a
  -- shallow clone (`1` = only the tip commit); `opts.branch` checks out a named branch
  -- or tag instead of the remote default. Replaces `git clone`. (git's
  -- `--filter=blob:none` has no `gix` analog — a shallow `depth` supplies the same
  -- speed-up.) Rejects (`EGIT`) on any clone/fetch failure.
  function surface.clone(url, dir, opts)
    opts = opts or {}
    return run_git(bridge, {
      op = "clone",
      url = url,
      dir = dir,
      depth = opts.depth,
      branch = opts.branch,
    })
  end

  -- `checkout(dir, rev, opts)` -> promise resolving nil. Checks out `rev` (a sha, tag,
  -- or ref) in the repo at `dir`, updating the worktree. `opts.detach` (the supported
  -- mode) detaches HEAD onto the commit. Replaces `git checkout --detach <sha>`.
  function surface.checkout(dir, rev, opts)
    opts = opts or {}
    return run_git(bridge, { op = "checkout", dir = dir, rev = rev, detach = opts.detach })
  end

  -- `pull(dir)` -> promise of { updated, sha }. Fetches the repo's remote and
  -- **fast-forwards only** the current branch (rejecting `ENOTFF` on a divergence,
  -- never merging). `updated` is false when already current. Replaces `git pull
  -- --ff-only`.
  function surface.pull(dir)
    return run_git(bridge, { op = "pull", dir = dir })
  end

  -- `submodule_update(dir, opts)` -> promise resolving nil. Clones-if-missing
  -- (`opts.init`) and checks out every submodule to its recorded commit,
  -- `opts.recursive`-ly into nested ones. Replaces `git submodule update --init
  -- --recursive`.
  function surface.submodule_update(dir, opts)
    opts = opts or {}
    return run_git(
      bridge,
      { op = "submodule_update", dir = dir, init = opts.init, recursive = opts.recursive }
    )
  end
end

define(nx.git, nx._git_op)
define(nx.git_local, nx._local_git_op)
