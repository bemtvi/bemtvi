# Each editor keeps its own working directory

**The bug.** Several local editor servers can run in one process: `bemtvi-mitsuami`
mounts two editors in one window (`crates/bemtvi-mitsuami/tests/embed.rs`), and a GUI
`:workspace` swap runs the new session's server next to the old one while it winds down.
Each local server keeps the *process* cwd equal to its editor's effective dir. So editor
A's `:cd /a` and then editor B's `:cd /b` leaves A's relative `:e foo`, its LSP paths,
`btv.run`, its terminals and `btv.fs` all resolving against `/b`. A `:workspace` in one
editor moves the cwd under all the others.

**The fix.** A local server never touches the process cwd. The editor's own effective
dir (`cwd::DirState`, already per window / tab / global) is the only source of truth,
and every relative path resolves against it — which is what a daemon session already
does, because there the process cwd is the wrong machine's. Most of the resolvers exist
(`session_cwd`, `abs_buffer_path`, `spawn_cwd`, `rebase_fs_job`, the open rebase in
`drain_pending_opens`) and are switched off for local sessions by the `remote_cwd_seeded`
flag or a `has_remote_*` check.

## What depends on the process cwd today

Surveyed 2026-10-27 at `fe990510`.

- **Moves it (3):** `ex_chdir` (`excmd.rs` `set_current_dir`), `fix_current_dir`'s local
  branch (`lifecycle.rs`), the `--workspace` boot (`lib.rs`). `DirState` is seeded from
  `std::env::current_dir()` at startup (`lib.rs`).
- **Reads it (6):** core `absolutize_normalize` / `same_path` (buffer dedup, about 31
  callers through `find_buffer_by_path`), `lsp/mod.rs` `display_path` and `absolutize`,
  the PTY cwd's local branch (`terminal.rs`), the `getcwd` fallback (`bemtvi-lua`
  `install.rs`).
- **Hands relative paths to the OS (~20):** the core's `HostFs` calls (`:w`, `:wa`, the
  write commit, open/stat/read, restamp, change detection, atomic-save canonicalize);
  the server's `BufReadCmd` stat, `:source rel`, `source_init` (`-u rel`),
  `buffer_for_uri`'s fallback, the local image read; the spawns (`spawn_cwd` gated off
  locally, `LoopOp::ProcOpen` never rebased on either path); `btv.fs` jobs (rebase gated
  off locally); `btv.git` jobs (never rebased); the Lua VM's own `io.*`, `os.remove` /
  `os.rename`, `loadfile` / `dofile`, `package.path`'s `./?.lua`, `io.popen` /
  `os.execute`.
- **Legitimately the process's:** the CLI resolving its own arguments, the daemon
  reporting its own cwd to seed a client, and the boot seed (which should come from
  `ServerInit`).

## Design

- **Core.** `Editor` owns `cwd: PathBuf` with a setter. `absolutize_normalize` /
  `same_path` read it. The core hands `HostFs` absolute paths (it absolutizes before
  each call), and keeps a buffer's name as typed for display.
- **Server.** The three `set_current_dir` calls go. `DirState` is seeded from a new
  `ServerInit::cwd` (the CLI passes its launch dir; mitsuami one per editor), and
  `publish_cwd_mirror` — the one point every change passes through — also sets the
  core's `cwd`. A local `:cd` canonicalizes through `HostFs` and checks it's a
  directory. `fix_current_dir` becomes the daemon logic for both. The
  `remote_cwd_seeded` gates become "rebase relative paths" (true for local and daemon,
  false for serverless OPFS, whose paths are root-relative). `ProcOpen` gets `spawn_cwd`,
  git jobs a rebase, and the terminal / `display_path` / `absolutize` / `:source` /
  `buffer_for_uri` / `BufReadCmd` / image read take the session cwd. Children get `PWD`.
- **Lua.** `btv._cwd` is published before any Lua runs, and the `getcwd` fallback becomes
  an error. The prelude wraps the path-taking `io` / `os` / `loadfile` / `dofile` to join
  a relative path onto `btv._cwd`. `io.popen` / `os.execute` run a shell in the process
  cwd; documented, not rerouted.

## Phases

Each is committed and paused for review; `cargo test --workspace` green at each.

1. **Core and server.** `Editor::cwd`, `ServerInit::cwd`, no `set_current_dir`, the
   `HostFs` paths, LSP paths, the terminal, the server's own fs reads. The tests that
   `chdir` the process or assert it (`chdir.rs`, `workspace_chdir.rs`, `terminal.rs`,
   `lsp_nav.rs`, `lsp_config_keys.rs`, `picker.rs`, the `daemon_*` suites, `bemtvi`
   `cli.rs`, …) move to asserting the editor's dir. New: two editors in one process
   (`bemtvi-mitsuami/tests/embed.rs`), `:cd` in each, a relative `:e` in A opens A's file.
2. **Spawns, `btv.fs`, `btv.git`.** The rebase gates, `ProcOpen`, git jobs, `PWD`. Tests:
   `btv.run`, `btv.process.open`, `btv.fs.read` and `btv.git` with relative paths in two
   editors.
3. **Lua.** `btv._cwd` before any Lua, the wrapped `io` / `os` / `loadfile` / `dofile`.

## Open questions

- **Buffer names.** Absolutize at open, or keep the typed name and anchor it to the
  editor's cwd (as today, per process)? Vim keeps the full name and displays it
  relative. Absolutizing touches shada, session restore and the `%` expansion tests.
- **Third-party Lua** that calls `io.popen` / `os.execute` with relative paths still
  resolves against the process cwd (the launch dir). Wrapping them means prefixing
  `cd <dir> &&` to a shell line, which is fragile.

## Risks

- `DirChanged` timing changes for local sessions (to the daemon semantics: fire on a real
  boundary); `chdir.rs` pins it.
- A local `:cd` must canonicalize exactly as `set_current_dir` + `current_dir()` did
  (symlinks, `..`), or `DirState`'s comparisons drift.
- Windows clients on POSIX daemons go through `PathStyle`, not `std::path`.
- Other process-wide state (`$VAR`s) is shared by embedded editors the same way; out of
  scope here.
