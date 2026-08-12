# Remote-aware `:cd` / `:pwd` / `getcwd` in a daemon session

## The bug

In a daemon/SSH session the **edit-host runs locally** while files live on the
**remote daemon**. Two independent "current directories" exist today:

1. **The daemon's process cwd** (remote). Relative `:e foo.txt` resolves against
   it, because `drain_pending_opens` ships the *raw* relative path over `fs_read`
   and the daemon resolves it against its own `std::env::current_dir()`. This is
   why `:e /`, the remote explorer, and `btv.fs` reads all see the remote fs.
2. **The edit-host's `DirState`** (`crates/bemtvi-server/src/cwd.rs`), seeded from
   the **local** `std::env::current_dir()` at startup (`lib.rs:1024`). `:cd`,
   `:pwd`, `vim.fn.getcwd`, and `fix_current_dir` all drive / read it via
   `std::env::set_current_dir` / `current_dir` (`excmd.rs:244-294`,
   `lifecycle.rs:544`, `install.rs:864`).

So `:pwd` prints the *local* cwd, `:cd` chdir's the *local* process, and
`getcwd()` reports the *local* cwd — none of them touch the remote. That is the
bug: `:cd` / `:pwd` "act locally" while everything else is remote.

## The model we want

In a daemon session **the daemon's process cwd is the one true cwd**. The
edit-host's `DirState` becomes a mirror of it (per-scope `:lcd`/`:tcd`/`:cd`
bookkeeping included), and the local process cwd is left out of it entirely (a
remote-only path can't be `set_current_dir`'d locally anyway). Concretely:

- **`:cd` / `:tcd` / `:lcd`** validate + canonicalize the target on the daemon
  and chdir the daemon's process, so subsequent relative opens follow. Async
  (off-tick), like every other daemon fs op.
- **Focus change** (`fix_current_dir`) re-points the daemon's single process cwd
  at the newly-focused window's effective dir — so per-window `:lcd` still scopes
  relative resolution correctly with one remote cwd.
- **`:pwd` / `getcwd()`** read the authoritative cwd (the `DirState` effective
  dir / a `btv._cwd` mirror), not `std::env`. Equivalent to today for local
  sessions; correct for remote.
- **Startup** seeds `DirState` from the daemon's cwd (handshake), so a fresh
  remote session's `:pwd` already shows the remote dir.

Local (non-daemon) sessions are unchanged: `DirState` ⇄ process cwd as today.

The chosen approach is **daemon-validated (async)** — it validates existence
(real `E344` on a missing remote dir), does real canonicalization (symlinks), and
keeps relative opens following `:cd`, at the cost of the cd "landing" a tick later
and a new `fs_chdir` protocol leg. (The rejected alternative was edit-host-side
optimistic/lexical cd with no remote validation.)

## Phase 1 — read the authoritative cwd; seed it from the daemon

Goal: `:pwd` and `getcwd()` report the daemon's cwd in a remote session; local
sessions unchanged. `:cd` still local-only after this phase (Phase 2).

- **Seed `DirState` from the daemon's cwd.** Append the daemon's
  `std::env::current_dir()` as a trailing field of the `config_bundle` reply
  (`serve_config_bundle` / `encode_config_bundle` in `daemon.rs`; the
  positional-iterator `decode_config_bundle` tolerates the new trailing element).
  Surface it on `RemoteConfigBundle.cwd: Option<String>`, thread it through
  `main.rs` into a new `ServerInit.remote_cwd: Option<PathBuf>`, and have
  `lib.rs run()` seed `DirState::new(remote_cwd.unwrap_or_else(local cwd))`.
- **Authoritative-cwd mirror.** The server already keeps `DirState` as the source
  of truth; expose the current effective dir to Lua as `btv._cwd` (set wherever the
  buffer/cursor mirrors are refreshed in `effects.rs`, and on `:cd`/focus change).
  Re-point `vim.fn.getcwd()` to read `btv._cwd` (fall back to `std::env` when the
  mirror is unset, preserving the local path math). Keep the mirror equal to the
  process cwd in local sessions so nothing observable changes there.
- **`ex_pwd` reads `DirState::effective`** instead of `std::env::current_dir()`.

Tests (new `tests/daemon_chdir.rs`, mirroring `daemon_explorer.rs`'s fake-daemon
harness; the existing local `tests/chdir.rs` stays green):
- a remote session started in `/virtual/proj` → `:pwd` echoes `/virtual/proj` and
  `getcwd()` returns it (the path can't exist locally → it crossed the wire).

## Phase 2 — `:cd` over the wire  *(done)*

Goal: `:cd`/`:tcd`/`:lcd` change the validated remote cwd; relative opens follow;
`E344` on a missing remote dir; `DirChanged` fires.

**Key constraint discovered while building:** `serve_quic` serves *many concurrent
sessions in one daemon process*, so the daemon must **not** `std::env::set_current_dir`
(a process-global cwd would corrupt the other sessions). So the design is: the
**edit-host owns the logical cwd** (`DirState`), and `fs_chdir` is a *pure*
validate+canonicalize — it never mutates the daemon's process cwd. The edit-host
absolutizes its own relative paths against `DirState` before they cross the wire.

- **`fs_chdir` daemon leg.** `fs_chdir [path] -> ["ok", canonical]` / loud `E344`.
  The daemon expands `""`→`$HOME` / `~`→home (its own environment), validates the
  target is a directory via `HostFs::read_dir`, and returns `HostFs::canonicalize`
  — *no* process chdir. `serve_chdir` in the `serve_fs_daemon` request arm;
  `RemoteHostFs::chdir` + a `HostFsAsync::chdir` (default = loud "Unsupported", so a
  backend without it fails rather than silently succeeding).
- **No `bemtvi-core` change.** `:cd` is already a *server*-side ex-command
  (`excmd.rs`), so the off-tick job stays server-side (no `PendingChdir` in core).
  `ex_chdir`'s remote branch resolves `-` (DirState.prev) and relative args (join
  `DirState::effective`) edit-host-side, passes `""`/`~`/absolute through for the
  daemon, then calls `HostEffects::fs_chdir(target, ChdirCtx{scope,win,tab})`.
- **Apply on ack.** `NativeEffects::fs_chdir` spawns the daemon `fs_chdir`; the
  reply lands on the run loop's `chdir_done_rx` arm → `on_chdir_dones` →
  `apply_chdir`: on `["ok", canonical]` it `DirState::set` + refreshes `btv._cwd` +
  fires `DirChanged`; on error it echoes the daemon's `E344` — no silent swallow.
- **Relative opens follow `:cd`.** `drain_pending_opens` absolutizes a *relative*
  open against `DirState::effective` before `fs_fetch` (the daemon has no per-session
  cwd to resolve them). Absolute paths cross unchanged. (Side effect: a relative
  remote `:e foo` now names its buffer with the absolute path — consistent with the
  all-absolute remote-buffer convention, and it keeps reload identity stable.)
- **`fix_current_dir` remote-aware.** Already handled in Phase 1: in a daemon
  session it only refreshes the `btv._cwd` mirror to the focused window's effective
  dir (no local `set_current_dir`, no daemon round trip — the daemon is stateless,
  so a focus switch just re-points which `DirState` entry `getcwd` reads).

Tests (`tests/daemon_chdir.rs`): `:cd <abs>` moves the cwd + a relative `:e` follows;
`:cd <relative>` resolves against the effective dir; `:cd <missing>` → `E344`, cwd
unchanged; `:lcd` is window-local across a focus switch.

## Phase 3 — optimistic `:cd` + `DirChanged` on focus  *(done)*

Phase 2 made `:cd` validate remotely but land a *tick later*, so a quick `:cd X`
then relative `:e Y` (or `getcwd`) saw the **old** cwd until the ack. Phase 3
closes that race and adds the focus-switch announce.

- **Optimistic move.** `ex_chdir`'s remote branch now moves `DirState` + `btv._cwd`
  *immediately* (lexically resolving `-`/relative/absolute targets; `""`/`~` still
  defer, since only the daemon knows its `$HOME`), so an `:e`/`getcwd` in the same
  tick sees the new dir. The announcing `DirChanged` is deferred to the ack.
- **Reconcile on ack.** `DirState::set_optimistic` returns a `CdUndo` (a snapshot of
  the three scope slots `set` touches). On `["ok", canonical]`, `apply_chdir`
  reverses the optimistic intermediate and installs the *canonical* dir cleanly
  (correct `:cd -` `prev`, symlinks resolved), then fires `DirChanged` — unless a
  later `:cd` already superseded it (the guarded `rollback_optimistic` reports
  that). On `E344`, it rolls the optimistic move back and echoes the error; no
  `DirChanged` ever fired for the rejected dir.
- **Seam refactor.** `HostEffects::fs_chdir(target, token: u64)` now carries an
  opaque token into `EditHost::pending_chdirs` (which holds the scope/win/tab +
  `CdUndo`), so the `CdUndo`/`CdScope` types stay crate-internal — the public trait
  signature is just primitives (no `ChdirCtx`/`CdScope` re-export).
- **`DirChanged` on focus (remote).** `publish_cwd_mirror` now reports whether the
  cwd actually *moved* (tracked in `published_cwd`); `fix_current_dir`'s daemon
  branch fires `DirChanged` when a window/tab switch crosses a `:lcd`/`:tcd`
  boundary — the remote analogue of the local announce.

Tests (`tests/daemon_chdir.rs`): `:cd X` then an *immediate* relative `:e` (one
feed, no wait) reads `X/file`; a rejected `:cd` rolls the optimistic move back;
a focus switch across an `:lcd` boundary fires `DirChanged`.

Known limitation: two `:cd`s issued faster than one network round trip both move
optimistically; the guarded rollback keeps `DirState` consistent (the newer one
wins, the older one's ack is dropped if superseded), but the transient mirror may
flicker. Not worth more machinery — interactive use never hits it.

## Out of scope

- wasm/OPFS sessions (`host_fs_offtick` is also true there): the same
  `PendingChdir` path applies if/when an OPFS `chdir` is wired, but no OPFS cwd
  semantics are added here.
- `:cd -` history and `DirChanged` *patterns* are unchanged (still
  `DirState`-driven).
