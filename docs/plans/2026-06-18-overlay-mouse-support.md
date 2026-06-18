# Overlay mouse support — pickers, completion popup, completion/cmdline docs, wildmenu

Status: **in progress** (started 2026-06-18).

## Goal

Add mouse support to the floating list/overlay widgets that today are
keyboard-only (or, in one case, mouse-handled client-side in the TUI only):

- the **insert-mode completion popup** (`nx.complete`) and its **docs sidebar**,
- the **fuzzy picker** (`nx.picker`) — list, prompt, and preview pane — and the
  promptless **select** (`nx.ui.select`),
- the **command-line wildmenu** (`nx.cmdline_complete`) and its docs sidebar.

Regular tree floats (`nx.view` / `nvim_open_win`) are already core-hit-tested in
`mouse.rs` (border-inset aware) — they are out of scope; this is specifically the
`Menu`-backed overlays projected by `redraw.rs::project_menu`.

## Approach: core-owned hit-testing (decided)

The mouse architecture is **"core owns which cells; clients forward raw cells"**
(`docs/architecture.md`, `editor/mouse.rs`). Every region — text, status lines,
tablines, dock edges, split dividers, **and regular floats** — is hit-tested in
core. The one exception is the completion popup, handled client-side in
`nxvim-tui` (`pmenu_geometry`/`pmenu_doc_geometry` + the `nxvim_complete_select` /
`nxvim_complete_accept` RPCs). That geometry is computed three times (core
`MenuView` → server `project_menu` → TUI mirror) and exists only in the TUI.

We bring menu-box geometry into core and hit-test these overlays there, like
every other region. The recent unified-geometry work (`36aeae6`) already moved the
shared placement routine `place_aligned` + `Extent`/`Align`/`Margin` into core
(`editor/windows.rs`), so the keystone refactor builds on existing core code.

Decisions (2026-06-18):
- **Replace** the TUI client-side completion handling with core hit-testing — one
  geometry, and the GUI + web clients gain completion/picker/cmdline mouse for free.
- Click **outside** an open box: **cancel** a picker (telescope-like), **no-op** a
  `select` (avoid accidental dismissal of a small choice popup).

Border convention: core assumes a **1-cell border** around the box (0 for the
completion popup's omitted top border), exactly as `hit_test` already does for
floats (`mouse.rs` border inset). Clients keep mapping their own pixel/cell border
so the cells line up (the web's ~1px rule already aligns to a cell).

## Phase 0 — Geometry keystone (pure refactor, no behavior change)

Add `Editor::menu_geom(metrics) -> Option<MenuGeom>` to core, where `MenuGeom`
carries the resolved box rect (in text-area cells), the `start` scroll offset, and
the chrome sub-layout (prompt row, separator, list-rows band, plus preview/docs
anchors). It folds in the `Cursor` / `Cmdline` placement math currently inline in
`project_menu` and reuses the existing core `place_aligned` for the `Editor` branch.

`metrics` = the cursor-screen inputs `project_menu` reads from `view.focused()`
(`cursor_row`, `cursor_screen_col`, `leftcol`, `text_width`, `text_height`). At
redraw the server passes them from `focused`; in Phase 1 core computes them itself.

`project_menu` is rewritten to consume `menu_geom` for the box rect + `start`,
keeping the content projection (labels, match spans, preview, docs) server-side.
**Verification: existing `redraw` + `picker.rs` + completion tests stay green** —
this phase changes no observable output.

## Phase 1 — Completion popup ✅ (committed)

- Added core menu hit-testing: `Editor::menu_hit(row, col) -> Option<MenuHit>`
  (`Item(idx)` / `Chrome`) + `menu_screen()` (box rect + list sub-rect in global
  cells, via `menu_geom` + the focused window's screen origin + the client border
  convention) + `menu_anchor()` (recomputes the cursor-screen metrics from the same
  projection the redraw uses). Dispatched in `Editor::mouse` **before** the text
  hit-test, gated on `completion_active()`.
- `menu_geom` now returns an authoritative `start` (scroll offset) for **all**
  placements via a shared `menu_start` helper, so core and clients agree on the
  visible window.
- Completion gestures: click a row → `complete_select_index`; click the already
  selected row → `complete_accept`; wheel over the popup → move the highlight one
  row, non-wrapping. Click on the box border / a filler is consumed, no-op.
- **Deleted** the TUI's dead `pmenu_geometry`/`pmenu_doc_geometry` click + wheel
  handling (it keyed off `view.pmenu`, which the server retired to always-`Nil`) and
  the unused `within` helper + `doc_scroll` plumbing; the left-press / wheel now
  forward raw cells, so the core handles the popup.
- Tests: `complete.rs::clicking_a_completion_row_selects_it_then_accepts_on_a_second_click`,
  `…::wheeling_over_the_completion_popup_moves_the_highlight_without_wrapping`
  (`feed_mouse`, `:set nonumber` for predictable coords).

### Deferred from Phase 1 (folded into later phases)

- **Docs-sidebar scroll**: the completion docs sidebar (via the unified `menu`)
  currently has *no* working scroll on any client (the old `doc_scroll` only fed the
  retired `pmenu` render path), and its float geometry is server-derived from LSP
  content (not core-computable without bringing the docs text into core). Mouse-
  scrolling docs is a marginal nice-to-have; revisit only if wanted.
- **GUI + web dead-`pmenu` cleanup and bespoke-RPC removal**: the GUI (and the web
  client) carry the same dead `view.pmenu` mouse branches, which already fall
  through to raw-cell forwarding — so completion mouse works there via core *now*.
  Their cosmetic cleanup + dropping `nxvim_complete_select`/`_accept` belongs to the
  Phase 4 cross-client pass (where each client is verified end-to-end).

## Phase 2 — Picker + select ✅ (committed)

- `menu_screen` extended: fixed `content_x = box_x + 1` (Cursor `geom.col` is the
  content anchor, Editor `geom.col` is the outer-box left — both resolve through the
  same `box_x`), added the preview-pane split (the same `~60%` fraction
  `project_preview` reserves, core-computed), and `MenuHit::Preview`.
- A picker / `select` grabs the mouse **modally** while open (new dispatch arms
  gated on `picker_or_select_active()`, before the chrome / text arms): a left-press
  highlights a row (`menu_cursor_to`), clicking the already-highlighted row confirms
  it (`menu_confirm` → `apply_*_action("confirm")`), a click off the box cancels a
  picker (`menu_cancel`) / no-ops a `select`; the wheel moves the highlight
  (`menu_step`) or, over the preview, scrolls it (`menu_preview_scroll`, the
  `<C-d>`/`<C-u>` half-page gesture); drag / release are swallowed.
- Server fix: `nx_input_mouse` now runs `run_pending()` after `editor.mouse`, like
  the keyboard path, so a mouse-driven confirm/cancel actually drains `menu_results`
  (and a completion accept's `complete_accept_request` — also benefits Phase 1's LSP
  path, which the native-source tests didn't cover).
- Tests: `picker.rs` (click-select-then-confirm, click-off-cancels, list-wheel,
  preview-wheel) and `ui_select.rs` (click-select-then-confirm).

## Phase 3 — Cmdline wildmenu ✅ (committed)

- `menu_screen` gained the **Cmdline frame**: the box anchors to the command-line
  area (global x = the token column, bottom abutting the command-line row at
  `self.height`) and grows **upward** with a top border and no bottom one — vs every
  other menu, which anchors to the focused window's text inner and grows down.
- Non-grabbing like the completion popup (new dispatch arms gated on
  `cmdline_complete_active()` + `menu_hit().is_some()`): a left-press highlights a
  candidate (and previews it on the line via `cmdline_complete_select_index` →
  `cmdline_complete_preview`), clicking the highlighted one accepts it into the line
  (`cmdline_complete_accept`), a wheel cycles the highlight. Off the box the press
  falls through.
- Command-mode mouse is gated on `'mouse'` containing `c` (the default `"nvi"`
  omits it — correct vim behavior); the test and the Phase 4 example set `mouse=a`.
- Tests: `cmdline_complete.rs` (click-selects-then-accepts-into-line, wheel-cycles).

## Phase 4 — Example, docs, cross-client cleanup + verify ✅ (committed)

- **4a — dead-code cleanup** (`24266b4`): removed the GUI's dead `view.pmenu` mouse
  handling (`render::pmenu_hit`/`PmenuHit`, the `doc_scroll` field, `mouse::within`)
  — it forwards raw cells now — and dropped the unused
  `nxvim_complete_select`/`nxvim_complete_accept` dispatch arms. The web client
  already forwarded raw cells, so it needed no change.
- **4b — example**: `examples/mouse-widgets/` (init.lua + sample.txt) wires all four
  overlays with `mouse=a`; `complete.rs::example_mouse_widgets_config_loads_and_completion_is_clickable`
  drives a click-to-accept end-to-end through the shipped config.
- **4c — docs**: `docs/architecture.md` mouse section gained the floating-overlay
  hit-test paragraph (menu_geom ↔ menu_hit) and the GUI mouse list was de-stale'd.
- **4d — web verify**: added a completion-popup mouse-click check to
  `crates/nxvim-edithost/web/verify-ui.mjs`; **all checks pass in a real browser**.
  This surfaced a real bug: the wasm `EditHost::mouse` tick did `editor.mouse` +
  `redraw` but skipped the `run_pending` settle the native dispatch runs — so a
  mouse confirm/accept queued but never ran its handler. Fixed `EditHost::mouse` to
  run `run_pending` + `dispatch_statusline_clicks` + `drain_feedkeys`, matching the
  native path (benefits every wasm mouse-driven widget, not just completion).
- GUI eyeballed by the user (GUI windows aren't agent-screencapturable).

## Testing

Black-box throughout: the harness `feed_mouse` / `feed_mouse_at` + `TestClock`
drive `nx_input_mouse`; assertions on `nvim_buf_get_lines` / cursor / the projected
`menu` redraw surface and confirm-readback via `nvim_exec_lua`. No unit tests.

## Risks / notes

- **Coordinate spaces**: `project_menu`'s `row`/`col` are text-area cells; the
  hit-test needs the text-area origin (the focused window's screen rect / the main
  region geom). Worked out in Phase 1.
- **`Cursor` placement above/below flip**: `menu_geom` must reproduce the four-tier
  fit fallback exactly so the hit-test box matches what's painted.
- Keep Phase 0 a strict no-op (diff-tested against existing redraw output) before
  any behavior lands in Phase 1.
