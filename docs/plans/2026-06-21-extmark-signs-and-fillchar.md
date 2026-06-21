# Extmark gutter signs + line-fill (fillchar) — core capabilities

Two render-only extmark decorations nxvim accepts-but-doesn't-paint today (see
`EXTMARK_OPT_DECORATION` in `prelude/api.lua`), needed so the **nxvim-diff** plugin can
honor its deferred `signs` and `fillchar` options — but both are general-purpose (gutter
signs back any gitsigns/marks/dap-breakpoint plugin; line-fill backs any "blank this row
with a glyph" need).

## Background — what exists

- A **sign column** already renders, but only for LSP diagnostics: `diagnostics_signs_for`
  / `sign_width_for` (`lsp/diagnostics.rs`) project per-row `[glyph, severity, style_id]`
  + a width; `redraw.rs` emits `diagnostics_signs` + `sign_width`; the TUI paints it
  (`render_sign_column`). Extmark `sign_text` is parsed-and-dropped.
- Extmark decor (`VirtDecor`) carries `virt_text` / `virt_lines` only. `sign_text` /
  `sign_hl_group` reach the Lua bridge (`virt_decor_from_table`) but are ignored, and a
  *sign-only* decor returns `None` (dropped entirely).
- `'fillchars'` parses but only honors `eob`.

## Phase A — extmark gutter signs (`sign_text` / `sign_hl_group`)

1. **Core** (`extmark.rs`): add `sign_text: Option<String>`, `sign_hl_group:
   Option<String>` to `VirtDecor`.
2. **Lua bridge**: `VirtDecorData` (+ `virt_decor_from_table`, `virt_decor_to_core`) carry
   the two fields; `virt_decor_from_table` must NOT early-return `None` when only signs are
   present.
3. **Projection**: a new `extmark_sign_cells` collects extmark signs per row (first
   wrap-segment row only, like the number); merge with the diagnostic signs into one
   typed `SignCell { glyph, code, style_id, priority }` list — per row the highest
   `priority` wins (diagnostics sit at a fixed low priority so an explicit extmark sign
   shows; ties favor the diagnostic). One `signs` Value + a unified `sign_width` drive the
   existing wire keys, so **no client change** is needed; runs on native AND wasm (extmark
   store is shared) — diagnostics stay native-only.
4. **Round-trip**: thread `sign_text` / `sign_hl_group` through `ExtmarkMirror` (+ the Lua
   `_set_extmark_mirror` rebuild) so `get_extmarks(details=true)` returns them after a tick.
5. **Test**: a core redraw test — set an extmark with `sign_text`, assert the sign column
   shows the glyph + reserves width; and a details round-trip assert.

## Phase B — line-fill / fillchar (`line_fill`)

1. **Core** (`extmark.rs`): add `line_fill: Option<VirtChunk>` to `VirtDecor` — fill the
   anchored line's text area with `text` (repeated to the window text width) in `hl_group`.
2. **Lua bridge**: accept an `nx`-native key (e.g. `line_fill = { text, hl_group }`) on
   `set_extmark` and lower it.
3. **Projection**: in `window_value`, where the window text width is known, expand a
   `line_fill` mark on its row into an `Overlay` `virt_text` placement sized to the
   remaining width — reuses the existing virt_text wire + client paint, so no client
   change.
4. **Test**: a core redraw test — a blank line with a `line_fill` mark renders the char
   across the row.

## Phase C — nxvim-diff (the plugin repo)

Flip `signs` / `fillchar` from deferred to working: place `+`/`~`/`-` hunk signs and paint
the `fillchar` on filler rows; update `decor_spec`, the plan, and the README.

Each phase commits on its own; A and B are independent.
