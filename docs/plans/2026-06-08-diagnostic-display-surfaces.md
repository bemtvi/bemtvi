# Diagnostic display surfaces — completion plan

> **Status: complete (Phases 1–3 done).**
> nxvim caches `publishDiagnostics` per buffer and paints the underline squiggles,
> the inline **virtual text** (Phase 1), and the gutter **sign column** (Phase 2),
> plus an under-cursor message line, the `:LspDiagnostics` loclist, `[d`/`]d`
> navigation, and the on-demand **float** (`vim.diagnostic.open_float`, Phase 3).
> `vim.diagnostic.config` now honors `underline`, `virtual_text`, and `signs`;
> `open_float` honors the `float` surface (only the `config.float` defaults that
> pre-style it, and the filter keys, stay stored Lua-side but **inert** — the
> `INCOMPLETE` tag in `crates/nxvim-lua/src/prelude/diagnostic.lua`). All three
> diagnostic surfaces neovim users expect now ship.

## Why this document exists

`docs/known-approximations.md` lists "No diagnostic-display surfaces —
virtual-text / signs / diagnostic float (distinct from the floating-window
primitive, which exists)" as a cross-cutting gap, and `vim.diagnostic.config`
keys other than `underline` as inert. This is the single most-visible everyday
LSP gap: a real nvim setup shows inline `■ mismatched types` virtual text and a
gutter `E`/`W` sign, and `vim.diagnostic.open_float()` pops the full message. We
have none of that.

The same fail-loud, no-silent-stub rule from the LSP completion plan applies: a
config key we don't honor yet stays a documented approximation, never a silent
no-op that looks like it worked.

## What's already in place (the seams these phases extend)

- **Cache + mirror.** `Server::diagnostics_of(buffer)` → `(&[Diagnostic],
  PositionEncoding)`; the Lua mirror `nx._diagnostics` (keyed by bufnr).
- **Per-window projection.** `Server::diagnostics_for(buffer, &numbers, styles)`
  (`crates/nxvim-server/src/lsp/diagnostics.rs`) builds the per-row underline
  spans; `redraw.rs::window_value` attaches them under the `diagnostics` key;
  `WindowView.diagnostics: Vec<Vec<DiagSpan>>`
  (`crates/nxvim-view/src/view.rs`); `render_text` composes the underline last.
- **Config bridge.** `vim.diagnostic.config` → `nx._diagnostic_config(underline)`
  (`install.rs`) → `LspOp::DiagnosticConfig { underline }`
  (`crates/nxvim-lua/src/ops.rs`) → `Server::diagnostics_underline`
  (`lsp/sync.rs`).
- **Severity helpers.** `severity_code` / `severity_group` / `severity_short`
  and the `DiagnosticUnderline*` highlight groups (`lsp/mod.rs`).
- **Gutter.** `render_gutter` / `gutter_cell` paint a `number_width` line-number
  column; there is no sign column yet.
- **Float/panel.** `Editor::open_panel` backs hover; floats exist as a window
  primitive (`WindowView.floating`/`border`/`title`).

The config flag `diagnostics_underline: bool` on `Server` becomes a small
`DiagnosticConfig` struct in Phase 1, so signs/virt-text/float each add a field
rather than a parallel bool.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Phase 1 — Inline virtual text ✅

**Goal.** Paint each diagnostic's message inline, after the end of its line
(`■ mismatched types`), colored by severity — the headline `virtual_text`
surface. Driven by `vim.diagnostic.config({ virtual_text = true | { … } })`.

**Why.** This is the surface users notice first and the reason "diagnostics don't
show" reads as broken even though squiggles work. It also establishes the
per-window *decoration* projection (text positioned relative to a row, not a
column span) that signs and the float reuse.

**Scope.**
- `crates/nxvim-server/src/lib.rs` — replace `diagnostics_underline: bool` with a
  `DiagnosticConfig { underline, virtual_text, signs }` struct (defaults
  `underline = true`, `virtual_text = false`, `signs = false` — neovim's 0.10
  default is signs+underline on, virt-text off; we keep virt-text opt-in).
- `crates/nxvim-server/src/lsp/diagnostics.rs` — `diagnostics_virt_text_for(buffer,
  &numbers)`: per visible row, the highest-severity (`severity_sort`-aware)
  diagnostic *starting* on that row → `{ text, severity, style_id }`, prefixed
  per config (`prefix`, default `■`). Reuses `severity_code` / the
  `DiagnosticVirtualText*` highlight groups (added alongside the underline ones).
- `crates/nxvim-view/src/view.rs` — `WindowView.diagnostics_virt: Vec<Option<DiagVirt>>`.
- `crates/nxvim-server/src/redraw.rs` — project it under a `diagnostics_virt`
  window key.
- `crates/nxvim-tui/src/render.rs` — in `render_text`, after a row's text + EOL
  gap, paint the virt-text span (clamped to the window width, after a one-cell
  gap), styled by the resolved group or a built-in severity color.
- `crates/nxvim-lua/src/{ops.rs,install.rs}` + `prelude/diagnostic.lua` — thread
  `virtual_text` (bool, and `prefix` from the table form) through the config op.

**Approach.** Mirror `diagnostics_for` exactly, but emit one optional decoration
per row (not a span list) carrying display text. The text is the diagnostic's
`first_line(message)`; `severity_sort` (config) picks the winner when multiple
diagnostics start on the row. Virt-text lives only in the input frame's window
projection — it is persistent state (redraw take-latest is safe).

**Tests** (`crates/nxvim-server/tests/editing/` via `nvim_exec_lua` + the redraw
view, or `crates/nxvim/tests/lsp.rs` via the scripted mock):
- a published diagnostic with `virtual_text = true` surfaces its message on the
  diagnostic's row in `diagnostics_virt`, prefixed;
- `virtual_text = false` (default) shows nothing there;
- two diagnostics on one row → the higher severity wins (and `severity_sort`
  flips it);
- a custom `prefix` is honored.

**Done when.** ✅ A `publishDiagnostics` with `vim.diagnostic.config({ virtual_text
= true })` paints the message inline after its line — both in the `diagnostics_virt`
redraw key and on the rendered grid — while the default (virt-text off) is
unchanged. `DiagnosticConfig` (`lsp/mod.rs`) replaced the `diagnostics_underline`
bool; the projection is `Server::diagnostics_virt_text_for` → the
`diagnostics_virt` window key → `WindowView.diagnostics_virt: Vec<Option<DiagVirt>>`
→ `highlight_line` paints it after end-of-text (one-cell gap, truncated to the
viewport, severity foreground or the resolved `DiagnosticVirtualText*` group). The
config threads `virtual_text` (bool, plus the table form's `prefix`) through
`nx._diagnostic_config` → `LspOp::DiagnosticConfig`. The `INCOMPLETE` note in
`diagnostic.lua` lost its `virtual_text` clause. Verified by
`vim_diagnostic_config_virtual_text_paints_the_message_inline` /
`virtual_text_picks_the_highest_severity_on_a_row_and_honors_a_prefix`
(`crates/nxvim/tests/lsp/diagnostic_api.rs`) and the Tier-2 paint test
`inline_virtual_text_is_painted_after_the_line` (`tests/lsp/diagnostics.rs`).
Runnable demo: `examples/diagnostics/`.

*Known approximations:* the line's most-severe diagnostic wins the one inline
slot (no per-diagnostic stacking, no `virtual_lines`); `severity_sort` and the
`virtual_text` `format`/`severity` filters are not applied; `update_in_insert` is
always on.

**Depends on.** Nothing (extends the existing projection).

---

## Phase 2 — Sign column ✅

**Goal.** A gutter glyph (default `E`/`W`/`I`/`H`, configurable) on every line
that has a diagnostic, colored by the highest severity on that line — the
`signs` surface. Driven by `vim.diagnostic.config({ signs = true | { text = … } })`.

**Why.** The second always-on neovim surface; it needs a *sign column* in the
gutter, which nothing else has carved yet, so it is its own phase.

**Scope.**
- `crates/nxvim-server/src/lsp/diagnostics.rs` — `diagnostics_signs_for(buffer,
  &numbers)`: per visible row, `Option<{ glyph, severity, style_id }>` for the
  highest-severity diagnostic on that buffer line, gated on
  `DiagnosticConfig.signs`. Glyphs from config `text` (`{ [severity] = "E" }`)
  or the built-in severity letters; style from the `DiagnosticSign*` groups.
- `crates/nxvim-view/src/view.rs` — `WindowView.diagnostics_signs: Vec<Option<DiagSign>>`
  and a `sign_column: bool` (whether to reserve the column — true once any sign
  exists, matching vim's `signcolumn=auto`).
- `crates/nxvim-server/src/redraw.rs` — project under `diagnostics_signs` /
  `sign_column`.
- `crates/nxvim-tui/src/render.rs` — carve a 2-cell sign column *left of* the
  number gutter when `sign_column`; `render_gutter` (or a new
  `render_sign_column`) paints the glyph + style per row, blank for rows with no
  sign. The text-inner rect shifts right by the sign-column width.
- Config plumbing as in Phase 1 (`signs` bool + `text` map).

**Approach.** Same projection shape as Phase 1 but addressed to the gutter. The
sign column is `auto`: present iff the window's buffer currently has ≥1
diagnostic (so a clean buffer keeps its old layout). Width is fixed at 2 cells
(vim's sign width), independent of `number_width`.

**Tests.**
- a diagnostic makes its line's sign cell carry the severity glyph in the
  redraw; lines without one are blank;
- the highest severity on a line wins the glyph;
- `signs = false` reserves no column (layout identical to today);
- a custom `text = { [vim.diagnostic.severity.ERROR] = "✘" }` glyph is honored.

**Done when.** ✅ A buffer with diagnostics shows a severity glyph in a reserved
sign column when `signs` is on (default on); a clean buffer or `signs = false`
renders exactly as today. `DiagnosticConfig` (`lsp/mod.rs`) gained `signs: bool`
(default on) + `sign_text: [String; 4]` (the per-severity glyphs, default
`E`/`W`/`I`/`H`); the projection is `Server::diagnostics_signs_for` → the
`diagnostics_signs` window key (`[glyph, severity, style_id]` per row) plus a
`sign_column` bool from `Server::diagnostics_sign_column` (signs on **and** the
buffer has ≥1 diagnostic — vim's `signcolumn=auto`) →
`WindowView.diagnostics_signs: Vec<Option<DiagSign>>` / `sign_column` → the TUI
`render_sign_column` paints the glyph in a fixed 2-cell column carved *left of*
the number gutter (severity foreground or the resolved `DiagnosticSign*` group),
and `text_inner_rect` mirrors the carve so the completion popup still anchors past
both gutters. The config threads `signs` (bool, plus the table form's `text` map)
through `nx._diagnostic_config` → `LspOp::DiagnosticConfig`. Verified by
`signs_are_on_by_default_and_reserve_a_column` /
`signs_pick_the_highest_severity_on_a_line` / `signs_false_reserves_no_column` /
`signs_honor_a_custom_text_glyph` (`crates/nxvim/tests/lsp/diagnostic_api.rs`) and
the Tier-2 paint test `a_diagnostic_sign_is_painted_in_the_gutter`
(`tests/lsp/diagnostics.rs`). Runnable demo: `examples/diagnostics/`.

*Known approximations:* the sign column is **client-side only** — `nxvim-core`
computes `text_width` from `rect.width - number_width` and has no view of the
diagnostics cache, so it doesn't subtract the 2-cell sign column; under `nowrap` a
full-width line with signs on can clip its last two cells / nudge the horizontal
scroll. The most-severe diagnostic *starting* on a line wins its one sign cell (no
per-sign stacking, no priority/`culhl`); the column width is fixed at 2.

**Depends on.** Phase 1 (the `DiagnosticConfig` struct + config plumbing).

---

## Phase 3 — `vim.diagnostic.open_float` ✅

**Goal.** `vim.diagnostic.open_float([opts])` shows every diagnostic on the
cursor's line (full, multi-line messages with source/code) in a float — the
on-demand detail surface, the `<C-w>d` / `<leader>e` idiom.

**Why.** Underline + virt-text + sign are glanceable; the float is how you read
the *full* message (virt-text is truncated to one line). Configs bind it
directly; it completes the trio.

**Scope.**
- `crates/nxvim-server/src/lsp/diagnostics.rs` — `diagnostics_open_float()`:
  collect the cursor line's diagnostics (sorted by severity then column),
  format `severity  source: message [code]` lines, and open them via the
  existing `Editor::open_panel` / float surface (reuse `show_hover`'s path).
- `crates/nxvim-lua/src/{ops.rs,install.rs}` + `prelude/diagnostic.lua` —
  `vim.diagnostic.open_float` → `nx._diagnostic_open_float()` →
  `LspOp::DiagnosticOpenFloat`. No-op (loud nothing) when the line is clean.
- Optionally wire `config.float` later; the function is the deliverable.

**Approach.** Reuse the hover panel/float surface verbatim — this phase is mostly
formatting the cursor line's diagnostics into lines and routing through the
existing `open_panel`. No new render surface.

**Tests** (`crates/nxvim-server/tests/editing/` or `lsp.rs`):
- with the cursor on a diagnostic line, `vim.diagnostic.open_float()` opens the
  panel/float carrying the full message(s);
- a clean line opens nothing;
- multiple diagnostics on the line are all listed, severity-sorted.

**Done when.** ✅ `vim.diagnostic.open_float()` shows the cursor line's full
diagnostics in a float. The projection is `Server::diagnostics_open_float`
(`crates/nxvim-server/src/lsp/diagnostics.rs`): it collects the cursor line's
diagnostics (those *starting* on the line, neovim's `lnum` scope), severity- then
column-sorts them, formats each via the free `diagnostic_float_lines`
(`E  source: <first message line> [code]` header + any remaining message lines,
every line control-sanitized like `first_line`), and opens them through the
existing `Editor::open_panel` ("Diagnostics" title) — the same float surface hover
uses. A clean cursor line is a *loud* no-op: `echo("No diagnostics under cursor")`,
no panel. The op threads `vim.diagnostic.open_float` →
`nx._diagnostic_open_float()` (`install.rs`) → `LspOp::DiagnosticOpenFloat`
(`ops.rs`) → `Server::diagnostics_open_float` (`sync.rs`), reading the cursor at
apply time. The `INCOMPLETE` note in `diagnostic.lua` lost its bare `float`
clause (only the `config.float` pre-style defaults remain inert). Verified by
`vim_diagnostic_open_float_shows_the_cursor_lines_diagnostics` /
`vim_diagnostic_open_float_on_a_clean_line_opens_nothing` /
`vim_diagnostic_open_float_lists_all_diagnostics_severity_sorted`
(`crates/nxvim/tests/lsp/diagnostic_api.rs`). Runnable demo: `examples/diagnostics/`
(`<leader>d`).

*Known approximations:* `opts` (scope/severity filters, `format`, `header`,
`prefix`, `border`) is ignored — the default cursor-line scope is what shows; the
float is the bottom panel (markdown rendered as plain lines, like hover), not a
cursor-anchored bordered popup; `config.float` pre-style defaults are inert.

**Depends on.** Phase 1 (config struct); independent of Phase 2.

---

## Known approximations to expect

- **One config, no namespaces.** `vim.diagnostic.config(opts, namespace)` keeps
  ignoring `namespace` (one global config) — inherited from today.
- **`update_in_insert` is always on.** Diagnostics repaint immediately; nvim can
  defer redraw until leaving insert mode. Out of scope.
- **Virtual text is one line, end-of-line only.** No `virtual_lines` (the
  below-the-line multiline form), no per-diagnostic stacking — the highest
  severity on the row wins one inline string.
- **Markdown in the float is plain lines**, like hover today.
- **Signs share the gutter**, no per-sign priority/`culhl`, fixed 2-cell width.

## Suggested order

`1 → 2 → 3`. Phase 1 carries the config-struct refactor and the decoration
projection both later phases lean on; 2 and 3 are independent of each other.
Each phase ships with a runnable `examples/` config + sample file proving the
surface end to end.
