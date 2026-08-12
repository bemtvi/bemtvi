# Dock toggle & auto-hide — phased plan

> Working checklist for VSCode-style **toggle** (show/hide a dock while keeping its
> content) and **auto-hide** (a dock collapses when focus leaves it). Project
> convention: lives at `docs/plans/2026-06-14-dock-toggle-autohide.md` alongside the
> other `docs/plans/*.md`.
>
> **Status: ✅ all four phases landed 2026-06-14.** Hidden-state model + hide/show/
> toggle, the `autohide` option with a `switch_layer` focus-leave hook, the
> `btv.dock.toggle`/`hide`/`show` + `:Dock*` surface, and the `tests/dock.rs`
> coverage + example all in. One infra fix rode along: a waiting predicate-drain
> `wait_redraw` was added to the test harness to kill the take-latest redraw race
> that the extra tests exposed; `all_window_ids` now also excludes hidden docks so
> `nvim_list_wins` matches `window_ids`/`open_layers`.

## Context

The permanent-docks feature (`docs/plans/2026-06-14-permanent-docked-panels.md`) and
per-region tablines (`docs/plans/2026-06-14-per-region-tablines.md`) are both fully
landed. A dock is a global, cross-tab `WindowTree` parked in `Editor::dock_tabs:
[Option<TabStack>; 4]`; the *focused* layer's active tab is swapped live onto
`Editor::windows`. `editor/dock.rs` owns open/close/focus.

Today the only way to make a dock disappear is `close_dock` (`btv.dock.close`), which
**drops the whole `TabStack`** — every tab's tree. Reopening mints a fresh scratch
buffer: internal splits, the dock's tab pages, cursor positions and scroll are all
lost. VSCode's panel toggle instead **preserves** the panel's contents across
hide/show. That preservation is the entire value of this feature over close/open.

### Key structural finding

`Editor::dock_is_open(side)` is the single chokepoint nearly every *visibility*
decision flows through: `open_layers()` (→ relayout/render/`tree_of_window`), the
`<C-w><C-w>` cross to `last_dock` (command.rs:1251), the mouse hit-tests
(mouse.rs:451–466), and `dock_bands` (windows.rs:2062). Meanwhile the internal
*tree-resolution* helpers — `stack`/`stack_mut`, `layer_tree`, `parked_trees_mut`,
`slot_tree_mut` — read `dock_tabs` **directly**, not through `dock_is_open`.

So a **hidden** dock = a dock whose `TabStack` still lives in `dock_tabs` (content,
splits, tabs, cursor all preserved) but which is excluded from layout, render, mouse,
focus-crossing and `open_layers`. We get all of that for free by teaching the
*visibility* predicate about a hidden flag, while the tree-resolution helpers keep
seeing the parked content. **No client (TUI/GUI/web) changes are needed**: a hidden
dock simply reports zero bands in the redraw, exactly like a closed one.

## Naming / model

- Add `dock_hidden: [bool; 4]` to `Editor` (mod.rs, beside `dock_sizes` ~l.552).
- Split the predicate cleanly:
  - `dock_exists(side)` (new, private) = `dock_tabs[idx].is_some()` — "has state".
  - `dock_is_open(side)` (existing, **keep the name**) becomes "open **and visible**"
    = `dock_exists(side) && !dock_hidden[idx]`. Every current visibility call site
    already wants this, so they need no edits; the only edits are the lifecycle
    guards in `open_dock`/`close_dock`/`focus_dock`, which must switch to
    `dock_exists` so they still act on a *hidden* dock.
- A hidden dock is, by construction, a non-focused layer (we cross to main before
  hiding), so its `TabStack` has every tree `Some` — identical to any background
  layer. The "exactly one (layer,tab) tree is `None`" invariant is untouched.

---

## Phase 1 — Core: hidden state + hide/show/toggle (model only)

**Goal:** introduce the hidden flag and the three lifecycle methods, with no
user-facing surface yet. Existing behavior is byte-identical when nothing is hidden
(`dock_hidden` all `false` ⇒ `dock_is_open` reduces to today's definition).

Files: `editor/mod.rs`, `editor/dock.rs`.

Steps:
1. Add `dock_hidden: [bool; 4]` to `Editor` + `[false; 4]` in the constructor
   (mod.rs:~1084, beside `dock_tabs`/`dock_sizes`).
2. In dock.rs: add private `dock_exists(side)`; redefine `dock_is_open(side)` =
   `dock_exists && !dock_hidden[idx]`. Update its doc comment (the "presence *is*
   open" line is now "presence + not-hidden").
3. Fix the lifecycle guards to use `dock_exists`:
   - `open_dock`: branch on `dock_exists`. If it exists (whether hidden or visible):
     clear `dock_hidden[idx]`, honor the new size, optionally swap `buf`, focus it.
     Else create as today.
   - `close_dock`: top guard `if !self.dock_exists(side) { return; }`; on close also
     reset `dock_hidden[idx] = false` (drop state cleanly).
   - `focus_dock`: focusing implies showing — `if self.dock_exists(side) { clear
     hidden; switch_layer(Dock(side)); }`.
4. New methods:
   - `hide_dock(side)`: no-op unless visible. If it's the focused layer, cross to
     main first (`switch_layer(Main)`) so its tree parks. Set `dock_hidden[idx]=true`;
     `relayout()`; `ensure_visible()`. Content stays in the `TabStack`.
   - `show_dock(side)`: no-op unless `dock_exists`. Clear `dock_hidden[idx]`, then
     `focus_dock(side)` (show + focus, matching "bring up the panel"). `relayout()`.
   - `toggle_dock(side)`: visible → `hide_dock`; hidden (exists) → `show_dock`;
     absent → echo `btv.dock: no dock on {side} to toggle` (opening a fresh dock is
     `:DockOpen`'s job, since toggle has no size/buffer to mint one from).
5. String-keyed wrappers beside the existing `*_named` (loud `E474` on bad side):
   `hide_dock_named`/`show_dock_named`/`toggle_dock_named`, and a read
   `dock_is_hidden_named(side) -> bool`.

**Verify:** `cargo test --workspace` green with **zero test edits** (the predicate
redefinition is inert while nothing is hidden). Commit, pause for review.

---

## Phase 2 — Auto-hide option + focus-leave hook

**Goal:** a dock marked `autohide` collapses itself the moment focus crosses out of
it; re-showing (toggle/focus) brings its preserved content back.

Files: `options.rs`, `editor/dock.rs`, `bemtvi-lua/src/prelude/btv.lua`.

Steps:
1. Add `pub auto_hide: bool` to `DockOptions` (options.rs:199). Doc it as the
   VSCode-style collapse-on-blur flag.
2. `set_dock_option_num` (dock.rs:344): `"autohide" => self.dock_options[s.idx()]
   .auto_hide = value != 0`. Add `autohide` to the prelude's `DOCK_OPT_DEFAULT`
   (btv.lua:51) and the known-options list (btv.lua:90) so `btv.dock.opt(side).autohide
   = true` and inline `btv.dock.open{ autohide = true }` validate.
3. Focus-leave hook. The chokepoint for layer focus changes is `switch_layer`
   (dock.rs:174). Capture `let prev = self.focused_layer;` at the top; after the swap
   and `self.focused_layer = target;`, if `prev` was a `Dock(s)` with
   `dock_options[s].auto_hide` and `prev != target` → set `dock_hidden[s] = true`
   before the trailing `relayout()` (the dock's tree is already parked by the swap, so
   this is just the flag + the relayout that already runs).
   - Confirm every focus *cross* routes through `switch_layer`: the keyboard
     `<C-w><C-w>` path (command.rs `execute_window_layer`) and the mouse
     click-to-focus path (mouse.rs, from commit `1a9fe1d`). If the mouse path sets
     `focused_layer` without `switch_layer`, factor the hook into a tiny
     `auto_hide_on_leave(prev)` helper and call it from both. (Expectation: both go
     through `switch_layer`; verify during impl, don't assume.)
   - Guard against the re-entrancy: `hide_dock` calls `switch_layer(Main)` when the
     dock is focused — at that point `prev` is the auto-hide dock, so the hook would
     also fire and set hidden. That's harmless (idempotent: hide sets it too) but
     make `hide_dock` rely on the hook OR set the flag itself, not double-relayout.

**Verify (new `tests/dock.rs` cases):** open an `autohide` left dock, type in it,
`<C-w><C-w>l` (or click main) → dock collapses (zero band, main reclaims space);
`btv.dock.toggle('left')` brings it back with the typed text intact. A non-autohide
dock stays put when focus leaves. Commit, pause.

---

## Phase 3 — Lua / RPC / ex surface

**Goal:** user-facing `toggle`/`hide`/`show`, dogfooding the btv API.

Files: `bemtvi-lua/src/ops.rs`, `bemtvi-server/src/effects.rs`,
`bemtvi-lua/src/install.rs`, `bemtvi-lua/src/prelude/btv.lua`.

Steps:
1. `DockOp` (ops.rs:61): add `Toggle { side: String }`, `Hide { side: String }`,
   `Show { side: String }` beside `Close`/`Focus`.
2. effects.rs drain (effects.rs:173): route them to `toggle_dock_named` /
   `hide_dock_named` / `show_dock_named`.
3. install.rs (after the `focus` binding, ~l.253): add `btv.dock.toggle/hide/show`,
   each a one-arg `create_function` pushing its `DockOp` (mirror `close`/`focus`).
4. prelude btv.lua (after the `:DockFocus` block, ~l.114): `:DockToggle` / `:DockHide`
   / `:DockShow` ex-commands wrapping `btv.dock.toggle/hide/show`. Keep them thin like
   the existing `:DockClose`/`:DockFocus`.

**Verify:** `btv.dock.toggle('left')`, `:DockToggle left`, and an `autohide` set via
`btv.dock.opt('left').autohide = true` all drive docks end-to-end over RPC. Commit,
pause.

---

## Phase 4 — Tests, example, docs, memory

**Goal:** prove behavior end-to-end; ship a runnable showcase; record the design.

Black-box tests in `crates/bemtvi-server/tests/dock.rs` (harness per CLAUDE.md):
1. **Content survives toggle**: open a left dock, `<C-w>v` to split it + type text +
   move the cursor, cross to main, `btv.dock.toggle('left')` (hide) → dock gone, main
   reclaims columns, `nvim_list_wins` count drops; toggle again (show) → the *same*
   two windows, text, and cursor position return (not a fresh scratch).
2. **Hidden ≠ closed**: after hiding, `nvim_list_bufs` still lists the dock's buffer
   and the dock window is gone from `nvim_list_wins`; after closing, contrast that a
   reopen mints a fresh scratch (locks in the close-vs-hide distinction).
3. **Auto-hide on focus-leave**: `autohide` dock collapses on `<C-w><C-w>` cross and
   on a main-window mouse click; re-shows via toggle/focus with content intact.
4. **Toggle on an absent side** echoes and is a no-op (no panic).
5. **Edits to a hidden dock's buffer** (via its buffer id) still land — the parked
   tree stays live in `parked_trees_mut` (guards against hidden ⇒ orphaned).
6. **Redraw**: a hidden dock contributes zero band (row/col ordering unchanged from
   no-dock); showing it restores the band — mirrors the Phase-5 docks redraw asserts.

Example: extend `examples/dock/init.lua` (or `examples/per-region-tabs/`) with a
toggle keymap (e.g. `<leader>e` → `btv.dock.toggle('left')`) and one `autohide` dock,
verified by a `*_example_config_runs` test.

Docs/memory: add a *toggle / auto-hide* note to the docks bullet in
`docs/architecture.md`; update memory `[[per-region-tablines-and-dock-options]]` (or a
new dock memory) with the hidden-state model.

**Checkpoint:** `cargo test --workspace`, `cargo fmt --all`,
`cargo clippy --all-targets -- -D warnings`.

---

## Phase 5 — Collapsed-dock indicator (chips) — ✅ landed 2026-06-15

**Why:** a fully-invisible hidden dock gives no hint it exists. User chose a
**statusline-chip** affordance over an edge rail (keeps the main area full-size).

**Design:** a hidden dock shows a clickable chip `▸{label}` (its `btv.dock` title, or
the side keyword when untitled) on the **command-line row when idle** — the one
global, full-width bottom row present at every `laststatus` (the literal global
statusline only exists at `laststatus=3`; default is `2`). Chips render only when
that row is idle (projected message empty, not command-line mode), starting at col 0,
joined by a space; a transient message or a typed command takes the row and the chips
reappear after. No main-area space is lost.

Steps:
1. Core `dock.rs`: `hidden_dock_chips() -> Vec<(DockSide, String)>` — each existing &
   hidden dock in `DockSide::ALL` order, label = title or `keyword()`.
2. Core `view.rs`: `View.hidden_docks: Vec<String>` (labels), populated in
   `from_editor`. Empty ⇒ nothing hidden.
3. Server `redraw.rs`: encode `hidden_docks` as a string array.
4. `bemtvi-view`: parse `hidden_docks: Vec<String>`.
5. Clients (TUI/GUI/web): on the command row, when `hidden_docks` non-empty &&
   message empty && not command mode, paint `▸{label}` chips from col 0.
6. Core `mouse.rs`: `hidden_chip_at(row, col)` mirroring the chip geometry on the
   cmdline row (`row == self.height`), gated by the same idle condition; a left
   press there calls `show_dock(side)`. Wired into `mouse_left_press`.
7. Tests in `tests/dock.rs`: a hidden dock projects its chip; the chip clears on
   show; a click on the chip re-shows the dock; an untitled dock falls back to the
   side keyword.

## Risks & notes

- **The predicate split is the linchpin.** Every place that should *ignore* a hidden
  dock must go through `dock_is_open` (visibility); every place that must still *find*
  its parked content goes through the `dock_tabs`-reading helpers. Don't leak a raw
  `dock_tabs[idx].is_some()` into a visibility decision, or hidden docks reappear.
- **Cross-to-main before hiding** is mandatory (same reason `close_dock` does it): a
  hidden dock must be a parked layer, never the live `self.windows`.
- **Auto-hide must not fight focus.** The hook fires only on *leaving* an autohide
  dock; entering one (or any non-dock focus change) never hides anything. Re-entrancy
  via `hide_dock`'s own `switch_layer(Main)` is idempotent — verify no double relayout
  jitter.
- **Out of scope (v1):** a persistent activity-bar affordance showing a hidden dock
  exists (VSCode's thin strip); persisting hidden/autohide state across restarts
  (that's the separate "dock persistence" follow-up). Toggling a never-opened dock
  does not auto-create one.
