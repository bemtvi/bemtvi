# Named lists — a window-independent quickfix/location list

> **Status: shipped (all phases).** The design below is as-planned; the **Lua
> surface changed during implementation** — the `nx.qf.dynamic` (named,
> function-sourced) abstraction was dropped in favour of a direct
> `nx.qf.list` / `show` / `drop` API (see *Lua surface* and *Phases* for what
> actually landed). The authoritative current-state reference is
> [`docs/features/quickfix-dock-lists.md`](../features/quickfix-dock-lists.md).

## Motivation

nxvim models lists two ways (`QfWhich`):

- **Quickfix** — one global list (`Editor::qf`), shown in a single reused bottom-dock
  tab. `:copen` focuses it from any window. Window-independent, but there is only **one**
  (so a plugin list collides with `:grep` / `:make`).
- **Location list** — a per-window list (`Window::loclist`), one per window. Isolated
  (its own dock tab), but **bound to a window**: `:lopen` only opens the *current*
  window's list, and — critically — **closing the owner window destroys the list and its
  display tab** (`remove_window` drops the `Window`, `discard_loclist_display` closes the
  `:lopen` tab). A split inherits a *clone*.

A plugin that wants a persistent, named panel — e.g. nxvim-dap's "All Breakpoints" — fits
neither: the quickfix collides with grep, and a loclist evaporates when you close the code
window it was anchored to (and reopening it from elsewhere spawns a duplicate, since
`:lopen` is current-window-relative).

## The new type

A **named list**: like the quickfix list, but there can be many, each addressed by a
stable name. Storage lives on the `Editor` (not on a window), so it survives every window
close; it shows in its own bottom-dock tab; it is focused / reopened **by name** from
anywhere; selecting an entry jumps into the main editing layer (like the dock quickfix /
loclist). Not persisted (consistent with dropping qf/loc-list persistence).

It is mechanically quickfix-flavored (global storage, dock tab, main-layer jump) but a
distinct *instance* per name, so it never collides with the global quickfix.

## Core model

- `QfWhich` stays `Copy`: add `Named(NamedListId)` where `NamedListId(u32)` is a newtype.
  A `String` payload would break `Copy` (relied on at ~47 match sites), so names map to
  ids through a registry.
- `Editor` gains:
  - `named_lists: HashMap<NamedListId, NamedList>` — `{ name, stack: QfStack,
    display_bufnr: Option<BufferId> }`. (Shipped without a separate `title` field: the
    dock-tab label is the current `QfList`'s title, falling back to the name — one
    source of truth.)
  - `named_by_name: HashMap<String, NamedListId>` + a small id allocator.
  - `named_list_id(name) -> NamedListId` (intern: create-or-get).
- Thread `Named` through the `QfWhich` accessors and operations, mirroring the
  `Quickfix` arm: `qf_stack` / `qf_cur` / `qf_*_mut`, `qf_display_bufnr` /
  `qf_set_display_bufnr`, `qf_set_items`, `ex_qf_open` (open-or-focus its dock tab),
  `qf_window_id`, `qf_refresh_window`, `qf_render_text`, `qf_paint_severity`,
  `qf_context_of_buffer`, the jump (`qf_focus_target_window` already excludes dock
  windows → main-layer fallback), the mouse hit-test, and close (closing the tab drops the
  *display window*, never the registry entry → reopen re-renders).

## Lua surface

> **Changed during implementation.** The plan routed named lists through the
> existing `nx.qf.dynamic` (named, function-sourced) abstraction with a new
> `kind = "list"`. Mid-build that abstraction was judged redundant once a named
> list is the primitive — a plugin can push items directly instead of registering
> a `source` callback — so **the whole dynamic feature (every kind) was removed**
> and replaced with a direct API. The named-list *plumbing* the dynamic routing
> sat on (`QfSetOp.named`, `NamedListOp{Show,Drop}`, the core
> `named_list_id`/`show`/`drop`) stayed — it is what the direct API rides on.

What shipped:

- `nx.qf.list(name, items[, opts])` — create / replace a named list in place
  (`opts.title`, `opts.action` = `"r"` default / `" "` / `"a"`); repaints its tab
  if open, never opens one. Queues a `QfSetOp` with a `named` target.
- `nx.qf.show(name)` — open / focus a named list's tab by name (the clean reopen),
  sequenced server-side after any same-tick `list` (a `NamedListOp::Show` drained
  after the `QfSetOp`s), so no `set_current` + `on_next_tick` dance.
- `nx.qf.drop(name)` — close its tab and forget the core list
  (`NamedListOp::Drop`).

(`nx.qf.dynamic` / `nx.qf.refresh` and the `_dynamic_lists` registry were deleted,
along with the `examples/dynamic-lists` config — replaced by `examples/named-lists`.)

## Phases

1. **Core type + storage** ✅ — `NamedListId`, `QfWhich::Named`, the `Editor` registry
   and interning, and the read/write accessor arms.
2. **Show / render / refresh** ✅ — `ex_qf_open(Named)` opens-or-focuses the dock tab;
   `qf_set_items` + render; the `named_list_show` core entry point.
3. **Jump / close / mouse** ✅ — **no new code**: phase 1's thorough threading of
   `qf_context_of_buffer` + `qf_focus_target_window`, monotonic (never-reused)
   `BufferId`s, and the generic dock-tab / mouse handling already covered `<CR>`
   jump, close-keeps-registry, and tab click/scroll.
4. **Lua API** ✅ — landed first *through* `nx.qf.dynamic{ kind = "list" }`, then
   **superseded** by the direct `nx.qf.list` / `show` / `drop` API above (dynamic
   removed). The `QfSetOp.named` + `NamedListOp` plumbing is the lasting part.
5. **dap plugin** ✅ — `:DapBreakpoints` switched to a named list (`nx.qf.list` +
   `show`); deleted the owner-window binding and the `refresh():next(lopen)` dance
   (there was no `on_next_tick` hack in the end). The latent loclist bug also lived
   in the `examples/dynamic-lists` demo, replaced by `examples/named-lists`.
6. **Tests + docs** ✅ — core black-box coverage: create / show / repaint-in-place /
   drop / jump, **survives window close**, no quickfix collision, two lists side by
   side. The new type is documented in
   [`docs/features/quickfix-dock-lists.md`](../features/quickfix-dock-lists.md).

Committed per phase; paused for review between phases.
