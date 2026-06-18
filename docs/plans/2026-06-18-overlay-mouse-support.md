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

## Phase 1 — Completion popup + docs

- Add a `MenuHit` resolution in core (a `MouseTarget::Menu`-style arm or a
  dedicated pre-check in `Editor::mouse`, run **before** the text hit-test since
  menus float above windows). Maps a global cell to `{ list row index | prompt |
  preview | docs | border/outside }` using `menu_geom` + the text-area origin.
- Completion popup gestures: click a row → `complete_select_index`; click the
  already-selected row, or a double-click on any row → `complete_accept`; wheel
  over the list → move selection one row (non-wrapping, scrollbar-like).
- Docs sidebar: clicks inert; wheel over it scrolls — move the TUI's client-local
  `doc_scroll` into core menu state (a `docs_scroll` offset projected to clients).
- **Delete** the TUI `pmenu_geometry` / `pmenu_doc_geometry` click + wheel handling
  and the bespoke RPC calls; forward raw cells like text. Keep the
  `nxvim_complete_select`/`_accept` RPCs only if still used elsewhere, else drop.
- Tests in `tests/mouse.rs`: click-to-select, click-selected-to-accept,
  double-click-accept, wheel-cycles-selection, using `feed_mouse`/`TestClock`.

## Phase 2 — Picker + select

- Click a list row → select (move highlight); double-click / click the selected
  row → confirm (`apply_picker_action("confirm")` / `apply_select_action`).
- Wheel over the list → scroll selection (Editor placement is windowed, so this
  scrolls the visible window via `start`).
- Wheel over the preview pane → the existing `preview_scroll` gesture
  (`<C-d>/<C-u>/<C-f>/<C-b>` model).
- Click outside the box: **cancel** for a picker (push `None` to `menu_results`),
  **no-op** for a `select`.
- Tests in `tests/picker.rs` (reuse `poll_menu`/`menu_items`/confirm-readback).

## Phase 3 — Cmdline wildmenu

- Click a candidate → select + accept it into the command line
  (`cmdline_complete` select/accept path); wheel → cycle candidates.
- `mouse_enabled()` already gates Command mode on `'mouse'` containing `c`; confirm
  and add a `c`-mode test.

## Phase 4 — Example, docs, cross-client verify

- `examples/mouse-widgets/` config + sample (following `examples/window-geometry/`),
  loaded end-to-end by a test.
- Web verify via the edithost Playwright harness (`crates/nxvim-edithost/web`);
  GUI eyeballed by the user (GUI windows aren't agent-screencapturable).
- Update `docs/architecture.md` mouse section + the relevant `nx.*` API docs.

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
