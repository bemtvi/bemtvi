# Picker `<C-t>` open-in-tab + `'switchbuf'`-aware jumps

Status: implemented — 2026-06-21 (all four phases landed)

## Context

Two related editor gaps:

1. **Missing feature — `<C-t>` in the picker** should open the highlighted entry in a **new
   tab** (telescope's `select_tab`). Today the picker has no open-in-tab action; confirm only
   opens in the current window.

2. **Jump-to-already-open-buffer should switch tabs.** When a buffer is already shown in
   another tab, jumping to it (picker, LSP go-to, quickfix, marks) should focus that tab
   instead of re-opening it in the current window. Vim models this with `'switchbuf'`
   (`useopen`/`usetab`). The option **exists but `useopen`/`usetab` are unimplemented**
   (`crates/bemtvi-core/src/options.rs:139-144` — "usetab is not yet acted on"). We implement
   them and **default `'switchbuf'` to `usetab`** so the behavior is on out of the box.

Decisions (confirmed with the user):
- Honor `'switchbuf'` (gated), **default `usetab`**.
- Apply to **all** jump paths — picker, LSP go-to, quickfix, marks. The global default change
  (e.g. LSP go-to may now pull focus to another tab) is **intentional**.

The machinery already exists — `window_showing` (the tab/window a buffer is in,
`windows.rs:1254`), `goto_tab_window` (focus a window, switching tabs first, `tabs.rs:345`),
`open_buffer` (find-or-load without reusing current window), `new_tab` (`tabs.rs:297`). This is
wiring, not new subsystems — per the "reuse existing APIs" principle. `:drop` (`ex.rs:2004`)
already does the usetab-style "focus the window already showing it" dance; we generalize that
to be `'switchbuf'`-gated and reachable from every jump.

## Conventions for this work

- **TDD**: each phase writes black-box tests that **fail first** on current `main`, then
  implements until green (`crates/bemtvi-server/tests/{picker,tabs}.rs`, driven via
  `feed`/`exec_lua`, asserting on `nvim_get_current_tabpage` / cursor / lines).
- **Cadence**: implement one phase, run `cargo fmt --all` + `cargo clippy --all-targets -- -D
  warnings` + the phase's tests, **commit, and pause for review** before the next phase.
- Commit to the current branch (no new branch).

---

## Phase 1 — `'switchbuf'` core plumbing (Feature 2, core)

**Goal:** `editor.jump_to` honors `useopen`/`usetab`; default is `usetab`. LSP/quickfix/marks
jumps (which all funnel through `jump_to`) gain the behavior for free.

**Changes (`bemtvi-core`):**
- `options.rs`: default `switchbuf: "usetab".to_string()` (~line 249); refresh docs at
  lines 139-144 and the catalog entry ~1265-1270 (drop "not yet acted on").
- `windows.rs`: add `switchbuf_window(&self, buf) -> Option<(usize, WindowId)>` — wrap
  `window_showing(buf)`; `usetab` returns the match, `useopen`-only returns it **only when in
  the current tab**, neither flag → `None`.
- `buffers.rs`: add `open_path_switchbuf(&mut self, path) -> Option<BufferId>`
  (`find_buffer_by_path` → `switchbuf_window` → `goto_tab_window`; else
  `edit_in_current_window`).
- `buffers.rs` `jump_to` (line 1310): swap the inner `edit_in_current_window` call (line 1327)
  for `open_path_switchbuf`. Extract the cursor-landing tail (lines 1335-1340) into a private
  `land_cursor(line, col)` (reused in Phase 3).
- `quickfix.rs`: refresh the `qf_focus_target_window` doc (line 887) — `usetab` now applies via
  `jump_to`. No functional change (default `usetab` performs no split, so no stray-split).

**Tests (`tabs.rs`):**
1. `usetab` (default): file A in tab 1, file B in current tab 2; `btv._jump_to(A,…)` →
   current tab becomes tab 1, no new window in tab 2.
2. `useopen` (`btv.o.switchbuf="useopen"`): buffer only in another tab is NOT followed; one in
   the current tab is reused.
3. empty (`btv.o.switchbuf=""`): jump opens in the current window even when shown elsewhere.
4. default reports `usetab` (`:set switchbuf?` / `btv.o.switchbuf`).

**Checkpoint:** fmt + clippy + `cargo test -p bemtvi-server --test tabs`; commit; pause.

---

## Phase 2 — picker file/buffer opens honor `'switchbuf'` (Feature 2, picker)

**Goal:** picking a buffer/file already open in another tab focuses that tab.

**Changes (bridges + Lua):**
- `ops.rs`: add `WindowOp::OpenSwitchbuf { path }` and `WindowOp::BufSwitch { buf }`.
- `install.rs` (~line 1340): register `btv._open(path)` and `btv._buf_switch(bufnr)`.
- `effects.rs` (~line 1301): apply them → `editor.open_path_switchbuf` /
  `editor.switch_to_buffer_switchbuf` (new core method: `switchbuf_window` →
  `goto_tab_window`; else `switch_buffer`, no forced cursor).
- `prelude/picker.lua`:
  - `buffers` confirm (line 532): `btv._buf_switch(item.bufnr)` (was `vim.cmd("buffer "..)`).
  - `btv.picker.edit` (line 439): located → `btv._jump_to(...)` (now switchbuf-aware);
    location-less → `btv._open(item.path)` (was `vim.cmd("edit "..)`).

**Tests (`picker.rs`):** buffers picker — buffer open in another tab → confirming switches to
that tab; with `switchbuf=""` it opens in the current window (gating guard).

**Checkpoint:** fmt + clippy + `cargo test -p bemtvi-server --test picker`; commit; pause.

---

## Phase 3 — `<C-t>` open-in-tab (Feature 1)

**Goal:** `<C-t>` in the picker opens the entry in a new tab (cursor landed for located items).

**Changes:**
- `mod.rs`: add `picker_result_mode: Option<PickerOpenMode>` (enum `Current | Tab`) by
  `menu_results` (~line 832).
- `menu.rs` `apply_picker_action` (line 1100): add a `"confirm_tab"` arm beside `"confirm"`
  (factor the shared chosen-key logic); both push the key + `close_menu`, `confirm_tab` also
  sets `picker_result_mode = Some(Tab)`.
- `effects.rs` (~line 2712): `take()` the mode (default `Current`) and pass to
  `run_picker_result(result, mode)`.
- `runtime.rs` `run_picker_result` (line 1557): add `mode: &str`, forwarded to
  `btv._picker_result`.
- `buffers.rs`: add `jump_to_tab(path, line, col)` — clone window options, `open_buffer(path)`
  (find-or-load, off-tick aware, mirrors `ex_tabnew` `ex.rs:1919`), `new_tab`, then
  `land_cursor`. Always a new tab (ignores `'switchbuf'`).
- Extend the `Jump` op + `btv._jump_to` bridge with an optional 4th `new_tab` arg
  (`ops.rs`/`install.rs`/`effects.rs`); `new_tab` dispatches to `jump_to_tab` (core ordering is
  atomic — avoids a Lua tabedit-then-set_cursor race).
- `prelude/picker.lua`: add `"confirm_tab"` to the actions loop (lines 37-54) and default
  binding `{ "<C-t>", "confirm_tab", "Open in new tab" }` (lines 65-84); `btv._picker_result`
  passes `mode` to `source.confirm(item, mode)`; `btv.picker.edit(item, mode)` handles
  `mode=="tab"` (located → `btv._jump_to(path,row,col,"tab")`; else `vim.cmd("tabedit "..)`);
  buffers tab mode → `vim.cmd("tabnew")` then `vim.cmd("buffer "..bufnr)` (deferred FIFO order).

**Tests (`picker.rs`):** `<C-t>` on `files` (location-less) and `live_grep`/buffers entries →
tab count grows, new tab shows the entry (located: cursor at row/col); plain `<CR>` unaffected.

**Checkpoint:** fmt + clippy + `cargo test -p bemtvi-server --test picker`; commit; pause.

---

## Phase 4 — example config, docs, full sweep

**Goal:** dogfood + guard the whole change.

- Add/extend a runnable `examples/` snippet (picker `<C-t>` + `switchbuf`), verified e2e per the
  example-config convention.
- `cargo test --workspace` (no regressions); final fmt + clippy.
- Manual smoke: `cargo run -p bemtvi -- file.txt`; `<leader>fb` + `<C-t>` (new tab); open a file
  in two tabs and confirm picking/jumping focuses the existing tab.

**Checkpoint:** commit; done.

---

## Phase 5 — `<C-x>` / `<C-v>` split opens (added 2026-06-21)

Generalized the Phase 3 confirm mode to cover splits:
- The bool `picker_confirm_in_tab` became `picker_confirm_mode: PickerOpenMode` (`Current`/`Tab`/
  `Split`/`Vsplit`); `menu.rs` maps `confirm`/`confirm_tab`/`confirm_split`/`confirm_vsplit`.
- The `Jump` op's `new_tab: bool` became `target: OpenTarget` (mirrors `PickerOpenMode`);
  `effects.rs` dispatches to `jump_to` / `jump_to_tab` / new `Editor::jump_to_split(.., vertical)`
  (split + `edit_in_current_window` + `land_cursor`, ignoring `'switchbuf'`).
- `picker.lua`: `confirm_split`/`confirm_vsplit` actions + default `<C-x>`/`<C-v>` maps;
  `btv.picker.edit` routes tab/split/vsplit through `btv._jump_to(.., mode)`; the buffers source
  opens the window (`:split`/`:vsplit`) then swaps the buffer in.
- Tests: 3 new in `picker.rs` (buffers `<C-x>` horizontal, buffers `<C-v>` vertical, located
  `<C-x>` lands cursor). Docs + the ui-picker example updated.
