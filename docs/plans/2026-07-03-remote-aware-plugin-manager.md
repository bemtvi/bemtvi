# Remote-aware plugin manager: manage plugins locally even in a daemon session

**Status:** done (Phases 1–3) · **Date:** 2026-07-03

## The bug

In a native `--connect-daemon nxvim://…` session the editor is *born remote*:
`run_edit_host_session` (`crates/nxvim/src/main.rs`) injects the daemon's
`host_proc` / `host_fs_async` / `fs_jobs` into `ServerInit`, so **every** `nx.fs`
and `nx.run` (and thus every process spawn) routes to the daemon (main.rs comment:
*"every fs/process/LSP/Lua-fs path is routed to the daemon"*).

But the `nx.plugins` manager (`crates/nxvim-lua/src/prelude/plugins.lua`) is a
**local-VM** concern: it clones repos with `nx.run` (git), discovers `plugin/`
scripts and checks presence with `nx.fs`, and then adds each plugin dir to the
runtimepath via `nx._add_rtp` and loads it with `require` — and `_add_rtp` /
`require` / `package.path` resolve **locally**, on the machine running the editor.

So in a daemon session the manager clones and inspects plugins on the **remote**
while `require` loads from the **local** disk. The two never meet → plugins don't
load. This is wrong in both modes:

- **no `--remote-config`** (local config, default): the local `init.lua` declares
  `nx.plugins`, but clone/discover/source hit the daemon. It should behave exactly
  like a local launch — plugins managed on local disk.
- **`--remote-config`**: the remote config is fetched and *materialized locally*
  (`remote_config.rs`), then run locally. Its `nx.plugins` declarations should
  therefore also **clone into the local data dir** — "syncing should download the
  plugins locally."

Either way: **plugin management is always local.** The daemon only backs *runtime*
file editing (buffers, `nx.fs`/`nx.run` a plugin calls while running), which
correctly stays remote.

## Design

Add a **local-always seam** for the two op families the manager uses, routed to a
local backend regardless of the session's remote routing:

- `nx.fs.*` management ops → a local `StdLuaFs` (native: real disk; wasm: MEMFS).
- git spawns (`nx.run`) → a local `StdHostProc`.

Runtime plugin code keeps using session-routed `nx.fs` / `nx.run` (remote in a
daemon session) — only the *manager's own* clone/discover/source switches to the
local seam.

### Mechanism

Thread a `local: bool` through the two event-loop ops the manager needs, and give
the event loop a local backend for each:

- `LoopOp::Fs { id, job, local }` and `LoopOp::Spawn { …, local }` (ops.rs).
- Lua seams `nx._local_fs_op(job, id)` / a `local` argument on `nx._system_async`,
  bound in install.rs to push the op with `local = true`. `nx._fs_op` /
  `nx._system_async` default `local = false` (unchanged runtime behavior).
- `EventLoop` gains `local_fs: FsBackend::Local(StdLuaFs)` and
  `local_host_proc: StdHostProc`, always present (native). A `local`-flagged
  `Fs`/`Spawn` routes there instead of the session `fs`/`host_proc`. When the
  session is *already* local (no daemon), local == session, so no behavior change.
- effects.rs forwards `local` into the `LoopCommand`; evloop.rs picks the backend.
- wasm: `local` fs routes to the edit-host's local `StdLuaFs` (MEMFS); a local
  spawn is a loud no-op (a browser has no git — plugin *sync* on web is already
  N/A; plugins arrive via `config_bundle`). Native is the target of this fix.

### plugins.lua

A tiny local-fs shim (`exists` / `readdir` / `read_text` / `mkdir`, over
`nx._local_fs_op`) and a `local_run` (git over the local spawn). Replace the
manager's management-path `nx.fs.*` / `nx.run` calls (`source_runtime`,
`source_config`, `clone`, the present-checks in `load` / `activate_eager`) with the
local shim. Nothing else in `nx.*` changes — runtime plugin code is untouched.

## Phases

### Phase 1 — native local seam + plugin manager rewire (test-driven) ✅ done

- ops.rs: `local` on `LoopOp::Fs` / `LoopOp::Spawn`.
- install.rs: `nx._local_fs_op`; `local` arg on the spawn bridge.
- evloop.rs: `local_fs` + `local_host_proc`; route `local`-flagged ops.
- lib.rs: build + pass the local backends (native `StdLuaFs`/`StdHostProc`).
- effects.rs: forward `local`.
- plugins.lua: management ops → local shim.
- Test `crates/nxvim-server/tests/plugins.rs` (or a new daemon test): a daemon
  session whose remote fs is a *different* tree; a `nx.plugins` local-dir plugin
  present only on the local disk loads and its `plugin/` script sources — proving
  management stayed local.

### Phase 2 — wasm compile parity + web verify ✅ done

- `local` threaded through the wasm fs seam: `WasmEffects::fs_op(id, job, local)` →
  the `fs_ops` sink → `fs_job_to_json` emits `"local"` → the Worker
  (`drainFsOpRequests`) routes a `local` op to OPFS, never the daemon
  (`daemonUri && !forceLocal`). A `local` *spawn* (git) fails loud on web (a
  browser has no local process host; web plugins ride `config_bundle`).
- `web/verify-fs-op.mjs` gains a check: against a real `nxvim --daemon`, a session
  `nx.fs.exists` sees a daemon-only file (true) while `nx._local_fs_op` exists on
  the same path is false (OPFS) — proving the local↔session split on web.
- Native + `--no-default-features` + the real `wasm32-unknown-emscripten` build
  (`build.sh`) all compile green. (The Playwright verify needs a browser harness —
  not run in the dev container here; the wasm build is confirmed.)

### Phase 3 — docs + example ✅ done

- `docs/edit-host-split.md` (split-brain filesystem section): a "Plugin management is
  always local" note — `nx.plugins` clones/discovers/sources on the local disk in every
  session; distinct from the `pack/*/start` plugins a `--remote-config` session fetches
  in the `config_bundle`.
- `examples/remote-config/README.md`: a "Two kinds of plugin, two paths" note making the
  same distinction concrete against the example's `pack/*/start` plugin.

## Out of scope

Running git *on the daemon* to pre-stage plugins there (opposite of the ask), and
cross-session plugin caching.
