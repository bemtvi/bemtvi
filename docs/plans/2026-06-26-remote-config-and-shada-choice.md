# Remote vs local config (and shada) when connecting to a daemon

Status: Phases 1–3 landed (2026-06-26).

## As-built notes (Phases 2–3)

A few specifics differ from the sketch below; the shape is the same:

- **Per-instance directory mirror (Phase 3 superseded Phase 2's single file).**
  The remote shada lives in `<state_dir>/remote[/ns/<NS>]/` holding the same
  `<pid>.<nanos>.<seq>.redb` per-instance files as the local store. Each session
  downloads + merges every sibling, uploads its own file, and at clean exit
  removes the siblings it absorbed — so the dir is bounded by live sessions, not
  launches (verified by `remote_shada_compacts_and_merges_across_sessions`). The
  dir is a *sibling* of the daemon's native `shada_dir()` and `read_dir` is
  non-recursive, so a daemon that also runs nxvim natively never globs these as
  its own store (no extension trick needed — Phase 2's `.shada` rename is gone).
  Two concurrent remote sessions merge by recency; cross-machine liveness can't
  be detected, so a live sibling may be transiently removed and re-uploaded
  (harmless, like the local model's crash-redundancy).
- **`--shada-namespace` isolates a project's remote shada** under `remote/ns/<NS>/`,
  mirroring the local layout (`remote_shada_namespace_isolates_projects`). The
  GUI has no `--shada-namespace` yet, so it always uses the global remote dir.
- **Two new generic fs seam ops:** `fs_mkdir` (the session creates its
  `remote[/ns/<NS>]` dir before the first upload — `fs_write` doesn't create
  parents) and `fs_remove` (clean-exit compaction). Both are plain whole-path
  ops on the existing fs leg, added to `HostFs` (default-erroring; `StdHostFs`
  real) + `HostFsAsync` + `RemoteHostFs`. No offset-addressed primitives.
- **Upload reuses the editor's existing daemon fs handle.** The
  `Arc<dyn HostFsAsync>` is cloned in `run` *before* it's moved into
  `NativeEffects`; no second connection/handle is needed. Checkpoint uploads are
  fire-and-forget + coalesced (a `busy` flag); the clean-exit upload + remote
  compaction are awaited in `run` after `shada_flush_final`.
- **`current_path()` on `ShadaStore`** exposes the staged instance file the
  uploader reads after each flush (the staged redb *is* the on-remote artifact).

---

Status: planned (2026-06-26)

## Goal

When the editor connects to a remote daemon, let the user choose whether the
session runs the **daemon's** config or the **local** config — and make shada
(cross-session marks/registers/history/session) follow that choice:

- **remote config → remote shada** (lives on the daemon's machine, travels with
  the remote workspace),
- **local config → local shada** (current local behavior).

### Decisions (locked)

- **Selection = a CLI flag.** Native clients default to **local** config +
  local shada; the new flag opts into remote. The **web** client defaults to
  **remote** (it has no local disk / local config) — its current behavior, so no
  web change is needed for the default.
- **"Remote shada" = stored on the remote machine, as a real redb store**
  (Approach **A**, whole-file transfer). The daemon fs seam is whole-file
  read/write + dir-list only (no random access, no cross-machine lock), so redb
  cannot run *live* over the seam. Instead: download the redb file over the
  seam at connect, run the unchanged local `RedbFileStore` against a **local
  staging copy** (fast random access, real local lock), and upload the bytes
  back on checkpoint / clean exit. No new offset-addressed wire primitives.

## Current behavior (baseline)

- `crates/nxvim/src/main.rs`: connecting (`--connect-daemon`, or a `nxvim://`
  URI) routes through `run_with_daemon` / `run_with_daemon_quic` →
  `run_edit_host_session`, which **always** does
  `client.config.fetch().await` → `materialize_remote_config(bundle)` and points
  `config_dir`/`runtimepath` at the local cache. There is no way to keep local
  config.
- Shada is **always** local: `ServerInit::shada = Some(shada.store())` (a
  `RedbFileStore` under `stdpath("state")/shada`), in both the embedded and the
  edit-host paths. `shada.rs`'s module header states shada is "never on the
  remote daemon" — this invariant changes (for the remote-config case only).
- The `config_bundle` RPC (`daemon.rs` / `remote_config.rs`) returns
  `[config_dir?, [runtimepath…], [[abspath, bytes]…], [ts_lang…], cwd?]`.

## Phase 1 — config source choice (shada stays local)

Ship the config-source switch first; shada follows in Phase 2.

1. **CLI flag.** Add `--remote-config` (bool) to `Cli`
   (`crates/nxvim/src/main.rs`). Only meaningful with a connect target; fail
   loud (`bail!`) if given without `--connect-daemon`/`nxvim://`. Define a small
   `enum ConfigSource { Local, Remote }` and thread it through
   `run_with_daemon` / `run_with_daemon_quic` → `run_edit_host_session`.

2. **Extend `config_bundle`** (backward compatible, like the `cwd` field was):
   - request arg `include_files: bool` (default true for old callers) so local
     mode can fetch the cheap metadata (cwd, ts_languages, state_dir) **without**
     transferring every remote config file.
   - reply gains `state_dir` (the daemon's `shada::shada_dir()` base) — consumed
     in Phase 2; harmless/ignored now. Older peer omits it → `None`.
   - Touch points: `serve_config_bundle` / `encode_config_bundle` /
     `collect_config_bundle` (`daemon.rs`, `lib.rs`), `RemoteConfigBundle` +
     `decode_config_bundle` (`remote_config.rs`), `RemoteConfig::fetch`
     (signature grows an `include_files` arg).

3. **Branch in `run_edit_host_session`:**
   - `ConfigSource::Remote` → today's path (fetch full bundle, materialize,
     seed `remote_cwd`, `ts_autoinstall`).
   - `ConfigSource::Local` → `default_runtime()` for `config_dir`/`runtimepath`
     (the embedded path), but still fetch the **lite** bundle
     (`include_files = false`) to seed `remote_cwd` (so relative paths resolve on
     the daemon's disk — buffers/fs are still remote) and `ts_autoinstall`.
   - Shada stays `shada.store()` (local) in both branches this phase.

4. **Tests** (`nxvim-server` edit-host suite): connect with and without
   `--remote-config`; assert the loaded config differs (e.g. a sentinel option
   the daemon's `init.lua` sets vs the local one). Reuse the per-leg
   `RemoteConfig::connect` test seam.

## Phase 2 — remote shada (Approach A, single remote file)

1. **Remote shada path.** Client computes `<state_dir>/shada` (+ `ns/<NS>` when
   `--shada-namespace` is set) from the bundle's `state_dir`. v1 uses a **single
   fixed remote file** in that dir (e.g. `store.redb`) — last-writer-wins across
   concurrent remote sessions (documented; see Phase 3 for the upgrade).

2. **Download at connect** (before building the editor, alongside the config
   fetch in `run_edit_host_session`): `client.host_fs.read(remote_file)`. If
   `["file", bytes]` → write to a local **staging dir** (per-pid, under
   `$XDG_CACHE_HOME/nxvim/remote-shada/<pid>`); if `["new"]` → start empty.
   Construct `RedbFileStore::new(staging)` (unchanged) as `ServerInit::shada`.
   The staged file is merged as a sibling into our fresh instance file exactly
   like the local model.

3. **Upload back.** Isolate the async sync-back as an EditHost capability
   (`remote_shada: Option<RemoteShadaSync>`), so the `ShadaStore` trait stays
   sync (no async leak). It holds the `host_fs` handle + the remote file path +
   the store's instance-file path (add `ShadaStore::current_path() -> Option<…>`,
   default `None`; `RedbFileStore` returns `self.path`).
   - **checkpoint** (`shada_checkpoint`): after `store.flush(false)`, read the
     instance file bytes (sync) and **fire-and-forget** an async
     `host_fs.write(remote_file, bytes)` (best-effort, matches the existing
     checkpoint contract).
   - **clean exit**: after `shada_flush_final` (sync, `compact=true` locally),
     `await` a final `host_fs.write` in `run_server` **before** `legs.shutdown()`
     so the last state is durable on the remote. (Fire-and-forget on a sync
     `flush` can't guarantee it lands before process exit.)

4. **Couple to config source:** `ConfigSource::Remote` → remote shada (staged +
   synced as above); `ConfigSource::Local` → local shada (today's path,
   untouched). `--shada-namespace` / `--restore-session` work in both.

5. **Update the `shada.rs` module header** — the "never on the remote daemon"
   invariant now has the remote-config exception. State the Approach-A model
   (staged local redb + whole-file sync-back), and that the daemon itself still
   runs no shada logic (it's pure fs I/O; the store logic stays client-side).

6. **Tests:** with `--remote-config`, set a mark + register, quit (final upload),
   reconnect, assert the state restores from the remote (the harness daemon's
   fs). Assert the **local** shada dir is not written. Mirror the per-leg
   `host_fs` test seam for the round trip.

## Phase 3 — (future) per-instance remote mirror

For faithful multi-client merge + compaction on the remote: mirror **all**
per-instance `.redb` files (download via the existing `fs_read` "dir" listing,
upload each on flush) and add one new seam op, `fs_remove`, so clean-exit
compaction deletes absorbed siblings remotely instead of letting the remote dir
grow. Deferred — v1's single-file last-writer-wins is enough for the common
single-session case, and cross-machine locking is absent either way.

## Out of scope / notes

- The daemon stays pure I/O — it runs **no** shada logic; only whole files cross
  the wire. All store logic remains client-side.
- Web keeps its current remote default (OPFS JSON `PersistState` blob); no web
  toggle in this plan.
- Per the project cadence: implement one phase at a time, commit, pause for
  review. Ship a runnable `examples/remote-config/` demonstrating both modes
  once Phase 2 lands.
