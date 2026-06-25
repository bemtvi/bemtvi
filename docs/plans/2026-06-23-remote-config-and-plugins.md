# Remote config & plugins: fetched from the daemon, run locally

**Status:** proposed · **Date:** 2026-06-23

## Goal

In an edit-host (daemon) session, the user's **config and plugins come from the
remote daemon's machine**, not the local client's disk. They are *fetched* over
the daemon connection and *run locally* in the client's Lua VM — "fetched and
loaded from the remote, run locally."

Today this is the opposite. `run_edit_host_session` (`crates/nxvim/src/main.rs`)
resolves `config_dir` + `runtimepath` via `nxvim_server::default_runtime()` — the
**local** client's `$NXVIM_CONFIG` / `$XDG_CONFIG_HOME/nxvim` / `$HOME/.config/nxvim`
and its local `pack/*/start/*`. The seam comment is explicit: *"Config and the
keystroke path stay local; only fs/process/watch/LSP cross to the daemon."* That
is the line we are moving.

## Why prefetch-and-materialize (not lazy remote require)

Startup sourcing (`source_init` / `source_plugins`) runs inside the async `run()`
and *could* await the daemon. But Lua `require()`, `package.path`, and
`get_runtime_file` (`crates/nxvim-lua/src/host.rs`) resolve files **synchronously
inside Lua callbacks** — they cannot await, and blocking-over-async there hits the
PUC-Lua `pcall`-yield trap. So we fetch the whole config surface up front, write it
to a local cache, and point `config_dir` + `runtimepath` at the **local** copy.
Every existing synchronous Lua path (require, plugin discovery, runtime-file lookup)
then works unchanged against fetched files.

## Decisions (confirmed with the user)

- **Approach:** prefetch + materialize the remote tree into a local cache.
- **Freshness:** re-fetch fresh on every connect (simple, always correct).
- **Scope:** `config_dir` + everything reachable via `runtimepath`
  (`init.lua`, `pack/*/start/*`, `plugin/`, `after/plugin/`, `lua/` modules for
  `require`). Runtime `nx.fs` / `vim.fn` file reads already route to the daemon via
  the existing `fs_jobs` / `luafs` legs — out of scope here. Raw `io.*`/`os.*`
  redirection is explicitly out of scope.

## Environment switch: startup-only (born remote)

"Reloading the whole editor under the remote environment" is handled by the
**launch path**, not a live switch. When the process starts with a daemon target
(`--connect-daemon` / `nxvim://` URI), the entire editor is *born* remote: the Lua
VM (`LuaRuntime::new(runtimepath)`), `package.path`, config sourcing, plugin
sourcing, and lifecycle all initialize against the remote-derived
`config_dir`/`runtimepath`. There is no prior local editor to reload *from*, so no
in-place teardown or `:connect`-style live re-init is in scope. (If a live
environment switch is ever wanted, the robust shape is a session-preserving rebuild
through this same startup path — persist via the shada/session machinery the daemon
path already wires, rebuild the server, restore — but that is explicitly **not**
part of this plan.)

## Architecture

One new request/response leg on the existing daemon connection (multiplexed by
method namespace exactly like `fs_*`, routed by msgid — no new transport):

```
edit-host → daemon:  config_bundle []  →  { config_dir, runtimepath:[..], files:[(abspath, bytes)] }
```

The daemon computes its roots with the **same** `default_runtime()` it already
exposes — run on the *daemon's* machine, so it reflects the remote user's config —
walks each root with local fs, and returns the whole tree in one shot. The client
writes the files under a local cache root, rebases `config_dir` + each
`runtimepath` entry onto the cache, and feeds those local paths into `ServerInit`.

```
remote /home/u/.config/nxvim/init.lua
  →  $XDG_CACHE_HOME/nxvim/remote/<conn>/home/u/.config/nxvim/init.lua   (local)
```

`require("foo")` → local `package.path` (seeded from the rebased runtimepath) →
local cache file. Fully synchronous, fully local.

### Native artifacts: compiled locally, not fetched

Tree-sitter parsers (and any compiled helper) are **built locally on the client**,
so the bundle must not drag remote-arch binaries across — they'd be the wrong arch
and are regenerated locally anyway. The materialize walk **skips native artifacts**
(`.so` / `.dylib` / `.dll` and the local parser build-output dir); only source —
`.lua`, queries, colorschemes, plain text — is fetched. The client's local
tree-sitter compilation produces the parser objects in the local cache as usual.

**Lazily install the remote's parser set (via Lua).** Skipping the binaries would leave
a remote session without highlighting for languages the remote had set up. So
`config_bundle` also carries the daemon's **installed parser language list**
(`nxvim_ts::installed_parsers`). The server filters it to languages not already installed
here and hands the rest to Lua — `nx._remote_ts_autoinstall(langs)` registers a `FileType`
autocmd that `:TSInstall`s a language the first time a buffer of that type opens (deduped
per session). Dogfoods the public `FileType` + `:TSInstall` surface; the only Rust glue is
`set_up_remote_ts_autoinstall` passing the list (`ServerInit.ts_autoinstall`) to Lua,
registered before `init.lua` so the startup buffer's own `FileType` is caught. So you only
pay to build the parsers for filetypes you actually open, and opening never blocks.

## Phases

Each phase commits independently and pauses for review (repo cadence).

### Phase 1 — daemon `config_bundle` leg (wire + daemon side) ✅ done

- Add `CONFIG_BUNDLE` method constant + protocol doc-comment to `daemon.rs`.
- Daemon handler in `run_daemon_io` (`lib.rs`): call `default_runtime()`, walk each
  root with local fs (reuse the `collect`/`read_dir` helpers), encode
  `{ config_dir, runtimepath, files:[(abspath, bytes)] }`. Bound + fail-loud on
  unreadable roots (no silent empty bundle).
- Client decoder + a `RemoteConfig { rpc }` handle on `DaemonClient` (reuses the
  fs-leg connection's `Rpc`), exposing `async fn fetch() -> RemoteConfigBundle`.
- Test `crates/nxvim-server/tests/daemon_config.rs`: drive a daemon over an
  in-process pipe with `NXVIM_CONFIG` pointed at a temp dir containing `init.lua`
  + a `pack/*/start/*` plugin; assert the bundle returns both with correct
  roots/paths/bytes.

### Phase 2 — client materialize + rebase, wired into the session ✅ done

- New `crates/nxvim-server/src/remote_config.rs` (server crate so it's reusable and
  testable with the daemon harness): `materialize(bundle) -> (PathBuf, Vec<PathBuf>)`
  — write files under `$XDG_CACHE_HOME/nxvim/remote/<conn>/…` (cleared fresh per
  connect), return the rebased local `config_dir` + `runtimepath`.
- Wire into `run_edit_host_session` (`main.rs:567`): replace the local
  `default_runtime()` with `client.config.fetch().await` → `materialize(..)` →
  feed the rebased roots into `ServerInit`. Cover both the stdio split
  (`run_with_daemon`) and QUIC (`run_with_daemon_quic`), and the `nx_eval` path if
  it also runs over a daemon.
- Test: materialize a synthetic bundle to a temp cache, assert the rebased
  `config_dir`/`runtimepath` resolve the right local files.

### Phase 3 — end-to-end verification + example ✅ done

- E2E test: full edit-host session against an in-process daemon whose `config_dir`
  has an `init.lua` setting a distinctive option **and** a plugin that registers a
  user command / autocmd; assert (over the client RPC) the option took effect and
  the plugin loaded — proving config came from the **daemon**, not local disk.
  Also assert a `require("<module>")` from the remote `lua/` tree resolves.
- `examples/remote-config/`: a runnable remote config dir + a short README on
  launching `--daemon` on one side and connecting from the other.

### Phase 4 — the web build: fetch + materialize in the browser ✅ done

The native edit-host stages the daemon's config on disk; the **wasm** edit-host had none
of this — `mod daemon` / `mod remote_config` / `collect_config_bundle` are all
`#[cfg(feature = "native")]`, so a daemon-connected browser session only ever sourced its
local OPFS `/init.lua`. This phase makes the browser *born remote* too, reusing the same
materialize strategy: the edit-host targets `wasm32-unknown-emscripten`, whose `std::fs`
hits emscripten's in-memory **MEMFS** — exactly the synchronous FS Lua's `require` /
`package.path` and `nvim_get_runtime_file` read from. So "stage to a local cache, point
the roots at it" ports directly, with MEMFS as the cache.

- Un-gate the shared half: move `RemoteConfigBundle` + the wire decoder out of the
  native-only `daemon` module into `remote_config.rs` (now un-gated), and add
  `decode_config_bundle_bytes(&[u8])` (rmpv) so the wasm side can reconstruct the bundle
  from raw msgpack. The native daemon client reuses the same decoder.
- `EditHost::apply_remote_config(bundle)` (`#[cfg(not(native))]`, in `lib.rs`): materialize
  into `/nxvim/remote` (MEMFS) via `materialize_remote_config_into`, seed the rebased
  runtimepath into the VM (`LuaRuntime::add_runtimepath` — the typed twin of `nx._add_rtp`),
  seed the daemon's cwd, register `nx._remote_ts_autoinstall`, then `source_init` +
  `source_plugins` — the exact native order, over the staged FS.
- FFI `eh_apply_remote_config(h, ptr, len)` + a `_eh_apply_remote_config` export.
- `web/worker.mjs`: in a `?daemon=` session, fetch `config_bundle`, re-encode the reply to
  msgpack, hand the bytes to Rust, and **skip** the serverless OPFS config path (born
  remote). Shada stays local (out of scope, as on native).
- Test `web/verify-remote-config.mjs`: a real `nxvim --daemon` with `NXVIM_CONFIG` on Node's
  disk (unreadable by the page origin) ships an `init.lua` (sets an option), a `lua/` module
  (`require`d), and a `pack/*/start/*` plugin; the browser asserts all three took effect —
  proving config + plugins + `require` came from the daemon over WebTransport.

## Touch list

- `crates/nxvim-server/src/daemon.rs` — `CONFIG_BUNDLE` constant, encode/decode,
  client `RemoteConfig` handle, `DaemonClient` field.
- `crates/nxvim-server/src/lib.rs` — daemon-side handler in `run_daemon_io`.
- `crates/nxvim-server/src/remote_config.rs` — new: materialize + rebase.
- `crates/nxvim/src/main.rs` — `run_edit_host_session` fetch+materialize wiring.
- Tests: `daemon_config.rs`, `remote_config` unit-ish via harness, an e2e suite.
- `examples/remote-config/`.

## Out of scope

Cache-with-validation across sessions (incremental refetch) and raw `io.*`/`os.*`
redirection. (Native parser artifacts are handled by skipping them in the fetch —
tree-sitter is compiled locally — not deferred.)
