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

  -- `status(path, opts)` -> promise of `{ dirty, entries }` in `git status --porcelain`
  -- XY terms. Each entry is `{ path, index, worktree, orig_path }`:
  --
  -- ```
  -- path       repo-relative path of the changed file
  -- index      the staged (index-vs-HEAD) column, one letter
  -- worktree   the unstaged (worktree-vs-index) column, one letter
  -- orig_path  where the content came from, for a rename/copy; "" otherwise
  -- ```
  --
  -- A column is `" "` when the file is unmodified in it. There is exactly ONE entry
  -- per path: a file that is staged and then edited again arrives once with both
  -- columns set (porcelain's `MM`), never as two entries carrying one column each.
  -- An untracked file is porcelain's `??` — both columns.
  --
  -- Unlike `git status`, an unstaged rename IS detected: it reads `R` on the
  -- destination with `orig_path` set, where git prints a deletion plus an untracked
  -- file.
  --
  -- `opts.ignored` (default `false`) additionally reports git-**ignored** paths as
  -- porcelain's `!!` — both columns — like `git status --porcelain --ignored`. It is
  -- opt-in because the default walk PRUNES ignored directories, which is what keeps a
  -- status over a repo with a large `target/` fast; asked for, the walk must descend
  -- into them. A wholly-ignored directory collapses to ONE entry naming the directory
  -- (`target`, not its 50k files), exactly like a collapsed untracked directory — so a
  -- consumer resolves directory-ness from its own model and matches descendants by path
  -- prefix:
  --
  -- ```lua
  -- local st = nx.await(nx.git.status(root, { ignored = true }))
  -- for _, e in ipairs(st.entries) do
  --   if e.index == "!" then ignored[e.path] = true end   -- a file OR a directory
  -- end
  -- ```
  function surface.status(path, opts)
    opts = opts or {}
    return run_git(bridge, { op = "status", path = path, ignored = opts.ignored == true })
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

  -- `checkout(dir, rev, opts)` -> promise resolving nil. Checks out `rev` in the repo at
  -- `dir`, updating the worktree to match. Two modes:
  --
  -- ```lua
  -- nx.git.checkout(dir, sha, { detach = true })  -- git checkout --detach <sha>
  -- nx.git.checkout(dir, "main")                  -- git checkout main
  -- ```
  --
  -- With `opts.detach` the `rev` is any revision (a sha, tag, or ref) and HEAD is left
  -- pointing straight at that commit — how an exact pin is applied. Without it, `rev`
  -- names a BRANCH and HEAD stays symbolic on it; a branch that exists only as a
  -- remote-tracking ref (the usual case for anything but the default branch of a fresh
  -- clone) is created locally from it first, as `git checkout <branch>` does.
  --
  -- Attaching is what makes a detached checkout movable again: `pull` fast-forwards the
  -- current *branch*, so it rejects outright while HEAD names a bare commit. Rejects
  -- (`ENOENT`) when an attach target is neither a local nor a remote-tracking branch.
  function surface.checkout(dir, rev, opts)
    opts = opts or {}
    return run_git(bridge, { op = "checkout", dir = dir, rev = rev, detach = opts.detach })
  end

  -- `fetch(dir, opts)` -> promise resolving nil. Fetches the repo's remote, updating the
  -- remote-tracking refs and leaving HEAD and the worktree alone — the half of `pull`
  -- that touches no working state. Replaces `git fetch`.
  --
  -- `opts.unshallow` drops a shallow clone's boundary (`git fetch --unshallow`), so
  -- history a `depth = 1` clone omitted becomes reachable. That is the prerequisite for
  -- checking out an arbitrary older revision *in place* — without it, a shallow clone
  -- simply does not contain the commit and the checkout rejects. A no-op on a repo that
  -- was never shallow.
  function surface.fetch(dir, opts)
    opts = opts or {}
    return run_git(bridge, { op = "fetch", dir = dir, unshallow = opts.unshallow })
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
