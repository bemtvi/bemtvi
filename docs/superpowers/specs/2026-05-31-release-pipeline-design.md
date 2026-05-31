# GitHub Release Pipeline — Design

**Date:** 2026-05-31
**Status:** Approved (pending spec review)
**Scope:** CI/CD that builds and publishes `nxvim` binaries for all major OSes, with a
rolling bleeding-edge channel, build provenance attestation, and a git-cliff changelog.

## Goal

Stand up a hand-rolled GitHub Actions release pipeline that:

1. Builds the `nxvim` binary for the five native targets below.
2. Publishes a **stable** GitHub Release on every `v*` tag.
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

## Workflow structure

```
.github/workflows/
  build.yml     # reusable (on: workflow_call): builds all 5 targets, uploads archives as artifacts. Publish-agnostic.
  release.yml   # on: push tag 'v*'      -> calls build.yml -> stable Release + SHA256SUMS + CHANGELOG.md commit + attestation
  edge.yml      # on: push branch 'main' -> calls build.yml -> rolling 'edge' prerelease + SHA256SUMS + attestation
cliff.toml      # git-cliff config
CHANGELOG.md    # generated, committed on stable releases
```

The five-target matrix is defined **once**, in `build.yml`. The two caller workflows differ
only in how they publish, eliminating matrix duplication.

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

### `release.yml` (stable)

- `on: push: tags: ['v*']`.
- `permissions: { contents: write, id-token: write, attestations: write }`.
- Job A: `uses: ./.github/workflows/build.yml` with `version: <tag without leading v>`.
- Job B (`needs: build`):
  1. `actions/download-artifact@v4` collecting all five archives into `dist/`.
  2. Generate `dist/SHA256SUMS` aggregating the five archives.
  3. `actions/attest-build-provenance@v2` over `dist/*` (the archives).
  4. git-cliff: `git cliff --current` -> `RELEASE_NOTES.md` (this tag's grouped notes).
  5. `gh release create <tag> dist/* --notes-file RELEASE_NOTES.md --title <tag>` (a normal,
     non-prerelease Release).
  6. Regenerate full `CHANGELOG.md` (`git cliff -o CHANGELOG.md`) and commit it back to
     `main` (see Changelog loop-safety).

### `edge.yml` (rolling bleeding-edge)

- `on: push: branches: [main], paths-ignore: ['CHANGELOG.md']`.
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
- **Stable notes:** `git cliff --current` → the tag's grouped notes become the Release body.
- **Committed `CHANGELOG.md`:** `git cliff -o CHANGELOG.md` (full history) regenerated and
  committed to `main` as part of the stable release.
- **Edge notes:** `git cliff --unreleased` → "changes since the last `v*` tag" become the
  edge body.

### Loop-safety

`edge.yml` triggers on `main` pushes. To stop the CHANGELOG commit from triggering a
redundant edge build, `edge.yml` declares `paths-ignore: ['CHANGELOG.md']`. (Belt-and-braces:
the commit message can also carry a skip marker, but `paths-ignore` is the primary guard.)

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
- **CHANGELOG commit-back permissions.** The release job pushes to `main`; confirm
  `contents: write` + default `GITHUB_TOKEN` suffices and that branch protection (if any)
  permits the automated commit.

## Testing strategy

This is CI configuration, so it is validated by running it, not by the repo's integration
test harness:

1. Open a PR with the workflows; confirm `build.yml` is syntactically valid and the five
   matrix jobs are scheduled (build may be exercised via a throwaway branch/tag).
2. Push a throwaway `v0.0.0-test` tag (or use a fork) to dry-run `release.yml` end to end:
   all five assets, `SHA256SUMS`, attestation, grouped notes, `CHANGELOG.md` commit.
3. Push to `main` to verify the `edge` rolling release: asset overwrite, stable URLs, edge
   notes, attestation, and that the CHANGELOG commit does **not** retrigger edge.
4. Verify caching: a second no-op `main` push should complete substantially faster than the
   first (warm caches).
5. `gh attestation verify` a downloaded asset to confirm provenance.

## Implementation order (for the plan)

1. `cargo.toml` `strip = true`.
2. `cliff.toml` + an initial `CHANGELOG.md`.
3. `build.yml` reusable workflow.
4. `release.yml`.
5. `edge.yml`.
6. Verification doc note for attestation.
