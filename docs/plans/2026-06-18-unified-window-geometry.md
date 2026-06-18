# Unified window geometry — viewport-relative size · alignment · margin

Status: **done** (2026-06-18).

## Problem

nxvim had four disconnected ways to size and place a windowed surface:

- **Pickers / select menus** supported fractional sizes (`"80vw"`, `"50%"`) via
  `MenuExtent { Cells, Frac }`, but were always **centered** — no alignment, no margin.
- **Floats** (`nx.view:mount{float}`, `nvim_open_win`) had rich placement
  (`relative`/`anchor`/`row`/`col`) but **absolute-cell** sizes only.
- **Docks** took an absolute `size`.
- **The bottom panel** took an absolute `height`, bottom-anchored only.

Goal: **one** geometry vocabulary every surface shares — fractional sizes that
**reflow on resize**, a 9-grid **alignment** (`top-left`…`center`…`bottom-right`),
and a **margin** so a box can sit in a corner *without touching the screen edge*.

## The shared core (`nxvim-core`)

- `Extent { Cells(u16), Frac(f32) }` (promoted from `MenuExtent`) with
  `.resolve(reference)` — `crates/nxvim-core/src/editor/menu.rs`.
- `Align` (9-grid, `from_keyword`/`as_str` mirroring `FloatAnchor`), `Margin`, and
  `place_aligned(bounds, w, h, align, margin) -> (x, y)` — the single placement
  routine — `crates/nxvim-core/src/editor/windows.rs`.
- `FloatConfig.width/height` are now `Extent`; added `align: Option<Align>` +
  `margin: Margin`; dropped `Eq` (an `f32` isn't `Eq`; `MenuView` was already
  `PartialEq`-only). `place_float` resolves the `Extent`s against `bounds` (the
  editor area — never `origin`, which is zero-size for `relative=cursor`) **every
  layout**, so fractional floats reflow; when `align` is set it positions via
  `place_aligned` (ignoring `anchor`/`row`/`col`).
- `MenuView` gained `align`/`margin`; the server's picker projection
  (`redraw.rs`) uses `place_aligned` (default `Center`) instead of hardcoded
  centering — so pickers can be cornered.

## Server (`nxvim-server`)

- One size parser `parse_extent` (was `parse_menu_extent`) + `parse_align` +
  `build_margin`, shared by every surface (`effects.rs`), reused by both
  `nvim_open_win` parsers — `build_float_config` (Lua-effect) **and**
  `dispatch.rs::parse_float_config` (RPC).
- `nvim_win_get_config` reports the **resolved inner cells off the laid-out rect**
  (`window_content_size`), not the raw `Extent` — so a fractional float reports its
  true on-screen size and a cell-sized float round-trips exactly. Both readers
  updated: `float_mirror` (the `nx._wins` mirror) and `win_config_value` (the RPC).
- Wire ops (`crates/nxvim-lua/src/ops.rs`): float/panel/picker ops carry size as a
  **string** spec plus `align` + a `[top,right,bottom,left]` margin.

## Lua surface

- `nx._geom` (`crates/nxvim-lua/src/prelude/geometry.lua`) — one normalizer that
  validates size specs / alignment words / margin (number | `{v,h}` | `{t,r,b,l}` |
  `{top=,…}`) and emits the wire shape, failing loud on bad input.
- Wired into `view.lua` (`mount{float}` — drops the number-only size rejection),
  `picker.lua` (adds `align`/`margin`, per-open over per-source), and `nx.lua`
  (wraps `nx.panel.open`). `nvim_open_win`'s mutation surface stays nil in Lua (per
  ADR 0002 / the absent-mutation-API rule); the RPC path carries the new keys.

## Panel

Stays bottom-anchored. `height` accepts an `Extent` (resolved against the editor
height); `margin` is a gap from the edges, applied as a one-off inset of the panel
window's rect at the end of `relayout` (a tiled window has no native inset concept,
so this is isolated to the panel — `apply_panel_margin`).

## Tests (black-box)

- `picker.rs::picker_align_and_margin_place_the_box_in_a_corner_with_a_gap`
- `nx_view.rs::view_float_frac_size_aligns_and_reflows_on_resize`
- `nx_view.rs::nvim_open_win_cell_size_round_trips_exactly`
- `nx_view.rs::example_window_geometry_config_opens_an_aligned_float`
- `editing/listings.rs::nx_panel_open_honors_fractional_height_and_margin`

## Example

`examples/window-geometry/` — a float (top-right, margin), a centered float, a
cornered picker, and a 30vh panel with a margin, verified end-to-end by the test
above.
