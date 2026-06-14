# Per-region tab-pages & tablines — phased plan

> Working checklist for giving the main editor region **and each open dock** its own
> independent tab-page stack and tabline. Project convention: lives at
> `docs/plans/2026-06-14-per-region-tablines.md` alongside the other `docs/plans/*.md`.

## Context

The permanent-docks feature (landed 2026-06-14, `docs/plans/2026-06-14-permanent-docked-panels.md`)
gave nxvim VSCode-style edge regions: a *focused layer*'s `WindowTree` is always live on
`Editor::windows`; non-focused layers park in `Editor::docks: [Option<WindowTree>; 4]` /
`Editor::main_parked`. Layers: `Main`, `Dock(Left|Right|Top|Bottom)`.

Tab pages (`docs/plans/2026-06-07-tab-pages.md`) are an **orthogonal** swap dimension that today
applies **only to the main layer**: `Editor::tabs: Vec<TabSlot>` + `current_tab: usize`, with the
active tab's tree live on `windows` (or parked in `main_parked` when a dock is focused) and
inactive tabs stashed in `tabs[i].tree`. Docks are *cross-tab* and single-tree — they survive tab
switches and cannot themselves be tabbed. The tabline is one global bar drawn full-width above the
main area (`tab_labels()` → `View.tabline` → each client's `render_tabline`).

**The ask (decided with the user):** each region — the main window region and *each* open dock —
gets its **own independent set of regular vim tab-pages**, each with its own tabline drawn at the
top of that region's band. `:tabnew`/`gt`/`gT`/`:tabclose` act on the **focused** region. The
main region's tabline becomes the top of the main column (it is no longer a separate global bar
spanning the docks); each dock's tabline sits at the top of its own band.

```
┌left──────┐┌main──────────────────┐┌right─────┐
│[1 expl ] ││[1 main.rs ][2 docs.md]││[1 refs  ]│   <- each region's own tabline
│ explorer ││ main.rs               ││ refs     │      (first row of its rect)
│          ││                       ││          │
└──────────┘└───────────────────────┘└──────────┘
┌bottom─────────────────────────────────────────┐
│[1 :term ][2 output][3 logs]                    │   <- bottom dock tabs independently
│ $                                              │
└────────────────────────────────────────────────┘
```

## Core idea: generalize the tab-swap to every layer

`switch_tab()` (tabs.rs) and `switch_layer()` (dock.rs) are *the same move*: stash the live
`self.windows` tree into a parked slot, swap a parked tree onto `self.windows`, update an index,
`enter_window`. They differ only in **which** slot. Today the two dimensions are tracked by
separate fields (`tabs`/`current_tab` for tabs; `docks`/`main_parked` for layers).

Unify them. Give **every layer its own tab stack**:

```rust
/// One layer's tab pages plus its active index. Exactly one (layer, tab) tree is
/// `None` across the whole editor — the focused layer's active tab — because that
/// tree is live on `Editor::windows`. Every other tab of every layer is parked in
/// its own `TabSlot::tree`.
struct TabStack {
    tabs: Vec<TabSlot>,   // TabSlot { id, tree: Option<WindowTree> } — unchanged
    current: usize,
}
```

`Editor` then holds:

```rust
main_tabs: TabStack,                 // replaces `tabs` + `current_tab`
dock_tabs: [Option<TabStack>; 4],    // replaces `docks` + (subsumes) `main_parked`
focused_layer: Layer,                // unchanged
```

The live tree on `self.windows` is, by definition, `stack(focused_layer).tabs[stack.current].tree`
— physically held in `self.windows` with that slot left `None`. This removes `main_parked` (main's
active tab parks in `main_tabs.tabs[current].tree` when a dock is focused) and the dock
`Option<WindowTree>` (a dock's active tab parks in `dock_tabs[side].tabs[current].tree`).

**Why this is the right shape, not just less code:** `dock_is_open(side)` becomes
`dock_tabs[side].is_some()` (presence = open, regardless of focus — no more "the focused dock's
slot reads `None`" special case). `switch_layer` and `switch_tab` collapse to one private
`swap_live_tree(into_slot, take_slot)` helper. The "exactly one `None` slot" invariant is the same
one tabs already maintain, now spanning two axes.

---

## Phase 1 — Core data-model refactor (NO behavioral change)

**Goal:** introduce `TabStack`, move tab state per-layer, route everything through it. UX is
byte-identical to today: docks stay single-tab, tabs stay main-only (tab ops keep calling
`ensure_main_layer()`). This isolates the risky pure refactor behind the existing test suite.

Files: `editor/mod.rs`, `editor/dock.rs`, `editor/tabs.rs`, `editor/windows.rs`, and the
`self.tabs`/`current_tab`/`main_parked`/`docks` read sites in `editor/{jumps,ex,mouse}.rs`,
`view.rs`, plus the server mirrors in `nxvim-lua/{ops,install,runtime}.rs`,
`nxvim-server/{dispatch,lifecycle,effects}.rs` (most go through existing helpers and need no
change once the helpers are rewritten).

Steps:
1. Add `struct TabStack { tabs: Vec<TabSlot>, current: usize }` with small helpers
   (`active_slot_mut`, `len`, `is_active(idx)`). Keep `TabSlot` as-is.
2. Replace fields: `tabs`/`current_tab` → `main_tabs`; `docks`/`main_parked` → `dock_tabs`.
   Update the constructor (mod.rs:~1010).
3. Add `fn layer_stack(&self, Layer) -> Option<&TabStack>` / `_mut`, and a private
   `swap_live_tree(&mut self, from: Layer, to: Layer, to_tab: usize)` that both `switch_layer`
   and `switch_tab` delegate to.
4. Rewrite in dock.rs: `dock_is_open` (= `dock_tabs[idx].is_some()`), `layer_tree`/`_mut`
   (focused → `&self.windows`; else the layer's active slot tree), `switch_layer`, `open_dock`
   (creates a `TabStack` of one tab), `close_dock` (drops the whole `TabStack`).
5. Rewrite in tabs.rs: `tab_tree`, `switch_tab`, `new_tab`, `close_tab_at`, the `tab_*`
   id/index helpers, and `tab_labels` to read `self.main_tabs` (still main-only this phase).
   `all_window_ids` walks `main_tabs` then each `dock_tabs` stack's every tab.
6. `tabline_visible`/`tabline_rows`/`relayout`/`dock_bands` keep reading `main_tabs.len()` for
   now (geometry unchanged this phase).

**Verify:** `cargo test --workspace` green with **zero test edits**. This is the gate — a pure
refactor must not move any observable behavior. Commit, pause for review.

---

## Phase 2 — Tab ops act on the focused layer (model + RPC, no client paint)

**Goal:** a focused dock can now hold >1 tab; tab navigation is per-region. Data only — verified
over RPC, not yet painted by clients.

Steps:
1. Drop `ensure_main_layer()` from `new_tab`/`switch_tab`/`close_tab`/`goto_tab_*`/`tab_split`/
   `tab_only`/`tabmove` so they target `stack_mut(focused_layer)`. Generalize each to take/operate
   on the focused layer's `TabStack` instead of the hard-coded `main_tabs`.
2. `close_tab` on a dock with a single tab: closing the **last** tab of a *dock* closes the dock
   (calls `close_dock`), not `E784` — `E784` still guards the last tab of **main**.
3. Per-layer label projection: `fn tab_labels_for(&self, Layer) -> Vec<TabLabel>` and a
   `fn region_tablines(&self) -> RegionTablines { main, left, right, top, bottom }` (each a
   `Vec<TabLabel>` + active index, empty when that region's tabline is hidden per its own
   `showtabline` gate). Keep the old `tab_labels()` as `tab_labels_for(Layer::Main)` so existing
   call sites compile.
4. Server: extend `View` (core `view.rs`) + the redraw map (`redraw.rs`) to carry per-region
   tablines + per-region current index, in addition to (for now) the legacy `tabline`/`current_tab`
   so nothing breaks mid-migration. Add `nvim_*`/`nx` read surface as needed for tests.
5. RPC niceties: `current_tab_id`/`tab_ids`/`tab_count` etc. gain a notion of *which layer* —
   default to focused layer to preserve `nvim_get_current_tabpage` semantics for main.

**Verify (new `tests/tabs.rs` cases):** open a dock; `:tabnew` while it's focused → dock has 2
tabs, main still 1; `gt`/`gT` in the dock cycle only the dock's tabs; cross to main (`<C-w><C-w>`)
and `gt` cycles only main's; closing the dock's last tab closes the dock. Commit, pause.

---

## Phase 3 — Layout: a tabline row per region

**Goal:** each region reserves its **own** top row for its tabline; the global full-width tabline
row disappears. Core geometry + `View` projection only; assert via TUI paint math, then clients in
Phase 4.

The model: a region's tabline is the **first row of that region's rectangle** (when visible). The
`WindowTree` lays out below it. This replaces the single global `tabline_rows()` chrome row.

Steps:
1. Per-region visibility: `fn tabline_rows_for(&self, Layer) -> usize` gating on that layer's own
   `showtabline` + `stack(layer).len()`. (`showtabline` stays a global option; the *count* is
   per-region.)
2. `relayout` (windows.rs:1924): for each open layer, shrink its rect height by
   `tabline_rows_for(layer)` and lower its `y`-origin handling so the tree occupies the rows
   *below* its tabline. The middle-band `mid_h` no longer subtracts a global tabline row; instead
   the main rect itself loses its tabline row. Top dock: its tabline is its own first row (it no
   longer sits "above the global tabline" — there is no global tabline).
3. `dock_bands` chrome: drop `tabline_rows()` from the global `chrome` term; each band's content
   already accounts for its own tabline via step 2. Re-check the clamp loops keep ≥1 content row
   per region after its tabline is subtracted.
4. `View` (core `view.rs` `from_editor`): emit per-region `tabline_rows` alongside the per-region
   labels from Phase 2. Remove the legacy single `tabline`/`current_tab`/`tabline_segments` once
   all consumers move (Phase 4) — keep until then.
5. `nxvim-view` (`view.rs`): add the per-region tabline fields + parsing; keep legacy fields until
   Phase 4 flips clients.

**Verify:** core unit-of-behavior via the harness redraw map — open docks, assert each region's
reported origin/height accounts for its tabline row; main with 2 tabs shrinks the main column (not
the full width) by one row while the left dock with 1 tab is unaffected. Commit, pause.

---

## Phase 4 — Clients render per-region tablines

**Goal:** TUI, GUI, web each paint every region's tabline in its reserved top row at the correct
absolute origin. Mechanical mirroring of the Phase-3 math across the three renderers.

Per memory `[[gui-window-not-screencapturable-from-agent]]` and `[[web-client-driveable-via-playwright]]`:
TUI is fully agent-verifiable (paint tests); GUI runs but isn't screencapturable here; web needs an
emscripten wasm rebuild (`emcc` not installed) + Playwright — mirror the verified TUI math and say
so plainly (`[[dont-conflate-loads-with-works]]`).

Steps:
1. **TUI** (`nxvim-tui/src/render.rs`): `DockLayout` already computes each region's rect; draw that
   region's tabline into its first row, then the tree below. Generalize `render_tabline` to take a
   region's labels + active index + rect. Remove the standalone global tabline area.
2. **GUI** (`nxvim-gui/src/render.rs`): same — `region_origin` already exists; paint each region's
   tabline at its origin row using `build_tabline` per region.
3. **Web** (`nxvim-edithost/web/index.html`): mirror in JS — `regionOrigin`/`dockGeo` already give
   per-region origins; render each region's tabline at its top row. (Verified-by-mirror only.)
4. Delete the now-dead legacy global `tabline`/`current_tab` fields from core `View`, `nxvim-view`,
   and `redraw.rs` once all three clients consume the per-region fields.

**Verify:** TUI `tests/paint.rs` — main 2 tabs + left dock 1 tab + bottom dock 3 tabs: assert each
tabline string lands on the right region's first row at the right columns, active cell highlighted.
Commit, pause.

---

## Phase 5 — Polish, example, docs, memory

1. Mouse: click a region's tabline cell to switch that region's tab (extend `editor/mouse.rs`
   hit-testing to the per-region tabline rows). Server test driving a click.
2. `examples/per-region-tabs/`: a runnable config opening a couple of docks with multiple tabs +
   a sample file, verified end-to-end (`[[example-config-for-testing]]`).
3. Update `docs/architecture.md` (tabline/dock sections) and the docks plan's cross-reference.
4. Update memory `[[permanent-docks-feature]]` with the per-region-tab generalization (TabStack,
   one-`None`-slot invariant across two axes, per-region tabline = first row of region rect).

---

## Risks & notes

- **The invariant** "exactly one (layer, tab) tree is `None`" is the linchpin — debug-assert it in
  `relayout`/`swap_live_tree` to catch a stash/swap that drops or duplicates a tree.
- **`showtabline` is global, count is per-region.** At the default `showtabline=1`, a region shows
  its tabline only with ≥2 of *its own* tabs — so a fresh single-tab dock shows none, matching vim
  intuition per region.
- **`all_window_ids` / `nvim_list_wins` ordering** now spans main tabs then each dock's tabs;
  keep a stable, documented order so the server window mirror and `vim.fn.*` stay deterministic.
- **`next_win_id`/`next_tab_id`** stay global monotonic counters (ids never reused across any
  layer/tab), unchanged.
- Phase 1 is the only phase that may NOT touch tests; every later phase adds coverage. Each phase
  ends green + committed + a review pause (`[[big-feature-workflow-cadence]]`).
