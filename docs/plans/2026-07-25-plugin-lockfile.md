# Plugin lockfile for `nx.plugins`

**Status:** COMPLETE (2026-07-25) — phases 1, 2, 4 then 3.

## Problem

`nx.plugins` supports pinning — `commit` / `tag` / `version`, `commit` wins
(`prelude/plugins.lua`, `normalize`) — and `_update` correctly refuses to move a pinned
plugin. But nothing *records* what an unpinned plugin resolved to, so:

* **Install is whatever `HEAD` was that day.** A missing plugin is cloned `depth = 1` off
  the remote default branch (`_install`). Two users running first-run setup a week apart
  get different code.
* **`:PluginSync` / `:PluginUpdate` fast-forward silently.** `_update` returns `"pinned"`
  only when `spec.commit or spec.tag` is set; everything else gets pulled.
* **There is no way back.** An update that breaks a plugin cannot be undone — the previous
  SHA was never written down anywhere.
* **The built-in recommended set is entirely unpinned.** All 7 entries of
  `M._default_recommended` are bare `"nxvim/<repo>"` + hooks. The set a real user installs
  on first run is not reproducible.

This is inconsistent with how the *same* plugins are treated elsewhere in the repo:
`crates/nxvim-edithost/build-plugins.sh` pins every bundled plugin to an exact SHA. The
wasm/bundled build is reproducible; the user-facing install is not.

Pinning the recommended set to fixed SHAs was the alternative considered. Rejected: it
freezes everyone on one revision, needs a repo commit to move, and still does nothing for
the plugins a user declares themselves. A lockfile solves the general case.

## What a lockfile has to do

1. **Record** the resolved commit of every managed plugin after install/update.
2. **Reproduce** it: a fresh machine with the same config + lockfile installs the same
   commits.
3. **Restore** it: undo an update that broke something.
4. **Report drift**: show when a checkout no longer matches the lock.

## Design decisions

### Format — JSON, generated, sorted

`<config>/nxvim-lock.json`, a flat map keyed by plugin name:

```json
{
  "catppuccin": { "branch": "main", "commit": "0b0a9a1..." },
  "nxvim-line": { "commit": "ada94b5..." }
}
```

* **JSON, not Lua.** A lockfile is data-only by definition, and it is meant to be
  committed to the user's config repo — the same reasoning that makes project-local
  config (`.nxvim/workspace.json`) plain JSON rather than embedded Lua: no code-exec
  vector in a file that travels between machines.
* **`nx.json` already exists** (`prelude/stdlib.lua`) and `encode(t, { pretty = true })`
  emits a 2-space-indented document with **sorted object keys** — `serde_json` is built
  without `preserve_order`, so its `Map` is a `BTreeMap`. Deterministic, diff-friendly
  output with no hand-rolled serializer and no key-order churn. Verified empirically.
* **Flat map, no schema version.** Matches lazy.nvim's shape, keeps diffs to one line per
  changed plugin, and avoids a top-level key that could collide with a plugin's name. The
  file is *generated*: unknown per-entry fields are ignored on read and not carried over on
  write. Documented as generated, like `Cargo.lock`.

### Location — the config dir, not the data dir

`config_dir()` (`stdpath("config")`, already overridable via `setup_manager{ config = }`),
next to the `lua/plugins.lua` that first-run setup writes. The lockfile is part of the
user's *config* — it belongs in the repo they version, not in the manager-owned install
root that `:PluginClean` prunes. Overridable as `setup_manager{ lockfile = }` so tests are
hermetic.

### Local-always, like the rest of the manager

Read and written through `lfs` (`nx.fs_local`), never the session-routed `nx.fs`. Plugin
management is a local concern even in a daemon / wasm session — plugins load into *this*
Lua VM via the local runtimepath — so the lockfile lives on the client disk beside the
clones it describes. This is the tier-1 rule applied: the feature works identically over a
remote session because it never touches the session's fs.

### Precedence

Highest wins:

1. `spec.commit` — an explicit declaration in the config. The lock is a *record*; a
   hand-written pin is an *instruction*, and must not be silently overridden.
2. the lockfile entry
3. `spec.tag` / `spec.version`
4. `spec.branch`
5. the remote's default branch

A dev `dir` plugin is never locked (it is a working checkout, not a reproducible artifact)
and neither is a plugin whose clone is missing.

### The one real obstacle: shallow clones

An unpinned install is `depth = 1`, and there is **no `fetch` verb** — `nxvim-git`'s job
surface is `Discover / Head / Show / DiffFile / Status / Clone / Checkout / Pull /
SubmoduleUpdate` (`run_git_job`). So an arbitrary locked SHA may simply not be present in
an existing shallow checkout, and there is no way to deepen it.

Consequences, and how each phase handles it:

* **Install** is fine: `_install` already full-clones when a commit is pinned (it skips
  `depth = 1` for exactly this reason), so treating a locked SHA as an implicit `commit`
  reuses that path unchanged.
* **Restore** into an existing shallow clone can fail. `restore()` will attempt the
  checkout and **fail loud**, naming each plugin whose locked commit is unreachable —
  never a silent no-op that leaves the user believing they rolled back. An explicit
  `:PluginRestore!` re-clones those (destructive, so opt-in, never automatic).

The clean fix is a `fetch` / unshallow verb in `nxvim-git`, which would make restore always
work in place. That is a real gix addition, so it is **deferred to Phase 4 and left as a
decision** rather than bundled in — Phases 1–3 are useful without it.

## Phases

### Phase 1 — record and read ✅ DONE

Landed as described below. Notes from the implementation:

* `nx.json.encode(t, { pretty = true })` sorting object keys was confirmed empirically, not
  assumed — `serde_json` is pinned `=1.0.150` with no `preserve_order`, so its `Map` is a
  `BTreeMap`. If that dependency ever gains `preserve_order`, the lockfile's diff-stability
  regresses silently; `the_lockfile_is_pretty_printed_with_sorted_keys` is the guard.
* `lock()` compares against the FILE, not the in-memory `M._lock`, so an externally edited
  lockfile is reconciled instead of assumed to match our last write. An unchanged sync
  rewrites nothing, keeping the user's git status clean.
* `lock_after()` never fails the verb it follows: a lockfile that couldn't be written is
  reported loud on the message line, but must not make a successful install look failed.
* Rejecting with a `{ code, message }` table needs the prelude's
  `local e = {…}; error(e, 0)` idiom — selene types `error`'s argument as a string and
  flags the inline literal (same as `test.lua`'s `fail`).
* **Discovered, not fixed:** the book's API extractor never picks up `nx.plugins.*` at all.
  `book/gen/generate.py` matches `function nx.NS.name`, and plugins.lua declares
  `function M.lock()` over `local M = nx.plugins` — so the whole manager API (~30
  functions) is missing from the generated reference. Out of scope here (it is an extractor
  change affecting one whole module), documented in the hand-written guide instead.

* `setup_manager{ lockfile = }`; `lockfile_path()` defaulting to
  `config_dir() .. "/nxvim-lock.json"`.
* `M._read_lock()` → promise of the decoded table (`{}` when absent). A malformed
  lockfile fails loud rather than being silently treated as empty — a corrupt lock must
  not look like "nothing pinned".
* `M.lock()` → resolve every managed, installed plugin's current SHA via
  `lgit.head(dir)` (returns `{ branch, detached, sha }`), write the file, resolve the
  table. Skips dev `dir` and missing clones.
* `M.locked()` → sync snapshot of the last read/written lock, for the UI.
* Auto-write at the end of `install` / `update` / `sync` when the resolved set changed, so
  first-run setup produces a lockfile without the user doing anything.
* `:PluginLock` command.
* Tests (`tests/plugins.rs`, black-box, local `file://` repos): a sync writes a lockfile
  with the cloned SHA; the SHA matches `git rev-parse HEAD`; a dev `dir` plugin is absent;
  keys are sorted; a malformed lockfile fails loud.

### Phase 2 — install reproduces the lock ✅ DONE

* `_install` consults the lock: an entry with no `spec.commit` → full clone + detach onto
  the locked SHA (the existing pinned-install path).
* Document the precedence list above in the book.
* Tests: two install roots sharing one lockfile land on the same SHA; a spec `commit`
  overrides a differing lock entry; an unlocked plugin still shallow-clones.

Notes from the implementation:

* **The obstacle bit harder than the plan expected.** `sync()` is install-then-update, and
  a locked install leaves a DETACHED HEAD — which `pull` rejects outright. So the first
  working version of lock-respecting install made `:PluginSync` fail on every locked
  plugin. `_update` now reports `"detached"` and leaves such a plugin alone.
* That leaves a real hole: there is no in-editor way to move a lock-installed plugin
  *forward*. Re-attaching to a branch would need `checkout(detach = false)`, which
  `nxvim-git` explicitly refuses (`"checkout without detach is not implemented"`), and
  there is no fetch/unshallow verb either. The documented escape hatch is to drop the
  entry from the lockfile and re-sync. This makes the Phase 4 case concrete: **without a
  git-layer addition the lockfile can reproduce but not un-reproduce.**
* A locked install detaches HEAD, so `head()` reports no branch and re-locking would
  ERASE the recorded branch. `resolve_lock` now carries it over from the spec or the
  previous entry — the branch is a property of the spec, not of the checked-out commit.
* An unreachable locked commit fails loud with an actionable message (plugin, commit, and
  the lockfile path to edit). Falling back to the tip would silently hand back a different
  tree than the lockfile promises.

### Phase 3 — restore and drift ✅ DONE

* `M.restore()` / `:PluginRestore` — check out each plugin to its locked SHA; loud report
  of any unreachable ones; `:PluginRestore!` re-clones those.
* `list()` / `status()` gain `locked` (the SHA) and `drifted` (checkout ≠ lock);
  `:PluginList` and the `:Plugins` dashboard render it, plus an `R` verb keymap.
* Tests: update-then-restore returns the checkout to the locked SHA; drift is reported;
  an unreachable SHA fails loud and does **not** report success.

Notes from the implementation:

* Phase 4 landing first changed the shape here for the better: `restore` deepens the clone
  (`fetch{ unshallow = true }`) and retries when the locked commit is absent, so the
  planned destructive `:PluginRestore!` re-clone escape hatch turned out to be
  **unnecessary and was dropped** — if a commit is still unreachable after unshallowing it
  is gone from the remote, and re-cloning cannot produce it either.
* `restore()` resolves `{ restored, current, failed }` rather than a count, so the three
  outcomes are distinguishable. It resolves (not rejects) even with failures, so one dead
  plugin doesn't hide the others' outcomes; `_restore_notify` reports the failures at level
  4 separately from the summary, and is shared by `:PluginRestore` and the dashboard's `R`
  so the two surfaces cannot word it differently.
* Only ONE unshallow attempt per plugin, and a failed restore leaves the checkout exactly
  as it was.
* `status()` re-reads the lockfile from disk before computing `drifted`, so drift is
  measured against the CURRENT file — the user may have just checked out a different one.
* Coverage gap worth naming: the dashboard's per-row `drifted` flag is an end-of-line
  `virt_text` decoration, and there is no test seam for the dashboard's decor list, so the
  UI test asserts the hint line (real buffer text) plus the underlying `status().drifted`
  rather than the rendered glyph.

### Phase 4 — the git-layer additions ✅ DONE (done before Phase 3)

Pulled forward, because Phase 2 hit the wall first and Phase 3 could not have been built
honestly without it: a `restore` verb on the old surface would have failed on exactly the
case people reach for it.

Two additions to `nxvim-git`, each crossing all five touchpoints (`GitJob` in
`nxvim-lua/ops.rs` → the gix impl → the Lua job parser in `install.rs` → the **daemon wire**
codec in `gitwire.rs` → the `nx.git` surface in `prelude/git.lua`):

* **`checkout` gained its ATTACH mode.** It previously returned
  `EGIT "checkout without detach is not implemented"`. Now `detach = false` means `rev`
  names a branch and HEAD is left symbolic on it (`attach_head`, the mirror of
  `detach_head`). A branch that exists only as a remote-tracking ref — every branch but the
  default one, in a fresh clone — is materialized locally first (`resolve_local_branch`),
  as `git checkout <branch>` does; ambiguity across remotes errors rather than guessing.
* **`fetch(dir, { unshallow })`** — `pull`'s half that touches no working state.
  `unshallow` maps to gix's `Shallow::undo()`, dropping a shallow clone's boundary so
  history a `depth = 1` clone omitted becomes reachable.

Verified over the **daemon wire** too, not just locally
(`daemon_git.rs::nx_git_fetch_and_attach_checkout_run_on_the_daemon_over_the_wire`) — a new
git verb that worked locally and silently not over a remote session would violate the
tier-1 rule.

With attach available, the Phase 2 hole closed: `_update` re-attaches a lock-detached
plugin to its tracked branch and fast-forwards, so `:PluginUpdate` advances past the lock
while `:PluginSync` reproduces it (the `cargo update` / `cargo build` split). `_update`
resolves `"locked"` rather than `"detached"` now. A plugin detached with NO branch recorded
anywhere fails loud instead of guessing.

## Out of scope

* Pinning `M._default_recommended` to SHAs — the lockfile makes it unnecessary.
* A lockfile for the *system* plugin tier (`M._system`): it is client-seeded, outside
  `_specs`/`_order`, and the managed verbs deliberately never touch it.
* Bumping `build-plugins.sh`'s bundled pins — a separate, already-explicit mechanism.
