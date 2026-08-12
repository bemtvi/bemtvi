# Plugin view persistence

Persist and restore **plugin-owned views** (`btv.view`) across sessions, so a workspace
restore rebuilds not just file windows but the file tree, symbol list, terminal panel,
etc. — each in its exact slot, with the plugin in charge of its own content.

## The idea (and why it fits)

Persistence today already splits into two halves that nothing currently rejoins:

- **Core owns layout/structure.** `SessionState` persists the full tab+split tree, dock
  bands, sizes, per-window cursor/scroll (`persist.rs:128–195`). Buffers resolve by
  **path**, never by session-local id — that's why file windows survive a restart.
- **Plugins own opaque data.** `btv.shada.plugin()` already gives every plugin a
  namespaced, budgeted (1 MiB) KV store keyed by plugin identity (`stdlib.lua:210–312`).

The gap: plugin-owned `View` buffers (`BufferKind::View`, read-only) are **deliberately
not persisted**. `capture_layout` drops read-only buffers (`persist.rs:534–538`); docks
reopen empty (`persist.rs:688`) and the plugin repopulates wherever its boot logic
happens to put things. We lose *which slot*, *which tab*, and *multiple distinct
instances*.

The fix is a **join key**: the plugin supplies a stable identifier at view creation;
core round-trips that opaque string through the session (slot + id, nothing else); on
restore core reserves the slot and hands the id back; the plugin, which has keyed its own
shada by that id, rebuilds its content. Core stays content-agnostic (existing principle);
no view content bloats the session file (unlike `unnamed_contents`); the plugin decides
what's worth saving.

## The hard constraint: restore runs before plugins load

Boot order (`bemtvi-server/src/lib.rs`):

1. `host.shada_load()` (2865) → `editor.restore_session()` (2215) — layout rebuilt
   **eagerly, once, here**.
2. `host.source_init()` (2903) — `init.lua`.
3. `host.source_plugins()` (2909) — package `plugin/` scripts; this is where a plugin
   registers its restorer.
4. `host.emit_lifecycle_events()` + `host.fire_vim_enter()` (2950–2956).

So the plugin **does not exist** when `restore_session` runs. The handshake therefore
*cannot* be synchronous. Core must:

- at restore time, reconstruct each persisted view's slot as a **reserved placeholder
  window** and record a pending claim `{namespace, id, win}`;
- after plugins load, fire a restore event so each plugin **adopts** its reserved
  window(s);
- collapse any slot left unclaimed (plugin uninstalled), exactly like a missing-file
  window collapses today.

This is also why a placeholder is the right model rather than "core recreates the view":
core can't — content, callbacks, keymaps, and the per-line userdata all live in the
plugin.

## Design decisions

1. **Identifier = plugin-chosen string, namespaced by core.** `btv.view.create{ persist =
   "explorer:1" }`. Core captures it together with the plugin's shada **namespace**
   (resolved the same way `btv.shada.plugin()` resolves it — `assign_namespace`,
   `stdlib.lua:180–208`: longest-matching-rtp-entry → manager name / `"user"` / dir
   basename), so the stored key is `(namespace, id)`. Restore dispatch is then *exact*
   (route to the owning plugin), and an orphan is unambiguous (namespace has no loaded
   plugin → collapse). Instance-uniqueness within a namespace is the plugin's
   responsibility. `persist` absent ⇒ today's behavior (ephemeral, not persisted) — fully
   opt-in.

   **Escape hatch (symmetric with `btv.shada.plugin`).** The `debug.getinfo` stack walk only
   resolves when `create` is called *from a file under a runtimepath entry*. A context that
   attributes to nothing — a bare `:lua`, an RPC `exec_lua`, a test, or a shared helper
   module on a different rtp path — can't be auto-namespaced. So `create` takes an optional
   `namespace` field with the **same contract as `btv.shada.plugin(dev_namespace)`**
   (`stdlib.lua:244–268`): it is *required* when attribution fails, and is an *error* when
   attribution succeeds (the namespace there is always the assigned one — passing it would
   let a plugin masquerade as another). This keeps the two persistence surfaces consistent
   and is what makes the API testable at all (the black-box harness drives Lua over RPC,
   which attributes to no rtp entry).

2. **Slot ownership: core-reserved placeholder; plugin adopts.** Restore inverts
   `view:mount` — today the plugin creates the window; on restore *core* owns the
   window/geometry and the plugin supplies the buffer. New primitive `view:place_in(win)`
   (Lua) / `btv.view._adopt(view_id, win)` (bridge) retargets an existing reserved window to
   the view's backing buffer and installs the view keymaps.

3. **Restore timing: a registry fired after `source_plugins()`.** `btv.view.on_restore(fn)`
   registers a restorer; core invokes it for each pending claim belonging to that plugin's
   namespace. Fired after step 3, at/just-before `VimEnter`. Cross-tick safe: the reserved
   `win` is a real, already-existing window id (synchronous), and a freshly-created view's
   bufnr is known synchronously in core at create time (`view.lua:117` mirror is only for
   *reading later*; `_create`+`_adopt` queue in one tick) — so neither hits the
   `btv.schedule` winid trap noted in CLAUDE.md.

4. **Lifecycle / GC.** The plugin deletes its own shada key on permanent close (via its
   existing `on_close` + `btv.shada.plugin():delete(key)`). Core collapses unclaimed
   reserved slots at the end of the restore-event tick. An orphan-namespace shada sweep
   (uninstalled plugins leaking keys) is noted in Phase 3 but kept minimal.

5. **Capture gate.** `capture_layout`'s read-only refusal gains one exception: a
   `BufferKind::View` whose buffer carries a `(namespace, persist_id)` is captured as a
   *view-persist leaf* (no path, no `unnamed_contents`, just the id pair). Independent of
   `'workspacepersistunnamed'` (that governs editable scratch; this is its own opt-in).
   Because docks already route through `capture_layout`, persisted dock views fall out for
   free.

## API surface (new)

```lua
-- create with a stable, plugin-chosen persist id (namespaced by core)
local v = btv.view.create{ name = "Explorer", filetype = "btvtree", persist = "main" }

-- escape hatch: a context that attributes to no rtp entry (bare :lua / RPC / test /
-- off-path helper) must name its namespace explicitly; passing it from a real plugin
-- file is an error (same contract as btv.shada.plugin(dev_namespace)).
local v = btv.view.create{ persist = "main", namespace = "my-plugin" }

-- register a restorer; core calls it for each of THIS plugin's pending views
btv.view.on_restore(function(id, place)
  -- id == "main" (the string this plugin passed to create last session)
  local data = btv.shada.plugin():get("view:" .. id)   -- plugin's own stored state
  local view = btv.view.create{ name = "Explorer", filetype = "btvtree", persist = id }
  view:set_lines(rebuild_lines(data))                 -- plugin rebuilds its content
  view:on_select(...)                                 -- plugin re-wires callbacks
  place(view)   -- drop `view` into core's reserved window/slot (calls _adopt)
end)
```

`btv.view.pending_restores()` (returns `{ {id=, win=}, … }` for the calling plugin's
namespace) backs the registry and is the black-box test hook; kept internal-ish.

## Phases

Per the big-feature cadence: commit + pause for review between phases.

### Phase 1 — Plumb the id (capture + restore-to-placeholder)

- `btv.view.create{ persist = }`: derive the namespace (reuse `btv.shada`'s resolver),
  pass `(namespace, id)` through `btv.view._create`.
- Core: store `(namespace, persist_id)` for the view buffer (a side map on the editor,
  keyed by the view handle/buffer id).
- `SessionWindow`: add `view_persist: Option<(String, String)>` with
  `#[serde(default)]` (back-compat for old sessions).
- `capture_layout` (`persist.rs:526`): emit a view-persist leaf for a `View` buffer that
  has the id pair, instead of returning `None`. Works for main windows and docks.
- `restore_session` / `build_layout` / `build_leaf_buffer` (`persist.rs:599–712`): for a
  view-persist leaf, mint a **reserved placeholder window** (empty ordinary buffer) and
  record `{namespace, id, win}` on a `pending_view_restores` list on the editor.
- Expose `btv.view.pending_restores()` (per-namespace filter).
- **End-of-Phase-1 behavior:** with no claimant yet, reserved slots collapse at the end
  of the restore tick (net: same as today). The id round-trip is observable via the
  pending list.
- **Tests** (`tests/session.rs`): session 1 mounts a persisted view in a vsplit and a
  dock; session 2 restore → `btv.view.pending_restores()` returns both ids with valid
  reserved win ids; an *un*persisted view leaves no pending entry.

### Phase 2 — The claim handshake (adopt into the reserved slot)

- `btv.view.on_restore(fn)` registry; core dispatches each pending claim to the restorer
  registered by the owning namespace, after `source_plugins()` (wire the firing point in
  `lib.rs` near 2909/2956).
- `view:place_in(win)` / `btv.view._adopt(view_id, win)`: retarget the reserved window to
  the view's backing buffer; install view keymaps (`btv._install_view_keymaps`); the
  placeholder buffer is discarded.
- Orphan collapse: any pending claim not adopted by the end of the restore-event tick →
  close the reserved window, collapse the split / empty the dock (reuse the missing-file
  collapse path).
- **Tests:** full round-trip — session 1 mounts a persisted file-tree view (content +
  geometry, in both a dock and a split-in-a-second-tab); session 2's config registers
  `on_restore` and rebuilds from `btv.shada.plugin()`; assert lines, dock side/size, split
  orientation, and active focus all match. Plus: an unclaimed id (no `on_restore`
  registered) collapses its slot cleanly.

### Phase 3 — Polish, parity, docs, example

- **Web/wasm parity — RESOLVED: native-only by inheritance.** Session/layout restore is
  already native-only on web: `EditHost::import_persist` deliberately does *not* carry
  `SessionState` ("the session / exit_cursor are layered on by the caller — only the native
  flush carries them", `lib.rs:1483`), and only the native `shada_load` calls
  `restore_session`. Plugin-view persistence rides `SessionState`, so it is native-only too
  — no wasm wiring needed. The new `view_persist` field is a `#[serde(default)]` member of
  `SessionWindow`, so it round-trips harmlessly anywhere `SessionState` is ever serialized.
  If/when web gains session restore, plugin views come along for free (the dispatch is in
  `lifecycle.rs`, shared) provided that path also calls `restore_persisted_views`.
- **GC.** Document the `on_close` → `shada:delete` convention; consider an
  orphan-namespace sweep on flush for uninstalled plugins (keep minimal / optional).
- **Docs.** Update the `btv.view` section of the native plugin API spec
  (`docs/specs/2026-06-11-native-plugin-api.md`) with `persist` / `on_restore` /
  `place_in`.
- **Example.** `examples/view-persist/` — a runnable mini "pinned notes" view plugin that
  persists its lines across restart, with a sample workspace, verified end-to-end (per the
  example-config convention).
- **Dogfood.** The first-party tree/explorer plugin is the real consumer — once the API
  lands, port it to persist its expanded-dir set + scroll keyed by the view's persist id.

## Risks / notes

- The entire risk is the async restore handshake (Phase 2), not the concept. Pin the
  firing point precisely and lean on `pending_restores()` for deterministic tests.
- Keep core strictly content-agnostic: it stores only `(namespace, id)` + slot. If we ever
  feel tempted to let core cache view *lines*, that's the `unnamed_contents` mistake for
  plugin buffers — don't.
- `place_in` is the one genuinely new core primitive; everything else extends existing
  capture/restore and the `btv.view` handle.
