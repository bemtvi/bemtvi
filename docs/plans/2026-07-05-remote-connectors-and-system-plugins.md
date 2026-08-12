# Remote connectors + system-plugin tier: VS Code-style "install a connector, reload onto the remote"

**Status:** ✅ complete (Phases 1–5; `merged` config-source deferred) · **Date:** 2026-07-05

## Goal

Let a user install a **connector plugin** (e.g. `remote-containers`, `remote-ssh`)
that, on a single command, provisions a remote/container, installs + launches the
bemtvi daemon there, and **reloads the editor onto it** — the VS Code Remote model.
`:connect bemtvi://host` (or a connector-defined command) carries no extra
parameters: the connector owns the whole bootstrap.

Two things make this possible and are the substance of this plan:

1. A **system-plugin tier** — local, always-loaded plugins seeded by the *client*
   into **every** session it brings up (the initial local one *and* every
   post-swap remote one). A connector must be *injectable into this tier* so it is
   guaranteed present before any connect and survives the reload.
2. A **client-persistent, Lua-triggerable session swap** — bemtvi's equivalent of
   VS Code's "reload window": the client window lives, the server/VM behind it is
   torn down and rebuilt against a new backend.

## Background — most of the transport already exists

- **Edit-host split** (`bemtvi --connect-daemon`, `main.rs`): a full local editor
  whose fs/proc/watch/LSP **seams** point at a `--daemon` child. The daemon wire
  runs over the child's **stdin/stdout**, so
  `BEMTVI_DAEMON_CMD="ssh host bemtvi --daemon"` already gives an SSH-tunnelled remote
  session with no ports/NAT
  (`connect_daemon` in `daemon.rs`). `config_bundle` materializes config + plugins
  for the session. This is the VS Code Remote-SSH shape, minus provisioning and
  minus runtime control.
- **Reconnect** (`connect_daemon_reconnecting`, `daemon.rs:971`; the `_on` variant
  at `daemon.rs:892`): takes a
  spawn closure `F: Fn() -> Fut`, so reconnection **re-spawns** the command — a
  plugin-provided spawn command drops straight in.
- **GUI session swap**: the GUI already intercepts `:connect`/`:workspace` and
  performs a client-persistent swap (registered as no-op server commands via
  `client_init_lua`, `lib.rs:387`, consumed at `lib.rs:3111`). This is the "reload
  window" primitive; it is
  just not Lua-triggerable, not in the TUI, and only dials a QUIC listener.
- **Local-always seam** (`2026-07-03-remote-aware-plugin-manager.md`): the plugin
  manager clones/discovers/sources on the **local** disk regardless of session
  routing, via `btv._local_fs_op` / `btv._local_system_async`. A connector's
  provisioning (ssh, scp, docker) rides this seam.

### The gaps

1. There is **no system tier**: the prelude is baked (`PRELUDE_MODULES`,
   `runtime.rs:452`), and everything else (recommended, `cmdline_complete`,
   `init.lua`, managed plugins) runs *inside* a session and is not guaranteed to
   survive a swap.
2. The swap is **GUI-only, QUIC-only, and not Lua-triggerable**. The TUI has no
   live `:connect` at all (`main.rs:635` — connect target must be a launch flag).
3. **Connect is not Lua-pluggable**: the spawn command is a fixed env var
   (`BEMTVI_DAEMON_CMD`), decided at launch, with no provisioning step.

## Key insight — bemtvi skips VS Code's hard part

VS Code needs UI-vs-workspace **extension classification** because it runs two
extension hosts (local + remote). bemtvi's edit-host split runs **one VM**;
remoteness lives entirely in the **fs/proc/lsp seams**. After a swap:

- a plugin using **session-routed** `btv.run` / `btv.fs` / `btv.lsp` automatically
  acts *on the remote* (an LSP-config plugin spawns its server in the container
  with zero per-plugin tagging);
- a plugin using the **local-always** seam stays local — the connector itself.

**The seam a plugin calls *is* its UI-vs-workspace classification.** No
install-time metadata is needed. This plan therefore does not port VS Code's
dual-host machinery; it adds a system tier, a swap primitive, and a connect
provider API, and the rest falls out of the existing seam split.

---

## Design

Four mechanisms (A–D) plus the connector as the first consumer (E).

### A. System-plugin tier

A **system plugin** is a local plugin the *client* loads into every session,
before `init.lua`, un-shadowable-from-config in load order but never able to hijack
a user module name.

**Where the set lives (client-owned, not session-owned).** The tier must be
decided *before* a session's `init.lua` runs and must be *identical* across a swap,
so it cannot be declared in `init.lua` (chicken/egg — `init.lua` runs inside the
session). The **client** owns a system-plugin registry, seeded from a **system
dir** on local disk (`$BEMTVI_DATA/system/*` — one plugin repo per subdir), scanned
at client startup.

The client resolves the dir into a `Vec<SystemPluginSpec>` and threads it onto
**every** `ServerInit` it constructs — the initial local session and every swapped
session. That is what makes a connector "always run": the local client re-seeds the
tier into the remote session too, so the connector persists across the reload even
when the post-swap config source is the *remote's* config (which knows nothing about
it).

**Not embedded.** Unlike the prelude, a system plugin is *not* baked into the
binary. The prelude is embedded because it *is* the `btv.*` API — it must exist with
zero disk / zero network in every context (wasm included) and has a bootstrapping
problem. A connector has none of those properties: by the time it runs, the full
plugin machinery exists, and it is a normal git repo that lives on local disk. So a
system plugin is always a real on-disk dir, cloned into the system dir like any
managed plugin — no `include_dir!`, no extract-on-boot.

**New wiring:**

- `ServerInit.system_plugins: Vec<SystemPluginSpec>` (`lib.rs`). Default **empty**
  → headless suites and `--lua` stay hermetic, exactly like `offer_default_recommended`.
  The interactive TUI/GUI binaries populate it.
- `SystemPluginSpec { name: String, dir: PathBuf }` — a resolved **local dir**;
  the server only ever sees real dirs, giving a uniform load path + real files →
  tracebacks + LS visibility.
- A new **system-load phase** at the front of the server startup lifecycle (before
  `client_init_lua` / recommended / `init.lua`). *As built:* the tier dirs are
  spliced into the runtimepath at VM construction (see the next bullet), then in a
  dedicated phase after `shada_load` the server registers them in the `btv.plugins`
  tier (`btv.plugins._register_system`) and sources their `plugin/`/`after/plugin/`
  scripts via `EditHost::source_specific_plugins` — the same sourcing loop as
  `source_plugins`, run synchronously so they are ready *before* `init.lua`. The
  later `source_plugins` pass skips any dir the live tier registry reports
  (`btv.plugins._system_dirs()`), so a system plugin loads **exactly once** even
  though its dir is on the runtimepath. Sourcing reads the local disk (`std::fs`),
  so a system plugin loads locally in a daemon session, consistent with the
  remote-aware manager. (The *runtime-promotion* path, §A route B, uses the Lua
  manager load path — `btv._add_rtp` + `source_runtime` — since it runs after boot.)
- **package.path priority (shadow-safety).** `seed_package_path` (`host.rs:70`)
  already rebuilds `package.path` from the **full runtimepath in order** — config
  dir first (it is `rtp[0]`), then plugins in rtp order, then the captured stock
  tail (`08806ead`, closing the `plugin-rtp-shadows-config-modules` footgun). So
  ordering is not a special case here: it falls out of **where the system-plugin
  dirs are inserted into the runtimepath** — after the config dir, ahead of managed
  plugins. Insert them there (in the system-load phase, via `btv._add_rtp` at that
  position) and the resulting `package.path` gives system plugins "prelude-like"
  priority automatically: a user's own module name still wins, a system plugin still
  out-prioritizes ordinary managed plugins.

**Injecting a plugin into the tier (the user's ask).** Two routes, same registry:

- **Declarative:** a repo under `$BEMTVI_DATA/system/<name>/` (dropped there by the
  connector's installer, or by the user). Picked up on next client start / swap.
- **Runtime promotion:** `btv.plugins.system{ "owner/repo", ... }` /
  `btv.plugins.promote(name)` — clones/copies the plugin into the system dir via the
  **local-always seam** and registers it. It also loads immediately in the current
  session (as a normal plugin) so promotion takes effect now *and* for every future
  session/swap. `btv.plugins.system(spec)` is the callable form for `init.lua`.

System specs are **excluded** from `sync`/`update`/`clean`/`remove` (a system
plugin is never a dangling managed clone).

### B. Client-persistent, Lua-triggerable session swap ("reload window")

Generalize the GUI's existing swap into a first-class operation available to both
clients and initiable from Lua.

- **Server→client control message.** A Lua call queues an effect that drains to the
  client as a notification, e.g.
  `btv_session_reconnect { transport, config_source, keep_buffers }`. The client
  (which owns the window + transport) tears down the
  current RPC session and brings up a new one, **keeping the window** — the reload.
  This is the seam beyond "resolve a connect target": a plugin *initiates* the
  reload from inside the running VM.
- **Transport spec** — reuse the two shapes that already exist:
  `{ kind = "spawn", cmd = "ssh host bemtvi --daemon" }` → `connect_daemon` over the
  child's stdio; `{ kind = "quic", addr = "bemtvi://host:port" }` → the QUIC path.
  The client builds the new `ServerInit` from the spec, **carrying `system_plugins`
  forward** (§A) and feeding the spawn command into `connect_daemon_reconnecting`'s
  closure so reconnect re-runs it.
- **TUI parity.** Teach the TUI the same swap (today `main.rs:635` = startup flags
  only): factor the GUI's teardown+reconnect into a shared client routine both
  front ends call on the `btv_session_reconnect` notification.
- **Failure is loud.** Provisioning/spawn failure surfaces via `btv.daemon.status()`
  + a message and leaves the *current* session intact (no half-swap): resolve fully,
  then swap.

### C. `btv.connect` provider registry + `btv.session.reconnect`

The Lua surface the connector uses.

- `btv.connect.register(scheme_or_matcher, resolver)` — register an async resolver
  keyed by URL scheme (or a host-pattern matcher). Lives in a system plugin so it is
  registered before any `:connect`.
- `:connect <url>` (now live in both clients) is a **real ex-command that runs in the
  local VM** (not client-intercepted): it calls `btv.connect.connect(url)`, the matching
  resolver runs (async, may provision, may stream progress) → returns a **transport spec**
  → the VM swaps via `btv.session.reconnect` (§B). *As built:* routing lives in the VM rather
  than in a client resolve-request round-trip, so both front ends share one path and the
  command gets normal completion / history. With **no matching provider**, the VM pushes a
  `btv_connect_fallback` notification carrying the raw URL, and the client runs its own
  built-in direct dial (QUIC URI / ssh host) — the GUI keeps its `SSH_ASKPASS` path, so
  nothing regresses.
- `btv.session.reconnect(spec)` — the imperative form: a plugin swaps the current
  client to `spec` directly (skipping URL routing), e.g. after building a container.
  This is what the connector calls once provisioning is done.
- **Progress.** The resolver may emit progress (`btv.notify`) — "detecting arch… installing
  binary… starting daemon…" — shown on the message line as it runs in the VM, so the user
  sees the bootstrap.

### D. Config-source policy after the swap

Today `config_bundle` pulls the **remote's** config. A dev-container often wants
local editor settings + the container's toolchain. Make it a knob on the swap:

- `config_source = "remote" | "local" | "merged"` on the transport/reconnect spec.
  - `remote` — current behavior (materialize the daemon's config).
  - `local` — keep the local `init.lua`/plugins; the daemon backs only the seams.
  - `merged` — local UI/editor config layered over the remote's project config
    (deferred; spec the seam, implement `remote`/`local` first).
- Independent of config source, the **system tier is always the client's** (§A), so
  the connector persists regardless.

### E. The `bemtvi-remotes` connector (first consumer)

A pure-Lua system plugin proving the stack end-to-end:

1. Registers `btv.connect` providers for its schemes (e.g. `container://`,
   `ssh://`) in its `plugin/` script.
2. On invoke, provisions via the **local-always seam** (`btv.run`/`lfs`, and now
   `btv.http.fetch_local` — the local-always HTTP twin, `83b8a716`): detect remote
   arch/OS (`ssh host uname -sm` / `docker exec … uname -sm`), fetch the matching
   bemtvi binary (`btv.http.fetch_local` for an HTTP release URL, or scp) or reuse a
   cached one, `chmod`, keep a long-lived SSH control-master / tunnel handle alive
   for the session.
3. Returns
   `{ kind = "spawn", cmd = <ssh|docker exec … bemtvi --daemon>, config_source = "remote" }`.
4. Registers a teardown hook on `:disconnect` / session drop to tear down the
   control-master + temp state, and reflects state via `btv.daemon.status()`.

Installed by the user into `$BEMTVI_DATA/system/` (declared via
`btv.plugins.system{...}` or cloned by its own installer) — which makes it
*always run*.

**New primitive it needs beyond existing seams:** long-lived local process handles
(the SSH control-master/tunnel must outlive the spawning call and be killable on
disconnect). Everything else is `btv.run`/`btv.fs`/`btv.http.fetch_local` over the
local-always seam.

---

## Phases

Each phase is committed and paused for review before the next
(`big-feature-workflow-cadence`).

- **Phase 1 — System-plugin tier (§A). ✅ DONE.** `ServerInit.system_plugins` +
  `SystemPluginSpec`, the client registry (`discover_system_plugins` /
  `system_plugin_dir` scanning `stdpath("data")/system/*`, wired into the TUI, GUI,
  and daemon-session `ServerInit`s; default-empty everywhere else), the runtimepath
  splice (after the config dir) + early system-load phase + skip-set in
  `source_plugins`, and the Lua tier (`btv.plugins._system` registry,
  `_register_system` / `_system_dirs` / `list_system`, `btv.plugins.system` /
  `promote` cloning into the system dir via the local-always seam, kept out of
  `_specs`/`_order` so sync/update/clean ignore them). Tests:
  `bemtvi-server/tests/system_plugins.rs` (loads before `init.lua`, absent with the
  default-empty set, config module shadows a same-named system module, sourced
  exactly once) + `tests/plugins.rs` (`btv.plugins.system`/`promote` clone into the
  system dir and load; unknown-plugin promote rejects).
- **Phase 2 — Client-persistent, Lua-triggerable swap (§B).**
  - **2a — control-message seam. ✅ DONE.** `btv.session.reconnect(spec)` (prelude
    `btv.lua`, fail-loud spec validation + normalization) → a `session_reconnects`
    bucket on the Lua `Shared` store (`btv._session_reconnect`, `take_session_reconnects`)
    → drained in `apply_lua_effects` into a `fx.notify("btv_session_reconnect", [spec])`
    client notification (the spec rides verbatim as an `rmpv::Value`; the server/Lua
    stay agnostic to transport semantics). Test: `tests/session_reconnect.rs` (the
    notification fires with the normalized spawn/quic spec + defaults; a malformed spec
    fails loud and emits nothing).
  - **2b — client swap consumers. ✅ DONE (needs manual verification).** Shared spec
    parser `bemtvi_server::ReconnectSpec::from_value` (`reconnect.rs`) — the wire form
    both front ends decode. **GUI:** a `btv_session_reconnect` arm in the IO swap loop
    (`bemtvi-gui/lib.rs`) builds the session from the spec (`session::spawn_session_from_spec`)
    and reuses the existing teardown+re-attach. **TUI:** `bemtvi-tui::run` gains a swap
    loop that keeps the terminal up across swaps; `event_loop` returns an `Outcome`
    (`Exit`/`Swap`), builds the new backend off the event loop (so the current session
    keeps rendering; failure is reported and leaves it intact — no half-swap), and
    forwards the raw spec params to a builder the binary supplies. **Binary:**
    `drive_tui_with_swaps` + `build_session_from_spec` + a factored
    `spawn_edit_host_session`/`assemble_edit_host_init` (blocks on the handshake, so a
    connect failure is loud and non-committing), carrying `system_plugins` forward and
    feeding `cmd`/`addr` into `connect_daemon_reconnecting`/`connect_quic_reconnecting`.
    Both fire on `btv_session_reconnect`; `keep_buffers = true` is rejected loudly (not
    yet implemented — the swap starts fresh from the new backend). A `spawn` transport
    accepts a structured `argv = {…}` (run without a shell — `SpawnCommand::Argv`, the
    preferred form) or a `cmd` shell line (`sh -c`, `SpawnCommand::Shell`, mirroring
    `BEMTVI_DAEMON_CMD` for ssh/docker one-liners). Client code, not
    harness-testable (the GUI is not screencapturable) → **verify by driving the real
    binaries**: from a running session, `:lua btv.session.reconnect{ transport = { kind
    = "spawn", cmd = "<path>/bemtvi --daemon" } }` should keep the window and reload onto
    the daemon-backed session.
- **Phase 3 — `btv.connect` + `btv.session.reconnect` (§C). ✅ DONE (client swap needs
  manual verification).** The provider registry + `:connect` routing live in a new
  prelude module (`prelude/connect.lua`, loaded after `btv.lua`): `btv.connect.register(
  scheme_or_matcher, resolver)` (a scheme string or a `fn(url)->bool` matcher; the resolver
  returns a spec or a **promise** of one and may `btv.notify` progress), `btv.connect._resolve`
  (newest-registration-wins scan), and `btv.connect.connect(url)` — the entry point behind a
  real `:connect` ex-command that routes **through the VM**. A matching resolver runs via
  `btv.promise.try` (sync return / async promise / thrown error all fold into one chain) and
  its spec swaps the window through the existing `btv.session.reconnect` seam (§B); with **no
  provider**, `btv._connect_fallback(url)` queues a new `connect_fallbacks` bucket
  (`runtime.rs`/`install.rs`) drained in `effects.rs` into a `btv_connect_fallback` client
  notification carrying the raw URL, so each front end keeps its **own** built-in direct
  dial. **GUI:** `:connect` dropped from `CLIENT_INIT_LUA` + the client-side cmdline intercept
  (now a real VM command); a `btv_connect_fallback` arm dials via the pre-existing
  `spawn_session` path (so the `SSH_ASKPASS` dialog is preserved), parsing with the extracted
  `remote::connect_target`. **TUI:** gains `:connect` for free; a `btv_connect_fallback` arm
  forwards to the same builder, which disambiguates a fallback URL (a string →
  `ReconnectSpec::fallback_from_url`: `bemtvi://` QUIC / `[user@]host[:port]` ssh argv, `-`
  host + remote-file + unknown-scheme rejected loud) from a `btv.session.reconnect` spec (a
  map). Tests: `bemtvi-server/tests/connect.rs` (sync + async resolver → `btv_session_reconnect`;
  no-provider + real `:connect` → `btv_connect_fallback`; a failing resolver swaps nothing;
  the `fallback_from_url` parser). Example: `examples/connect/` (a `demo://` connector onto a
  local `bemtvi --daemon`). Client swap is client code (not harness-testable) → verify by
  driving the real binaries: with the example config, `:connect demo://here` should reload the
  window onto the daemon-backed session (`:lua print(btv.daemon.status())` → `"connected"`).
- **Phase 4 — Config-source policy (§D). ✅ DONE (merged deferred).** The
  `config_source = remote|local` **mechanism already landed** across Phase 2b + the
  remote-config plan (`2026-06-26-remote-config-and-shada-choice.md`): the swap spec's
  `config_source` threads through both client builders (TUI `assemble_edit_host_init`, GUI
  `server_init`) into `RemoteConfig::resolve(config_source)`, which for `Remote` fetches +
  materializes the daemon's config/plugins (and keeps shada on the daemon) and for `Local`
  runs `default_runtime()` + local shada, fetching only the daemon's cwd / parser set
  (`daemon.rs` `resolve`: `.fetch(matches!(source, Remote))`). Independent of the source, both
  builders re-seed the client-owned system-plugin tier (§A) — `system_plugins:
  discover_system_plugins()` — so a connector persists across the swap regardless. This phase
  closes it out: a resolver's spec `config_source` (§C) is carried through to the swap; the
  deferred **`"merged"`** value is now RESERVED and fails loud (a targeted "not implemented
  yet" at both the Lua `btv.session.reconnect` seam and `ReconnectSpec::from_value`) rather than
  silently picking a side; and the docs (`btv.session.reconnect` / `btv.connect.register`
  docstrings) spell out the remote/local/merged semantics. Tests:
  `tests/connect.rs::a_resolver_can_pick_the_config_source` (a resolver's `config_source =
  "local"` rides through to `btv_session_reconnect`) + `tests/session_reconnect.rs::
  config_source_merged_is_reserved_and_fails_loud`. (Honoring `Local` vs `Remote` config
  resolution is the daemon-config leg, covered by `tests/remote_config.rs`.)
- **Phase 5 — `bemtvi-remotes` connector (§E). ✅ DONE.** The connector plugin
  lives in its own repo `~/work/nxvim-plugins/nxvim-remotes` (renamed from
  `bemtvi-remote-containers` — it does ssh hosts too, not just containers; pure Lua, built on
  the public seams). It registers `btv.connect` providers for `container://` (docker exec) and
  `ssh://` (multiplexed) in its `plugin/` script; on invoke it provisions FROM THE LOCAL
  MACHINE over the **public local-always seam** delivered in Phase 5a (`btv.run_local` /
  `btv.fs_local` / `btv.http.fetch_local`): detects the remote's arch/OS (`uname -sm`, doubling
  as a reachability check), resolves the bemtvi daemon command (assume it's on the remote PATH,
  or fetch the matching release binary to a local cache + `docker cp`/`scp` in + `chmod`), and
  returns a structured-argv spawn transport with `config_source` (§D). Progress rides
  `btv.notify`. Layout: `lua/bemtvi-remotes/{init,provision,backends}.lua` (a
  backend-agnostic engine + per-transport descriptors), `plugin/`, `examples/init.lua`,
  README.
  - **Long-lived proc handle — not needed.** The plan expected a new core primitive for the
    ssh control-master; instead the connector uses ssh's own multiplexing (`ControlMaster=auto`
    + a per-host `ControlPath` + `ControlPersist`): the master is created by the first call,
    reused by the daemon spawn + every reconnect, and self-expires. `:RemoteContainersClean` /
    `M.disconnect` close masters via `ssh -O exit`, keyed by on-disk `.host` sidecars that
    survive a session swap (Lua state doesn't). So no long-lived-local-process API was added.
  - **Tests (hermetic, `bemtvi --test-plugin .` → 19 passed):** `backends_spec` (URL parsing +
    argv for docker/ssh, option-injection rejection, control-master options), `provision_spec`
    (platform mapping + release-URL template), `resolve_spec` (the full resolve flow against a
    fake `docker` /bin/sh script — arch detect, spawn spec, `config_source`, unreachable → loud,
    install-policy reuse). No Docker / network / host. Verified end-to-end that
    `:connect container://<name>` routes through `btv.connect` to `btv.session.reconnect` with the
    right spawn spec.
- **Phase 5a — public local-always seam (prerequisite for Phase 5). ✅ DONE.** Exposed
  `btv.run_local` / `btv.fs_local` (twins of `btv.run` / `btv.fs`, forced onto the client machine;
  `btv.http.fetch_local` already existed) as a public prelude module (`localseam.lua`), so
  connectors — and the plugin manager, which now dogfoods them — share one surface instead of
  the manager's old privates. Tests: `bemtvi-server/tests/local_seam.rs`.

## Open decisions

- **System dir location** — `$BEMTVI_DATA/system/` vs `$BEMTVI_CONFIG/system/`. Lean
  DATA (managed artifacts, not hand-edited config).
- **Config `merged` semantics** — seam specced in Phase 4 (the value is reserved and
  fails loud at the `btv.session.reconnect` / wire boundary, with the intended "local UI
  config over the remote's project config" semantics documented). Implementation still
  deferred until a concrete need exists.
- **First-run install of the connector** — leave it a pure user install
  (`btv.plugins.system{...}`), or have the interactive binary offer to clone it into
  the system dir on first run (like the recommended-set welcome). Lean user-install
  first.
