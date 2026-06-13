# Floating windows — implementation plan

## Why this document exists

nxvim has **multiple windows** — splits, the layout tree, per-window view state,
the `<C-w>` family, and the `nvim_win_*` / Lua API (see
[`architecture.md` → *Windows*](../architecture.md#windows)). What it does **not**
have is the *other* kind of window neovim grew: a **floating window** — a free
window positioned by absolute coordinates *on top of* the tiled layout rather than
tiled into it. Floats are how every rich editor UI is drawn: completion docs,
hover, signature help, key-hint popups, fuzzy-finder pickers, notifications,
borders around `vim.ui.select`. Today `nvim_open_win` accepts **only the split
form** and raises/ignores the float config:

| surface | neovim semantics | nxvim today | where |
| --- | --- | --- | --- |
| `nvim_open_win(buf, enter, {relative=…})` | open a float positioned by `relative`/`anchor`/`row`/`col` | **split form only** — `relative` is dropped, a split is made instead | `dispatch.rs` `nvim_open_win`; `nvim_api.lua` `nvim_open_win` |
| `nvim_win_set_config(win, cfg)` | move/resize a float, convert split↔float | **absent** (`nx._notimpl`/unknown method) | — |
| `nvim_win_get_config(win)` | read a window's float config (`{relative=""}` for non-floats) | **absent** | — |
| a window drawn over the tiled area with a border | a float with `border`, `title`, `zindex` | only the **completion pmenu** floats, and it is bespoke client chrome, not a window | `render.rs` `render_pmenu` |

The seams were **built to carry floats** when windows landed (the windows commit
says so explicitly, and `architecture.md` lists floats as the next item on the
windows axis): `nvim_open_win` already returns a synchronously-predicted id, the
`WindowOp` queue already carries window mutations, the `View` already projects a
**list** of windows with per-window rects, and the TUI already paints each window
at an arbitrary rect (and already overlays the pmenu with `Clear` + a bordered
block — the exact rendering primitive a float needs). This plan fills the gap.

The plan is divided into self-contained phases. **Each is sized to be picked up
and implemented in one focused session without the others loaded.** Phases list
their dependencies; later phases assume earlier ones landed. The running
scoreboard is the float surface a real plugin config exercises (`nvim_open_win`
float form → `nvim_win_set_config` → border/title → autocmds); each phase clears
part of it.

---

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

| phase | title | status |
| --- | --- | --- |
| 1 | The float in the core — model, positioning, lifecycle (queryable, not yet painted) | ✅ |
| 2 | Painting floats — the `View` projection + the TUI overlay | ✅ |
| 3 | The full config surface — `nvim_win_set_config`/`get_config`, split↔float, the Lua API + mirror | ✅ |
| 4 | Autocmds, edge semantics (`:q`/`:only`/focus), and hardening | ✅ |

**Phase 1 implementation note.** Landed as designed. The float model lives on
`WindowTree` (`Window.float: Option<FloatConfig>` + a z-sorted `floats: Vec<WindowId>`
outside the `Node` tree); `WindowTree::layout` gained a `position_floats` second
pass (editor/win/cursor origin → anchor math → on-screen clamp, in `place_float`);
`Editor::open_float_window`/`window_float_config` and a float-aware `remove_window`
are the lifecycle; `window_ids` appends floats after the tiled leaves. RPC:
`nvim_open_win` branches on `config.relative`, plus new `nvim_win_get_config` and
`nvim_win_get_position`; unsupported `relative`/`anchor`/`border` values fail loud
(`dispatch.rs::parse_float_config`). Two divergences from the literal plan text,
both deliberate: (1) the `WindowView`/`window_layouts` painting fields are
**deferred to Phase 2** so the float stays unpainted (the clean "queryable but not
drawn" boundary — including floats in the `View` now would make the unchanged TUI
render them as misplaced tiled windows); (2) `relayout` guards its cursor-cell
computation against the transient invalid `current` during a focused-window close
(`cursor_virtcol` reads the current window's buffer), and `remove_window` re-lays
once the survivor is entered so cursor-relative floats settle correctly. Coverage:
9 float tests in `crates/nxvim-server/tests/windows.rs` (positioning, anchor,
editor/win/cursor relativity, zindex order, focus/close, off-screen clamp,
get_config round-trip, loud rejection).

---

## The one constraint that shapes everything

**A float is a `Window` that the layout tree does not own.** Every tiled window
today is a `Node::Leaf(WindowId)` inside `WindowTree::root`; `WindowTree::layout`
divides the windows area across that tree and writes each window's `rect`
(`crates/nxvim-core/src/editor.rs`). A float must **not** be in that tree — it
steals no space from its siblings, it sits at absolute coordinates, and it paints
on top. So the model is:

> The `WindowTree` keeps its `root: Node` for the tiled windows *exactly as
> today*, and gains a **separate, ordered list of floating `WindowId`s** living
> in the same `windows: BTreeMap`. `layout()` runs the tiled pass unchanged, then
> a **second pass** positions each float absolutely from its `FloatConfig`
> against the editor / a parent window / the cursor. Focus, the window list, and
> the lifecycle (`nvim_list_wins`, close, autocmds) span **both** sets; the
> *tiling math* touches only the tree.

This keeps the proven tiling logic untouched (a single-`Leaf` tree still lays out
identically; splits still divide the tree's area) and isolates all float-specific
behavior to the float list and the second layout pass. **Do not thread float-ness
into `Node`** — a float is never a tree leaf. If a design pressures you to put a
float in the `Node` tree, it is wrong; re-derive it from this separation.

This mirrors how the pmenu already works in the renderer: tiled windows paint
first, the overlay paints second with `Clear`. We are promoting that "paint a box
on top" idea from bespoke client chrome (`render_pmenu`) into a first-class
*window* the core owns and positions.

---

## The current state (what we are extending — the seams)

`crates/nxvim-core/src/editor.rs`:

- **`Window`** — `{ buffer, saved_cursor, saved_top, rect }`. A float adds its
  config; the cleanest shape is `float: Option<FloatConfig>` (`None` = tiled).
- **`Node`** (`Leaf` | `Split`) and **`WindowTree`** (`{ windows: BTreeMap<WindowId,
  Window>, root: Node, current, next_id, separators }`). Floats are ids in
  `windows` that **no** `Node::Leaf` references.
- **`WindowTree::layout(total)`** → `layout_node(...)` assigns tiled rects + the
  `separators`. We add a float-positioning pass after it.
- **`window_layouts()`** → `Vec<WindowLayout>` in tree order, the `View`'s input.
  Floats are appended here, in z-order, after the tiled windows.
- **`open_split_window(buf, vertical)`** — the split-form entry. The float entry
  is its sibling: `open_float_window(buf, config) -> WindowId`.
- **`focus_window`**, **`remove_window`**, **`next_window_id`**,
  **`window_rect`/`window_buffer`/`window_cursor`** — already id-addressed; they
  work on a float id as soon as the float is in `windows`.

`crates/nxvim-core/src/view.rs`:

- **`WindowView`** (`rect`, `buffer`, `focused`, `lines`, cursor, `selection`,
  `search`, `numbers`, …) and **`View { windows, separators, … }`**. A float adds
  a few fields (`floating`, `border`, `zindex`) so the client can overlay it.
- **`ViewRect`**, **`Separator`** — the wire rect/border types.

`crates/nxvim-server/src/`:

- **`dispatch.rs`** `nvim_open_win` — split form only today; **`resolve_win`** maps
  a wire handle (`0` = current) to a `WindowId`.
- **`redraw.rs`** `redraw()` — projects `view.windows` + `separators` into the
  msgpack map. New float fields ride here.
- **`effects.rs`** — drains `WindowOp`s and runs the `emit_lifecycle_events`
  diff (`WinNew`/`WinEnter`/`WinLeave`/`WinClosed`).

`crates/nxvim-tui/src/`:

- **`render.rs`** `render()` paints each window at `window_area(...)`, then
  `render_separators`; **`render_pmenu`** is the float-overlay precedent
  (`Clear` + bordered `Block` + inner content). **`view.rs`** parses `windows`.

`crates/nxvim-lua/src/`:

- **`ops.rs`** `WindowOp` (`SetCurrent`/`SetBuf`/`SetCursor`/`SetWidth`/`SetHeight`/
  `Close`/`Open`). Floats add `OpenFloat` and `SetConfig`.
- **`prelude/nvim_api.lua`** `nvim_open_win` (split form, write-through to `nx._wins`),
  `nx._next_win`, the `nx._wins` mirror the server refreshes before each chunk.

---

## Target architecture

```
  nvim_open_win({relative=…})  ┐
  nvim_win_set_config          ├─▶ WindowOp::OpenFloat / SetConfig ─▶ effects drain ─▶ Editor
                               ┘                                                        │
                                                                                        ▼
        Editor.windows (WindowTree)                                      ┌─ layout(total) ─┐
        ├─ root: Node          ── tiled pass (unchanged) ───────────────▶│ tiled rects     │
        │   └─ Leaf/Split …                                              │ + separators    │
        └─ floats: [WindowId]  ── float pass (new): position each ──────▶│ float rects     │
            (ids in `windows`,    against editor / win / cursor,         │ (z-ordered)     │
             not in `root`)       clamp to screen, z-sort                └────────┬────────┘
                                                                                   ▼
                          window_layouts()  →  [tiled WindowViews] ++ [float WindowViews]
                                                                                   ▼
   redraw():  windows[] (each {rect, floating, border, zindex, …}) + separators ──▶ wire
                                                                                   ▼
   TUI render():  paint tiled windows + separators,  THEN floats in z-order (Clear+border on top),
                  THEN the pmenu (highest layer).  Terminal cursor → focused window (tiled or float).
```

**Four mechanisms, introduced across the phases:**

- **`FloatConfig` + the float list (Phase 1).** `Window.float: Option<FloatConfig>`;
  `WindowTree` tracks float ids ordered by `zindex`. `layout()`'s second pass
  resolves each to an absolute `rect`.
- **Float-tagged `WindowView` (Phase 2).** `WindowView` carries `floating`,
  `border`, `zindex`; the redraw serializes them; the TUI overlays them.
- **`OpenFloat`/`SetConfig` `WindowOp`s + the config RPC (Phase 3).**
  `nvim_open_win` float form, `nvim_win_set_config`/`get_config`, the `nx._wins`
  mirror's float fields, write-through in `nvim_api.lua`.
- **The lifecycle span (Phase 4).** Floats participate in `nvim_list_wins`, the
  focus cycle (honoring `focusable`), `:q`/`:only`/`<C-w>` semantics, and the
  `WinNew`/`WinEnter`/`WinClosed`/`WinResized` autocmd diff.

---

## Phase 1 — The float in the core: model, positioning, lifecycle ✅

**Goal.** A float is a **real, queryable, focusable window** that the layout tree
does not own. `nvim_open_win(buf, enter, {relative="editor", row, col, width,
height})` creates one; it gets an absolute rect from its config; it appears in
`nvim_list_wins`; it can be focused and closed; and it **does not disturb the
tiled layout**. `nvim_win_get_config(win)` returns its float config (and
`{relative=""}` for a tiled window, as neovim does). The float is **intentionally
not painted yet** — that is Phase 2 — but everything *about* it is observable over
RPC, so the phase is fully testable and is not a silent stub: a caller can open a
float and read back its exact rect, buffer, and focus state.

**Why.** This is the spine: the model + positioning math + the fact that the rest
of the window machinery (focus, list, close) already keys off `WindowId` and so
spans floats for free once they live in `windows`. Build and test the *geometry*
in isolation, where a wrong rect is a failed assertion on `nvim_win_get_position`,
before any pixels are involved.

**Scope (files).**
- `crates/nxvim-core/src/editor.rs` — `FloatConfig`, `Window.float`, the float
  list on `WindowTree`, the second layout pass, `open_float_window`,
  `window_layouts` appends floats, the config getter.
- `crates/nxvim-server/src/dispatch.rs` — `nvim_open_win` branches on
  `config.relative`; new `nvim_win_get_config`, `nvim_win_get_position`.

**Approach.**

1. **`FloatConfig` (core).**
   ```rust
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   enum FloatRelative { Editor, Win(WindowId), Cursor }
   #[derive(Debug, Clone, Copy, PartialEq, Eq)]
   enum FloatAnchor { NW, NE, SW, SE }
   struct FloatConfig {
       relative: FloatRelative,
       anchor: FloatAnchor,
       row: isize, col: isize,     // anchor offset from the relative origin
       width: usize, height: usize,
       zindex: u32,                // default 50, as neovim
       focusable: bool,            // default true
       border: BorderStyle,        // None/Single/Rounded/Double/Solid (Phase 2 paints it)
   }
   ```
   Add `float: Option<FloatConfig>` to `Window` (`None` = tiled). Border *width*
   matters to geometry even in Phase 1 (a bordered float's inner text area is
   `width-2 × height-2`), so carry `border` now; only its *painting* waits.

2. **The float list (`WindowTree`).** Add `floats: Vec<WindowId>` kept sorted by
   `(zindex, id)` (id breaks ties → stable creation order). `with_one` leaves it
   empty. Invariant to assert: every id in `floats` has `float.is_some()` and is
   **absent** from `root`; every `Leaf` id has `float.is_none()`.

3. **The second layout pass.** After `layout_node` fills tiled rects, position
   each float:
   - resolve the **origin rect**: `Editor` → the whole windows area (`total`);
     `Win(id)` → that window's already-computed `rect`; `Cursor` → a 1×1 rect at
     the focused window's cursor cell (text-area origin + `cursor_screen_col` /
     `cursor_row`).
   - apply `row`/`col` from the origin's top-left, then shift by `anchor` so the
     named corner of the float lands there (`NE` subtracts `width`, `SW`
     subtracts `height`, `SE` both) — neovim's anchor math.
   - **clamp** the resulting rect into the windows area (never position a float
     partly off-screen at open; a deliberate off-screen `col` is clamped to the
     edge, matching neovim's "kept on screen" behavior for the common case).
   Floats are positioned **after** tiled windows precisely because `Win`/`Cursor`
   relativity reads tiled rects. Write the rect onto the `Window`.

4. **`open_float_window(buf, config) -> WindowId`** (the `open_split_window`
   sibling). Mint an id, insert a `Window { buffer: buf, float: Some(config), … }`,
   push the id into `floats` (sorted), `relayout()`, focus it if `enter`. It does
   **not** call `split` — the tiled tree is untouched.

5. **`window_layouts()` appends floats.** After the tiled leaves (tree order),
   append the floats in z-order, each as a `WindowLayout` (Phase 2 reads a new
   `float`/`border`/`zindex` off it — add those fields now, defaulted for tiled).

6. **RPC.**
   - `nvim_open_win`: if `config.relative` is a non-empty string, parse the float
     config and call `open_float_window`; else keep the existing split path.
   - `nvim_win_get_config(win)`: return the float config as a neovim-shaped map
     (`relative`, `anchor`, `row`, `col`, `width`, `height`, `zindex`,
     `focusable`, `border`), or `{ relative = "" }` for a tiled window.
   - `nvim_win_get_position(win)`: `[row, col]` of the window's top-left in the
     windows area (works for tiled and float; the test oracle for geometry).

**No-stub discipline.** Per the project rule, an unsupported float option must
fail **loud**, not be silently dropped: a `relative` value nxvim doesn't position
yet (`"mouse"`, `"laststatus"`, `"tabline"`) raises with the name, rather than
quietly falling back to `editor`. Supported set in Phase 1: `editor`, `win`,
`cursor`.

**Tests** (`crates/nxvim-server/tests/windows.rs`, black-box).
- `nvim_open_win(buf, true, {relative="editor", row=2, col=5, width=20, height=4})`
  then `nvim_win_get_position` is `[2,5]` and the *tiled* window's rect is
  **unchanged** (a float steals no space — the regression guard for the model).
- `anchor="NE"` with `col=10` puts the float's **right** edge at col 10
  (right-corner anchor math).
- `relative="cursor"` opens at the cursor cell offset; move the cursor, open
  another, assert it tracks (positioned at open time against the live cursor).
- `relative="win"` against a split positions inside that window's rect.
- `nvim_list_wins` includes the float **after** the tiled windows; two floats
  come back ordered by `zindex`.
- `nvim_set_current_win(float)` focuses it (`nvim_get_current_win` returns it);
  `nvim_win_close(float)` removes it and the tiled layout is restored.
- An off-screen `col` clamps into the windows area (no panic, rect on-screen).
- A `relative="mouse"` raises a clear error (no-stub guard).

**Done when.** A float opened via `nvim_open_win` is a real window: positioned by
`relative`/`anchor`/`row`/`col`, queryable through `get_config`/`get_position`/
`list_wins`, focusable, closable, and provably non-disturbing to the tiled tree.
It is **not yet painted** — `architecture.md` and the roadmap say so explicitly so
the boundary isn't mistaken for a bug. The `Window.float`/`FloatConfig`/`floats`
list/second-pass machinery exists and is exercised by the geometry tests above.

**Depends on.** Nothing (builds on the existing `WindowTree`/`layout`/RPC).

---

## Phase 2 — Painting floats: the `View` projection + the TUI overlay ✅

**Phase 2 implementation note.** Landed as designed: `WindowLayout` and
`WindowView` gained `floating`/`border`/`title`; `window_layouts` now appends the
floats (z-ordered) after the tiled leaves; `window_view` insets a bordered
float's content (`rect` minus one cell each side) so its `lines`/gutter/status
align with the painted box; `redraw.rs` serializes the three fields and the TUI
parses them. `render()` splits into a tiled pass + an on-top float pass
(`Clear` → bordered `Block` with `border_type`/`title` → reuse `render_window`
into the inner area), with the pmenu staying the highest layer; a focused float
owns the terminal cursor (`text_inner_rect` insets by the border too).
`BorderStyle::Solid` maps to ratatui `QuadrantInside`. **One deliberate
divergence from the literal phase split:** the *minimal* Lua open-float
(`vim.api.nvim_open_win` float form → `WindowOp::OpenFloat` → `effects.rs` →
`open_float_window`, with loud validation in `nvim_api.lua`) was pulled forward from
Phase 3, because the project's example-config convention requires a *runnable*
`examples/floats/` and a pure-RPC float can't be opened from an `init.lua`. The
*rest* of the config surface — `nvim_win_set_config`/`get_config` fidelity, the
`nx._wins` float mirror, split↔float conversion — stays Phase 3. Coverage: 4
screen tests in `crates/nxvim/tests/screen.rs` (opacity/`Clear`, border+title,
zindex-over-creation-order, focused-float cursor) + 1 that boots the shipped
`examples/floats/` config and asserts the startup float paints; 2 Lua-path tests
in `windows.rs` (open-from-Lua round-trip, loud border rejection).

## Phase 2 (original plan) — Painting floats: the `View` projection + the TUI overlay

**Goal.** A float **appears on screen**: drawn on top of the tiled windows at its
rect, with an optional border and title, its own gutter/text/status inside, and —
when focused — the terminal cursor placed in it. The completion pmenu still floats
above everything. This is the phase where `nvim_open_win` becomes visible.

**Why.** Phase 1 made floats real in the model; rendering is a genuinely separate
concern living in a different crate (`nxvim-tui`), with the pmenu already proving
the overlay primitive (`Clear` + bordered `Block`). Splitting it out keeps each
phase end-to-end testable: Phase 1 tested geometry over RPC, Phase 2 tests pixels
over the screen harness.

**Scope (files).**
- `crates/nxvim-core/src/view.rs` — `WindowView` gains `floating: bool`,
  `border: BorderStyle`, `title: Option<String>`, `zindex: u32`; `window_view`
  populates them from the `WindowLayout`. `View.windows` stays one list (tiled +
  floats), already z-ordered by `window_layouts`.
- `crates/nxvim-server/src/redraw.rs` — serialize the new per-window fields into
  each `windows[i]` sub-map.
- `crates/nxvim-tui/src/view.rs` — parse `floating`/`border`/`title`/`zindex` on
  each `WindowView`.
- `crates/nxvim-tui/src/render.rs` — the overlay pass.

**Approach.**

1. **Project the float fields.** `window_view` copies `floating`/`border`/`title`/
   `zindex` through; a tiled window is `floating=false, border=None`. The text
   body of a *bordered* float is the rect inset by one cell on each side (the core
   already sized `FloatConfig.width/height` as the outer box in Phase 1, so the
   inner `lines` slice is `height-2` rows — make `window_view` honor the inset so
   `lines`/`numbers`/`selection` align with what the client draws).

2. **Render order (the heart of it).** In `render()`, after painting the tiled
   windows and `render_separators`, iterate `view.windows.filter(floating)` **in
   z-order** (already sorted) and for each:
   ```rust
   frame.render_widget(Clear, area);                 // opaque — hide what's under
   if border != None { let block = bordered(border, title); 
       frame.render_widget(block, area); area = inner; }
   render_window(frame, area, win, view, None);      // reuse the existing painter
   ```
   `render_window` already paints gutter + text + the per-window status line into
   any rect — reuse it verbatim for the float's inner area. Keep the pmenu pass
   **last** so completion still sits above floats (its z is effectively ∞).

3. **The focused cursor.** `render()` currently places the terminal cursor in the
   focused window. A focused float is just another `focused` window in the list —
   the existing "draw the cursor in the focused window" path already handles it
   once the float is painted; verify the cursor lands inside the float's inner
   (post-border) text area, not the tiled window beneath.

4. **Border styles.** A small `BorderStyle` → ratatui `BorderType` map
   (`Single`→`Plain`, `Rounded`→`Rounded`, `Double`→`Double`, `Solid`→`QuadrantInside`
   or the nearest); `None` skips the block. `title` renders on the top border
   (ratatui `Block::title`).

**Tests** (Tier-2 screen tests — `crates/nxvim-server/tests/screen.rs` and the
example below; follow the take-latest redraw helper rule).
- Open a float over text; the float's text cells appear at its rect and the cells
  it covers no longer show the underlying buffer (the `Clear` proof).
- A bordered float draws its border glyphs at the rect edges and the title on the
  top row; the inner text is inset by one cell.
- Two overlapping floats: the higher `zindex` paints over the lower (assert a cell
  in the overlap belongs to the top float).
- A focused float gets the terminal cursor inside its inner area.
- **`examples/floats/`** — a runnable `init.lua` that opens a bordered float
  ("hello from a float") over a sample buffer, plus the sample file, verified
  end-to-end (the project's example-config convention). Confirm by running the
  TUI, not just by the test.

**Done when.** A float opened via `nvim_open_win` is drawn on top of the tiled
layout at its rect, with border/title, its own gutter/text/status, and the cursor
when focused; overlapping floats respect `zindex`; the pmenu still floats above;
and `examples/floats/` shows it working in the real TUI. `architecture.md`'s
*Windows* and *View protocol* sections are updated to describe the float layer.

**Depends on.** Phase 1 (the model + positioned rects + `window_layouts`).

---

## Phase 3 — The full config surface: `set_config`/`get_config`, split↔float, the Lua API ✅

**Phase 3 implementation note.** Landed as designed. The merge ("absent keys are
unchanged") lives in **one place** — `Editor::set_window_config(id, spec)` in
`editor.rs`, taking a partial `WindowConfigSpec` (all-`Option` fields plus a
`make_tiled` flag for the `relative = ""` form) — so both callers send only the
keys they were given: the `nvim_win_set_config` RPC (`dispatch.rs::parse_window_config`)
and the `WindowOp::SetConfig` drain (`effects.rs`). `set_window_config` does three
things by case: move/resize/restyle a float (merge over its live `FloatConfig`);
**tiled → float** (`remove_leaf` detaches it from the tree, a sibling expands, the
window joins `floats` — refused for the last tiled window via an `echo`); and
**float → tiled** (`convert_float_to_tiled` clears `float` and `split_leaf`s it
back into the tree as a horizontal split of the focused window). The `nx._wins`
mirror gained the float fields: `WindowMirror` became a struct with an
`Option<FloatMirror>` (the placement pre-formatted into the strings
`nvim_win_get_config` returns, so nxvim-lua never sees the core's enums —
`effects.rs::float_mirror` translates), serialized as a nested `float` table by
`runtime.rs::set_buf_mirror`. `nvim_api.lua` gained `nvim_win_get_config` (reads
`w.float` off the mirror) and `nvim_win_set_config` (loud-validates the enumerated
fields, queues `_win_set_config`, and write-throughs `w.float` so a `get_config`
later in the same chunk agrees); `nvim_open_win`'s write-through now seeds the
float record too. **Deliberate divergences / limitations:** (1) a tiled → float
conversion that omits `width`/`height` seeds them from the window's current tiled
rect (its on-screen size), rather than erroring; (2) `set_config` cannot *clear* a
title from Lua (a `nil` field is indistinguishable from absent — the RPC path can,
via an empty `title` string); (3) `convert_float_to_tiled` always makes a
*horizontal* split of the focused window (the simplest "make it a normal window"
placement neovim's docs leave unspecified). Coverage: 5 RPC tests in `windows.rs`
(move-keeps-absent-fields, resize, tiled↔float round-trip, same-chunk get_config
write-through, Lua `nvim_win_set_config`) + 3 example-driven screen tests in
`crates/nxvim/tests/screen.rs` (the shipped `examples/floats/` `:FloatMove` /
`:FloatGrow` / `:FloatToSplit` commands, which exercise `set_config`/`get_config`
and the split conversion through the real client). The `examples/floats/init.lua`
config grew those three commands.

## Phase 3 (original plan) — The full config surface: `set_config`/`get_config`, split↔float, the Lua API

**Already landed in Phase 2 (don't redo):** `WindowOp::OpenFloat`, the
`nx._open_float` bridge, `nvim_api.lua`'s `nvim_open_win` float branch (with loud
validation), and the `effects.rs` drain into `open_float_window`. So the **open**
path from Lua works. Phase 3 is the *remaining* surface below: `set_config`
(move/resize/restyle + split↔float conversion), `get_config` reading the live
`nx._wins` mirror, and seeding the float fields into that mirror so a
`get_config` *within the same chunk* sees a just-opened float.

**Goal.** Floats are **dynamic** and reachable from Lua. `nvim_win_set_config(win,
config)` moves/resizes a float, changes its border/title/zindex, and converts a
tiled window into a float (and a float back into a split); `nvim_win_get_config`
round-trips full fidelity; `vim.api.nvim_open_win` builds the float config and the
`nx._wins` mirror reflects it so reads within the same `:lua` chunk see it.

**Why.** Phase 1–2 made a float openable and visible from the RPC layer; the
*plugin* surface is `vim.api` + `set_config` (every float-using UI repositions
or resizes after open — a picker resizing on `VimResized`, hover moving with
the cursor). This phase makes floats a first-class Lua citizen, following the
established **"Lua queues, core mutates"** flow windows already use.

**Scope (files).**
- `crates/nxvim-lua/src/ops.rs` — `WindowOp::OpenFloat { ... }` and
  `WindowOp::SetConfig { win, ... }` (the float-config payload as plain fields, no
  Lua types, like the existing `Open`).
- `crates/nxvim-lua/src/prelude/nvim_api.lua` — `nvim_open_win` builds the float config
  and write-throughs it into `nx._wins`; new `nvim_win_set_config`/
  `nvim_win_get_config` against the mirror + a queued op.
- `crates/nxvim-server/src/effects.rs` — drain `OpenFloat`/`SetConfig` into
  `open_float_window` / a new `Editor::set_window_config`; the mirror push
  (`effects.rs` already builds `nx._wins`) gains the float fields.
- `crates/nxvim-server/src/dispatch.rs` — `nvim_win_set_config` RPC entry.
- `crates/nxvim-core/src/editor.rs` — `set_window_config(id, config)`: reposition
  a float, or **convert** (tiled→float removes the id from `root` and collapses
  its split, then adds it to `floats`; float→split re-inserts it into the tree as
  a split of the current window). Relayout after.

**Approach.**

1. **`WindowOp::OpenFloat` / `SetConfig`.** The split-form `Open` stays; add the
   float ops carrying the resolved config. `effects.rs` drains them after the Lua
   chunk (the same drain point as `Open`), calling `open_float_window` /
   `set_window_config`. `nvim_open_win`'s split-vs-float decision moves into
   `nvim_api.lua` (which op to queue), matching where `vertical` is decided today.

2. **The mirror (`nx._wins`).** `effects.rs` pushes `nx._wins` before each
   chunk; add the float fields (`relative`, `anchor`, `row`, `col`, `width`,
   `height`, `zindex`, `focusable`, `border`) so `nvim_win_get_config` reads the
   live value. `nvim_api.lua`'s `nvim_open_win` write-through (it already seeds a
   `nx._wins[id]` entry) seeds the float fields too, so a `get_config` *later in
   the same chunk* sees the just-opened float before the op drains — the exact
   write-through pattern already there for the split form.

3. **`nvim_win_set_config`.** `nvim_api.lua` validates + queues `WindowOp::SetConfig`
   and write-throughs the mirror. Core's `set_window_config`:
   - **float → moved/resized float:** overwrite the `FloatConfig`, `relayout()`.
   - **tiled → float:** the window leaves `root` (collapse its parent split, the
     `remove_window` neighbor-expand logic, but *keep the window* — move it to
     `floats` with the given config) and `relayout()`.
   - **float → tiled (split):** `relative=""` on a float re-tiles it; insert it
     into `root` as a split of `current` (the `split_leaf` path), drop its
     `float`. Define the split direction/placement (neovim drops it to a normal
     window in the current layout — simplest: a horizontal split of the focused
     window).

4. **`nvim_win_get_config`** gains full fidelity (Phase 1's getter extended with
   `border`/`title` once Phase 2 added them).

**Tests** (`crates/nxvim-server/tests/windows.rs`).
- `nvim_win_set_config(float, {relative="editor", row=0, col=0})` moves it;
  `get_position` reflects the move; `get_config` round-trips the new values.
- `set_config` resize changes the painted inner area (cross-check via a screen
  assertion or the inner `lines` length).
- Convert a tiled window to a float (`set_config` with a `relative`): the tree
  collapses (sibling expands), the window survives as a float; convert back
  (`relative=""`): it re-tiles.
- A `:lua` chunk that opens a float and then `nvim_win_get_config`s it **in the
  same chunk** sees the float (mirror write-through), before the op drains.
- `vim.api.nvim_open_win(buf, true, {relative="editor", …})` from Lua lands a
  visible float (drive via `nvim_exec_lua`, assert via `list_wins` +
  `get_position`).

**Done when.** `nvim_win_set_config`/`get_config` work and round-trip; a float can
be moved, resized, restyled, and converted to/from a split; `vim.api.nvim_open_win`
float form works from Lua with mirror write-through; the `WindowOp::OpenFloat`/
`SetConfig` ops drain through `effects.rs` like every other window mutation.

**Depends on.** Phases 1–2 (the model + the paint; `border`/`title` exist to
configure).

---

## Phase 4 — Autocmds, edge semantics, and hardening ✅

**Phase 4 implementation note.** Landed as designed, mostly as small edits to the
existing `WindowTree`/`Editor` edge logic in `editor.rs` (no new types). (1)
**Autocmds** needed *no* new code — the `lifecycle.rs` diff already keys off
`window_ids()` (which spans floats) and a rect snapshot, so `WinNew`/`WinEnter`/
`WinClosed` fire for floats and `WinResized` fires when `set_config` changes a
float's rect; Phase 4 just adds the coverage. (2) **`:q` / last-window rule:**
`ex_quit` now branches on whether the focused window is a float (close just the
float, never quit) and otherwise counts **tiled** windows (`leaves().len()`) for
the last-window test, and `remove_window`'s guard became "refuse the last *tiled*
window" (`!is_float && leaves().len() <= 1`) so a tiled window can't be closed
down to floats-only and a float is always closable. (3) **Parent-close:**
`remove_window` collects the target plus every float transitively anchored to it
(`relative="win"`) into a `victims` list and removes them together. (4)
**`:only`:** `only_window` clears `floats` alongside the tiled retain, and refuses
to run from a focused float (neovim's E5601). (5) **Focusable focus cycle:**
`focus_cycle` (`<C-w>w`/`<C-w>W`) now appends focusable floats (z-order) after the
tiled leaves; the spatial `focus_dir` (`<C-w>h/j/k/l`) stays tiled-only. (6)
**Resize re-clamp** needed no code — `editor.resize` → `relayout` → the float pass
already re-clamps every layout; Phase 4 adds the test. **Deliberate divergence
from the literal plan:** the spatial `<C-w>h/j/k/l` does **not** descend into
floats (only the `<C-w>w` cycle does), since a float overlapping the tiled grid
has no well-defined direction — neovim's directional moves likewise stay in the
tiled layout. Coverage: 8 edge tests in `crates/nxvim-server/tests/windows.rs`
(float `:q`, last-tiled quit, `:only` closes floats, parent-close cascade, cycle
includes/skips focusable, last-tiled-close refusal, resize re-clamp) + 2 autocmd
tests in `autocmds.rs` (float WinNew/WinEnter/WinClosed, `set_config` WinResized)
+ a `:FloatNote` non-focusable demo command and an EDGE BEHAVIORS note in
`examples/floats/init.lua`.

## Phase 4 (original plan) — Autocmds, edge semantics, and hardening

**Goal.** Floats behave correctly at the **boundaries**: the window autocmds fire
for them, `:q`/`:qa`/`:only`/`<C-w>` do the right thing with floats present, focus
honors `focusable`, a terminal resize re-clamps floats, and the degenerate cases
(a `relative="win"` parent closing, a zero-size or off-screen float, zindex ties)
are defined rather than accidental.

**Why.** The happy path (open, paint, configure) is Phases 1–3; the behaviors that
make floats *trustworthy* — and that real configs hit immediately (`<Esc>` to
close a float, `:q` not quitting the editor because a hover float is open, a
plugin's `WinClosed` cleanup) — are concentrated here. Doing them as one hardening
pass keeps the earlier phases focused and gives the edge rules a single home.

**Scope (files).**
- `crates/nxvim-server/src/effects.rs` — the `emit_lifecycle_events` diff already
  fires `WinNew`/`WinEnter`/`WinLeave`/`WinClosed`; confirm it spans floats and add
  `WinResized` on a `set_config` size change.
- `crates/nxvim-core/src/editor.rs` — the `:q`/`:only`/`<C-w>o`/`<C-w>w`/focus
  rules with floats; `focusable`; terminal-resize re-clamp; parent-close handling.

**Approach & the semantics to pin down.**

1. **Autocmds.** A float is a window, so the existing lifecycle diff should already
   emit `WinNew` on open, `WinEnter`/`WinLeave` on focus in/out, `WinClosed` on
   close — verify and test, don't reinvent. Add `WinResized` when `set_config`
   changes a float's size (the diff currently keys focus/open/close; extend it to
   notice a rect change, as the tiled `:resize` path does).

2. **`:q` / `:qa` / the last-window rule.** Define precisely (matching neovim):
   - `:q` on a **focused float** closes *just the float* (never quits the editor),
     then focuses the previous window.
   - The **"last window" E37/quit** rule counts **tiled** windows only — a float
     open over a single split does not make `:q` "close a window and stay"; closing
     the last *tiled* window still quits. (Floats are not a reason to keep the
     editor alive.)
   - `:qa`/`:wqa` quit regardless (unchanged).

3. **`:only` / `<C-w>o`.** Closes other tiled windows **and all floats** (neovim's
   `:only` closes floats too). `<C-w>c` / `nvim_win_close` on a float closes that
   float.

4. **Focus.** `<C-w>w`/`<C-w>W` cyclic focus and `<C-w>h/j/k/l` spatial focus
   **skip `focusable=false` floats**; `nvim_set_current_win` on a non-focusable
   float is allowed (explicit), but the `<C-w>` cycle is not. Document which
   commands include floats (neovim: `<C-w>w` includes focusable floats).

5. **Re-clamp on resize.** A terminal resize re-runs `layout()`; the float pass
   re-clamps `editor`-relative floats into the new windows area (a float that was
   at the old right edge stays on-screen). `win`/`cursor`-relative floats
   reposition against their (possibly moved) anchor.

6. **Degenerate cases (define, don't crash).**
   - A `relative="win"` float whose **parent window closes**: neovim closes the
     float. Implement that (on `remove_window`, also close floats anchored to it).
   - A float clamped to **zero usable inner size** (tiny terminal): keep it valid
     (min 1×1 inner) or hide it — pick one, test it, document it.
   - **zindex ties** break by creation order (the `(zindex, id)` sort from Phase 1).

**Tests** (`crates/nxvim-server/tests/windows.rs` + `autocmds.rs`).
- Opening a float fires `WinNew` + `WinEnter`; closing it fires `WinClosed`;
  `set_config` resize fires `WinResized` (extend the existing window-autocmd
  coverage).
- `:q` on a focused float closes the float and the editor stays alive; `:q` with
  only floats over a single split still quits on the last tiled window.
- `:only` with a float open closes the float; `<C-w>w` skips a `focusable=false`
  float but lands on a focusable one.
- Closing a `relative="win"` float's parent closes the float.
- Terminal resize keeps an `editor`-relative float on-screen (re-clamp).

**Done when.** Floats fire the window autocmds (including `WinResized`); `:q`/`:qa`/
`:only`/`<C-w>` and focus have defined, tested behavior with floats present;
`focusable` is honored by the focus cycle; resize re-clamps; the degenerate cases
are defined and tested. `architecture.md`'s *Windows* section documents the float
semantics and the roadmap's floating-windows item is retired.

**Depends on.** Phases 1–3.

---

## Suggested order & scoreboard

`1 → 2 → 3 → 4`. Phase 1 is the model + geometry (a float is real and queryable);
Phase 2 makes it visible; Phase 3 makes it dynamic and Lua-reachable; Phase 4
makes it behave at the edges. After Phase 2 a config can open a *visible* float;
after Phase 3 a plugin's `nvim_open_win`/`nvim_win_set_config` works; after Phase 4
floats are trustworthy enough for hover/completion-doc/`vim.ui` plugins.

The running scoreboard is the float API surface a real plugin exercises:
`nvim_open_win{relative}` (P1 model / P2 paint) → `nvim_win_set_config` /
`get_config` (P3) → border/title/zindex (P2/P3) → `WinClosed`/`WinResized` +
`:q`/focus semantics (P4). Re-run a float-using config (or the
`examples/floats/` config) after each phase; the gap shrinks.

---

## Testing appendix — observing floats in a black-box harness

The conventions are unchanged (`architecture.md` → *Testing philosophy*): **no
unit tests**; drive a real server over RPC and assert on observable state
(`crates/nxvim-server/tests/windows.rs`, helpers `start`/`feed`/`lines`/`cursor`;
screen assertions follow the **take-latest redraw** rule — drain to the most
recent `redraw`, never the first, or the test flakes under load). Two oracles
cover the float surface:

1. **Geometry over RPC (Phases 1, 3).** `nvim_win_get_position` / `get_config` /
   `list_wins` are the geometry oracle — assert a float's `[row, col]`, its config
   round-trip, and its place in the window list **without rendering anything**.
   This is why Phase 1 is fully testable before a single float pixel exists.

2. **Pixels over the screen harness (Phases 2, 4).** The Tier-2 screen tests
   assert on the projected frame: a float's cells land at its rect, the `Clear`
   makes it opaque over the buffer beneath, the higher `zindex` wins an overlap,
   the border glyphs and title appear, and the focused cursor sits in the float's
   inner area. Use the take-latest redraw helper (the float frame is persistent
   state, so "latest" is correct).

Every feature ships a runnable `examples/floats/` config + sample, verified by
actually running the TUI — not just green tests — per the project's
example-config convention.

---

## Risks & non-goals

- **Never put a float in the `Node` tree.** A float is an id in `windows` that no
  `Leaf` references; the tiling math touches only the tree. If a change pressures
  you to make a float a tree leaf, it is wrong — re-derive from *The one
  constraint*.
- **No silent option drops.** An unsupported `relative` / `border` / config key
  fails **loud** with its name (project rule), never a quiet fallback that makes a
  mispositioned float look intentional.
- **`relative="mouse"` and `bufpos` are out of scope** (no mouse model yet; the
  pmenu/`vim.ui` paths don't need them). Raise on them; add when a consumer
  demands it, the architecture doc's "grows as plugins demand it" rule.
- **Cursor-relative floats are positioned at open / `set_config`, not live.** A
  float does not *follow* the cursor every motion (neovim doesn't either without a
  plugin re-positioning it); a plugin re-`set_config`s it. Don't build a
  cursor-tracking loop.
- **`style="minimal"`, `footer`, `title_pos`, `hide`, `noautocmd`** are fidelity
  knobs to add incrementally — land the core (`relative`/`anchor`/`row`/`col`/
  `width`/`height`/`zindex`/`focusable`/`border`/`title`) first; note the rest as
  known refinements, not gaps.
- **The pmenu stays bespoke for now.** The completion popup is its own client
  chrome and already works; reimplementing it *as* a float is a tempting unify but
  out of scope — leave it as the highest layer above floats. (A future cleanup may
  fold it in once floats are proven.)
- **Window-local options** (`wrap`, `cursorline`) remain the separate pending item
  the windows work named; floats share the global options like tiled windows do
  until that lands.
