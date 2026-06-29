# Persisted views via nx.component

Make cross-session persistence a first-class **component** capability, so a plugin
declares a persistent UI with `nx.view.component` instead of hand-wiring
`nx.view.create{ persist=}` + `nx.view.on_restore` + a `VimEnter` fresh-fallback. Then
rewrite `examples/view-persist/` on top of it (the user's "use components, not raw
nx.view" request), which also dissolves the namespace-attribution bug that bites when
the example's callbacks re-resolve the namespace off the runtimepath.

## Why the raw-view example is fragile

`nx.view.create{ persist=}`, `nx.view.on_restore`, and `nx.shada.plugin()` each
**re-resolve** the owning namespace from the *calling stack* (`caller_source` →
`assign_namespace`) at every call. That's fine at `init.lua` top level (the chunk is on
the stack, under the runtimepath), but a call made from a deferred/async context — or
from a session whose config root isn't this dir — attributes to *nothing* and raises
`this caller attributes to no plugin`. The example calls `create` from inside the
`on_restore` handler and the `VimEnter` callback, so it's exposed.

The component fixes this structurally: resolve the namespace **once, synchronously, at
the `:mount()` call site** (where `init.lua` is guaranteed on the stack) and thread it
*explicitly* into `create` / `shada.plugin` / `on_restore` thereafter — no callback ever
re-resolves.

**Root cause of the reported error (separate, also fixed).** The user's launch used a
**trailing slash** (`NXVIM_CONFIG=examples/view-persist/`), so the runtimepath entry was
`examples/view-persist/`. `assign_namespace` matched sources against `dir .. "/"` →
`examples/view-persist//` (double slash), which never prefixes `@examples/view-persist/
init.lua` — so even `init.lua` top level attributed to *no plugin*. Fixed by trimming
trailing separators off each rtp entry before the prefix match (regression test:
`plugin_namespace_tolerates_a_trailing_slash_rtp_entry`).

## Phase 1 — framework (`crates/nxvim-lua/src/prelude/component.lua`)

Extend the **view backend** and the `nx.component` / `nx.view.component` mount path:

- `mount{ persist = "<id>", namespace? = "<ns>", dock=/split=/float= }`:
  - When `persist` is set, resolve `ns = nx._resolve_namespace(opts.namespace, ...)`
    **synchronously in `M.mount`** (init.lua is on the stack). Store `ns` + `persist`
    on the instance; pass them to `nx.view.create{ persist=, namespace=ns }`.
  - **Fresh vs restore, handled by the framework** (the dance the example does by hand):
    1. Register into a shared per-namespace dispatcher
       `nx._component_restorers[ns] = { [id] = adopt_fn }`, backed by **one**
       `nx.view.on_restore(router, ns)` per namespace (routes `(id, place)` → the
       matching `adopt_fn`, which creates the view and `place_in`s the reserved slot
       instead of mounting fresh). One handler per ns, so multiple persistent
       components coexist.
    2. A deferred **fresh fallback**: `nx.on_next_tick` (guaranteed to run *after*
       `restore_persisted_views` has dispatched during boot) → if this instance was not
       adopted by a restore, mount it fresh. A `claimed` flag guards against both paths
       firing.
- `ctx` for a persistent component gains:
  - `ctx.namespace` — the resolved owner ns.
  - `ctx.store` — `nx.shada.plugin(ns)` (the explicit-ns handle; never re-resolves).
  - `ctx.persist_id` — the stable id.
  So `setup` reads/writes its own cross-session state with no attribution worry.
- Non-persistent mounts are byte-for-byte the current behavior (no `persist` ⇒ no ns
  resolution, no restorer, fresh mount as today).

Tests (`crates/nxvim-server/tests/nx_view.rs`, mirroring the existing
`session.rs` persisted-view round trip): (a) a persistent component mounts fresh,
its `ctx.store` round-trips; (b) the full save→restart→restore cycle adopts the reserved
slot and rebuilds content from `ctx.store`; (c) a fresh start with no reservation mounts
fresh via the fallback; (d) two persistent components in one ns both restore.

## Phase 2 — rewrite `examples/view-persist/`

Replace the raw-view init.lua with an `nx.view.component`-based "pinned notes" plugin:
reactive `notes` list in `setup`, `<leader>na` / `<leader>nd` mutate it (auto re-render),
`ctx.store` persists on every mutation, `mount{ persist="notes", dock="left" }`. No
`on_restore`, no `VimEnter` fallback, no manual `render()` — the framework owns all of it.
Verify the documented launch end-to-end (save two notes, `:qa`, re-run, notes return).

## Phase 3 — docs

- `component.lua` header: document `persist` / `namespace` / `ctx.store` / the automatic
  fresh-vs-restore lifecycle.
- `docs/specs/2026-06-11-native-plugin-api.md`: note the component is the recommended
  surface for a persistent plugin view; raw `nx.view.create{ persist=}` +
  `nx.view.on_restore` remain the low-level escape hatch.

## Cadence

Commit + pause for review between phases. Branch: whatever is checked out (`main3`).
