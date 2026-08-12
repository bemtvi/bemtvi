# Virtual text rendering (extmark `virt_text` + `virt_lines`)

Status: **in progress** — Phases 1–5 **done** 2026-06-16; Phase 6 mostly done
2026-06-16 (GUI/web paint parity, `hl_mode`, wasm parity landed; `virt_lines_leftcol`
and scroll-band virtual rows remain as documented refinements).

**Client coverage note:** rendering is implemented in the **TUI** (`bemtvi-tui`,
the agent-verifiable reference client) for every phase. As of Phase 6 the **GUI**
(`bemtvi-gui`) and **web** (`bemtvi-edithost`) clients also paint `virt_text` and
`virt_lines`, **including chunk backgrounds** (a chunk whose hl group sets a `bg`
reads as a filled badge, not dark-on-dark) — see Phase 6 for the exact per-client
coverage (the GUI paints the chunk `bg` as a quad behind the glyph; the web's
JS-highlight `renderLine` path paints every position — inline rides DOM flow). The GUI output is not agent-verifiable
(window not screencapturable); the web is verifiable via the edithost Playwright
harness but that wasn't run in-session. Build + clippy are clean on every touched
crate; the visual output of the GUI/web paint is **not** yet eyeballed.

## Problem

Extmark virtual-text fields (`virt_text`, `virt_text_pos`, `virt_lines`, `hl_mode`,
…) are **accepted and stored Lua-side** (`btv._extmarks[b][ns][id].decoration`) so
`nvim_buf_get_extmarks(…, {details=true})` can return them, but they **never reach
Rust** and are **not rendered**. The core `Extmark` struct
(`crates/bemtvi-core/src/extmark.rs:77`) carries only `hl_group`; the
`btv._extmark_set` effect (`install.rs:691`, `ExtmarkOp::Set` in `ops.rs:360`)
forwards only position + `hl_group` + `priority`.

Goal: render the full virtual-text surface — all `virt_text` positions
(`eol`, `inline`, `overlay`, `right_align`, fixed `virt_text_win_col`) **and**
`virt_lines` (extra screen rows above/below a buffer line).

Out of scope (remain accepted-but-unrendered, separate follow-ups): `sign_text`,
`line_hl_group` / `cursorline_hl_group` / `number_hl_group`, `conceal`, `spell`,
`url`, `ui_watched`.

## What already exists (reuse)

- **eol virtual text**: `diagnostics_virt_text_for()`
  (`crates/bemtvi-server/src/lsp/diagnostics.rs:140`) projects one decoration per
  visible row → `diagnostics_virt` wire key; TUI renders it after a one-cell gap
  past EOL (`crates/bemtvi-tui/src/render.rs:1271`). The extmark eol path mirrors
  this, but per-mark / multi-chunk.
- **inline splicing**: `inlay_hints` already splice text into a row at an anchor
  column, pushing real text right (same `render.rs` `highlight_line`). Inline
  `virt_text` reuses this.
- **hl_group → style_id palette**: `merge_intervals` / the frame `StyleTable`
  (`crates/bemtvi-server/src/extmarks.rs`, `redraw.rs`) resolve groups to deduped
  per-frame style ids. virt chunks resolve the same way.
- **per-window parallel-to-row arrays**: `lines` / `numbers` / `highlights` /
  `diagnostics_virt` are screen-row-indexed (`redraw.rs` `window_value`). New
  `virt_text` follows the same shape; `virt_lines` requires interleaving extra
  rows (Phase 5).

## Data model (Phase 1)

New pure types in `bemtvi-core` (no mlua, no async — core stays pure):

```rust
// crates/bemtvi-core/src/extmark.rs
pub struct VirtChunk { pub text: String, pub hl_group: Option<String> }

pub enum VirtTextPos { Eol, Inline, Overlay, RightAlign, WinCol(u16) }

pub enum HlMode { Replace, Combine, Blend }   // 'replace' | 'combine' | 'blend'

pub struct VirtDecor {
    pub virt_text: Vec<VirtChunk>,
    pub virt_text_pos: VirtTextPos,
    pub virt_text_hide: bool,
    pub hl_mode: HlMode,
    pub virt_lines: Vec<Vec<VirtChunk>>,   // each inner Vec = one virtual line
    pub virt_lines_above: bool,
}

pub struct Extmark {
    pub id: u64,
    pub start: usize,
    pub end: Option<usize>,
    pub hl_group: Option<String>,
    pub priority: u32,
    pub decor: Option<Box<VirtDecor>>,     // boxed: hl-only marks stay small
}
```

`ExtmarkStore::set` gains a `decor: Option<Box<VirtDecor>>` param.

## Threading (Phase 1)

1. `api.lua` `btv.buf.set_extmark` already collects the `decoration` table — forward
   it as a 10th arg to `btv._extmark_set`.
2. `install.rs` `_extmark_set` bridge: convert the `Option<Table>` → core
   `VirtDecor` via a new `virt_decor_from_table` helper (this is where mlua →
   core conversion happens; `ops.rs` stays mlua-free). Add `decor` to
   `ExtmarkOp::Set`.
3. `effects.rs` `apply_extmark_op` passes `decor` into `ExtmarkStore::set`.

Proof of Phase 1 lands with Phase 2's first render test (black-box only; no unit
tests per project convention).

## Phases

- **Phase 1 — Core data model + threading. ✅ DONE.** Added `VirtChunk` /
  `VirtTextPos` / `HlMode` / `VirtDecor` to `bemtvi-core` (`extmark.rs`), a boxed
  `decor` field on `Extmark`, and the `decor` param on `ExtmarkStore::set`.
  Threaded the payload: `api.lua` forwards the `decoration` table → `install.rs`
  `virt_decor_from_table` lowers it into the new `VirtDecorData` (mlua-free) on
  `ExtmarkOp::Set` (validating `virt_text_pos`/`hl_mode` loud) → `effects.rs`
  `virt_decor_to_core` resolves it into the typed core `VirtDecor` and stores it.
  `btv.decor` provider marks pass `decor: None` (hl-only; wiring virt onto provider
  marks is a later follow-up). All 8 `extmarks.rs` tests pass; clippy/fmt clean.

- **Phase 2 — `eol` virt_text. ✅ DONE.** Server `virt_text_for()` in
  `extmarks.rs` buckets virt_text marks by anchor line (order `(start, priority,
  id)`) and emits, per row, placements `[pos, col, hl_mode, [[text, style_id]…]]`
  — **Phase 2 emits only `pos==0` (eol)**; the shape is final so later positions
  are server-emit + client-render only, no re-parse. Wired into `redraw.rs`
  `window_value` as the new `virt_text` key (native; empty array on wasm). Client:
  `bemtvi-view` `parse_virt_text` + `VirtPlacement`/`VirtChunk` types + `WindowView`
  field; `bemtvi-tui` `highlight_line` paints eol placements after EOL (one-cell
  gap, per-chunk style via `virt_chunk_style`, truncated to viewport). Unknown/
  absent `hl_group` → `Nil` style_id → normal color (same graceful fallback as the
  hl span path). Test `eol_virt_text_paints_after_the_line` asserts the chunk text
  reaches the client. Full workspace builds (incl. gui); clippy/fmt clean.

- **Phase 3 — `inline` virt_text. ✅ DONE.** Server `virt_text_for()` now emits
  `pos==1` placements, mapping the mark's byte anchor to a screen `col` via
  `unicode::virtcol` (the same tab/wide-char mapping inlay hints + hl spans use).
  TUI `highlight_line` splices inline placements into the row at their column —
  reusing the inlay-hint splice machinery (a second `vi` cursor + `emit_inline_virt`
  for the multi-chunk run, tracking `inserted`). Cursor shift: new
  `virt_cursor_shift` adds inline-virt width before the cursor to the existing
  `inlay_cursor_shift`, so the focused cursor lands past the splice. Test
  `inline_virt_text_anchors_at_its_screen_column` (anchor after a tab → screen col,
  proving the virtcol mapping, not a raw byte offset). All 10 extmark tests pass;
  full workspace builds; clippy/fmt clean.

- **Phase 4 — `overlay` + `right_align` + `win_col`. ✅ DONE.** Server emits
  `pos==2` (overlay, at the anchor's screen col), `pos==3` (right_align, col 0),
  `pos==4` (win_col, the fixed window col). TUI: overlay/win_col draw *over* cells
  during the walk (`emit_overlay` + an `overlay_end` suppression boundary so the
  covered real glyphs aren't painted — overlay replaces, no shift); placements
  at/past EOL pad to their column (e.g. a fixed-column guide); right_align flushes
  chunks to the right edge, clamped to never overlap painted text. Shared
  `push_virt_chunks` helper across the eol/overlay/win_col/right_align paths. Test
  `overlay_rightalign_wincol_positions_project` asserts the three `pos`/`col`
  projections. All 11 extmark tests pass; full build + clippy/fmt clean.
  **Deferred to Phase 6:** `hl_mode` `combine`/`blend` (today every mode renders as
  `replace`, the default — noted at `virt_chunk_style`); `virt_text_hide` (its
  "covered" semantics are a refinement — stored in core, not yet honored).

- **Phase 5 — `virt_lines` (screen-row expansion). ✅ DONE.** The big one.
  **Design deviation from the 5a/5b split below:** the per-row interleaving lives
  in **core** (`view::window_rows`), not the server. Every server per-row
  projection (`highlights_for` / `virt_text_for` / `diagnostics_*` / `inlay_hints_for`)
  already keys off `win.numbers` as the row→buffer-line map, so emitting virtual rows
  as `numbers[i] == None` entries makes them skip those rows automatically — strictly
  less churn than re-interleaving every array server-side, and it keeps all row layout
  in `view.rs` (its stated role: the single source of truth for row content). The
  server only resolves the new rows' chunk styles.
  - 5a (core): `Buffer::virt_lines_by_line` buckets each line's `virt_lines` into
    `(above, below)` chunk rows. `cursor.rs` `cursor_screen_row` / `scroll_top_for_bottom`
    make `ensure_visible` plines-aware (a line spends `1 + above + below` screen rows),
    so the cursor stays visible past virtual lines; `window_view` computes `cursor_row`
    and the secondary cursors as *screen* rows via `screen_row_of`.
  - 5b (core view, not server): `window_rows` lays out `height` screen rows,
    expanding each buffer line into `[above…][text][below…]`. A virtual row gets
    `numbers[i]=None`, `lines[i]=""`, and its chunks in the new `WindowView.virt_lines`
    parallel array; `scatter_rows` re-aligns the buffer-line-indexed selection / search
    arrays onto the expanded rows. The server adds the `virt_lines` wire key
    (`extmarks.rs::virt_lines_value` — shares `virt_chunks_value` with `virt_text`);
    native-only, empty on wasm like the other extmark projections.
  - 5c (clients): `bemtvi-view` `parse_virt_lines` + `WindowView.virt_lines`; **TUI**
    `render_text` paints a virtual row via `virt_line` (chunk text in its resolved
    style, no gutter number, no cursor, no `~`) — the agent-verifiable reference. GUI +
    web paint parity is batched into Phase 6 (the wire data already reaches them).
    `virt_lines_leftcol` / horizontal scroll of virtual rows is a Phase-6 refinement
    (today they start at the text body's left edge).
  - Tests (`crates/bemtvi/tests/extmarks.rs`):
    `virt_lines_interleave_above_and_below_their_line` (above/below placement +
    surrounding line order/numbers) and
    `scroll_accounts_for_virt_lines_to_keep_the_cursor_visible` (fills the buffer to
    exactly the text height, attaches 3 virtual lines below line 1, jumps to the last
    line, and asserts line 1 scrolled off — which only happens with plines-aware
    scrolling).
  - **Deferred to Phase 6:** virtual rows do **not** ride the scroll-animation band
    (`ScrollAnim` is buffer-line-based; virtual rows simply don't slide and appear at
    the settled frame); `virt_lines_leftcol`; GUI/web paint.

- **Phase 6 — Polish + parity. In progress.**
  - ✅ **`virt_text_hide`** (deferred from Phase 4) — honored **server-side** in
    `virt_text_for`: the focused window's per-row `selection` is threaded in, and a
    mark with `virt_text_hide` is omitted on any row the visual selection covers
    (matching neovim's "hide when the background text is selected"). Observable in the
    `virt_text` wire payload, so it's black-box testable
    (`virt_text_hide_drops_under_a_visual_selection`).
  - ✅ **Priority ordering** of multiple `virt_text` marks on one row — the
    `(start, priority, id)` sort already makes priority the tie-break at a shared
    anchor; locked with `virt_text_marks_emit_in_priority_order` (high-priority mark
    created first, so only priority — not id — can produce the asserted order).
  - ✅ **`hl_mode` `combine`/`blend`** (deferred from Phase 4). **Client-render-only**
    — the `hl_mode` code already rode the wire, so this is purely client paint and is
    **not** observable through the redraw harness (only visually). Applies to overlay /
    win_col placements (the only ones with cells underneath; eol / inline / right_align /
    virt_lines have nothing under them, so they always render `replace`). Coverage:
    **TUI** full (fg + bg via `apply_hl_mode` = `Style::patch` for combine, channel
    average for blend; resolved at the overlay's start column); **GUI** the chunk's
    own fg+bg paint (bg via a quad behind the glyph, `push_seg_backgrounds`), and the
    `hl_mode` *merge* applies to the foreground (`virt_overlay_fg`); the bg is painted
    as `replace` (the `combine`/`blend` bg blend is deferred — the GUI has no per-cell
    underlying bg to merge with); **web** fg + chunk bg, with `blend` averaging hex fg
    (`blendHex`). Documented at `apply_hl_mode` / `virt_overlay_fg` / `blendHex`.
  - ✅ **GUI + web client paint parity** for `virt_text` positions and `virt_lines`.
    **GUI** (`bemtvi-gui/render.rs`): eol, inline, overlay, win_col, right_align, and
    `virt_lines` rows all paint — inline/overlay via segment transforms (`apply_row_virt`
    + `splice_insertions`, merged with the inlay splice), eol/right_align as positioned
    text items, the cursor shift extended by `virt_inline_shift`. **web**
    (`bemtvi-edithost/web/index.html`): `virt_lines` rows, eol, inline, overlay/win_col
    (in-place cell overwrite), and right_align all paint on the JS-highlight `renderLine`
    path. Inline rides DOM flow — emitting the chunk span before its anchor cell shifts
    the following glyphs / cursor / selection right with no painted-column math (unlike
    the TUI/GUI grid). **Two follow-up fixes** after the first landing: (1) chunk
    backgrounds (a `bg`-carrying group rendered dark-on-dark — fixed by painting the bg);
    (2) the un-gate interned virt styles into the global palette, which flipped the web's
    `serverStyled` heuristic (`styles.length > 0`) on and routed code buffers to the
    no-virt `renderLineServer` path (and dropped JS highlighting) — fixed by keying
    `serverStyled` off real per-window highlight *spans*, not palette size. Build + clippy
    clean; web JS syntax-checked. Not agent-verified visually.
  - ✅ **wasm edit-host parity** — `virt_text` / `virt_lines` are no longer
    `#[cfg(feature = "native")]`-gated in `redraw.rs`; the projections are pure
    (core extmark store + `StyleTable`), so the wasm/serverless build emits the same
    wire. Builds clean on `native` and `--no-default-features`.
  - ⏳ **`virt_lines_leftcol`** (start a virtual line over the gutter rather than the
    text body) — accepted + stored Lua-side, but painting it needs a per-virtual-row
    flag threaded through the core row layout (`Buffer::virt_lines_by_line` /
    `window_rows` / `WindowView.virt_lines`) and the wire, then honored in all three
    clients. Until then virtual lines start at the text body's left edge. Documented in
    `api.lua`'s `EXTMARK_OPT_DECORATION` note.
  - ✅ **`virt_text_repeat_linebreak`** — a no-op **by design**: it only repeats the
    virt text at a soft-wrap boundary, and bemtvi has no `'wrap'` option, so there's no
    wrap point. Accepted + stored Lua-side; documented as a deliberate no-op in `api.lua`.
  - ✅ **`virt_text` placements on the scroll-animation band** — eol / inline / overlay /
    win_col / right_align text now rides the slide instead of flashing out and back when
    it settles. Projected in `project_band` keyed on `s.numbers` (like highlights / inlay,
    un-gated so wasm carries it), threaded through `ScrollData` / the TUI `Animation` / the
    GUI `ScrollAnim`. TUI slices it into the band like `inlay_hints` (full fidelity); web
    `bandWindow` passes it to `renderLine` (full); GUI band splices inline/overlay into the
    sliding segments (eol / right_align / chunk-bg are skipped mid-slide — a brief transient
    on the ~150ms animation, since the band paints at fractional `y` where the cell-row bg/
    eol helpers don't apply).
  - ✅ **`virt_lines` whole-rows + smooth scrolling — instant-scroll fallback.** The band
    is buffer-line-based (`ScrollAnim` is one row per buffer line, no interleaved virtual
    rows), so a slide can't place virtual rows without detaching them from their anchor
    line mid-slide (they flashed out and back). Rather than animate them wrong, core now
    **suppresses the scroll gesture when the slide's buffer-line range contains any
    `virt_lines`** (`view.rs`, gated on `Buffer::virt_lines_by_line().range(..)`), so that
    scroll snaps instantly to the settled frame — where the view's interleaved layout
    renders the virtual rows correctly. No flash, no detachment; the cost is that a scroll
    *through* a virt_lines region isn't smooth-animated. A scroll with only `virt_text`
    (or none) keeps its smooth slide. The full smooth-over-virtual-rows animation (the band
    rebuilt in screen-row units with the offset math reworked) remains a larger follow-up.
  - **`examples/virt-text/` added** (`init.lua` + `sample.txt`) covering every
    `virt_text` position, `virt_lines` (above / below), and a `virt_text_hide` mark.
    Its code paths are covered by the black-box tests; the example itself hasn't been
    launched interactively (the TUI isn't agent-verifiable). Eyeball it with
    `BEMTVI_CONFIG=examples/virt-text cargo run -p bemtvi -- examples/virt-text/sample.txt`.

## Risks / open questions

- **virt_lines scroll integration** is the riskiest: core scroll math currently
  assumes 1 buffer line = 1 screen row. plines accounting touches cursor
  visibility, `<C-e>`/`<C-y>`, `zz`, and page motions. Keep 5a isolated and test
  scrolling hard.
- **Wire churn**: adding a row-kind flag changes the per-window payload; all three
  clients (`bemtvi-tui`, `bemtvi-gui`, `bemtvi-web`) and `bemtvi-view` parsing must be
  updated together (Phase 5c).
- **Styles**: virt chunks with an unknown `hl_group` must fail loud at resolution
  (no silent skip), matching the extmark hl path.
