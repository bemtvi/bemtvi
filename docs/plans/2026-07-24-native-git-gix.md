# Native git via `gix` — a first-party `btv.git` API, no `git` binary

Status: **done** (Phase 1 + Phase 2 shipped). Author-date: 2026-07-24.

**Phase 2 landed (mutation/network verbs + plugin port).** `btv.git_local.clone /
checkout / pull / submodule_update` are implemented in `bemtvi-git` over gix's fetch /
worktree-state / reference primitives (gix has no one-call clone-`--filter`, submodule
update, or `reset --hard`, so those are hand-rolled — see each `fn` in
`crates/bemtvi-git/src/lib.rs`). `--filter=blob:none` has no gix analog; shallow `depth`
substitutes. The plugin manager (`prelude/plugins.lua`) is ported entirely onto
`btv.git_local.*` — `GIT_ENV`, the `git` executable opt, and the `btv.run_local` git
wrapper are gone; **no `git` process anywhere in bemtvi**. Tests: `git.rs` +8 including
three DIFFERENTIAL oracle tests (hand-rolled checkout / pull / submodule produce
byte-identical worktrees to real `git`); `daemon_git.rs` +1 (clone over the wire);
`plugins.rs` argv-inspection tests rewritten as behavior tests. Commits: `870e2c5a`
(engine), `07fa8113` (plugin port).

## Goal

bemtvi shells out to the `git` **binary** in three places, all Lua-side (`btv.run` /
`btv.run_local` — there is no Rust git shell-out):

- **Plugin manager** (`crates/bemtvi-lua/src/prelude/plugins.lua`) — `clone
  --filter=blob:none`, `checkout --detach <sha>`, `pull --ff-only`, `submodule
  update --init --recursive`, run via the **local** twin (`btv.run_local`).
- **bemtvi-line** (`~/work/nxvim-plugins/nxvim-line/lua/bemtvi-line/git.lua`) —
  `rev-parse --abbrev-ref HEAD` (branch), `diff -U0 -- <file>` (hunk +/~/-
  counts), `rev-parse --absolute-git-dir` (for the `.git` watch).
- **bemtvi-diff** (`~/work/nxvim-plugins/nxvim-diff/lua/bemtvi-diff/git.lua`) —
  `rev-parse --show-prefix` (repo-relative path), `show HEAD:<rel>` (blob@HEAD).

Replace **all** of it with a native, first-party async Lua API `btv.git.*` (plus a
`btv.git_local.*` twin), backed by the **`gix`** crate. **No `git` binary** anywhere
when we're done — the chosen scope is a full gix-only replacement including
clone/fetch/checkout/pull/submodule.

This dogfoods the same principle as `btv.fs` / `btv.run` / `btv.http.fetch`: a
promise-always API that runs **off the editor tick**, works identically over a
daemon, and exposes the engine to every plugin instead of baking behavior into
Rust.

## Design — model exactly on `btv.fs`

The `btv.fs` seam is the template (traced end-to-end; see
`docs/plans/2026-06-16-btv-fs-api.md` and `-off-tick-daemon-leg.md`). One typed job,
one executor, one `rmpv` wire codec, three call sites (native `spawn_blocking`,
daemon `serve_*_op`, wasm daemon leg) all funneling into the one executor, with a
`cb_id` threaded `btv._next_cb_id()` → `LoopOp.id` → `LoopEvent.id` →
`run_callback(id)` → `btv._run_cb(id)` → the promise closure.

Key structural difference from fs: **gix cannot compile to wasm and there is no
real filesystem in a serverless browser session.** So:

- **Data types + wire codec are wasm-safe** and live in `bemtvi-lua` (no gix): a new
  `GitJob` / `GitValue` / `GitError` in `ops.rs`, and a `gitwire.rs` codec over
  `rmpv::Value`. These compile everywhere.
- **The gix-backed executor lives in a new native-only crate `bemtvi-git`**
  (`run_git_job(repo_ctx, &GitJob) -> Result<GitValue, GitError>`), a peer of the
  other native engines (`bemtvi-ts`, `bemtvi-lsp`, `bemtvi-regex`). `bemtvi-server`
  depends on it **behind the `native` feature** — it is never in the wasm subset.
- **Topologies.** Native-bare: run the executor on the blocking pool. Native-daemon
  and **daemon-web**: ship the `GitJob` over a new `git_op` leg; it runs
  **daemon-side** against a real fs (this is how "remote git" works — git runs where
  the files are). **Serverless-web** (OPFS only, no host): **reject loud** — git has
  no OPFS analog and gix can't run in the sandbox. This matches existing precedent
  (`btv.fs.watch` and plugin-sync git already fail loud serverless). "gix-only" means
  *no `git` binary*, not *gix-in-the-browser*.

The **`btv.git_local` twin** reuses the fs `local: true` mechanism: a `local`-flagged
`GitJob` always uses the **local** git backend even in a daemon session (the plugin
manager operates on the client's on-disk plugin repos, which load into the local
Lua VM — exactly why `btv.run_local` exists today).

## The `btv.git.*` surface

Read verbs (Phase 1) — each returns a promise:

```
btv.git.discover(path)        -> { root=<toplevel abs>, git_dir=<abs>, prefix=<repo-rel dir> } | rejects if not a repo
btv.git.head(path)            -> { branch=<name|nil>, detached=<bool>, sha=<full oid> }
btv.git.show(path, rev, rel)  -> <blob bytes at `rev`:`rel`>   (rel is repo-relative; backs `git show HEAD:rel`)
btv.git.diff_file(path, file) -> { added=<n>, changed=<n>, removed=<n>, hunks={ {old_start,old_count,new_start,new_count}, … } }
btv.git.status(path)          -> { entries={ { path=, index=<X>, worktree=<Y>, orig_path= }, … }, dirty=<bool> }   (ONE entry per path)
```

`path` is any path *inside* the repo (a file or dir); the executor discovers the
repo from it. `btv.git.discover` replaces `rev-parse --show-toplevel /
--absolute-git-dir / --show-prefix`; `head` replaces `rev-parse --abbrev-ref HEAD`;
`show` replaces `git show HEAD:<rel>`; `diff_file` replaces `git diff -U0 -- <file>`
+ the plugin's `_parse_diff`. `status` is new (gives the two plugins and future
ones a canonical working-tree signal instead of re-deriving one).

`status` returns exactly ONE entry per path, both porcelain columns filled — gix
reports a path's staged (`TreeIndex`) and unstaged (`IndexWorktree`) halves as
separate items, and folding them is the engine's job, not each consumer's (an
unfolded read silently reports "staged, clean worktree" for a file with unstaged
edits). Untracked is porcelain's `??` in both columns. Worktree rename detection is
enabled, so an unstaged rename reads `R` on its destination with `orig_path` set —
more than git's own porcelain reports (it prints a deletion plus an untracked file).

Mutation / network verbs (Phase 2) — plugin-manager backing, promise-always:

```
btv.git_local.clone(url, dir, { filter="blob:none", depth= })  -> resolves dir
btv.git_local.checkout(dir, rev, { detach=true })              -> resolves nil
btv.git_local.pull(dir, { ff_only=true })                      -> resolves { updated=<bool>, sha= }
btv.git_local.submodule_update(dir, { init=true, recursive=true }) -> resolves nil
```

All reject loud (`{ code, message }`) on failure — never a silent no-op.

## Touchpoints (the fs reference in parens)

1. **Typed job/value/error** — `crates/bemtvi-lua/src/ops.rs`: `GitJob`, `GitValue`,
   `GitError`, and `LoopOp::Git { id, job, local }` (`FsJob`/`FsValue`/`FsError`,
   `LoopOp::Fs`). Plain data, no gix, wasm-safe.
2. **Executor** — new crate `crates/bemtvi-git`: gix wrapper + `run_git_job(&GitJob)
   -> Result<GitValue, GitError>` (`luafs.rs::run_fs_job`). A `GitError` shaper maps
   gix errors to `{ code, message }`. Native-only.
3. **Wire codec** — `crates/bemtvi-lua/src/gitwire.rs`: `git_job_to_value` /
   `_from_value`, `git_result_to_value` / `_from_value` over `rmpv::Value`; leg
   constant `"git_op"` (`fswire.rs`, `LUAFS_OP`).
4. **Lua prelude** — `crates/bemtvi-lua/src/prelude/git.lua`: `run_git(job)` mirroring
   `run_fs` (fs.lua:29), one wrapper per verb, `btv.git` + `btv.git_local` tables;
   register in `PRELUDE_MODULES` after `promise` (`runtime.rs:486`).
5. **Rust bridges** — `install.rs`: `btv._git_op(job, cb_id)` + `btv._local_git_op`
   pushing `LoopOp::Git` (fs at install.rs:1669/1687) + `git_job_from_table` parser
   (fail loud on unknown verb, install.rs:3933).
6. **Result marshalling** — `runtime.rs`: `CallbackArgs::GitResult` arm in
   `run_callback` (:2357) + `git_value_to_lua` (`fs_value_to_lua` :1122).
7. **Native actor** — `evloop.rs`: `GitBackend { Local(Arc<..>), Remote(RemoteGitJobs) }`
   (`FsBackend` :288), `LoopCommand::Git` + `LoopEvent::GitResult`, `spawn_blocking`
   handler (:610). Backend selection `lib.rs:3225`; `ServerInit.git_jobs:
   Option<RemoteGitJobs>` (:354).
8. **Effects dispatch** — `effects.rs`: native `LoopOp::Git` arm (:2529) + wasm arm
   (:2775), cfg-gated; native `LoopEvent::GitResult` landing (:2954).
9. **Daemon leg** — `daemon.rs`: `RemoteGitJobs` + `run_git_jobs` client job-server +
   `serve_git_op` + `serve_git_daemon_on` (`git_op` method), Control-leg group +
   reconnect (RemoteFsJobs :3142/3193/3317/3358); `DaemonClient.git_jobs`
   (:536/3746) wired from `main.rs:1118`.
10. **Wasm seam** — `edithost.rs` `HostEffects::git_op` (:196) + `has_remote_git`
    gate; `bemtvi-edithost/src/lib.rs` `WasmEffects` impl → `Sink.git_ops`, FFI
    `eh_take_git_op_requests` / `eh_git_op_result` → `EditHost::git_op_result`
    (:2183). **Serverless (no daemon): reject loud**; daemon-web: reuse the `git_op`
    leg over WebTransport (runs daemon-side).

## Dependency

Add `gix` (exact-pin `=0.86.x` in root `[workspace.dependencies]`) with a curated
feature set (`blocking-network-client` + an https transport for clone/fetch,
`status`, `dirwalk`, `blob-diff`, `revision`, `worktree-mutation` for checkout;
`default-features = false` to keep it out of any non-native path). Pulled into
`bemtvi-git` with `gix.workspace = true`; `bemtvi-git` itself is an **optional** dep of
`bemtvi-server` gated by `native`.

## Phasing (commit + pause for review between phases)

- **Phase 1 — read API + plugin ports.** Touchpoints 1–10 wired for the read verbs
  (`discover`/`head`/`show`/`diff_file`/`status`), native + daemon + `btv.git_local`
  twin + serverless-reject. Port bemtvi-line and bemtvi-diff off `btv.run "git"` onto
  `btv.git.*`. Sliced: (1a) crate+types+executor+codec; (1b) Lua prelude+bridges+
  marshalling; (1c) native actor; (1d) daemon leg; (1e) wasm seam; (1f) plugin ports
  + tests. Verify **both** builds (native `--test daemon_*`, and `--no-default-features`
  + the `verify-*.mjs` browser checks).
- **Phase 2 — plugin-manager mutation ops.** Add the mutation `GitJob`s
  (clone/fetch/checkout/pull/submodule) to `run_git_job` via gix, port `plugins.lua`
  onto `btv.git_local.*`. **Risk:** gix's recursive submodule update is not a one-call
  op and ff-pull needs hand-rolled ref-advance logic — these get the most test
  scrutiny (mutation-test each against a real temp repo). Delete the `git` executable
  config from `plugins.lua` and drop `GIT_ENV`.

## Testing

Black-box through the harness (no unit tests). Build a real temp git repo in the
test (commit a file, branch, stage a change) and drive `btv.git.*` via `exec_lua`,
asserting on real results — mutation-test each (break the code, watch it fail).
`bemtvi-line`/`bemtvi-diff` keep their own plugin test suites (`--test-plugin`);
update those to the native API. New server tests cover the daemon leg
(`daemon_git`) and the serverless-reject path. `examples/` throwaway check for any
config-facing surface, per repo convention.

## Non-goals / boundaries

- Serverless-web (OPFS-only) git is **out** — rejects loud. Daemon-web is in.
- Not reimplementing full `git` CLI surface; just the verbs the tree actually needs,
  extensible via `btv.git.*` (the API is the extension point, not command flags).
