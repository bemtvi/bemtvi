# Migrating the dependency majors

**Status:** deferred (2026-11-25). Phase 0 landed; phases 1–8 are on hold until picked up
again. Re-check every target version when a phase starts.

The 2026-11-25 dependency sweep raised every floor within its major and refreshed both
lockfiles. What it left behind is the set of new **major** versions, each of which needs
code changes. This plan takes them one at a time, from least to most risky, each as its own
commit with a pause for review between phases.

**Rules every phase follows**

- **The 2-day rule** (`CLAUDE.md` → *Dependency versions*): take the newest release published
  more than 2 days before the phase starts. Re-check this when a phase starts, not when this
  plan was written: the "target" versions below are only what qualified on 2026-11-25. Crates
  are checked against the sparse index's `pubtime`. npm lockfiles are written with
  `npm install --before=<now − 2 days>`.
- **Lock hygiene:** after `cargo update -p <crate>`, re-check the whole lock for releases younger
  than 2 days, and pin back any the update dragged in (`cargo update -p x@new --precise old`).
  Do the same for `crates/bemtvi-edithost/Cargo.lock` whenever a shared crate moves (the
  edit-host keeps exact `=` pins matching the workspace).
- **Bug-fix discipline applies to behavior changes:** if a changelog announces a semantic change
  in something we rely on (vt100 resize, notify event kinds), first write a test that pins
  today's behavior, and watch it pass on the old version. Then bump, and fix whatever the test
  catches.
- **Done means:** `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`,
  `RUST_TEST_THREADS=1 cargo test --workspace --no-fail-fast`, plus the phase's own verification
  below (the web build, `--ignored` live tests, a hand check). The wasm-eligible
  `--no-default-features` build must still compile for every phase that touches a crate the
  web build links.

**Not in this plan**

- **The CI Linux linker's Zig (`build.yml`, `mlugg/setup-zig` → `0.13.0`) stays put.** It is
  what `cargo-zigbuild` uses to give the Linux binaries their glibc 2.17 floor, and it doesn't
  move for the sake of a sweep (David, 2026-11-25). This is separate from the `:TSInstall`
  toolchain, which phase 0 bumps.
- **`lsp-types` 0.95 → 0.97 is blocked.** `async-lsp` 0.2.4, the newest, requires
  `lsp-types ^0.95`. Bumping alone would resolve a second `lsp-types`, and the types we build
  would no longer match async-lsp's API (see the comment above both in the root
  `Cargo.toml`). Revisit when async-lsp releases a line on 0.96+ (the `url::Url` →
  `fluent_uri::Uri` move: ~70 `Url` uses across `bemtvi-lsp`, `bemtvi-server` and the test
  harness).

---

## Phase 0 — the sweep itself (done)

- `:TSInstall` Zig 0.15.2 → 0.17.1: version + six SHA-256s, archive names spelled from one
  `zig_version!()`, the `ts_install` test reading the version from `bemtvi_test_harness::ZIG_VERSION`.
  Verified on aarch64-macos with `ts_install` `--include-ignored` (32/32, including the live
  download → checksum → compile → indent test).
- Compatible floors raised (23 crates), both lockfiles refreshed under the 2-day rule.
  npm minors: pyodide 314.0.8, playwright 1.65.1, pyright 1.1.414, bash-language-server 5.8.1.
  `actions/setup-node` v6 → v7.
- The 2-day rule added to `CLAUDE.md`.

Committed as two commits: the Zig bump, then the rest. The full suite passed apart from
six tests that fail identically on the pre-sweep lock on macOS (a Latin-1 filename APFS
refuses, two timing ratios, the LSP apply-edit file creation, the multiplexer clipboard
test, the daemon watch-arm message) — pre-existing, not caused by the sweep.

The Zig bump stays despite the old-glibc question: the pinned Zig 0.17.1 was run on
CentOS 7 (glibc 2.17) under podman, built a grammar exactly as `:TSInstall` does (only
`GLIBC_2.2.5` symbols) and the result `dlopen`ed fine. Zig's Linux binary is static and
targets the host glibc, so its version is independent of the release binaries' glibc floor.

## Phase 1 — a CI guard for the pinned Zig

Before migrating anything else, close the gap phase 0 exposed: no CI job ever runs the real
pinned Zig. The normal suite sets `$BEMTVI_CC` to the system compiler, so a wrong checksum or a
Zig release that can't build our grammars on some target would only show up for users.

- Add a job to `test.yml` with a matrix over the six pinned targets: `ubuntu-latest`,
  `ubuntu-24.04-arm`, `macos-latest` (arm64), `macos-15-intel`, `windows-latest`, `windows-11-arm`.
  Each runs `cargo test --locked -p bemtvi --test ts_install -- --ignored ts_install_rust_over_the_real_network`
  with `CC` unset, so the test takes the fetched-Zig path and asserts `[zig (fetched)]`.
- `test.yml` runs on every release tag, so a Zig bump can't ship until every target has
  downloaded, verified and built a grammar with it.

**Verify:** run the workflow by hand (`gh workflow run test.yml --ref main`) and watch all six
legs go green. Then break one SHA locally, push to a scratch branch, and watch that leg fail on
the checksum.

## Phase 2 — notify 8 → 9 (target 9.0.0)

Used in 3 files: `bemtvi-server/src/evloop.rs` (the per-buffer file watch behind
autoread / W11 / W12), `bemtvi-server/src/daemon.rs` (the remote watch leg),
`bemtvi-lsp/src/dispatch.rs` (`workspace/didChangeWatchedFiles`). We enable `macos_kqueue`.

What changes for us ([migration guide](https://github.com/notify-rs/notify/blob/main/docs/UPGRADING_V8_TO_V9.md)):

- `Watcher::paths_mut` is gone; batched changes go through `Watcher::update_paths`.
  `watched_paths` returns `Vec<WatchedPath>`.
- `Config` is no longer `Copy`; `ErrorKind` matches need a wildcard arm.
- Event paths keep the registered path as given, relative roots included. Check that we
  always register canonical absolute paths, or the buffer lookup by event path can miss.
- kqueue / inotify `unwatch` now removes only explicit watches. Check the "close the last buffer
  on a file → unwatch" path still releases the watch.
- MSRV 1.91 (fine: CI tracks stable, local is 1.96).

**Verify:** `daemon_fs_watch`, `daemon_watch`, `lsp_watchfiles`, and the autoread / checktime
coverage in `buffers` / `editing`. By hand on macOS: edit a file externally while it is open
(clean buffer autoreloads; modified buffer warns W12). Run on Linux CI too (inotify is a
different backend).

## Phase 3 — vt100 0.16 → 0.18 (target 0.18.1)

Used in `bemtvi-server/src/terminal.rs` (the `:terminal` emulator) and two files in `bemtvi`.

API changes: `Parser::new*` and `Screen::set_size` take `NonZeroU16`, and `Screen::size`
returns them. The `*_formatted` / `*_diff` methods return `String` instead of `Vec<u8>`.
Edition 2024, MSRV 1.85.

**The behavior change is the risk.** `Screen::set_size` now behaves like xterm: shrinking the
height pushes rows off the top into scrollback, growing it pulls rows back out of scrollback,
and the scroll region and scrollback offset reset. `terminal.rs` keeps an incremental history
mirror keyed on vt100's scrollback length (`scrollback_len` at last projection), and assumes
the scrollback only grows between projections. A resize can now shrink it.

1. **Test first, on 0.16:** a terminal with scrollback is resized shorter, then taller, and the
   buffer must show every line exactly once, in order, with no duplicate or lost rows at the
   seam. Add it to `tests/terminal.rs`; it should pass on 0.16.
2. Bump, fix compile errors, run the new test, then fix the mirror for a shrinking or
   re-growing scrollback. Keep the per-burst work bounded: no full rescan on resize
   (`CLAUDE.md` → *The editor must never freeze*; the flood tests guard this).
3. Leave for later, but note: 0.18's `Callbacks::on_scroll` streams each row as it leaves the
   scrollback, which could replace part of the length-diff bookkeeping. That is a separate
   change, not part of this bump.

**Verify:** `terminal`, `daemon_terminal`, `daemon_terminal_client`, `term_startup`, the flood /
`^C` trim tests. By hand: `:terminal`, run something long, resize the window both ways, scroll
history. The web terminal rides the server emulator, so rebuild the edit-host and run its
terminal verify.

## Phase 4 — gix 0.89 → 0.91 (targets gix 0.91.0, gix-index 0.59.0, gix-worktree-state 0.38.0)

These three move together: gix 0.91 requires exactly `gix-index ^0.59` and
`gix-worktree-state ^0.38`, and a mismatch would build two copies of the index types. All
use is in `bemtvi-git` (2 files, ~70 paths: refs transactions, remote fetch, status /
index-worktree, `diff::index`, `dir::entry`, `config::section`, `worktree::stack`). Read the
gix changelog for 0.90 and 0.91 entries on exactly those modules before starting; gix breaks
plumbing signatures routinely, but its higher-level API rarely changes meaning.

**Verify:** `git`, `daemon_git`, and the bemtvi-line / diff plugin paths that consume git status
(a modified file shows in the statusline; hunks show in the gutter). Check that the
`default-features = false` feature set still builds for the daemon.

## Phase 5 — redb 4 → 5 (target 5.0.0)

The shada store (`bemtvi-server/src/shada.rs`, native only; web shada is an OPFS JSON blob and
unaffected).

**On-disk format: no migration needed.** redb 5 changed the encoding of
`Option<&str|String|bool|char>`. Every shada table is keyed and valued by `&str`, `u64`,
tuples of those, or `&[u8]` (msgpack blobs), with no `Option` and no custom `Value` impls,
so existing files read as before. This must still be proved, not assumed (step 1).

API changes we hit: `FileBackend::new` returns `FileBackend` (no `Result`; drop the `map_err`
at `shada.rs:322`); the `StorageBackend` I/O methods return `BackendError`, so the
`SharedFileBackend` delegate impl needs re-checking against the new trait; `compact()` takes a
`CompactionMode` (use `Balanced` to keep today's behavior); `range()` / `extract_if()` signature
changes if we use them.

**Locking changes, and why they matter for us:** redb 5 locks with byte ranges only. shada's
"is this sibling instance still alive?" test is "can I open its file?" (a live instance holds
the lock). A bemtvi built on redb ≤ 4.2 running beside one on redb 5, on Linux, would not see
each other's locks. Every released bemtvi is on 4.4+, but confirm that the
`LockProtocolMismatch` read-only case (4.3 / 4.4) can't make a live sibling look dead, or a
dead one look live.

1. **Compatibility test first, on 4.6:** write a shada store with the current build (registers,
   marks, file marks, histories, jumplist, folds) and commit it as a fixture
   (`tests/fixtures/shada-redb4.db`). Add a test that loads it and asserts each item. It passes
   on 4.6.
2. Bump, fix compile errors, run the fixture test. It proves a user's existing shada survives
   the upgrade.
3. By hand: run the previous release binary beside the new build in the same workspace and
   check carry-forward merge and the clean-exit compaction both ways.

**Verify:** `shada`, `session`, `session_reconnect`, `daemon_*` persistence paths, and
`web/verify-shada.mjs` (it should be untouched, but the cfg-split means checking both).

## Phase 6 — tree-sitter 0.26 → 0.27, natively and in the browser

The biggest phase; split it into 6a (native) and 6b (web) commits if 6a works on its own.

Target: `tree-sitter` 0.27.2, `tree-sitter-generate` 0.27.2; npm `web-tree-sitter` 0.27.2 and
`tree-sitter-c-sharp` 0.24.0. `tree-sitter-md` 0.5.4 is already on the 0.27 line.

Before writing code, answer:

- **Parser ABI range.** Which `LANGUAGE_VERSION` / `MIN_COMPATIBLE_LANGUAGE_VERSION` does 0.27
  accept? Every grammar `:TSInstall` builds from the pinned nvim-treesitter ref, every one
  `tree-sitter-generate` writes, and the bundled `tree-sitter-rust` (which itself depends on
  `tree-sitter ^0.25`) must still load. If 0.27 drops ABI 14, some catalog grammars stop
  loading; find out which before deciding.
- **QuickJS.** `tree-sitter-generate` 0.27 depends on `rquickjs ^0.12.2`, while the workspace
  evaluates `grammar.js` with `rquickjs` 0.14. Today 0.26's `^0.10` dependency is behind a
  feature we don't enable (one `rquickjs` in the lock). Confirm that is still so, or two
  QuickJS C builds get linked.
- **The query API.** The just-landed treesitter query API for plugins (`2026-11-21` plan) leans
  on query / cursor / predicate types. Check the 0.27 changelog for `Query`, `QueryCursor` and
  `StreamingIterator` changes.

6a, native: bump, fix compile errors. **Verify:** every `bemtvi-ts` suite (folds, injections,
textobjects, query bridge, query inherits, …), `treesitter_*`, `syntax`, `indent`,
`ts_install` with `--include-ignored` (live install + in-process generate), and phase 1's CI
job.

6b, web: `crates/bemtvi-edithost/treesitter/package.json` (`web-tree-sitter` + the bundled
grammar packages, regenerated under `npm install --before=…`) and `web/grammars.js` versions.
Bundled grammar `.wasm` files built against an older ABI have to load under
web-tree-sitter 0.27; the 0.26.9 pin exists precisely because modern grammar packages need a
matching runtime. **Verify:** `build.sh`, then `verify-treesitter.mjs` (bundled highlight,
runtime `:TSInstall zig` from the CDN, OPFS persistence), plus a by-hand look at each bundled
language in the browser.

## Phase 7 — emsdk 6 → 7 (target 7.0.1; 6.0.13 as fallback)

`build.yml` pins `mymindstorm/setup-emsdk` to emsdk `6.0.0`, and `~/emsdk` has the same locally.
Rust's `wasm32-unknown-emscripten` target links against Emscripten's ABI, and the edit-host
already needed `-fwasm-exceptions` to fix an exception-handling mismatch once. A major Emscripten
bump is where that kind of break comes from.

1. Locally: `./emsdk install 7.0.1 && ./emsdk activate 7.0.1`, then
   `crates/bemtvi-edithost/build.sh` and every `web/verify-*.mjs`.
2. If 7.x breaks the Rust link and the fix isn't small, take 6.0.13 (same major, also past the
   2-day rule) and record why 7 waits.
3. Bump `build.yml`, then `gh workflow run edge.yml` to build the real web artifact in CI.

## Phase 8 — LSP e2e tooling majors

`scripts/lsp-e2e/npm` (the server binaries the LSP end-to-end script installs; not shipped):

- `typescript` 5.9 → 7.x is the native (Go) rewrite. `typescript-language-server` drives
  `tsserver` from the `typescript` package, and TS 7 may not ship a `tsserver` at all. Check
  whether `typescript-language-server` 6 supports TS 7 before bumping either. If it doesn't,
  hold `typescript` on 5.9.x, bump only the language server, and record the hold here.
- `typescript-language-server` 5 → 6, `yaml-language-server` 1.25 → 2.0: read their breaking
  notes for settings / initializationOptions our `lsp_config` presets send.

**Verify:** `scripts/lsp-e2e/lsp-e2e.sh` for `ts_ls`, `yamlls` (and the rest of the manifest
as a regression check), `npm ci` integrity passing on the new lock.
