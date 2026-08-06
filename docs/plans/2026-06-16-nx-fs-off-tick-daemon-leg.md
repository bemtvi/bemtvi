# `nx.fs` off-tick + the daemon `luafs` leg over WebTransport

> **SUPERSEDED in part (2026-06-17, commit `474813f`) — see
> [[no-blocking-io-fs-async-only]] / the edit-host plan.** The "no blocking IO at all"
> consolidation changed two things this doc describes as current:
> - **Phase 3c is MOOT, not "remaining".** The synchronous `vim.fn` fs builtins
>   (`filereadable`/`isdirectory`/`getftime`/`executable`/`exepath`/`resolve`/`readblob`/
>   `glob`) and `nx._readdir` were **removed outright**, not given async parity — there is
>   no point keeping `vim.fn.*` sugar that can't behave like neovim's synchronous version.
>   `nx.fs` (async) is the sole fs API; `nx.lsp.enable`'s `find_root` walks it directly.
> - **The low-level per-op `luafs` leg + `RemoteLuaFs` are GONE.** They only existed to
>   back those sync builtins. Native-daemon `nx.fs` now uses the **`luafs_op`** leg (whole
>   `FsJob`, one round-trip, decomposed daemon-side) — the same leg + codec the wasm
>   edit-host uses. The event-loop actor holds `evloop::FsBackend` (`Local(StdLuaFs)` |
>   `Remote(RemoteFsJobs)`); nothing parks the editor thread. So where this doc says
>   "`RemoteLuaFs`", "the `luafs` leg", or "`vim.fn` fs builtins stay synchronous", read
>   the consolidated `luafs_op`-only design instead.
>
> The Phase 1/2/3a/3b mechanics below (the off-tick seam, `FsJob`/`FsValue` codec,
> `luafs_op`, OPFS, `luafs_watch`) are still accurate.

Status: **Phases 1–3b done (2026-06-16/17); Phase 3c (the `vim.fn` / `nx._readdir`
async-parity tail) is moot — those sync builtins were removed, not made async (see the
banner above).** `nx.fs` now works in every world — native, native daemon,
browser+daemon, and serverless browser (OPFS) — including the streaming
`nx.fs.watch` over the daemon. Makes `nx.fs` (and the `LuaFs` seam under it)
**truly async** and, on the browser edit-host, **routes its ops to the remote
daemon** instead of the local in-browser MEMFS. This is the
[edit-host plan](2026-06-09-edit-host-and-browser-lua.md)'s remaining **Phase 6
`luafs` leg over WebTransport** (its status line: *"the daemon lsp/sys_run/luafs
legs over WebTransport remain"*), and it supersedes the
[`nx.fs` API plan](2026-06-16-nx-fs-api.md)'s "Plumbing" posture (one-shot ops run
*synchronously inline* — that plan already named this as the planned next step:
*"Offloading a heavy op to the loop actor à la `nx._system_async` — fully off-tick
— remains a future optimization … the promise surface already permits it without
an API change."*).

## Problem

`nx.fs.*` one-shot ops are **promise-shaped but not actually async**: each
`nx._fs_*` bridge calls `resolve_lua_fs(&sh).<op>()` **inline on the editor thread**
and wraps the already-computed result in a pre-resolved promise
(`crates/nxvim-lua/src/install.rs` `_fs_*`; `crates/nxvim-lua/src/prelude/fs.lua`
`settle()`). Two consequences:

1. **It blocks the tick.** In a native daemon session the op is a synchronous wire
   round-trip on the editor thread (`RemoteLuaFs`, a dedicated link thread the
   edit-host parks on). A large/slow remote `readdir` janks the editor.
2. **On the browser it can't reach the daemon at all.** `resolve_lua_fs` defaults
   to `StdLuaFs` and the wasm edit-host **never calls `set_lua_fs`**, so browser
   `nx.fs` hits emscripten's in-memory **MEMFS**, not the host. A synchronous
   `LuaFs` *cannot* route to the daemon on wasm: the single Worker thread can't
   block on an async WebTransport reply. So "browser Lua plugin reads files from
   the remote daemon" — a primary use case — is impossible by construction today.

The fix for (2) **is** the fix for (1): route `nx.fs` through the **off-tick**
machinery (queue an op, return a *pending* promise, resolve it inbound on a later
tick), which is non-blocking on native and the *only* way to reach the daemon on
wasm.

## The precedent: the proc leg (Phase 6d, done)

`vim.system` / `nx.run` is the exact dual-path shape to mirror — an off-tick op
that runs on the **loop actor natively** and the **daemon over WebTransport on
wasm**:

| | native | wasm + daemon |
| --- | --- | --- |
| queue | `LoopOp::Spawn` → `LoopCommand::Spawn` | `HostEffects::proc_spawn` (`#[cfg(not(native))]`) |
| run | event-loop actor (`evloop.rs`, `tokio::spawn`) | daemon over WebTransport |
| result | inbound `LoopEvent` → `run_callback` | inbound `proc_spawned`/`proc_exited` pushes |
| Lua | pending promise resolved on the later tick | same |

`nx.fs` gets the identical treatment, with a `FsJob` op-descriptor in place of the
process spec and the typed fs result in place of `{code, stdout, stderr}`. The
daemon side already exists: `serve_luafs_op` (`daemon.rs:2232`) runs the **whole
`LuaFs` op set** through `["op", args…] → ["ok", payload] | ["err", msg]`; it is
served on the native daemon's `luafs` RPC link but **not yet over the WebTransport
leg**.

## Design

### The op descriptor + result (shared, `nxvim-lua/src/ops.rs`)

A plain-data `FsJob` enum (one variant per surfaced op — `Stat`/`Lstat`/`Exists`/
`Readdir`/`Read`/`ReadText{encoding}`/`Write`/`Append`/`Mkdir{recursive}`/`Rename`/
`Remove{recursive}`/`Copy{recursive}`/`Realpath`), carried by a new
`LoopOp::Fs { id, job }` (native) and the wasm `HostEffects::fs_op` path. The reply
is a new `CallbackArgs::FsResult { id, result }` where `result` is
`Result<FsValue, FsError>` — `FsValue` a small typed enum (`Nil` / `Bool` /
`Bytes` / `Stat(LuaStat)` / `Dir(Vec<LuaDirEntry>)` / `Text(String)`), `FsError`
the `{ code, message }` the promise rejects with. **The per-op result marshalling
moves out of the inline bridges into the `FsResult → Lua` conversion**
(`runtime.rs::run_callback`), so it happens once regardless of native/wasm origin.
`read_text` transcodes (`encoding_rs`) at marshal time, failing loud on invalid
bytes (unchanged semantics).

### The fs handle: `Rc<dyn LuaFs>` → `Arc<dyn LuaFs + Send + Sync>`

The enabling change. The actor (a separate `tokio` task) must hold the fs to run
ops off-thread, so the handle has to cross threads. Both backends already qualify —
`StdLuaFs` is `Send + Sync` (its fd table is behind a `Mutex`, per its own
doc-comment), `RemoteLuaFs` is a `Send + Sync` channel sender. Changes:
`Shared::lua_fs: Option<Arc<dyn LuaFs + Send + Sync>>`, `set_lua_fs`/`resolve_lua_fs`
return `Arc`, `ServerInit::lua_fs: Option<Box<dyn LuaFs + Send + Sync>>` (today only
`+ Send`), and the construction sites (`main.rs`, `nxvim-gui/session.rs`,
`daemon.rs`). The synchronous editor-thread callers (`vim.fn` fs builtins,
`host.rs::glob_paths`, the `nx._readdir`/`nx._read_file` legacy bridges) keep
calling it inline — `Arc` derefs exactly like `Rc`, so they are untouched. **`vim.fn`
fs builtins stay synchronous** (they're a bounded compat surface, not the async
`nx.fs` API); only `nx.fs` goes off-tick.
>
> **(Superseded — see top banner.)** The `vim.fn` fs builtins did NOT stay — they were
> removed (no blocking IO), so `Shared::lua_fs`/`set_lua_fs`/`resolve_lua_fs` and the
> `ServerInit::lua_fs` injection are gone too. The actor now holds an `evloop::FsBackend`
> (local `StdLuaFs` or a daemon `RemoteFsJobs`), not a shared `Arc<dyn LuaFs>`.

### Native path (Phase 1)

`nx.fs.x()` → `fs.lua` allocates a `cb_id`, registers the promise's
resolve/reject in `nx._cb_fns`, and calls `nx._fs_op(job, cb_id)` which pushes
`LoopOp::Fs`. The server drains it to `LoopCommand::Fs`; the actor runs the op on
`spawn_blocking` against its `Arc<dyn LuaFs + Send + Sync>` clone and sends
`LoopEvent::FsResult { id, result }`; `on_loop_event` routes it to `run_callback`,
which resolves/rejects the promise on that tick. (The actor receives the fs handle
once via a `LoopCommand::SetFs(Arc<…>)` sent right after `set_lua_fs`, mirroring how
`host_proc` is handed in.) This makes native bare **and** native daemon off-tick;
the `RemoteLuaFs` wire round-trip now happens on the actor's blocking pool, off the
editor tick.

### Wasm path (Phase 2 — the actual goal)

No actor on wasm, so the queue goes through the off-tick seam:
`#[cfg(not(native))] HostEffects::fs_op(&mut self, id, job)` forwards the op to the
daemon over WebTransport (gated on `has_remote_fs`; serverless-OPFS handling is
Phase 3). The daemon serves it with the **existing** `serve_luafs_op` exposed on a
new WebTransport `luafs` request (mirroring the proc leg's handler), and pushes the
`["ok"|"err", …]` reply back inbound; `EditHost::fs_op_result(id, …)` (the wasm twin
of `proc_exited`) decodes it into `CallbackArgs::FsResult` and resolves the promise.
The JS Worker (`web/`) gains the `luafs` request/response forwarding the proc leg
already models. Net: a browser Lua plugin's `nx.fs.readdir(remote_path)` returns the
daemon's listing.

### `fs.lua` rewrite

`settle()` stops running the bridge inline. Each op becomes the
`nx.ui.input`/`nx.run` shape: build a promise, `nx._next_cb_id()`, store
`resolve`/`reject` (the bridge fires `nx._run_cb(id, false, err, value)` — `err` the
`{code,message}` table or nil), and call `nx._fs_op(<job table>, id)`. `exists`
keeps its never-reject contract (maps an `ENOENT` reject to `resolve(false)` in the
wrapper). **No `nx.fs.*` surface change** — `nx.await`/`:next`/`:catch` are
unchanged; only the timing moves from "resolved-already" to "resolved next tick".

## Phases

- **Phase 1 — native off-tick (foundation, hermetic). DONE (2026-06-16).**
  `FsJob`/`FsValue`/`FsError` (`nxvim-lua/src/ops.rs`) + `LoopOp::Fs` +
  `LoopCommand::Fs` + `LoopEvent::FsResult` + `CallbackArgs::FsResult`; the
  `Arc + Send + Sync` fs-handle change (`Shared::lua_fs`, `set_lua_fs`,
  `resolve_lua_fs`, `ServerInit::lua_fs`); the op executor `run_fs_job`
  (`luafs.rs`, with the compound write/remove/copy helpers + `errno_code` moved
  there from `install.rs`); the actor `spawn_blocking` arm (`evloop.rs`); the
  one `nx._fs_op(job, cb_id)` bridge replacing the 13 inline `nx._fs_*` bridges;
  `fs.lua` rewritten to pending promises; result marshalling (`fs_value_to_lua`
  + `fs_stat_table`) in `run_callback`. `vim.fn` fs builtins and the legacy
  `nx._readdir`/`_read_file` stay inline (Arc derefs like Rc). The fs handle is
  handed to the actor at `EventLoop::new` (constructor, mirroring `host_proc`),
  **not** a `LoopCommand::SetFs` — preserves the actor's lazy start (a `SetFs`
  command would start it eagerly), one of the two options the plan sanctioned.
  Tests: the existing `tests/fs.rs` ops pass, **but** their observation had to
  move from "read the global in the next chunk" to **polling** for the off-tick
  settle (a 3-op chain like write→append→read reliably loses the now-real race;
  the single-op ones were passing only by timing luck and would flake under load
  — the redraw-race lesson). Added `op_is_pending_within_the_tick_and_settles_on_a_later_one`,
  a race-free proof the op is genuinely off-tick (the promise's `_state` is still
  `"pending"` synchronously within the queuing chunk — an inline op would have
  resolved it). Daemon fs suite + `complete`/`lsp_config` (root detection) green.
  Both feature configs build (the wasm `LoopOp::Fs` arm fails *loud* until the
  Phase 2 daemon leg lands — never silently hits MEMFS).
- **Phase 2 — the wasm `luafs` leg over WebTransport (the goal). DONE (2026-06-16).**
  A browser `nx.fs.*` op now routes to the connected daemon and back. Pieces:
  - **Wire codec** (`nxvim-lua/src/fswire.rs`): `FsJob`/`FsValue`/`FsError` ⟷
    `rmpv::Value` — `fs_job_from_value` (daemon decodes the request **map**),
    `fs_result_to_value` (daemon encodes the `["ok", <fs-value>] | ["err", code,
    message]` reply), `fs_result_from_value` (edit-host decodes it). Encode (daemon)
    and decode (edit-host) live *together* so they stay in lock-step. Bytes ride as
    msgpack `bin` so `read`'s content / `write`'s payload cross intact.
  - **Daemon leg** (`daemon.rs`): a new `luafs_op` method served on the **same**
    `StdLuaFs` leg as `luafs` (`serve_luafs_daemon_on` dispatches both) — `serve_fs_op`
    decodes the `FsJob` and runs the **whole** [`run_fs_job`](../../crates/nxvim-lua/src/luafs.rs)
    on a `spawn_blocking` thread, so a compound op (recursive copy/remove) decomposes
    into local daemon syscalls, **one** wire round-trip regardless of fan-out — *not*
    the low-level `serve_luafs_op` the plan first named (that's per-`LuaFs`-op; the
    high-level job needs the decomposition the executor already owns). `run_daemon_io`
    routes `luafs` **and** `luafs_op` to the luafs leg.
  - **Seam** (`edithost.rs` + `effects.rs` + `lib.rs`): wasm-only
    `HostEffects::fs_op(id, job)`; the wasm `LoopOp::Fs` arm forwards to it **iff a
    daemon is connected** (`has_remote_proc`), else rejects the promise *loud* (the
    serverless-OPFS `nx.fs` route is Phase 3 — never a silent MEMFS hit). Inbound:
    `EditHost::fs_op_result(id, reply_bytes)` decodes the msgpack reply through the
    codec into `CallbackArgs::FsResult` and resolves/rejects, then settles + repaints —
    the wasm twin of the native `LoopEvent::FsResult` arm.
  - **Wasm FFI** (`nxvim-edithost`): a `Sink.fs_ops` queue, `WasmEffects::fs_op`,
    `eh_take_fs_op_requests` (drains the queue as a JSON `[{ id, op, … }]` array), and
    `eh_fs_op_result(id, ptr, len)` (the reply rides as pointer+length — a `read`
    result is raw bytes). Both exports added to `build.sh`'s `EXPORTED_FUNCTIONS`.
  - **Worker** (`web/worker.mjs`): `drainFsOpRequests` forwards each job as one
    `luafs_op` request (`data` → `Uint8Array` so it crosses as `bin`), awaits the reply
    **within the unparked tick pass** (like `fulfillFsRequests`, not the push-based proc
    leg), and `landFsOpResult` re-encodes it to msgpack and hands it back — JS stays a
    near-dumb pipe (all typed encode/decode in Rust). Wired into the SAB run loop,
    `pump5cDaemon`, and the 5c feed/mouse/exec handlers; `fsOpWork` drives the repaint +
    shada-dirty conditions.
  - **The choice the plan flagged**: gated on **daemon-connected** (`has_remote_proc`,
    the only "is a daemon there" signal on wasm) rather than `has_remote_fs` (always
    `true` on wasm — OPFS) — gating on the latter would have hung serverless `nx.fs` on
    a wire with no daemon. Serverless routing stays Phase 3.
  - **Test** (`web/verify-fs-op.mjs`, Playwright against a real `nxvim --daemon`):
    `read_text` returns a file written only to the **daemon's** disk; `readdir` lists
    daemon entries with their kinds; `write` mutates the daemon's tree (Node reads it
    back); a missing path rejects with `err.code == "ENOENT"`. All pass — and crucially
    prove it is the daemon, not the in-browser MEMFS. Both cargo configs (native +
    `--no-default-features`) build + clippy clean; the wasm `eh.mjs` links; full
    workspace tests green.
- **Phase 3a — serverless-OPFS `nx.fs`. DONE (2026-06-17).** The decision was
  *route to OPFS* (not keep failing loud): a serverless browser `nx.fs.*` op now runs
  against the Origin Private File System — the same sandbox `:e`/`:w` persist to — so a
  plugin's `nx.fs` works in the common no-daemon mode, not just with a daemon. Pieces:
  - **Always enqueue** (`effects.rs`): the wasm `LoopOp::Fs` arm dropped the
    daemon-connected gate and now *always* `fx.fs_op(id, job)` — there is always *some*
    fs on wasm (OPFS is the serverless fallback), so it never needs the proc leg's "no
    host" loud reject and never silently hits MEMFS.
  - **OPFS executor** (`web/worker.mjs`): `drainFsOpRequests` routes
    `daemonUri ? daemonFsOp : opfsFsOp` — the same daemon-or-OPFS split `:e`/`:w` use.
    `opfsFsOp` is the JS twin of the daemon's `run_fs_job`: OPFS has no synchronous
    path-based fs (handle acquisition is async), so the op set is reimplemented in JS on
    the existing OPFS primitives (stat/lstat/exists/readdir/read/read_text/write/append/
    mkdir/rename/remove/copy/realpath), producing the same `["ok"|"err", …]` envelope the
    `fswire` codec decodes. Documented OPFS divergences (not faked): no symlinks
    (`lstat == stat`, no `link` kind), no `mode`/`ino`/`uid`/… (0), `mtime` best-effort
    (`lastModified`), `read_text` fails loud via `TextDecoder({fatal})` (EILSEQ) /
    unknown-label (EINVAL), DOMException names → libuv errno (`errCode`).
  - **Test** (`web/verify-fs-op-serverless.mjs`, Playwright, **no daemon**): write +
    read_text round-trip, mkdir + readdir (kinds), stat (type + size), copy, rename
    (old gone / new intact), remove (exists→false), and a missing-path ENOENT reject —
    all against OPFS. (Test gotcha worth noting: `execLua` renders its return via rmpv's
    Debug, so a stashed global reads back as `…Ok("nil")…` until set — poll on *that*,
    not `^nil$`; and a `page.evaluate` browser closure can't capture a Node-side var, so
    the global name must cross as an evaluate **argument**.) Both cargo configs build +
    clippy clean; the daemon `verify-fs-op.mjs` still green (no regression).
- **Phase 3b — `nx.fs.watch` over the daemon wire. DONE (2026-06-17).** A browser
  `nx.fs.watch` now streams daemon-side changes; daemon-only (serverless OPFS has no
  change source, so it fails the watch loud). The existing buffer-reconcile `fs_watch`
  leg was *insufficient* (a coarse single-path stat-**poll**, path-keyed, no kind/
  recursive) — so this is a **new** `luafs_watch` leg, not an extension of it:
  - **Daemon leg** (`serve_luafs_watch_daemon_on`): a `select!` loop owning a per-stream-
    `id` watcher map; arms reuse the event-loop actor's `start_fs_watch_coalesced` (made
    `pub(crate)`) — the *same* recursive 10 ms-coalesced `notify` watcher native
    `nx.fs.watch` rides — and forward its `LoopEvent::FsEvent` as `luafs_change [id, kind,
    paths]` / a terminal `luafs_watch_err [id, message]`. `run_daemon_io` routes
    `luafs_watch`/`luafs_unwatch` to it.
  - **Seam** (`edithost.rs`/`effects.rs`/`lib.rs`): wasm-only
    `HostEffects::fs_watch_stream(id, path, recursive)` + `fs_unwatch_stream(id)`; the
    wasm `LoopOp::FsWatch` arm forwards iff a daemon is connected, else rejects the
    stream's first pull loud. Inbound `EditHost::fs_watch_event(id, kind, paths)` /
    `fs_watch_error(id, message)` drive the stream's `nx._run_fs_watch` pump.
  - **Wasm FFI + Worker**: `Sink.fs_watch_arms`/`fs_watch_disarms`,
    `eh_take_fs_watch_requests` (JSON), `eh_fs_watch_change`/`eh_fs_watch_err`;
    `drainFsWatchRequests` forwards the arms/disarms and `applyDaemonNotifications`
    lands the `luafs_change`/`luafs_watch_err` pushes. `liveFsWatches` joins the
    async-park gate so the WebTransport reader stays live to receive change pushes
    (the same treatment armed watches / in-flight procs get).
  - **Native edit-host** (added later, closing the native-daemon half): `RemoteFsWatch`
    (`daemon.rs`) arms/disarms over the same leg and the Control demux decodes the pushes
    back into the very `LoopEvent::FsEvent`s the local `notify` watcher produces, so the
    actor routes `FsEventStart`/`FsEventStop` there whenever the session has one. It also
    **re-arms** every live watch on a re-dial (a fresh daemon knows about none of them) —
    the browser leg still ends its streams with `luafs_watch_err` on a drop instead.
    Tests: `crates/nxvim-server/tests/daemon_fs_watch.rs`.
  - **Test** (`web/verify-fs-watch.mjs`, Playwright + real daemon): a file Node creates
    on the daemon's disk surfaces in the browser watch stream (kind + path); a missing
    path rejects the stream loud; `:stop()` ends the iteration. (Debug note: the test
    spawns `target/debug/nxvim` — a daemon-side server change needs `cargo build -p
    nxvim`, not just the lib, or the spawned daemon is stale and drops the new method.)
- **Phase 3c — MOOT (resolved 2026-06-17, not as written).** Rather than route the
  `vim.fn` fs builtins / `nx._readdir` through the async path *or* leave them sync, they
  were **removed entirely** ("no blocking IO at all" — see the top banner and
  [[no-blocking-io-fs-async-only]]). `nx.fs` is the only fs API; the one in-tree consumer,
  `nx.lsp.enable`'s `find_root`, became async on `nx.fs.readdir`.

## Risks / decisions to confirm

- **Ordering.** Off-tick ops resolve in completion order, not call order. `nx.fs` is
  promise-based so callers already `await`/chain; document that two un-awaited ops
  have no ordering guarantee (same as `vim.system`).
- **`vim.fn` fs stays sync.** Keeping `isdirectory`/`glob`/… inline means browser
  `vim.fn` fs still hits MEMFS. Acceptable: `nx.fs` is the supported async surface;
  `vim.fn` is bounded compat. Revisit in Phase 3 if a plugin needs it.
- **Actor fs-handle handoff.** `SetFs` after `set_lua_fs` vs. constructing the actor
  with the fs — `set_lua_fs` runs at server init before the first tick, so either
  works; `SetFs` keeps the actor's construction unchanged.
