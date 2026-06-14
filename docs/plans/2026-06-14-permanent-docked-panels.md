# Permanent panels (VSCode-like docked panels) — phased plan

> On approval, this document is saved to the repo as
> `docs/plans/2026-06-14-permanent-docked-panels.md` (project convention, cf. the other
> `docs/plans/*.md`) and becomes the working checklist.

## Context

nxvim today has exactly two window concepts: the per-tab split tree (`Editor::windows`, a
`WindowTree`) and a single read-only bottom overlay (`Editor::panel`, the `:messages`/`:ls`
list). There is no way to keep an *editable* region pinned to a screen edge that survives
splits, window switches, and tab changes.

The user wants VSCode-style **docked panels**: editable buffer-window regions pinned to a
screen edge that the main editing area can never disturb. Locked-in decisions:

- **Content** — a panel holds normal editable buffer windows (can be split internally).
- **Scope** — **global across all tabs**; lives on `Editor`, outside the per-tab tree.
- **Focus** — `<C-w><C-w>` is a *layer-switch* prefix. Plain `<C-w>{cmd}` acts within the
  focused layer; `<C-w><C-w>{cmd}` crosses to the other layer (main ↔ panels) and runs the
  command there. Once focus is in a panel, single `<C-w>v`/`<C-w>l`/`<C-w>s` operate inside it;
  `<C-w><C-w>` returns to main.
- **Creation** — an `nx.*` Lua API plus a thin ex-command wrapper.
- **Top dock placement** — the top dock sits **above the tabline** (owns the very top rows).

### Naming
Use **`Dock`** internally and **`nx.dock.*`** for the Lua surface, to avoid collision with the
existing read-only `Panel` (`editor/panel.rs`, `Editor::panel`) and the existing `vim.panel.*`
Lua table. (User-facing term stays "panel"; the code type is `Dock`.)

## Core idea: reuse the tab-swap pattern

The whole editing machine reads its target from `self.windows.current` (e.g. `cur_buffer()` →
`self.windows.cur().buffer`, mod.rs:998; `split`/`close_window`/`focus_dir`/`resize_window` all
act on `self.windows`). Tabs already exploit this: `switch_tab()` (tabs.rs:161) does
`std::mem::replace(&mut self.windows, incoming)` so the active tab's tree is always live at
`self.windows`.

**Do the same for layers.** When a dock is focused, *its* `WindowTree` is swapped onto
`self.windows`; the displaced tree is parked. `split`/`close`/`focus_dir`/editing/redraw then
"just work" within the focused dock with **zero retargeting** of the ~40 `self.windows.*` sites.

---

## Phase 1 — Data model + swap plumbing

**Goal:** introduce docks/layers as state with no behavioral change yet.

Add near `windows`/`tabs` (mod.rs:418) and the `WinDir`/`SplitDir` enums (mod.rs:333):

```rust
pub(crate) enum DockSide { Left, Right, Top, Bottom }
pub(crate) enum Layer { Main, Dock(DockSide) }

docks: [Option<WindowTree>; 4],   // global; None = closed
main_parked: Option<WindowTree>,  // holds main tree while a dock is focused
focused_layer: Layer,
dock_sizes: [usize; 4],           // cols (L/R) or rows (T/B)
last_dock: DockSide,              // target of a non-directional <C-w><C-w>{cmd}
dock_separators: Vec<Separator>,  // dock/main edge borders, rebuilt each relayout
```

**Invariant (mirrors tabs):** the layer named by `focused_layer` has its tree live on
`self.windows`; the others are parked in `docks[..]` / `main_parked`; the active slot holds `None`.

Helpers (the only places that move trees):
- `park_active()` — `self.windows` → its home slot.
- `make_active(Layer)` — pull that layer's parked tree onto `self.windows`; update
  `focused_layer` (+ `last_dock` when target is a dock).
- `each_tree(_mut)()` — iterate live + parked trees (for relayout/render).
- `tree_of_window(id)` / `_mut` — resolve a `WindowId` across main + all docks.

Wire `switch_tab`/`new_tab` (tabs.rs:161/190) to **cross to main first** if `focused_layer != Main`
(see Phase 6 risk note) so the main tree is on `self.windows` before any tab swap.

Generalize the cross-tree scan sites to also include docks (global → scanned once):
`window_showing()` (windows.rs:906); id-targeted `window_buffer/cursor/rect/options` + `set_window_*`
(windows.rs:897–1185, route through `tree_of_window`); `window_ids()`/`window_count()`
(windows.rs:862/869); jumplist line-adjust (jumps.rs:189/196).

**Checkpoint:** compiles; `cargo test --workspace` green (no docks open ⇒ identical behavior).

---

## Phase 2 — Geometry / `relayout()` (windows.rs:1871)

**Goal:** reserve edge regions and lay out every tree; still no docks open ⇒ identical output.

1. Carve the **top dock first** from the full screen at `y=0` — it sits **above the tabline**.
   Then reduce by tabline (top) + bottom panel + global statusline as today; the remainder is
   `area`. Vertical order top→bottom:
   **top dock → tabline → [left dock | main | right dock] → bottom dock → read-only panel →
   global statusline**.
2. Carve the remaining docks in fixed order **bottom, left, right** (bottom full width; L/R take
   the remaining inner height). Subtract 1 cell per dock for its separator (like inter-child
   borders, windows.rs:570). Clamp so the main rect keeps ≥1 col/row (mirror the `panel_rows()`
   clamp, panel.rs:264). Push a `Separator` per dock edge into `dock_separators`.
3. Remaining `area` = main rect.
4. Lay out **every** tree via `each_tree_mut()`: each parked dock tree in its dock rect, the main
   tree in the main rect. `cursor_off` is only meaningful for the focused tree (reads
   `self.cursor`/`self.top`); pass `(0,0)` for the rest (same fallback the close path tolerates,
   windows.rs:432). Per-tree `WindowTree::layout` already positions that tree's floats within its
   rect (windows.rs:426).

**Checkpoint:** existing tests green; degenerate-size clamps verified by reasoning/tests.

---

## Phase 3 — Core dock methods (new file `editor/dock.rs`)

**Goal:** open/close/focus a dock from a test-only path.

- `open_dock(side, size, buf)` — if open, resize/refocus; else mint a scratch buffer (reuse the
  `:enew` path in `ex.rs`) or use `buf`, build `WindowTree::with_window(alloc_window_id(), buf,
  default)` (windows.rs:300), store in `docks[side]`, set `dock_sizes[side]`, then focus it
  (mirror `new_tab`, tabs.rs:190: stash → park → make_active → relayout → enter_window).
- `close_dock(side)` — if focused, cross to main first; drop `docks[side]`; `relayout()`. Buffers
  stay loaded (matches windows.rs:1190).
- `focus_dock(side)` — the layer swap targeting `Dock(side)`; no-op if closed.

**Checkpoint:** a temporary `exec_lua`/Rust test can open a dock and read back window count.

---

## Phase 4 — Focus & `<C-w><C-w>` (`editor/command.rs`)

**Goal:** the doubled prefix crosses layers; single `<C-w>` stays in the focused layer.

Parsing (today `<C-w><C-w>` → `FocusCycle`, command.rs:50):
1. Add `Stage::WindowLayerPending` beside `WindowPending` (command.rs:349).
2. In the `WindowPending` arm (command.rs:555): if next key is `<C-w>`, go to `WindowLayerPending`
   instead of resolving `FocusCycle`. `<C-w>w`/`<C-w>W` still cycle within a layer — intentional
   vim divergence; update the note at command.rs:51.
3. Add `LayerWindowCmd` + `ResolvedCommand::WindowLayer(..)` (command.rs:398/416) and a pure
   `layer_window_command(key)`: `h/j/k/l` → `CrossDir(WinDir)`; else →
   `CrossThenWindow(window_command(key)?)`.

Executor — `execute_window_layer(cmd)` next to `execute_window` (command.rs:1174). The layer
switch wraps the swap like `switch_tab`: `stash_focused_view()` → `park_active()` →
`make_active(target)` → `relayout()` → `enter_window(self.windows.current)`.
- From **main**: `CrossDir(Left/Right/Up/Down)` → focus L/R/T/B dock (no-op if closed);
  `CrossThenWindow(cmd)` → cross to `last_dock`, then run `cmd`.
- From a **dock**: any cross → return to `Layer::Main`, then run `cmd` there.

Single `<C-w>v`/`<C-w>l`/`<C-w>c`/resize need **no** change (act on `self.windows` = focused
layer). Guard the per-tab-specific arms in `execute_window` when `focused_layer != Main`:
`<C-w>q`/`<C-w>c` on a dock's last window → `close_dock(side)`; `<C-w>T` → no-op.

**Checkpoint:** keystroke-driven focus crossing works (verified by Phase 7 tests 2–4).

---

## Phase 5 — Rendering (core projection + all 3 clients)

**Coordinate ownership (key finding):** `ViewRect` is **windows-area-relative**, not absolute
(view.rs:89). The core lays the main tree out at origin (0,0); each **client** computes the
windows-area origin itself and offsets — TUI via a ratatui `Layout::vertical` (nxvim-tui
`render.rs:185`, `window_area` at `render.rs:325`), GUI via `origin=(0,tabline_rows)`
(nxvim-gui `render.rs:539`), web in JS. The server (`redraw.rs` `rect_value`/`separator_value`,
~l.578) sends rects as-is. So docks **cannot** be made visible by core/server alone — each client
must learn the dock bands.

**Approach — region-tagged windows + dock bands (one coordinate space per region):**
- Lay out each tree (main + each open dock) at **origin (0,0)** in its own region size (Phase 2
  already does this). Tag each `WindowView`/`WindowLayout` with a `region`
  (`Main`/`DockLeft`/`DockRight`/`DockTop`/`DockBottom`).
- Add the four band sizes to `View`: `dock_top`/`dock_bottom`/`dock_left`/`dock_right` (cells; 0
  when absent). With no docks open every band is 0 and `region=Main` ⇒ **identical output**.
- Each client maps region → absolute origin and places chrome:
  vertical `[top dock][tabline][left|main|right][bottom dock][read-only panel][cmdline]`;
  main origin `(dock_left, dock_top + tabline_rows)`. Dock separators/borders drawn from the
  per-region separators + `dock_separators`.

Core changes:
- `window_layouts()` (windows.rs:1385) — emit windows from main + every open dock via `each_tree()`
  with `region` set. **`focused` = `id == self.windows.current && tree is the focused layer`**.
  Parked trees' current windows use their `saved_*` view (stashed on layer-park), so the existing
  `if focused { self.cursor } else { w.saved_cursor }` branch (windows.rs:1396) holds.
- `from_editor` (view.rs:304) — `View.separators` from a new `all_separators()` (focused tree +
  parked trees + `dock_separators`); populate the four band sizes. `WindowView` gains `region`.
- `redraw.rs` — encode `region` on each window and the band sizes.

Client changes (TUI, GUI, web):
- **TUI** (`crates/nxvim-tui/src/render.rs`) — extend the vertical `Layout` with top/bottom dock
  bands, split the middle row horizontally into `[left | wins | right]`, offset each window by its
  region origin, paint dock separators/borders.
- **GUI** (`crates/nxvim-gui/src/render.rs`) — same band math against the cell grid `origin`.
- **Web** (`crates/nxvim-web` edithost JS) — mirror the band math in the JS renderer.

**Checkpoint:** redraw assertion shows dock content + single cursor (Phase 7 test 11); manual
verify in TUI (`cargo run`), GUI, and web (Playwright).

---

## Phase 6 — Creation API (`nx.dock.*` + ex command)

**Goal:** user-facing surface, dogfooding the nx API.

- **Effect op** — `DockOp { Open{side,size,buf}, Close{side}, Focus{side} }` in
  `nxvim-lua/src/ops.rs` (beside `WindowOp` ops.rs:419 / `PanelOp` ops.rs:33); drain in
  `nxvim-server/src/effects.rs` (beside `apply_window_op` effects.rs:394) into the Phase 3 methods.
- **Lua surface** — `nx.dock` table in `nxvim-lua/src/install.rs` (mirror `vim.panel`,
  install.rs:140) with `open/close/focus` pushing `DockOp`s, validating `side` loudly (no silent
  fallback — cf. `FloatAnchor::from_keyword`, windows.rs:69). Reads via an `nx._docks` mirror like
  `nx._wins` (api.lua:73): `nx.dock.is_open/list`.
- **Ex command** — `:DockOpen`/`:DockClose`/`:DockFocus` defined in the Lua prelude in terms of
  `nx.dock.*` (dogfood directive).

**Risk note (highest):** `switch_tab`/`new_tab` swap `self.windows`; if a dock tree is live there
both tab and layer state corrupt. The Phase-1 cross-to-main-first fix is mandatory; `docks` are
never touched by tab ops.

Other edge cases: close last window in a dock → `close_dock`; main tree still refuses its last
window (E444, windows.rs:1487). New dock shows a scratch unless `buf` given. v1: `<C-w>>`/`+`
inside a dock resizes intra-dock splits only — resizing the dock's reserved size is deferred
(later: `<C-w><C-w>>` adjusts `dock_sizes`, or separator drag via `resize_window_id`,
windows.rs:1630). Tabline window-count (windows.rs:838) keeps counting main-tree windows only.

**Checkpoint:** `nx.dock.open/close/focus` and the ex-commands drive docks end-to-end.

---

## Phase 7 — Tests + example

**Goal:** prove behavior end-to-end; ship a runnable example.

Black-box integration tests in `crates/nxvim-server/tests/dock.rs` (harness per CLAUDE.md — feed
vim keys, assert `nvim_buf_get_lines`/cursor/redraw):

1. `nx.dock.open{side='left',size=30}` → left window + separator appears, main shrinks,
   `nvim_list_wins` count grows.
2. `<C-w><C-w>h` focuses the left dock; typed text lands in the dock buffer, not main; a cross
   returns to main.
3. Focus a dock, `<C-w>v` → 2 windows in the dock; `<C-w>l` moves between them without leaving.
4. `<C-w><C-w>v` from main crosses to `last_dock` and splits there.
5. Edit independence: dock edits don't touch main and vice-versa.
6. Global across tabs: open dock, `:tabnew`/`gt` → same dock visible; switch back intact.
7. **Tab switch while dock focused** (risky case): no panic, main tree correct, editing routes
   to main.
8. Close last dock window collapses the dock; main reclaims space.
9. Four docks open at once: each edge reserved, main rect non-degenerate.
10. `nx.dock.close{side='left'}` removes it; buffer stays loaded (`nvim_list_bufs`).
11. Terminal cursor drawn only in the focused layer (redraw assertion).
12. Top dock renders **above** the tabline (redraw row ordering).

Ship a runnable `examples/dock/` config + sample (per project convention).

**Checkpoint:** `cargo test --workspace`, `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`.

---

## Critical files

- `crates/nxvim-core/src/editor/mod.rs` — Editor struct, layer/dock fields, swap helpers.
- `crates/nxvim-core/src/editor/windows.rs` — `relayout`, `window_layouts`, per-tree layout,
  separators, id-targeted window APIs.
- `crates/nxvim-core/src/editor/dock.rs` — new: open/close/focus.
- `crates/nxvim-core/src/editor/command.rs` — `Stage`, `LayerWindowCmd`, `execute_window_layer`.
- `crates/nxvim-core/src/editor/tabs.rs` — cross-to-main fix in `switch_tab`/`new_tab`.
- `crates/nxvim-core/src/view.rs` — `from_editor`, `all_separators`.
- `crates/nxvim-lua/src/ops.rs` + `crates/nxvim-server/src/effects.rs` +
  `crates/nxvim-lua/src/install.rs` — `DockOp` + `nx.dock` surface.
- `crates/nxvim-server/tests/dock.rs` — new: integration tests.

## Follow-up

Per-region tab pages & tablines build on this — each dock (and the main area) gained its own
independent tab stack and tabline, plus a per-dock option scope and tabline mouse clicks. See
[`2026-06-14-per-region-tablines.md`](2026-06-14-per-region-tablines.md).
