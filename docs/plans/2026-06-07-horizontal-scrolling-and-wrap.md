# Horizontal scrolling & line wrapping — implementation plan

## Why this document exists

nxvim has **no horizontal scrolling and no line wrapping**. Every text window
projects raw buffer lines that the TUI paints with a ratatui `Paragraph`, which
**clips at the right edge**, and the terminal cursor is placed at an absolute
`cursor_screen_col`. Two consequences, both wrong:

| situation | neovim | nxvim today | where |
| --- | --- | --- | --- |
| cursor moves right past the window's text width (long line) | the viewport scrolls horizontally (`w_leftcol`) to keep the cursor visible | the line stays clipped, the cursor pins at the edge — text never scrolls | `view.rs` `window_view`, `render.rs` `render_text` |
| a line longer than the window, `wrap` on (neovim default) | the line wraps onto extra display rows | the line is clipped; `wrap` does not exist | — |
| `:set nowrap` / `:set wrap` | toggles the above | `E518: Unknown option: wrap` | `options.rs` `canonical` |

This is the "major oversight" the flag refers to: there is no `leftcol` and no
`wrap`. nxvim's *only* text-window behavior is "clip and don't scroll", which is
neither vim's `wrap` (the default) nor a working `nowrap`.

This plan adds both, **phased deliberately by their cost**:

- **Phase 1 — horizontal scrolling (`nowrap`).** A per-window `leftcol` screen-
  column offset that mirrors the existing vertical `top` exactly. Small, contained,
  and it is the behavior nxvim *already* approximates (minus the scroll), so it
  needs no new display-row model. **This is the first deliverable.**
- **Phase 2 — line wrapping (`wrap`), MVP.** Introduces the `wrap` option and a
  **display-row projection** in the core: one buffer line becomes one *or more*
  screen rows, which rewrites the per-row parallel-array contract
  (`lines`/`numbers`/`selection`/`search`/`highlights`/`diagnostics`/`cursor_row`)
  and makes vertical scroll display-row aware. Much larger; kept separate.
- **Phase 3 — wrap refinements.** `gj`/`gk` display-line motions and
  `linebreak`/`breakindent`/`showbreak`. Deferred.

Agreed scoping decisions (from the requester): **`wrap` defaults off**, Phase 2 is
**MVP fidelity**, and horizontal scrolling lands **first**. Per CLAUDE.md every
phase is **test-driven**: write the failing editing/screen test first, then
implement.

Each phase is sized to be picked up in one focused session. Phase 1 has no
dependency on the others; Phase 2 builds the display-row model; Phase 3 assumes
Phase 2.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

| phase | title | status |
| --- | --- | --- |
| 1 | Horizontal scrolling (`nowrap`) — per-window `leftcol`, `sidescroll`/`sidescrolloff` | ✅ |
| 2 | Line wrapping (`wrap`) — display-row projection, MVP fidelity | ⬜ |
| 3 | Wrap refinements — `gj`/`gk`, `linebreak`/`breakindent`/`showbreak` | ⬜ |

**Phase 1 implementation note.** Landed as designed. `Editor::leftcol` mirrors
`top` (live for the focused window, stashed in `Window::saved_leftcol` /
`OpenBuffer::saved_leftcol` otherwise, threaded through `WindowLayout::leftcol`);
`sidescroll`/`sidescrolloff` joined `WindowOptions` (defaults `1`/`0`), wired
through `:set` (`apply_set_num` gained a window-local branch) and `options::canonical`.
`Editor::text_width` (the inset/gutter-aware horizontal analog of `text_height`)
and `Editor::ensure_visible_horizontal` (called at the end of `ensure_visible`, so
every motion gets it) compute the scroll; the `View`/redraw carry `leftcol`, and
the TUI `render_text`/`highlight_line` drop the first `leftcol` screen cells and
shift the cursor by it (the pre-existing float cursor-clamp is now the leftcol
offset plus a safety clamp). One behavioural note worth recording: **`leftcol` is
sticky** (vim-faithful) — it is set while typing past the edge and is *not*
re-minimized when the cursor later moves left but stays on screen, so a settled
cursor need not sit on the very last column. Coverage: 3 editing tests + the
example-config end-to-end test (`editing.rs`), a focused-bordered-float scroll test
(`windows.rs`), a Tier-2 painted-grid test (`screen.rs`), and the runnable
`examples/horizontal-scroll/`. Deferred as planned: numeric `vim.wo` /
`nvim_set_option_value` routing for `sidescroll`/`sidescrolloff` (the example drives
the wired `:set` surface, not `vim.o`, to avoid a silent no-op).

---

## The one constraint that shapes everything

**Phase 1 changes only *where* a row starts; Phase 2 changes *how many* rows a
line is.** That single distinction is why they are separate phases:

- `leftcol` (Phase 1) is a per-window **screen-column offset** measured in the
  same tab/wide-char–expanded virtual columns as `cursor_screen_col` and every
  existing span array. It mirrors the vertical `top` *exactly* — live on `Editor`
  for the focused window, stashed per-window otherwise — and the **1-row-per-
  buffer-line contract is untouched**. The core decides its value (the scroll
  policy); the client applies it when painting (the symmetric operation to the
  tab-expansion + right-clip it already does). No new wire shape, no animation.
- `wrap` (Phase 2) **breaks** the 1-row-per-line contract: a buffer line maps to
  N display rows. Every per-row parallel array, the cursor's `(row, col)`, and the
  vertical-scroll math (`top` becomes "buffer line + within-line display-row
  offset") must become display-row aware. This is a projection rewrite, not an
  offset.

If a Phase-1 design pressures you toward a display-row list, it is over-built:
`leftcol` is one integer per window and a render offset. Keep them separate.

While `wrap` is on, **`leftcol` is forced to `0`** (vim does not horizontally
scroll a wrapped window).

---

## The current state (what we are extending — the seams)

The render pipeline, end to end:

- **`crates/nxvim-core/src/editor.rs`** owns scroll/cursor state. `Editor.top` is
  the focused window's first visible buffer line; `Window.saved_top` /
  `WindowLayout.top` carry it for non-focused windows. `ensure_visible()` (~6335)
  re-clamps `top` after every motion and is called from ~20 sites.
  `text_height()` (~3072) = focused `cur().rect.height − 1`. `cursor_virtcol()`
  (~6206) gives the cursor's screen column. Window-local options live on
  `WindowOptions` (`options.rs`), routed by `:set` through `apply_set_bool`
  (~5985, `number`/`relativenumber` → `windows.cur_mut().options`) and
  `apply_set_num` (~6016, currently buffer-local only).
- **`crates/nxvim-core/src/view.rs`** `window_view()` (~249) projects each
  `WindowLayout` into a `WindowView`: raw `lines` (tabs intact), absolute
  screen-column `selection`/`cursor_screen_col`, 1-based `numbers`, and — for the
  new floats — a bordered float's content is the rect **inset by one cell**
  (`inset`, `content_width`, `width = content_width − number_width`, lines
  255–269). `WindowLayout` (editor.rs ~981) carries `floating`/`border`/`title`.
- **`crates/nxvim-server/src/redraw.rs`** `window_value()` (~116) serializes the
  `WindowView` map (`lines`, `cursor_screen_col`, `number_width`, `tabstop`,
  `floating`, `border`, …) and resolves syntax/diagnostics to **absolute** screen-
  column spans. The pmenu anchor column is computed near line 67.
- **`crates/nxvim-tui/src/render.rs`** `render_window` (~202) → the single
  `render_text(...)` call (~306) → `highlight_line` (~489): expands tabs
  (`expand_tabs`), walks cells left-to-right keying styles on the absolute screen
  `col`, and the cursor is placed at `inner.x + cursor_screen_col` (~166). Floats
  reuse `render_window`, so anything threaded there covers floats for free.
- **`crates/nxvim-tui/src/view.rs`** parses the window map into the TUI's own
  `WindowView` (`map_u16`/`map_u64`, ~278).

Tests: `crates/nxvim-server/tests/editing.rs` asserts on the `redraw` View
(`start`/`feed`/`latest_redraw`/`view_*` helpers); `crates/nxvim/tests/screen.rs`
(Tier 2) paints the real `View` and asserts on the cell grid (`GUTTER` const,
`paint`).

---

## Target architecture

`leftcol` joins `top` as a first-class per-window view coordinate:

```
Editor.leftcol (focused)  ──┐
Window.saved_leftcol        ├─ WindowLayout.leftcol ─→ WindowView.leftcol ─→ wire "leftcol" ─→ TUI render offset
(non-focused, stashed)    ──┘                          (core projection)      (redraw.rs)        (render.rs)
```

The core owns the **policy** (`ensure_visible_horizontal` decides the value from
`cursor_virtcol`, `text_width`, `sidescroll`, `sidescrolloff`); the client owns
the **paint** (drop the first `leftcol` screen cells, shift the cursor and every
span left by `leftcol`). Phase 2 later inserts a display-row layout step between
`window_view` and the wire, but **does not touch the `leftcol` machinery** (it
just pins it to 0 when `wrap` is on).

---

## Phase 1 — Horizontal scrolling (`nowrap`) ✅

The full, in-scope deliverable. No display-row model; `leftcol` mirrors `top`.

### 1a. State — `leftcol` mirrors `top` (`editor.rs`)

- Add `leftcol: usize` to `Editor` (focused window's h-scroll), init `0`.
- Add `saved_leftcol: usize` to the per-window `Window` struct (declared/initialized
  wherever `saved_top` is — ~253/431/507) and to `WindowLayout` (~981).
- Populate `WindowLayout.leftcol` in `window_layouts()` (~2436):
  `leftcol: if focused { self.leftcol } else { w.saved_leftcol }`.
- Stash/restore alongside `saved_top` at **every** site that touches `saved_top`:
  the focus stash/restore (~2146–2173, 2470–2500, 2680–2709) and `split()` (~2468).
  Grep `saved_top` and shadow each occurrence with `saved_leftcol`.

### 1b. Options — `sidescroll`, `sidescrolloff` (`options.rs` + `editor.rs`)

- Add `sidescroll: usize` and `sidescrolloff: usize` to `WindowOptions` (window-
  local, like `number`). Defaults **`sidescroll = 1`, `sidescrolloff = 0`**
  (confirm against neovim during impl).
- `canonical()`: `"sidescroll" | "ss"` and `"sidescrolloff" | "siso"` → `(name, Num)`.
- `apply_set_num()` (~6016) currently writes only buffer-local options. Add a
  window-local branch routing these two to `windows.cur_mut().options` with
  `min = 0`; mirror in the `Query` arm so `:set ss?` echoes. (Numeric `vim.wo` /
  `nvim_set_option_value` routing for them is an optional small extra — `:set` is
  the in-scope surface.)

### 1c. Width helper + scroll policy (`editor.rs`)

- Add `text_width(&self) -> usize` next to `text_height()`: focused window's text
  width = `cur().rect.width − 2*inset − number_width`, where `inset = 1` for a
  bordered float else `0` (mirror `window_view`'s inset, view.rs 255–269) and
  `number_width = number_width_for(cur().options, line_count)` for the focused
  buffer.
- Add `ensure_visible_horizontal(&mut self)`, the analog of `ensure_visible()`.
  With `vc = cursor_virtcol()`, `tw = text_width()`, `so = sidescrolloff` (capped
  at `tw/2`), `ss = sidescroll`:
  - `vc < leftcol + so` → scroll left: `ss == 0` centers
    (`leftcol = vc.saturating_sub(tw/2)`), else `leftcol = vc.saturating_sub(so)`.
  - `vc >= leftcol + tw − so` → scroll right: `ss == 0` centers, else
    `leftcol = (vc + so + 1).saturating_sub(tw)`.
  - Floor `leftcol` at 0; no-op when `tw == 0`.
- Call it from the **end of `ensure_visible()`** so all ~20 existing call sites
  get horizontal scrolling uniformly, with no other edits.

### 1d. Projection + wire (`view.rs`, `redraw.rs`)

- `WindowView`: add `pub leftcol: usize`; set from `w.leftcol` in `window_view()`.
  Span/cursor computations stay absolute — unchanged.
- `redraw.rs` `window_value()`: add `("leftcol", win.leftcol as u64)`. Subtract the
  focused `leftcol` from the pmenu anchor column (~67) so the completion popup
  tracks the scrolled text.

### 1e. Client paint (`nxvim-tui` `view.rs`, `render.rs`)

- TUI `WindowView`: add `leftcol: u16`, parsed `map_u16(m, "leftcol")`.
- Thread `win.leftcol` into the single `render_text(...)` → `highlight_line`:
  - after `expand_tabs`, emit only cells whose absolute `col >= leftcol`, placed
    at screen position `col − leftcol`; keep `cell_style(col, …)` on the
    **absolute** `col` so every existing span still lines up.
  - a wide char / tab straddling the `leftcol` boundary is skipped for MVP (minor
    left-edge artifact — note it).
  - selection trailing-pad: clamp the fill start to `max(col, leftcol)`.
  - cursor (~166): `inner.x + cursor_screen_col.saturating_sub(leftcol)`.
  - the number gutter is **not** offset; `~` filler rows unaffected; both the
    static and scroll-animation paths share `render_text`, so the slide band
    inherits `leftcol`.

### 1f. Tests (write first)

- `editing.rs`: cursor right past text width → `leftcol` advances, cursor stays
  visible (`cursor_screen_col − leftcol < text_width`); `0` returns `leftcol` to 0;
  `:set sidescrolloff=4` keeps a 4-col margin; `:set ss?` echoes; a bordered float
  scrolls using its inset width.
- `screen.rs`: long line, cursor moved right → painted body shows the scrolled
  window, gutter intact, terminal cursor at the right column.

### 1g. Example config (repo convention)

Ship a runnable `examples/horizontal-scroll/` (init + sample file with long
lines) demonstrating `nowrap` scrolling and `sidescrolloff`, verified end-to-end.

---

## Phase 2 — Line wrapping (`wrap`), MVP ⬜

**Depends on:** nothing structural from Phase 1 beyond the option plumbing; reuses
`WindowOptions`.

The big change: a **display-row projection**. Introduce the `wrap` window option
(default **off**) and, when on, expand each visible buffer line into one or more
display rows in the core, so the `View` carries display rows, not buffer lines.

- **Option:** add `wrap: bool` to `WindowOptions` (default `false`); `canonical`
  `"wrap"` (Bool); route through `apply_set_bool` to `windows.cur_mut().options`.
  Because flipping `wrap` on must *do something* (no silent stubs — CLAUDE.md), the
  projection work below lands **in the same phase** as the option.
- **Projection (`view.rs`):** between slicing buffer lines and emitting the
  `WindowView`, run a wrap pass keyed on the window's text `width`: break each
  buffer line at `width` screen columns into segments, and emit **per-display-row**
  `lines`, `numbers` (number on the first segment only, `None` on continuations),
  `selection`/`search`/`incsearch` (re-sliced per segment in screen columns), and a
  display-row `cursor_row`. `cursor_screen_col` becomes the column **within** the
  cursor's wrapped segment. When `wrap` is off, the pass is identity (Phase-1
  behavior) — so the two coexist.
- **Vertical scroll (`editor.rs`):** `top` gains a within-line display-row offset
  (or `text_height` is measured in display rows for the focused window).
  `ensure_visible()` becomes display-row aware so a tall wrapped line scrolls into
  view a row at a time. `leftcol` is pinned to 0 while `wrap` is on.
- **Server/client:** `redraw.rs` already serializes per-row arrays — it carries
  whatever count the projection emits. `render.rs` drops its right-clip when the
  rows are pre-wrapped (a display row already fits `width`). Syntax/diagnostic
  spans must be re-sliced per display row server-side (the one real server change).
- **Tests:** wrapped line occupies N rows; cursor on a wrapped segment lands on the
  right display row/col; `j`/`k` still move by **buffer** line (not display line);
  vertical scroll reveals a tall line correctly; `:set wrap`/`:set nowrap` toggle;
  Tier-2 paint of a wrapped paragraph. Example: extend `examples/horizontal-scroll/`
  or add `examples/wrap/`.

---

## Phase 3 — Wrap refinements ⬜

**Depends on:** Phase 2.

- `gj` / `gk` (and `g0`/`g$`): motion by **display** line, the counterpart to the
  buffer-line `j`/`k`. Needs the display-row layout from Phase 2 to map a display
  row back to a byte offset.
- `linebreak`: break wrapped lines at word boundaries (`breakat`) instead of mid-
  word — the panel already word-wraps (`editor.rs` panel projection), so reuse that
  break logic.
- `breakindent`: indent continuation rows to match the line's leading indent.
- `showbreak`: a prefix string (e.g. `↪`) on continuation rows.

Each is an independent, testable add-on; none blocks Phases 1–2.

---

## Suggested order & scoreboard

Phase 1 makes long lines usable (the reported oversight) and is low-risk —
`leftcol` is one integer that shadows `top`. Phase 2 makes nxvim's default text
window match vim's mental model for paragraphs once `wrap` is opted into; it is the
projection rewrite. Phase 3 is fidelity polish.

Scoreboard — the surface a user exercises:

- [x] cursor on a long line scrolls the window right; `0` scrolls it back (P1)
- [x] `:set sidescrolloff` / `:set sidescroll` honored (P1)
- [x] floats scroll horizontally within their inset width (P1)
- [ ] `:set wrap` wraps long lines onto extra rows; cursor maps correctly (P2)
- [ ] vertical scroll is display-row aware under `wrap` (P2)
- [ ] `gj`/`gk` move by display line; `linebreak`/`breakindent`/`showbreak` (P3)

---

## Testing appendix

- Editing-tier (`crates/nxvim-server/tests/editing.rs`): assert on the `redraw`
  View's `leftcol`, `cursor_screen_col`, and (P2) the per-display-row `lines`/
  `numbers` counts. Drive with `feed`; read with the take-latest helpers (never the
  first queued redraw — see CLAUDE.md / `redraw-test-helpers-take-latest`).
- Screen-tier (`crates/nxvim/tests/screen.rs`): paint and assert the cell grid;
  the gutter is `GUTTER` cells, text/cursor offset past it.
- Commands: `cargo test -p nxvim-server --test editing <name>`,
  `cargo test -p nxvim --test screen <name>`, then `cargo test --workspace`.
  Lint `cargo clippy --all-targets -- -D warnings` + `cargo fmt --all`. **Default
  features only — never `--all-features`** (CLAUDE.md: the Lua backend features are
  mutually exclusive). Manual check: `cargo run -p nxvim -- <long-lined file>`.

---

## Risks & non-goals

- **Wide-char / tab straddling the `leftcol` boundary** (P1) renders a one-cell
  left-edge artifact (the partial glyph is skipped). Acceptable for MVP; vim paints
  a `<` fill only with `listchars`, which nxvim does not have.
- **No `listchars` `extends`/`precedes` markers** (the `<`/`>` off-screen
  indicators) — out of scope; nxvim has no `listchars`.
- **`sidescroll`/`sidescrolloff` modeled window-local** for simplicity (vim makes
  `sidescroll` global, `sidescrolloff` global-local). Note the divergence; promote
  later if a plugin depends on the global scope.
- **Phase 2 is the invasive one** — it rewrites the per-row contract. Do not start
  it inside Phase 1; the clean boundary is "offset vs. row-count".
- **No silent stubs** (CLAUDE.md): do not ship the `wrap` *option* without the
  wrapping *behavior*. That is why the option and the projection are one phase.
