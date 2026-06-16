# Virtual text rendering (extmark `virt_text` + `virt_lines`)

Status: **in progress** — Phases 1–4 **done** 2026-06-16; Phase 5 next.

**Client coverage note:** rendering is implemented in the **TUI** (`nxvim-tui`,
the agent-verifiable reference client) for every phase. The **GUI** (`nxvim-gui`)
and **web** clients parse the new `virt_text` wire field (via `nxvim-view`) but do
not paint it yet — that render parity is batched into Phase 6, not done per-phase
(the GUI/web output isn't agent-verifiable). This is a tracked gap, not a silent
skip: the data reaches those clients; only their paint code is pending.

## Problem

Extmark virtual-text fields (`virt_text`, `virt_text_pos`, `virt_lines`, `hl_mode`,
…) are **accepted and stored Lua-side** (`nx._extmarks[b][ns][id].decoration`) so
`nvim_buf_get_extmarks(…, {details=true})` can return them, but they **never reach
Rust** and are **not rendered**. The core `Extmark` struct
(`crates/nxvim-core/src/extmark.rs:77`) carries only `hl_group`; the
`nx._extmark_set` effect (`install.rs:691`, `ExtmarkOp::Set` in `ops.rs:360`)
forwards only position + `hl_group` + `priority`.

Goal: render the full virtual-text surface — all `virt_text` positions
(`eol`, `inline`, `overlay`, `right_align`, fixed `virt_text_win_col`) **and**
`virt_lines` (extra screen rows above/below a buffer line).

Out of scope (remain accepted-but-unrendered, separate follow-ups): `sign_text`,
`line_hl_group` / `cursorline_hl_group` / `number_hl_group`, `conceal`, `spell`,
`url`, `ui_watched`.

## What already exists (reuse)

- **eol virtual text**: `diagnostics_virt_text_for()`
  (`crates/nxvim-server/src/lsp/diagnostics.rs:140`) projects one decoration per
  visible row → `diagnostics_virt` wire key; TUI renders it after a one-cell gap
  past EOL (`crates/nxvim-tui/src/render.rs:1271`). The extmark eol path mirrors
  this, but per-mark / multi-chunk.
- **inline splicing**: `inlay_hints` already splice text into a row at an anchor
  column, pushing real text right (same `render.rs` `highlight_line`). Inline
  `virt_text` reuses this.
- **hl_group → style_id palette**: `merge_intervals` / the frame `StyleTable`
  (`crates/nxvim-server/src/extmarks.rs`, `redraw.rs`) resolve groups to deduped
  per-frame style ids. virt chunks resolve the same way.
- **per-window parallel-to-row arrays**: `lines` / `numbers` / `highlights` /
  `diagnostics_virt` are screen-row-indexed (`redraw.rs` `window_value`). New
  `virt_text` follows the same shape; `virt_lines` requires interleaving extra
  rows (Phase 5).

## Data model (Phase 1)

New pure types in `nxvim-core` (no mlua, no async — core stays pure):

```rust
// crates/nxvim-core/src/extmark.rs
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

1. `api.lua` `nx.buf.set_extmark` already collects the `decoration` table — forward
   it as a 10th arg to `nx._extmark_set`.
2. `install.rs` `_extmark_set` bridge: convert the `Option<Table>` → core
   `VirtDecor` via a new `virt_decor_from_table` helper (this is where mlua →
   core conversion happens; `ops.rs` stays mlua-free). Add `decor` to
   `ExtmarkOp::Set`.
3. `effects.rs` `apply_extmark_op` passes `decor` into `ExtmarkStore::set`.

Proof of Phase 1 lands with Phase 2's first render test (black-box only; no unit
tests per project convention).

## Phases

- **Phase 1 — Core data model + threading. ✅ DONE.** Added `VirtChunk` /
  `VirtTextPos` / `HlMode` / `VirtDecor` to `nxvim-core` (`extmark.rs`), a boxed
  `decor` field on `Extmark`, and the `decor` param on `ExtmarkStore::set`.
  Threaded the payload: `api.lua` forwards the `decoration` table → `install.rs`
  `virt_decor_from_table` lowers it into the new `VirtDecorData` (mlua-free) on
  `ExtmarkOp::Set` (validating `virt_text_pos`/`hl_mode` loud) → `effects.rs`
  `virt_decor_to_core` resolves it into the typed core `VirtDecor` and stores it.
  `nx.decor` provider marks pass `decor: None` (hl-only; wiring virt onto provider
  marks is a later follow-up). All 8 `extmarks.rs` tests pass; clippy/fmt clean.

- **Phase 2 — `eol` virt_text. ✅ DONE.** Server `virt_text_for()` in
  `extmarks.rs` buckets virt_text marks by anchor line (order `(start, priority,
  id)`) and emits, per row, placements `[pos, col, hl_mode, [[text, style_id]…]]`
  — **Phase 2 emits only `pos==0` (eol)**; the shape is final so later positions
  are server-emit + client-render only, no re-parse. Wired into `redraw.rs`
  `window_value` as the new `virt_text` key (native; empty array on wasm). Client:
  `nxvim-view` `parse_virt_text` + `VirtPlacement`/`VirtChunk` types + `WindowView`
  field; `nxvim-tui` `highlight_line` paints eol placements after EOL (one-cell
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

- **Phase 5 — `virt_lines` (screen-row expansion).** The big one.
  - 5a (core): a `plines`-style helper — extra screen rows a buffer line's
    `virt_lines` adds (above+below). Integrate into `window_view` scroll/`top`
    math and `cursor_row` so the cursor stays visible with virtual lines present.
  - 5b (server): `window_value` interleaves virtual rows into the per-row arrays;
    virtual rows get `numbers[i]=None` + a row-kind flag distinguishing them from
    `~` fillers, and carry their chunk content (new `virt_lines` wire key or a
    row-kind + chunks array).
  - 5c (clients): render virtual rows from chunks — no gutter number, no cursor,
    `virt_lines_leftcol` honored. TUI + GUI + web.
  - Tests: above/below placement, scroll keeps cursor visible past virtual lines.

- **Phase 6 — Polish + parity.** `hl_mode` `combine`/`blend` (deferred from Phase
  4 — merge the chunk highlight with the cells under an overlay; today all render
  `replace`); `virt_text_hide` (deferred from Phase 4 — hide when the line is
  covered); **GUI + web client paint parity** for all `virt_text` positions (the
  wire data already reaches them; only `nxvim-gui` / web render code is pending —
  see the client-coverage note up top); priority ordering of multiple virt_text
  marks on one row; `virt_text_repeat_linebreak` (no-op without wrap, documented);
  wasm edit-host parity (cfg-gate as needed); runnable `examples/virt-text/` config
  + sample file verified end-to-end (per repo convention).

## Risks / open questions

- **virt_lines scroll integration** is the riskiest: core scroll math currently
  assumes 1 buffer line = 1 screen row. plines accounting touches cursor
  visibility, `<C-e>`/`<C-y>`, `zz`, and page motions. Keep 5a isolated and test
  scrolling hard.
- **Wire churn**: adding a row-kind flag changes the per-window payload; all three
  clients (`nxvim-tui`, `nxvim-gui`, `nxvim-web`) and `nxvim-view` parsing must be
  updated together (Phase 5c).
- **Styles**: virt chunks with an unknown `hl_group` must fail loud at resolution
  (no silent skip), matching the extmark hl path.
