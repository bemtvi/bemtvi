# A line-background layer for code blocks (and general `line_hl_group`)

Status: planned · 2026-07-05

## Problem

Fenced code blocks in a rendered markdown surface — the LSP **hover** float, the
**completion / cmdline docs** floats, and (eventually) a markdown-typed **buffer** — do
not read as code *regions*. We tried painting a `@markup.raw.block` background across the
block's lines as an ordinary highlight span (a `DOC_MD_NS` extmark), and it looked bad in
two ways, both reported:

1. **The background only covered the text, not the full line width** — a ragged right
   edge instead of a solid block.
2. **On a ```` ```lang ```` block, only the *non-highlighted* text got the background** —
   a patchwork, because the syntax-highlighted tokens overrode it.

That attempt was reverted (commit `revert(float): drop the partial code-block
background`). A fenced block with a language still reads as code via its per-language
syntax colouring; a **language-less** fence, and the "code region" look in general, is
what this plan restores — properly.

## Root cause

bemtvi's per-cell highlight resolution is **winner-takes-cell**, not attribute-layering.
`EditHost::highlights_for` (`crates/bemtvi-server/src/treesitter.rs`) collects the
treesitter spans, LSP semantic tokens, and extmark highlights for a line into
`HlInterval`s and calls `merge_intervals`
(`crates/bemtvi-server/src/extmarks.rs`), which "paints intervals by `(priority, order)`
and emits only the **winning** segments" — one group per cell. So a lower-priority
background span loses every cell a syntax span covers (symptom 2), and a highlight span
only spans char ranges, never the trailing width to the edge (symptom 1).

Making `merge_intervals` layer a background *under* the winning foreground would change
the global highlight-resolution semantics for every buffer and every highlight source —
too invasive, and not how neovim does it either.

## The key insight — reuse the `cursorline` model

bemtvi already paints a **full-width background under the text**: `'cursorline'`. Each
client tints the cursor's screen row across the whole window width *first*, then draws the
gutter, the text spans, and the overlays on top — "only cells they don't claim show the
tint" (`crates/bemtvi-tui/src/render.rs`, the `win.cursorline` block). The background and
the foreground compose **for free** because they're separate rendering passes, not merged
spans.

So the fix is a **line-background layer**, painted like `cursorline` but for an arbitrary
set of screen rows with per-row styles — neovim's `line_hl_group` (an
`nvim_buf_set_extmark` option) rendered with `hl_eol` semantics. Because it's painted
*under* the highlight spans, syntax colouring composes on top automatically. This solves
**both** symptoms at once with **no change to `merge_intervals`**.

## Design

A buffer line can carry a **line highlight group**. When a window projects that line
(each of its wrapped screen rows), the redraw resolves the group to a style and adds a
`(screen_row, style_id)` entry to a new per-window `line_bg` wire field. Each client
paints those rows' background across the full text-area width before the text, exactly
as it does `cursorline`.

The completion/hover renderer marks each fenced-code-block line with
`line_hl_group = "@markup.raw.block"`. The colorscheme already styles that group
(`runtime/colors/bemtvi.lua`, `bg = cursor_line`), so a code block reads as a solid,
full-width, code-tinted region with its per-language syntax colours layered on top.

## Changes

### Core — the line-highlight primitive (`crates/bemtvi-core`)

- **`extmark.rs`**: add `line_hl_group: Option<String>` to `VirtDecor` (the neovim
  `line_hl_group` field). An extmark carrying it means "this line's background is that
  group, full width". Add a query the redraw uses, e.g.
  `ExtmarkStore::line_hl_at(line_start_byte) -> Option<&str>` (highest-priority
  `line_hl_group` whose mark anchors on the line). Keep it in the same namespace model as
  the other decorations.
- **`editor/float.rs` → `render_markdown_into`**: for each `rendered.code` block, set an
  extmark on each block line with `VirtDecor { line_hl_group: Some("@markup.raw.block"),
  .. }` in `DOC_MD_NS`. This replaces the reverted per-char background loop. The
  per-language syntax spans continue to be lowered as before — they now sit *on top* of
  the line background.

### Server — projection (`crates/bemtvi-server`)

- **`redraw.rs` window projection** (`window_value` / the per-row build): alongside the
  existing `cursorline` handling, walk this window's visible `RowSeg`s; for a row whose
  buffer line carries a `line_hl_group`, resolve the group to a `style_id` (via
  `resolve_capture_winhl` + `styles.intern`, honouring the window's `winhighlight`) and
  push `(row_index, style_id)` onto a new `line_bg` array. Emit `line_bg` as a window
  field (empty array by default, so the wire shape stays stable). Wrapping is handled
  naturally: `RowSeg.line` maps each *screen* row to its buffer line, so every wrapped
  continuation row of a code line gets the same background.
- This is native + wasm: the marker is an extmark (core, shared), and the projection runs
  on both builds (like `virt_text`), so the browser edit-host gets it too.

### View decode (`crates/bemtvi-view`)

- Add `line_bg: Vec<(u16 /*row*/, Style)>` (or `Vec<Option<Style>>` indexed by row) to
  `WindowView` and decode it from the `line_bg` wire field, next to the `cursorline` /
  `cursor_line` decode.

### Clients — paint the background (mirror `cursorline`)

Each client already has the "tint a full row under the text" pass for `cursorline`;
generalize it to also paint the `line_bg` rows.

- **TUI** (`crates/bemtvi-tui/src/render.rs`): after the `Normal` background and the
  `cursorline` tint, for each `line_bg` entry render a full-width `Block` with that style
  on that screen row. Text spans / overlays draw on top as they already do.
- **GUI** (`crates/bemtvi-gui/src/render.rs`): fill each `line_bg` row's rect with the
  style's bg before the glyphs (the same `fill_rect` the cursorline / popup backgrounds
  use).
- **Web** (`crates/bemtvi-edithost/web/index.html`): set the row element's background for
  each `line_bg` row, as the cursorline CSS does.

### Colorscheme + docstrings

- `runtime/colors/bemtvi.lua` already defines `@markup.raw.block { bg = cursor_line }` —
  update its comment (it currently says the block bg is "for markdown-typed buffers") to
  note it now also backs the doc-float code blocks.
- Re-point the `float.rs` / `bemtvi.lua` comments the revert softened.

## Rendering & composition

Because `line_bg` is painted **under** the highlight spans (the cursorline model), a
code block renders as: the `@markup.raw.block` background across the full text width, with
the per-language syntax foreground (`@keyword`, `@function`, …) drawn on top of it, and
plain (uncaptured) code showing the background with `NormalFloat` foreground. No merge
change; no patchwork; full width. A **language-less** fence gets the same background with
no syntax on top — the exact case the revert left plain.

## Wrap handling

The doc floats wrap (`wrap` on). A `line_hl_group` marks a *buffer* line; the redraw
already iterates *screen* rows via `RowSeg`, each mapped to its buffer line, so a wrapped
code line contributes a `line_bg` entry for **every** screen row it occupies. No special
casing.

## Interactions & edge cases

- **`cursorline`**: the doc floats are non-focusable and don't set `cursorline`, so they
  never collide. In a markdown *buffer* both could apply to the same row; paint order is
  cursorline last (it's the active line) or define a precedence — cursorline wins the
  cursor's row. Decide when the buffer case lands; not needed for the float-only first
  cut.
- **Selection / search / diagnostics backgrounds**: these are already painted over the
  base background and under the text, in a defined order; `line_bg` slots in at the
  cursorline layer (below them), so a selection over a code block still shows.
- **Trailing width past the doc-float border**: the float's text-area width *is* the
  content width; painting `text_area.width` fills exactly the block, not into the border.
- **Empty lines inside a block**: they carry the marker too, so a blank line in a code
  block still shows the background (unlike the reverted char-range span, which skipped
  zero-width lines).

## Testing

Black-box, per project conventions (drive a real server, assert on the redraw):

- A completion/hover doc float with a **language-less** ```` ``` ```` block: the code
  rows carry a `line_bg` entry (resolved to the `@markup.raw.block` style under
  `:colorscheme bemtvi`), and the block body renders fence-stripped.
- A ```` ```lua ```` block: the code rows carry the `line_bg` background **and** the
  syntax spans (`@keyword` etc.) — proving they compose rather than fight.
- Wrap: a long code line's wrapped continuation rows each carry a `line_bg` entry.
- A non-code line (prose) carries **no** `line_bg` entry.

## Out of scope / future

- Exposing `line_hl_group` (and its sibling `hl_eol`) on the public `btv.buf.set_extmark`
  / `vim.api.nvim_buf_set_extmark` Lua surface — the primitive lands here internally; the
  Lua option is a small follow-up once the wire + clients exist.
- Applying the same code-block background to markdown-typed **buffers** in ordinary
  windows (the treesitter `@markup.raw.block` capture would feed the same `line_bg`
  path) — a natural extension once the buffer-side marker is wired, with the
  cursorline-precedence question above resolved.
