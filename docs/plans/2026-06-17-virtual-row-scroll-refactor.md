# Virtual-row scroll refactor (+ word-wrap)

**Status:** done (all 3 phases landed 2026-06-17) · **Date:** 2026-06-17

> Landed in three commits: `refactor(view): fold window rows into one RenderRow
> projection primitive` (Phase 1), `feat(scroll): rebuild the smooth-scroll band
> on screen rows` (Phase 2), `feat(wrap): soft word-wrap on the row model`
> (Phase 3). 1506 tests green, clippy clean, wasm builds, web scroll verified.

Refactor smooth scrolling off its buffer-line coordinate model onto a single
screen-row ("virtual row") projection, then build word-wrap on top to prove the
abstraction carries the hard case.

## Problem

Smooth scroll is a **second, parallel projection of the window expressed in
buffer-line units**, distinct from the settled-frame projection. That causes the
two symptoms the refactor targets:

- **Every feature is special-cased.** The band (`ScrollAnim`,
  `view.rs:27`; `project_band`, `redraw.rs:765`) re-projects the same per-row
  arrays `window_value` already produces, over a taller range. Each new per-row
  feature must be wired into *both* paths. Worse, the band is a **lossy** mirror:
  `project_band` emits only `lines / selection / search / incsearch / numbers /
  highlights / inlay_hints / virt_text` — it is **missing** `secondary_selection`,
  `diagnostics_virt`, and `virt_lines` that the settled `window_value` emits. So
  multi-cursor selections and diagnostic virt-text already flash instead of
  sliding today.
- **virt_lines can't animate; word-wrap would break the same way.** The band's
  index is buffer lines (`from_top`/`to_top`/`base_line`), but the screen is
  screen rows. virt_lines (and, once it exists, word-wrap) make
  `screen_rows ≠ buffer_lines` *nonlinearly*. The band can't place them, so the
  code bails to an **instant snap** when any virt_line is in range
  (`view.rs:699-706`). Word-wrap (no `'wrap'` option exists yet) is the identical
  one-line→N-rows break and would force the same escape hatch.

## Root cause

There are two projection paths in different coordinate systems:

| | settled frame | scroll band |
|---|---|---|
| builder | `window_rows` → `RowLayout { lines, numbers, virt }` (`view.rs:984/1000`) | `window_view` band branch (`view.rs:687`) |
| unit | **screen rows** (virt_lines interleaved; per-row arrays `scatter_rows`'d onto `numbers`) | **buffer lines** (`[base_line, base_line+count)`, one entry per buffer line) |
| projector | `window_value` (`redraw.rs`) | `project_band` (`redraw.rs:765`) |
| consumer | client draws `height` rows from `top` | client lerps a buffer-line `top`, slices `skip(off).take(height)` (TUI `render.rs:692`) |

The settled path *already* models interleaved screen rows correctly. Only the
band is stuck in buffer-line space. The seeds of the row-index model already
exist: `cursor_screen_row` (`cursor.rs:355`), `scroll_top_for_bottom`
(`cursor.rs:376`), `screen_row_of` (`view.rs:1054`) all map buffer↔screen-row
accounting for virt_lines.

## Target architecture

One **screen-row** primitive used by both the settled frame and the band. A
rendered row is self-describing and `kind`-tagged:

- `kind`: real line `Some(buf_line)` · virt_line · wrap-continuation · `~` filler
- payload: text, highlights, virt_text, selection span, search/incsearch spans,
  inlay hints, number-or-none — i.e. what `RowLayout` + the `scatter_rows`'d
  arrays already carry, folded into one per-row record.

Then:

- **Settled view** = `height` rows of that layout starting at `top`'s row.
- **Scroll band** = the *same* layout, over-scanned, plus a **fractional
  screen-row offset** the client interpolates. `project_band` collapses into "the
  row layout, taller" — no second projection, nothing to keep in sync, nothing
  lossy.

Once a row carries everything, the scroll path never asks *what kind* a row is.
virt_lines, wrapped continuations, folds, diff fillers are all just more rows.
This mirrors neovim's own model (the `wlv` winline loop / screen-row grid with a
screen-line→buffer-line map); bemtvi already has the settled half and duplicates
the rest.

### Wire-protocol change

The band's coordinate keys move from buffer lines to screen rows:

- `base_line` (buffer line) → `base_row` (first band screen row)
- `from_top`/`to_top` (buffer lines) → `from_row`/`to_row` (screen-row offsets,
  fractional-capable)
- `numbers` stays — it *is* the row→buffer-line map, already on the band.

Clients interpolate a screen-row offset into the band array instead of a
buffer-line `top`. The TUI keeps whole-row stepping (rounds the offset; cells are
discrete); GUI/web interpolate the fractional offset for true sub-row smoothness.

## Decisions (locked)

- **Scope: phases 1–3** — unify the band *and* land word-wrap in this effort, so
  the new abstraction is validated against the case that motivated it.
- **Smoothness: sub-row on GUI/web** — TUI stays cell-stepped (whole screen
  rows); GUI interpolates the fractional offset (it already does sub-pixel, just
  in buffer-line units today); **web gains smooth scroll for the first time**
  (`eh-lib.js` has no band consumer — web snaps today).

## Phases

Work one phase at a time; commit and pause for review between phases (per the
big-feature cadence). Every phase is guarded by the existing black-box redraw
suites — no unit tests.

### Phase 1 — promote the row layout (pure refactor, no behavior change)

- Turn `RowLayout` into a `kind`-tagged `Vec<RenderRow>` carrying all per-row
  payload (fold the `scatter_rows`'d arrays into the row record).
- Reroute the settled `window_value` through it. Output bytes must be identical.
- **Verify:** full `cargo test --workspace` green; redraw assertions unchanged.
- **Commit**, pause for review.

### Phase 2 — band = over-scan of the row layout

- Rebuild the band as a taller slice of the Phase-1 row vector + `base_row` /
  `from_row` / `to_row`. Delete the buffer-line band fields and the
  `virt_lines`-snap escape hatch (`view.rs:699`).
- Update `project_band` to emit the row vector (drops the lossy hand-maintained
  subset; `secondary_selection`, `diagnostics_virt`, `virt_lines` now ride the
  band for free).
- Update consumers to interpolate a screen-row offset:
  - **TUI** (`render.rs:692`, `anim.rs`): round the offset; same feel, row units.
  - **GUI** (`render.rs`): switch `y = (base_line + k - top)*cell_h` to screen-row
    units; keep sub-pixel.
  - **Web** (`eh-lib.js` / `worker.mjs`): add a band consumer + rAF loop;
    fractional offset.
- **New behavior:** virt_lines, multi-cursor selections, and diagnostic
  virt-text now animate instead of snapping/flashing. Add redraw coverage that a
  virt_lines-containing scroll carries a gesture (no longer `None`).
- **Verify:** workspace tests + `verify-ui.mjs` (web) + GUI paint test.
- **Commit**, pause for review.

### Phase 3 — word-wrap on the row model

- Add the `'wrap'` option (+ `linebreak`/`showbreak` follow-on as warranted) to
  `options.rs`.
- `window_rows`/`RenderRow` builder emits wrap-continuation rows for one buffer
  line spanning multiple screen rows; horizontal `leftcol` logic
  (`ensure_visible_horizontal`, `cursor.rs:403`) becomes wrap-aware.
- Because the band is now screen-row based, wrapped scroll animates with no
  scroll-path changes — the validation of the whole refactor.
- **Verify:** new wrap redraw tests (cursor motion across wrapped rows, scroll
  through wrapped regions, `gj`/`gk` display-line motions); workspace green.
- **Commit**, pause for review.

## Risks / watch-items

- **Redraw byte-identity in Phase 1** is the safety net; if any array can't be
  reproduced identically through `RenderRow`, surface it rather than paper over.
- **Selection edge-clip during slide** (`sel_extends_down`, `render.rs:705`) is
  computed in buffer-line space today; re-derive it per screen row.
- **Per-whole-line TUI rate** changes meaning from buffer line to screen row —
  intended (virt_lines/wraps slide at the right rate), but worth an explicit test.
- **`cursor_screen_row` / `scroll_top_for_bottom`** should be reused, not
  re-implemented, as the canonical buffer↔row maps.
- Keep the harness take-latest-redraw discipline for any new `*_after` helpers.
