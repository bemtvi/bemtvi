# Remote connectors + system-plugin tier: VS Code-style "install a connector, reload onto the remote"

**Status:** planned · **Date:** 2026-07-05

## Goal

Let a user install a **connector plugin** (e.g. `remote-containers`, `remote-ssh`)
that, on a single command, provisions a remote/container, installs + launches the
nxvim daemon there, and **reloads the editor onto it** — the VS Code Remote model.
`:connect nxvim://host` (or a connector-defined command) carries no extra
parameters: the connector owns the whole bootstrap.

Two things make this possible and are the substance of this plan:

1. A **system-plugin tier** — local, always-loaded plugins seeded by the *client*
   into **every** session it brings up (the initial local one *and* every
   post-swap remote one). A connector must be *injectable into this tier* so it is
   guaranteed present before any connect and survives the reload.
2. A **client-persistent, Lua-triggerable session swap** — nxvim's equivalent of
   VS Code's "reload window": the client window lives, the server/VM behind it is
   torn down and rebuilt against a new backend.

## Background — most of the transport already exists

- **Edit-host split** (`nxvim --connect-daemon`, `main.rs`): a full local editor
  whose fs/proc/watch/LSP **seams** point at a `--daemon` child. The daemon wire
  runs over the child's **stdin/stdout**, so
  `NXVIM_DAEMON_CMD="ssh host nxvim --daemon"` already gives an SSH-tunnelled remote
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
  routing, via `nx._local_fs_op` / `nx._local_system_async`. A connector's
  provisioning (ssh, scp, docker) rides this seam.

### The gaps

1. There is **no system tier**: the prelude is baked (`PRELUDE_MODULES`,
   `runtime.rs:452`), and everything else (recommended, `cmdline_complete`,
   `init.lua`, managed plugins) runs *inside* a session and is not guaranteed to
   survive a swap.
2. The swap is **GUI-only, QUIC-only, and not Lua-triggerable**. The TUI has no
   live `:connect` at all (`main.rs:635` — connect target must be a launch flag).
3. **Connect is not Lua-pluggable**: the spawn command is a fixed env var
   (`NXVIM_DAEMON_CMD`), decided at launch, with no provisioning step.

## Key insight — nxvim skips VS Code's hard part

VS Code needs UI-vs-workspace **extension classification** because it runs two
extension hosts (local + remote). nxvim's edit-host split runs **one VM**;
remoteness lives entirely in the **fs/proc/lsp seams**. After a swap:

- a plugin using **session-routed** `nx.run` / `nx.fs` / `nx.lsp` automatically
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
dir** on local disk (`$NXVIM_DATA/system/*` — one plugin repo per subdir), scanned
at client startup.

The client resolves the dir into a `Vec<SystemPluginSpec>` and threads it onto
**every** `ServerInit` it constructs — the initial local session and every swapped
session. That is what makes a connector "always run": the local client re-seeds the
tier into the remote session too, so the connector persists across the reload even
when the post-swap config source is the *remote's* config (which knows nothing about
it).

**Not embedded.** Unlike the prelude, a system plugin is *not* baked into the
binary. The prelude is embedded because it *is* the `nx.*` API — it must exist with
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
  dedicated phase after `shada_load` the server registers them in the `nx.plugins`
  tier (`nx.plugins._register_system`) and sources their `plugin/`/`after/plugin/`
  scripts via `EditHost::source_specific_plugins` — the same sourcing loop as
  `source_plugins`, run synchronously so they are ready *before* `init.lua`. The
  later `source_plugins` pass skips any dir the live tier registry reports
  (`nx.plugins._system_dirs()`), so a system plugin loads **exactly once** even
  though its dir is on the runtimepath. Sourcing reads the local disk (`std::fs`),
  so a system plugin loads locally in a daemon session, consistent with the
  remote-aware manager. (The *runtime-promotion* path, §A route B, uses the Lua
  manager load path — `nx._add_rtp` + `source_runtime` — since it runs after boot.)
- **package.path priority (shadow-safety).** `seed_package_path` (`host.rs:70`)
  already rebuilds `package.path` from the **full runtimepath in order** — config
  dir first (it is `rtp[0]`), then plugins in rtp order, then the captured stock
  tail (`08806ead`, closing the `plugin-rtp-shadows-config-modules` footgun). So
  ordering is not a special case here: it falls out of **where the system-plugin
  dirs are inserted into the runtimepath** — after the config dir, ahead of managed
  plugins. Insert them there (in the system-load phase, via `nx._add_rtp` at that
  position) and the resulting `package.path` gives system plugins "prelude-like"
  priority automatically: a user's own module name still wins, a system plugin still
  out-prioritizes ordinary managed plugins.

**Injecting a plugin into the tier (the user's ask).** Two routes, same registry:

- **Declarative:** a repo under `$NXVIM_DATA/system/<name>/` (dropped there by the
  connector's installer, or by the user). Picked up on next client start / swap.
- **Runtime promotion:** `nx.plugins.system{ "owner/repo", ... }` /
  `nx.plugins.promote(name)` — clones/copies the plugin into the system dir via the
  **local-always seam** and registers it. It also loads immediately in the current
  session (as a normal plugin) so promotion takes effect now *and* for every future
  session/swap. `nx.plugins.system(spec)` is the callable form for `init.lua`.

System specs are **excluded** from `sync`/`update`/`clean`/`remove` (a system
plugin is never a dangling managed clone).

### B. Client-persistent, Lua-triggerable session swap ("reload window")

Generalize the GUI's existing swap into a first-class operation available to both
clients and initiable from Lua.

- **Server→client control message.** A Lua call queues an effect that drains to the
  client as a notification, e.g.
  `nx_session_reconnect { transport, config_source, keep_buffers }`. The client
  (which owns the window + transport) tears down the
  current RPC session and brings up a new one, **keeping the window** — the reload.
  This is the seam beyond "resolve a connect target": a plugin *initiates* the
  reload from inside the running VM.
- **Transport spec** — reuse the two shapes that already exist:
  `{ kind = "spawn", cmd = "ssh host nxvim --daemon" }` → `connect_daemon` over the
  child's stdio; `{ kind = "quic", addr = "nxvim://host:port" }` → the QUIC path.
  The client builds the new `ServerInit` from the spec, **carrying `system_plugins`
  forward** (§A) and feeding the spawn command into `connect_daemon_reconnecting`'s
  closure so reconnect re-runs it.
- **TUI parity.** Teach the TUI the same swap (today `main.rs:635` = startup flags
  only): factor the GUI's teardown+reconnect into a shared client routine both
  front ends call on the `nx_session_reconnect` notification.
- **Failure is loud.** Provisioning/spawn failure surfaces via `nx.daemon.status()`
  + a message and leaves the *current* session intact (no half-swap): resolve fully,
  then swap.

### C. `nx.connect` provider registry + `nx.session.reconnect`

The Lua surface the connector uses.

- `nx.connect.register(scheme_or_matcher, resolver)` — register an async resolver
  keyed by URL scheme (or a host-pattern matcher). Lives in a system plugin so it is
  registered before any `:connect`.
- `:connect <url>` (now live in both clients) routes through the local VM: the
  client sends a **resolve-connect** request → the matching resolver runs (async,
  may provision, may stream progress) → returns a **transport spec** → the client
  performs the swap (§B). With no matching provider, `:connect` falls back to
  today's direct dial (QUIC URI / bare `connect_daemon`), so nothing regresses.
- `nx.session.reconnect(spec)` — the imperative form: a plugin swaps the current
  client to `spec` directly (skipping URL routing), e.g. after building a container.
  This is what the connector calls once provisioning is done.
- **Progress.** The resolver may emit progress (`nx.notify` / a dedicated progress
  channel) — "detecting arch… installing binary… starting daemon…" — carried on the
  resolve round-trip so the user sees the bootstrap.

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

### E. The `remote-containers` connector (first consumer)

A pure-Lua system plugin proving the stack end-to-end:

1. Registers `nx.connect` providers for its schemes (e.g. `container://`,
   `ssh://`) in its `plugin/` script.
2. On invoke, provisions via the **local-always seam** (`nx.run`/`lfs`, and now
   `nx.http.fetch_local` — the local-always HTTP twin, `83b8a716`): detect remote
   arch/OS (`ssh host uname -sm` / `docker exec … uname -sm`), fetch the matching
   nxvim binary (`nx.http.fetch_local` for an HTTP release URL, or scp) or reuse a
   cached one, `chmod`, keep a long-lived SSH control-master / tunnel handle alive
   for the session.
3. Returns
   `{ kind = "spawn", cmd = <ssh|docker exec … nxvim --daemon>, config_source = "remote" }`.
4. Registers a teardown hook on `:disconnect` / session drop to tear down the
   control-master + temp state, and reflects state via `nx.daemon.status()`.

Installed by the user into `$NXVIM_DATA/system/` (declared via
`nx.plugins.system{...}` or cloned by its own installer) — which makes it
*always run*.

**New primitive it needs beyond existing seams:** long-lived local process handles
(the SSH control-master/tunnel must outlive the spawning call and be killable on
disconnect). Everything else is `nx.run`/`nx.fs`/`nx.http.fetch_local` over the
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
  `source_plugins`, and the Lua tier (`nx.plugins._system` registry,
  `_register_system` / `_system_dirs` / `list_system`, `nx.plugins.system` /
  `promote` cloning into the system dir via the local-always seam, kept out of
  `_specs`/`_order` so sync/update/clean ignore them). Tests:
  `nxvim-server/tests/system_plugins.rs` (loads before `init.lua`, absent with the
  default-empty set, config module shadows a same-named system module, sourced
  exactly once) + `tests/plugins.rs` (`nx.plugins.system`/`promote` clone into the
  system dir and load; unknown-plugin promote rejects).
- **Phase 2 — Client-persistent, Lua-triggerable swap (§B).** The
  `nx_session_reconnect` control message, the shared client swap routine (GUI
  generalized + TUI taught), carrying `system_plugins` forward and feeding
  `connect_daemon_reconnecting`. Test: a driven swap between two local `--daemon`
  targets keeps the window and reloads.
- **Phase 3 — `nx.connect` + `nx.session.reconnect` (§C).** Provider registry,
  live `:connect` routing through the local VM with fallback, progress on the
  resolve round-trip.
- **Phase 4 — Config-source policy (§D).** `config_source = remote|local` on the
  spec (merged deferred).
- **Phase 5 — `remote-containers` connector (§E).** The connector plugin (in
  `~/work/nxvim-plugins/remote-containers`), long-lived proc handles, an example
  under `examples/`, hermetic tests using a local `nxvim --daemon` as the "remote."

## Open decisions

- **System dir location** — `$NXVIM_DATA/system/` vs `$NXVIM_CONFIG/system/`. Lean
  DATA (managed artifacts, not hand-edited config).
- **Config `merged` semantics** — deferred; spec the seam in Phase 4, implement
  later once a concrete need exists.
- **First-run install of the connector** — leave it a pure user install
  (`nx.plugins.system{...}`), or have the interactive binary offer to clone it into
  the system dir on first run (like the recommended-set welcome). Lean user-install
  first.
