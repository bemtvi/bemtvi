# Picker: deprioritized paths, and `.gitignore`-respecting `files_git` / `live_grep_git`

Status: planned — 2026-10-29.

## Why

On a real Nuxt project (68k files, 59k of them under `node_modules`), the shipped
`live_grep` was unusable: a search for `the` streamed 8.2GB out of `rg` into Lua
(4GB of Lua heap), and the hits that matter — the project's own sources — were
buried under `node_modules`, `.output` source maps and lockfiles. A user config
worked around the ordering with a `ctx.push` wrapper over `btv.picker._sources` plus
a `btv.picker.scorer`, which proved the behaviour is wanted but is the wrong layer:

- it reaches behind the API (`_sources`) and duplicates the private result cap;
- it *holds back* low-priority rows until the source finishes, so they appear late
  and sit in a Lua table meanwhile — and, held rows being invisible to the cap, it
  let `rg` run the whole tree (fixed in the config, but only by intercepting
  `ctx.on_cancel` too);
- the scorer can't reach the two orderings that matter most: a dynamic source
  (`live_grep`) and an unfiltered list (empty `files` query) are both passthrough.

The canonical fix is a per-row priority the **core** orders by, decided at the one
point every candidate already crosses (the push sink, beside include/exclude).

Separately, users want both shapes of search: *everything* (today's `-uu` default,
now with the noise pushed down) and *what the project tracks* (respecting
`.gitignore`, the telescope default).

## Decisions (confirmed with the requester)

1. **Default list = generated / vendored output + lockfiles + minified files and
   source maps.** Test files are *not* deprioritized by default (many people grep
   tests on purpose); a config adds them. The list is replaceable through
   `btv.picker.setup`.
2. **`live_grep` keeps searching everything** (`-uu`, as today) with the new
   ordering; a new **`live_grep_git`** respects `.gitignore` (hidden files still
   searched, `.git` skipped).
3. **`files` gets the same pair**: `files` unchanged (unrestricted), new
   **`files_git`** respecting `.gitignore`.
4. **The everyday keys respect `.gitignore`; the shifted ones search everything**
   (revised 2026-10-29): `<leader>ff` → `files_git`, `<leader>fg` →
   `live_grep_git`, `<leader>fF` → `files`, `<leader>fG` → `live_grep`. This moves
   the existing `ff` / `fg` off the unrestricted pickers — a behaviour change worth
   a line in the commit and `docs/features/picker.md`.

All four keys are installed with `default = true` like the existing ones, so a user
mapping on the same key (e.g. a config's own `<leader>fG`) still wins.

## Phase 1 — `live_grep` reads a long line once (done, 820e0b8c)

The memory blow-up, fixed independently of everything below:

- `re:find_all(s)` on `btv.regex`: every match's 1-based inclusive offsets in ONE
  pass (a `:find` loop re-validates the whole string per call — quadratic).
- `live_grep`'s `rg` leg runs `--column` (each matching line once) instead of
  `--vimgrep` (the whole line once *per hit*), and fans the line out to a row per
  hit with `find_all`; the clip slices the line directly instead of copying a
  stripped body per hit. Same rows, same clip-around-the-hit, same highlights.
- Tests: `regex.rs::find_all_returns_every_match_position_in_one_pass`,
  `picker.rs::live_grep_reads_a_long_many_hit_line_once_not_once_per_hit` (fails
  on the old source: 1024 of 2000 rows after 5s).
- Measured on the Nuxt project: Lua heap +4.1GB → +216MB for `the`, and the
  picker reaches its 100k cap instead of 1.4k rows in 10s.

Commit this first, on its own.

## Phase 2 — core: a low-priority tier in the picker menu (done)

`bemtvi-core/src/editor/menu.rs`.

- `MenuItem` gains `low: bool`. `btv._picker_push` gains an optional parallel
  `lows` array, materialized on first use exactly like `hls` (a source that never
  marks a row ships nothing extra).
- The view orders **every non-low row before every low row**, stable within each
  tier, in all three modes:
  - *passthrough* (empty query / dynamic source): today the view *is* `all_items`
    with nothing materialized. Keep that zero-cost path while no low row has
    arrived; on the first low row, switch to an index view of two segments
    (`high`, `low`) appended O(1) per item. `view_len` / `item_at` concatenate.
  - *filtered* (static source + query): `refilter` ranks, then stable-partitions
    by tier (O(n), once per query edit); `extend_view` appends each batch's matches
    to its tier's segment — still O(batch), never a re-rank.
  - the per-repaint re-ranker (`settle_picker_rank`, `btv.picker.scorer`) reorders
    *within* a tier, never across it.
- Cursor, marks (multiselect), confirm, the progress readout and `resume` snapshots
  all go through `item_at`, so they stay correct; audit every direct `filtered`
  access (61 today) and route it through the segment accessors.
- The completion popup (`MenuKind::Complete`) never sets `low`; its blended sort is
  untouched.

Tests (`tests/picker.rs`):

- passthrough: a source pushing interleaved low/normal rows across several batches
  lists every normal row first, each tier in stream order;
- filtered: a low row with a *better* fuzzy score still lists after every normal
  match; streaming more batches keeps the partition;
- the cursor confirms the row it shows (no index drift across the partition);
- the 100k-item flood test still meets its time bound with a mix of tiers;
- `btv.picker.scorer` reorders within a tier but cannot lift a low row above a
  normal one.

## Phase 3 — Lua: `btv.picker.setup({ deprioritize = … })` (done)

`bemtvi-lua/src/prelude/picker.lua`.

- `deprioritize`: a list of globs in the **same syntax as `exclude`** (expanded by
  the same `filter_line` rules, compiled with `btv.glob.set`), replacing the
  default. `btv.picker.defaults.deprioritize` exposes the shipped list so a config
  can extend rather than retype it:

  ```lua
  btv.picker.setup({
    deprioritize = btv.list.extend(btv.tbl.deepcopy(btv.picker.defaults.deprioritize), { "*test*" }),
  })
  ```

- The push sink tests each candidate's `path` against the compiled set — at the
  same point, and only for the same `filter = true` sources, as include/exclude —
  and marks `low`. One glob-set test per row, like `exclude` already does; bench
  it on the 100k flood.
- **Two-pass search, normal paths first** (revised 2026-10-29, replacing a 90k
  low-row cap): the hits that matter are found before the low ones, so the 100k cap
  can only ever be filled by low rows once every normal hit is in, and `rg` never
  walks a low directory on the first pass at all.
  - `ctx.passes` — the runs a source should make, in order: `{ tier = "normal",
    rg_globs = … }` (the box globs plus every deprioritize glob *negated*, so `rg`
    prunes `node_modules/`) then `{ tier = "low", rg_globs = … }` (the box globs
    plus the deprioritize globs as *includes*; when the include box is non-empty the
    box's own includes are used alone instead, since `rg` ORs positive globs and the
    union would search more than either). One `{ tier = "all", rg_globs =
    ctx.rg_globs }` pass when the list is empty.
  - `ctx.begin_pass(pass)` tells the sink which pass is running: a row of the
    *other* tier is dropped (before the cap), so the glob set — not `rg`'s reading
    of the same globs — decides each path's tier and nothing lists twice.
  - `files` / `live_grep` loop their `rg` leg over `ctx.passes`. The `btv.fs`
    walk legs get their list in one piece and push the normal rows first. The
    `find` / `grep` legs stay single-pass and rely on the core's tiers for order —
    their known limit is the old one, low rows can fill the cap first.
  - A source that ignores `ctx.passes` still has its rows marked `low` and ordered
    by the core.
- Tests: the pass globs a source is handed; the sink dropping a cross-tier row in a
  pass; with `rg`, a capped `live_grep` over a tree whose low hits alone exceed the
  cap still lists the `src/` hit (skip-if-missing `rg`).
- Default list (globs; directory names match as whole path segments):
  - JS/TS: `node_modules/`, `bower_components/`, `jspm_packages/`, `.yarn/`,
    `.pnpm-store/`, `.nuxt/`, `.output/`, `.next/`, `.svelte-kit/`, `.angular/`,
    `.expo/`, `.docusaurus/`, `.vercel/`, `.netlify/`, `.serverless/`, `.turbo/`,
    `.parcel-cache/`, `.vite/`, `.cache/`, `storybook-static/`, `coverage/`,
    `.nyc_output/`
  - Python: `.venv/`, `venv/`, `__pycache__/`, `site-packages/`, `.pytest_cache/`,
    `.mypy_cache/`, `.ruff_cache/`, `.tox/`, `.nox/`, `.eggs/`, `*.egg-info/`,
    `.ipynb_checkpoints/`
  - Build output: `dist/`, `build/`, `out/`, `target/`, `_build/`, `vendor/`
  - Other: `.gradle/`, `.dart_tool/`, `Pods/`, `DerivedData/`, `.stack-work/`,
    `.terraform/`, `.bundle/`, `.sass-cache/`, `.direnv/`, `.idea/`, `.vscode/`
  - Lockfiles: `package-lock.json`, `npm-shrinkwrap.json`, `yarn.lock`,
    `pnpm-lock.yaml`, `bun.lock`, `bun.lockb`, `deno.lock`, `Cargo.lock`,
    `poetry.lock`, `Pipfile.lock`, `uv.lock`, `pdm.lock`, `pixi.lock`, `go.sum`,
    `go.work.sum`, `Gemfile.lock`, `composer.lock`, `mix.lock`, `pubspec.lock`,
    `Podfile.lock`, `Package.resolved`, `packages.lock.json`, `gradle.lockfile`,
    `flake.lock`, `.terraform.lock.hcl`, `renv.lock`, `conan.lock`, `shard.lock`
  - Minified / maps: `*.min.js`, `*.min.css`, `*.map`
- Tests: the default list pushes a `node_modules/` hit and a `yarn.lock` below a
  `src/` hit in `live_grep` *and* `files` (empty query and with a query); a
  look-alike (`src/distance.rs`, `build.rs`, `src/lockfile.rs`) is not pushed
  down; `setup({ deprioritize = {} })` restores pure stream / fuzzy order; a
  replaced list takes effect on the next open.

## Phase 4 — `files_git` and `live_grep_git` (done)

- `files_git`: `rg --files --hidden -g !.git` (no `-u`: `.gitignore`, `.ignore`
  and `.rgignore` are honoured). `live_grep_git`: the Phase-1 `live_grep` leg with
  `--hidden -g !.git` instead of `-uu`. Both `filter = true`, both deprioritized.
- Shared code: factor `files` / `live_grep` into one constructor each taking the
  restriction flags, so the pairs cannot drift.
- Fallback legs must keep the promise *"respects `.gitignore`"* — never silently
  list ignored files:
  - no `rg`, in a git repo: `git ls-files --cached --others --exclude-standard`
    (files), `git grep -n --column --untracked -I` (grep);
  - no shell at all (pure web client): the `btv.fs` walk with Phase 5's
    `gitignore = true`.
- Default keys (decision 4): `<leader>ff` / `<leader>fg` move to `files_git` /
  `live_grep_git`, and `<leader>fF` / `<leader>fG` open the unrestricted `files` /
  `live_grep` — all `default = true`, listed in the `pickers` picker and the
  command palette. Update the existing default-map tests to the new targets.
- Tests: a fixture with a `.gitignore`d dir and a hidden non-ignored file — the
  `_git` pickers omit the first and list the second, the unrestricted ones list
  both; the `git` fallback leg (rg masked off `PATH`) gives the same set.

## Phase 5 — `.gitignore` in the `btv.fs` walk (web / no-shell parity)

The tier-1 remote rule: the pure web client has no `rg` / `git`, so the `_git`
pickers must still honour `.gitignore` there rather than fail.

- `btv.fs.walk(dir, { gitignore = true })` (and `btv.fs.grep`, which passes opts
  through): read each directory's `.gitignore` (plus the repo's `.git/info/exclude`)
  over `btv.fs` as the walk descends, and prune with a gitignore matcher. Match
  semantics come from the Rust `ignore` crate's `gitignore` builder (exposed as a
  small `btv.fs.gitignore(lines, root)` matcher) rather than a hand-rolled Lua
  parser, so negation, anchoring and `**` behave exactly as in `rg`.
- Verify in both builds: native (`--test daemon_fs`) and wasm-eligible
  (`--no-default-features` + a `verify-*.mjs` browser check listing a `_git` picker
  over OPFS).

## Phase 6 — docs, example, config cleanup

- `docs/features/picker.md`: deprioritization, the setup key, the two `_git`
  pickers and their keys; the book pages via the prelude docstrings (markdown
  rules in CLAUDE.md).
- `examples/picker-priority/`: `init.lua` (numbered *type-this / see-that*
  sections: the default ordering, extending the list with `*test*`, `<leader>fg`
  vs `<leader>fG` on an ignored file — the first skips it, the second finds it) + `sample.txt` + a fixture tree, with its
  `test/*_spec.lua` driving the same keys.
- The requester's config drops `deprioritize_paths` / `deprioritized_scorer` /
  `_sources` access for one `btv.picker.setup({ deprioritize = … })` that adds
  `*test*` to the defaults.

## Out of scope

- Two-pass `find` / `grep` fallback legs (converting the globs to `find -path` /
  `--exclude-dir` reliably is its own project).
- More than two tiers / numeric priorities — a `low` flag covers the requirement;
  widen to a small integer later if a real use appears.
