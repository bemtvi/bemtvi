# Picker results → quickfix/loclist tabs in the bottom dock

**Status:** complete (all 5 phases) · **Date:** 2026-06-20

## Goal

Port (natively — there is no neovim compat) telescope's "send results to a
location list" idea, and make it *better*: the user can save several searches as
**named lists shown as tabs in the bottom dock**, and activating an entry opens
the file in the **main** editing layer (not inside the dock).

## Guiding constraint

**Reuse existing machinery; add new APIs only when absolutely necessary.** This
feature is assembled almost entirely from surfaces that already exist.

## Key realization — "named lists" already exist

Location lists are **per-window**. A dock tab is a window with its own
`WindowTree`. Therefore:

> one saved search = one bottom-dock tab = one window carrying its own loclist

No new `HashMap<String, QfList>` registry, no naming API. `QfList.title` is the
tab label (already projected through `region_tablines()`). Quickfix stays the
single global list and can also be a dock tab.

## Reuse map

| Need | Existing machinery |
|---|---|
| Populate a list | `qf_set_items(QfWhich::Location(win)/Quickfix, items, action, title)`; drains from `vim.fn.setloclist`/`setqflist` via `effects.rs` |
| Replace / append / new | `QfAction::Replace` / `Add` / `New` |
| Render + `<CR>` jump | `filetype=qf` buffer + buffer-local keymaps + `apply_qf_action("jump")` |
| Detect "I'm in the display" | `is_quickfix_buffer()`, `qf_window_id()` (`quickfix.rs`) |
| Host list in bottom dock | `open_dock(Bottom, …, buf)` + per-layer `TabStack` |
| Add another list tab | `focus_dock(Bottom)` + `new_tab(buf, opts)` (`tabs.rs:290`) |
| Cross out of the dock on jump | `switch_layer(Layer::Main)` (`dock.rs`) |
| Tab label | `QfList.title` → `region_tablines()` |
| Picker's current result set | `btv._picker.items` (Lua-side full item tables) |

## Seams (the only non-trivial parts)

1. **Jump lands in the main layer.** `qf_focus_target_window` (`quickfix.rs:806`)
   resolves targets from the *focused* layer's `window_ids()`. When `<CR>` is
   pressed in a dock-hosted display, that's the dock → it would land/split inside
   the dock. Fix: when `is_quickfix_buffer()` and the display window lives in a
   dock layer, `switch_layer(Layer::Main)` first, then run the existing
   target-window / `switchbuf` logic against the main tree. Surgical; no new API.

2. **Display opens as a dock tab.** Today `:copen`/`:lopen` use
   `open_bottom_window()` (a main-layer split). Decision (user): **the dock
   hosts the display.** So opening a list "into the dock" routes the
   `filetype=qf` display buffer to the bottom dock (`open_dock` for the first,
   `new_tab` for subsequent), instead of the main-layer bottom split.

## Phases (commit + pause for review between each)

### Phase 1 — Jump routing crosses to the main layer — DONE (no code needed)
- Test: `enter_in_dock_hosted_qf_jumps_into_the_main_layer` (`tests/quickfix.rs`).
  Hosts a qf display as a bottom-dock tab via existing APIs (`:copen` → grab buf →
  `:cclose` → `btv.dock.open{buf=…}`), then `<CR>` from the dock.
- **Finding:** seam 1 needs no change. `qf_focus_target_window` already resolves
  the target to a main-layer window (`qf_prev_win` / the loclist owner), and
  `set_current_window` (`windows.rs:1131`) already crosses layers (`switch_layer`
  to the target window's layer). The jump lands in main and reuses the main window
  (no dock split). The test is now a **regression guard**.
- Confirms the reuse thesis: cross-layer focus is already first-class.

### Phase 2 — Host the qf/loc display in the bottom dock (option-gated) — DONE
- Added the `'qfdock'` boolean option (default **on**), modeled in the catalog +
  `:set` apply arm. `ex_qf_open` honors it: `qf_place_in_dock` (open the bottom
  dock, or `new_tab` beside an existing one) vs the classic `open_bottom_window`.
- **Two dock-path bugs found via the broken existing tests and fixed:**
  1. Loclist owner lookups (`qf_stack`/`qf_cur_mut`/`qf_stack_ensure`/
     `qf_display_bufnr`/`qf_set_display_bufnr`) resolved the owner via
     `self.windows` (the *focused* tree only), while `qf_context_of_buffer` used
     the cross-layer `window()`. Once the display was hosted in the dock and focus
     was in the dock, the owner window was parked → loclist resolved empty → `<CR>`
     no-opped. Fixed by routing those lookups through the new cross-layer
     `window()` / `window_mut()` (the latter added next to `window()` in `dock.rs`).
  2. `:cclose`/`:lclose` couldn't tear down a single-tab dock — `close_window`'s
     last-window guard refused. `ex_qf_close` now closes the display as a *tab*
     (`close_tab`, which closes the dock on its last tab) when it's dock-hosted.
- Tests (`tests/quickfix.rs`): `copen_hosts_the_list_in_the_bottom_dock_by_default`,
  `cclose_closes_the_dock_hosted_list`, `noqfdock_opens_the_classic_bottom_split`.
  The split-mechanics tests (`copen_opens_a_small_window_at_the_bottom`, the
  cclose/owner-close/`btv.qf` wrappers) now `:set noqfdock` — they guard the split
  mode, which is the opt-out. Full quickfix (38) + dock (54) suites green.
- Original Phase 2 plan text follows.

### Phase 2 — Host the qf/loc display in the bottom dock (option-gated)
- **User-facing option** selecting list-display style — **dock (default, the
  bemtvi way)** vs **split (the telescope/vim way: a bottom split of the current
  window, the single global qf list / per-window loclist, replace-in-place).** The
  option governs where `:copen`/`:lopen` and the send-action place the display, so
  one switch flips the whole behavior. Default = dock.
- Model: each bottom-dock tab is a window that **owns and displays** its own
  location list. N saved searches = N tabs = N independent loclists. Jumps fall
  back to a main window (deterministic: `open_layers()` lists `Main` first, so
  `window_ids()` enumerates main before dock → `qf_focus_target_window`'s fallback
  lands in main). No change to `qf_focus_target_window` needed.
- Failing test: with the option at its default, opening a list into the dock puts
  the `filetype=qf` buffer in a bottom-dock tab; a second list adds a second tab;
  `<CR>` jumps into main. With the option set to split, behavior is the classic
  bottom-split (the existing `:copen` path, already covered).
- Reuse: `open_dock`/`focus_dock`/`new_tab` + `qf_set_items` + the qf render/jump
  machinery. New code is ~15 lines of placement orchestration + the option.

### Phase 3 — Lua glue to send a result set to a dock list — DONE
- Core: `Editor::loclist_to_dock(items, title)` — opens a **new** bottom-dock tab
  whose window both owns and displays its own location list (reuses
  `qf_place_in_dock` + `qf_set_items`); returns the owning window. Each call adds an
  independent list. `Editor::loclist_send(items, title)` wraps it and honors
  `'qfdock'`: dock tab (bemtvi) vs current-window loclist replace + split (vim).
- Bridge: a `send: bool` flag on `QfSetOp`; `btv._loclist_send(items, title)` queues
  it; `effects.rs` routes `op.send` to `loclist_send`. Lua API
  `btv.qf.send_to_loclist(list, { title })` (alias `btv.send_to_loclist`) — the
  telescope port. Reuses the whole `setloclist` item-marshalling path.
- Tests (`tests/quickfix.rs`): `send_to_loclist_saves_each_search_as_its_own_dock_tab`
  (two sends → two independent dock tabs, each with its own list, `<CR>` jumps to
  main) and `send_to_loclist_without_qfdock_replaces_and_splits`. 40 quickfix tests
  green, clippy clean.
- Note: `:tabprevious` acts on the main layer; dock tabs cycle via `gT`/`gt`
  (focused-layer tab nav). Not a bug — just where dock tab navigation lives.

### Phase 4 — Picker action (the telescope port proper) — DONE
- A `send_to_loclist` picker action (default-bound to **`<C-q>`**). It captures the
  picker's **filtered** result set — the matched item keys *in display order*, read
  server-side from the menu (`apply_picker_action` → new `Editor::picker_sends`
  channel), so a fuzzy-narrowed `files` picker sends only the visible rows, not
  every candidate — then closes the picker and hands the keys to Lua.
- `btv._picker_send(keys)` (via `run_picker_send`) maps the keys back to the source
  item tables, keeps those with a `path`, and `btv.schedule`s
  `btv.qf.send_to_loclist` (Phase 3) so the float has closed and focus is back in
  main before the dock list opens. `<C-q>` registered as a default `picker`-mode map
  (overridable).
- Test (`tests/picker.rs`): `ctrl_q_sends_filtered_results_to_a_dock_loclist` — a
  source with file-path items, filtered to a subset, `<C-q>`; asserts only the
  filtered rows land in a bottom-dock loclist and `<CR>` jumps into main. Picker
  (36) + quickfix (40) suites green, clippy clean.

### Phase 5 — DONE (all three parts)
- **Part 2 — `send_to_qflist` + `add_*` variants.** Generalized the send into
  `Editor::list_send(items, title, action, to_qf)`; Lua `btv.qf.{send,add}_to_{loc,qf}list`
  (+ bare aliases) via `btv._list_send`. quickfix list = one reused dock tab; loclist
  *send* = new tab, *add* = append to the focused tab. Honors `'qfdock'`.
- **Part 1 — picker multi-select.** `marked: Vec<usize>` on `Menu`;
  `toggle_select`/`clear_select` actions bound to `<Tab>`/`<S-Tab>`; `<C-q>` sends
  the marked rows when any, else the filtered view. The menu redraw projects a
  per-row `marked` array (`Editor::menu_marked_window`); the TUI draws a marker
  gutter only while marks are in play. (GUI/web marker rendering = follow-up.)
- **Part 3 — examples + docs.** `examples/picker-to-loclist/` (init.lua + sample +
  notes), smoke-tested to load end-to-end. Docs: `docs/features/quickfix-dock-lists.md`
  + `docs/features/picker.md` updates + the `features.md` index. Exposed `'qfdock'`
  through the `btv.o` mirror (name map + default + `GoMirror` + `set_global_option_bool`)
  so `btv.o.qfdock` reads/writes (the example's toggle).
- Tests: send/add_to_qflist + add_to_loclist, multi-select send-marked, `btv.o.qfdock`
  round-trip, example-config load. quickfix (43) + picker (38) green, clippy clean.

## Status: COMPLETE — all phases landed.

## Open questions / notes
- Default keybinding for the picker action(s) — telescope's `<C-q>` (qf) as the
  baseline; loclist variant unbound by default upstream, so pick a default here.
- v1 = "send all current results" (matches telescope's default); "send selected"
  waits on Phase 5 multi-select.
