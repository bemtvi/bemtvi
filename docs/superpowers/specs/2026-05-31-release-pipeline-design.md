# GitHub Release Pipeline — Design

**Date:** 2026-05-31
**Status:** Approved (pending spec review)
**Scope:** CI/CD that builds and publishes `nxvim` binaries for all major OSes, with a
rolling bleeding-edge channel, build provenance attestation, and a git-cliff changelog.

## Goal

Stand up a hand-rolled GitHub Actions release pipeline that:

1. Builds the `nxvim` binary for the five native targets below.
2. Publishes a **stable** GitHub Release via a review-gated flow: a maintainer-triggered
   release-prep PR (CHANGELOG + version bump) that, once merged, auto-tags `v<version>` and
   publishes. CI never pushes to `main`.
3. Maintains a single rolling **`edge`** prerelease that tracks `main`, overwriting its
   assets on every push, so there is always one stable URL for "latest main build."
4. Keeps incremental `main`-push builds **light** via aggressive Rust caching.
5. Attaches **SLSA build provenance attestations** to every published binary.
6. Generates **pretty, grouped changelogs** (git-cliff) for release notes and maintains a
   committed `CHANGELOG.md`.

Out of scope: a lint/test CI workflow (`ci.yml`). Mentioned as a possible follow-up but not
built here.

## Target matrix

Every target builds on a runner of its own architecture so the vendored-Lua C toolchain
(`mlua` with `features = ["vendored"]`) is always native — no cross-compilation of C.

| Target triple                  | Runner            | Asset name                                |
| ------------------------------ | ----------------- | ----------------------------------------- |
| `x86_64-unknown-linux-musl`    | `ubuntu-latest`   | `nxvim-<ver>-x86_64-linux-musl.tar.gz`    |
| `aarch64-unknown-linux-musl`   | `ubuntu-24.04-arm`| `nxvim-<ver>-aarch64-linux-musl.tar.gz`   |
| `x86_64-apple-darwin`          | `macos-13`        | `nxvim-<ver>-x86_64-macos.tar.gz`         |
| `aarch64-apple-darwin`         | `macos-14`        | `nxvim-<ver>-aarch64-macos.tar.gz`        |
| `x86_64-pc-windows-msvc`       | `windows-latest`  | `nxvim-<ver>-x86_64-windows.zip`          |

`<ver>` is the release version for stable (the tag minus the leading `v`, e.g. `0.2.0`) and
the literal string `edge` for the edge channel.

### Linux musl (static) specifics

- `sudo apt-get install -y musl-tools` provides `musl-gcc`.
- `rustup target add <target>` adds the musl std.
- The `cc` crate must compile vendored Lua with the musl C compiler, and the linker must be
  the musl one. Set, per the active target (uppercased triple):
  - `CC_x86_64_unknown_linux_musl=musl-gcc` (and the aarch64 equivalent on the arm runner)
  - `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc` (and aarch64 equivalent)
- musl implies `+crt-static`, producing a fully static binary.

### Submodule

`vendor/neovim` is reference-only and never built. CI uses `actions/checkout` with its
default (`submodules: false`) — faster checkouts, no needless data.

## Release flow (no pushes to `main`)

`main` is only ever modified through pull requests — CI never pushes to it. A stable release
is a three-step, human-gated flow:

1. **Prepare** — a maintainer runs `release-prep.yml` (manual `workflow_dispatch`) with a
   `version` input. It regenerates `CHANGELOG.md`, bumps the workspace version in
   `Cargo.toml`, and opens a PR. Nothing is published yet.
2. **Review & merge** — the maintainer reviews the PR (the changelog and version bump are
   visible in the diff) and merges it. This is the only way the changes reach `main`.
3. **Tag & publish** — merging the release PR triggers `release.yml`, which tags the merge
   commit `v<version>` and builds + publishes the stable Release in the same run.

Because the tag is pushed with the built-in `GITHUB_TOKEN` — which, by GitHub's rules, does
**not** trigger further workflow runs — the publish cannot depend on a separate tag-push
event. So `release.yml` tags **and** publishes within a single run rather than handing off to
a tag-triggered workflow. The whole flow stays secret-free (no PAT / GitHub App).

## Workflow structure

```
.github/workflows/
  build.yml         # reusable (on: workflow_call): builds all 5 targets, uploads archives as artifacts. Publish-agnostic.
  release-prep.yml  # on: workflow_dispatch(version) -> regenerate CHANGELOG.md + bump version -> open release PR
  release.yml       # on: pull_request closed (merged + 'release' label) -> tag v<version> + build + publish stable Release
  edge.yml          # on: push branch 'main' -> calls build.yml -> rolling 'edge' prerelease + SHA256SUMS + attestation
cliff.toml          # git-cliff config
CHANGELOG.md        # regenerated inside the release-prep PR; reaches main via merge, never pushed by CI
```

The five-target matrix is defined **once**, in `build.yml`; the publishing workflows reuse it.

### `build.yml` (reusable build)

- `on: workflow_call` with one input: `version` (string) — used to name the archives.
- A `strategy.matrix` over the five `{ target, runner, os }` entries; `fail-fast: false` so
  one target failing doesn't abort the others.
- Steps per matrix job:
  1. `actions/checkout@v4` (default, no submodules).
  2. Install the Rust toolchain (`dtolnay/rust-toolchain@stable`) with `targets: <target>`.
  3. Linux only: install `musl-tools`, export the `CC_*` / `*_LINKER` env vars.
  4. `Swatinem/rust-cache@v2` with `shared-key: <target>` (see Caching).
  5. `cargo build --release -p nxvim --target <target>`.
  6. Package: copy the binary (`nxvim` / `nxvim.exe`) into an archive — `.tar.gz` on
     Unix, `.zip` on Windows — named per the table above. Binary is stripped (via Cargo
     profile, below).
  7. `actions/upload-artifact@v4` uploading the single archive, named by target so the
     publish job can collect all five.

### `release-prep.yml` (open the release PR)

- `on: workflow_dispatch` with one input: `version` (e.g. `0.2.0`).
- `permissions: { contents: write, pull-requests: write }`.
- Steps:
  1. `actions/checkout@v4`.
  2. Validate `version` (semver shape) and that no `v<version>` tag already exists.
  3. Bump `[workspace.package] version` in the root `Cargo.toml` to `<version>`, then refresh
     `Cargo.lock` (e.g. `cargo update --workspace`) so the lockfile matches.
  4. git-cliff: regenerate full `CHANGELOG.md` with the upcoming release rendered under
     `v<version>` (`git cliff --tag v<version> -o CHANGELOG.md`).
  5. Open a PR via `peter-evans/create-pull-request@v6`:
     - branch `release/v<version>`, title `release: v<version>`, label `release`.
     - body: the `v<version>` changelog section, for at-a-glance review.
- Note: PRs opened with `GITHUB_TOKEN` don't themselves trigger other workflows — irrelevant
  here since publishing is driven by the *merge*, handled by `release.yml`.

### `release.yml` (tag + publish, on merged release PR)

- `on: pull_request: types: [closed]`, gated by
  `if: github.event.pull_request.merged == true && contains(github.event.pull_request.labels.*.name, 'release')`.
- `permissions: { contents: write, id-token: write, attestations: write }`.
- Job A — **tag** (`if` as above): check out the merge commit, read `<version>` from
  `Cargo.toml`, create and push tag `v<version>` (with `GITHUB_TOKEN`; recorded for
  provenance — it intentionally does not trigger any further workflow). Outputs `version`.
- Job B — **build** (`needs: tag`): `uses: ./.github/workflows/build.yml` with
  `version: <version>`.
- Job C — **publish** (`needs: [tag, build]`):
  1. `actions/download-artifact@v4` collecting all five archives into `dist/`.
  2. Generate `dist/SHA256SUMS` aggregating the five archives.
  3. `actions/attest-build-provenance@v2` over `dist/*` (the archives).
  4. git-cliff: `git cliff --current` -> `RELEASE_NOTES.md` (the `v<version>` grouped notes;
     the tag created in Job A makes it "current"), appending the attestation-verify footer.
  5. `gh release create v<version> dist/* --notes-file RELEASE_NOTES.md --title v<version>`
     (a normal, non-prerelease Release). No commit back to `main` — `CHANGELOG.md` is already
     correct on `main` from the merged prep PR.

### `edge.yml` (rolling bleeding-edge)

- `on: push: branches: [main]`. (No `paths-ignore` needed — CI never pushes to `main`, so
  there is no self-triggering commit to filter out. Merging a release PR is a normal `main`
  push and *should* refresh the edge build with the new version.)
- `concurrency: { group: edge, cancel-in-progress: true }` so rapid pushes don't race on the
  tag/release.
- `permissions: { contents: write, id-token: write, attestations: write }`.
- Job A: `uses: ./.github/workflows/build.yml` with `version: edge`.
- Job B (`needs: build`):
  1. Download all five archives into `dist/`; generate `dist/SHA256SUMS`.
  2. `actions/attest-build-provenance@v2` over `dist/*`.
  3. git-cliff: `git cliff --unreleased` -> notes of changes since the last `v*` tag, under a
     header `Built from <short-sha> on <date>` (date/sha injected by the step, since scripts
     cannot call `Date.now()`-style APIs — here it's a shell `date`/`git rev-parse`).
  4. Force-move the `edge` git tag to the current commit (`git tag -f edge && git push -f
     origin edge`).
  5. Idempotently update the single `edge` prerelease:
     - If it exists: `gh release edit edge --target <sha> --notes-file ...` then
       `gh release upload edge dist/* --clobber`.
     - Else: `gh release create edge dist/* --prerelease --target <sha> --notes-file ...`.
     - Using `--clobber` (not delete+recreate) keeps asset download URLs stable
       (`.../releases/download/edge/<file>`) and avoids spamming watchers with new-release
       events.

## Caching — "light main pushes"

`Swatinem/rust-cache@v2` in `build.yml`, keyed `shared-key: <target>`:

- **One cache per target**, shared across `edge` and `release` runs. Caches saved on `main`
  are restorable by tag refs, so a stable build also starts warm.
- Caches `target/` (including compiled vendored-Lua C objects) plus the cargo registry and
  git index. After the first warm build, a typical `main` push recompiles only changed
  crates — seconds, not a cold full build.
- rust-cache trims the cache (drops unreferenced deps) to stay within GitHub's per-repo
  ~10 GB cache budget across five targets.
- The vendored-Lua C object is rebuilt only when the `mlua` version changes.

## Changelog (git-cliff)

- `cliff.toml` at repo root. Conventional-commit grouping into sections: Features, Bug Fixes,
  Documentation, Performance, Refactor, and a catch-all Other. Commits link to the repo;
  `[unreleased]` is supported for the edge channel.
- **Committed `CHANGELOG.md`:** regenerated **inside the release-prep PR**
  (`git cliff --tag v<version> -o CHANGELOG.md`) and reviewed in the diff. It reaches `main`
  by merge — CI never pushes it. This is the only place the file is written.
- **Stable notes:** `git cliff --current` in `release.yml` → the just-tagged `v<version>`
  grouped notes become the Release body.
- **Edge notes:** `git cliff --unreleased` → "changes since the last `v*` tag" become the
  edge body.

No loop-safety guard is required: because CI never commits to `main`, there is no
self-triggering CHANGELOG push for `edge.yml` to filter.

## Provenance attestation

- `actions/attest-build-provenance@v2` runs in the publish job of **both** workflows, over
  the final asset files in `dist/`. It records that the artifacts were produced by this exact
  workflow run and commit (SLSA provenance), signed via GitHub's OIDC identity.
- Required permissions on the publish job: `id-token: write`, `attestations: write` (plus
  `contents: write` to publish).
- Free on public repositories; no PATs or external secrets — only the built-in
  `GITHUB_TOKEN`.
- Consumers verify with: `gh attestation verify <asset> --repo davidrios/nxvim`.
- A short note documents verification (in the release-notes footer and a brief `docs/` page).

## Cargo profile change

Add to the root `Cargo.toml` `[profile.release]` (which already sets `lto = "thin"` and
`codegen-units = 1`):

```toml
strip = true
```

Smaller, symbol-stripped download binaries. No behavioral change.

## Error handling & edge cases

- `fail-fast: false` on the build matrix: a single target's failure surfaces independently
  rather than masking the others.
- Edge concurrency cancels superseded in-flight runs so the `edge` tag/release reflects the
  newest `main` commit, not a slower earlier one.
- First-ever edge run: the `gh release create` path handles the not-yet-existing release.
- Tag builds with a cold cache still succeed — they're just slower; releases are infrequent.
- `gh` CLI calls that may legitimately no-op (e.g. checking release existence) are guarded so
  the job doesn't fail spuriously.

## Risks / to verify during implementation

- **musl-static + vendored Lua linking.** Well-trodden but must be confirmed in a real CI
  run, especially `aarch64-unknown-linux-musl` on the native arm runner. Fallback if a target
  fights musl: switch that single target to glibc (`*-unknown-linux-gnu`).
- **GitHub cache budget.** Five target caches could approach the ~10 GB repo limit; rely on
  rust-cache trimming and monitor. If exceeded, reduce cached scope or drop the least-used
  target's cache.
- **`release.yml` trigger reliability.** It fires on `pull_request: closed` filtered to
  `merged && label 'release'`. Confirm the prep PR carries the `release` label and that the
  workflow file already lives on `main` (so the event resolves it). Branch protection on
  `main` is fine — it only gates pushes, not the PR-merge event.
- **Tag push by `GITHUB_TOKEN`.** Intentionally does not trigger a downstream workflow; the
  publish runs in the same `release.yml` run, so nothing is missed. (If a tag-triggered
  workflow is ever wanted, it would need a PAT/App token — out of scope.)
- **`create-pull-request` permissions.** `release-prep.yml` needs `contents: write` +
  `pull-requests: write`; if org settings disallow Actions from creating PRs, that toggle must
  be enabled (Settings → Actions → "Allow GitHub Actions to create and approve pull requests").

## Testing strategy

This is CI configuration, so it is validated by running it, not by the repo's integration
test harness:

1. Merge the workflows to `main` (the `pull_request: closed` and `workflow_dispatch` triggers
   must exist on `main` to fire). Confirm `build.yml`/`edge.yml` are syntactically valid and
   the first `main` push runs the five `edge` matrix jobs.
2. Run `release-prep.yml` with `version: 0.2.0` (or similar). Verify the PR opens with the
   `release` label, the bumped `Cargo.toml`/`Cargo.lock`, and the regenerated `CHANGELOG.md`.
3. Merge that PR and confirm `release.yml` fires: tags `v0.2.0`, builds all five targets,
   produces `SHA256SUMS`, attestations, grouped notes, and a non-prerelease Release — with no
   commit pushed to `main`.
4. Verify the merge (a normal `main` push) refreshed the `edge` rolling release: asset
   overwrite, stable URLs, edge notes, attestation.
5. Verify caching: a second no-op `main` push completes substantially faster than the first
   (warm caches).
6. `gh attestation verify <asset> --repo davidrios/nxvim` confirms provenance.

## Implementation order (for the plan)

1. `Cargo.toml` `strip = true`.
2. `cliff.toml` + an initial `CHANGELOG.md`.
3. `build.yml` reusable workflow.
4. `edge.yml`.
5. `release-prep.yml`.
6. `release.yml`.
7. Verification doc note for attestation.
