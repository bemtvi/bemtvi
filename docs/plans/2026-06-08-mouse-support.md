# Mouse support — server-owned hit-testing plan

> **Status: IN PROGRESS.** Phases 0–3 have landed: the `nvim_input_mouse` RPC,
> the `MouseEvent` core type, the four `'mouse*'` options, server-side hit-testing,
> left-click to place the cursor + focus the window, left-drag to make a charwise
> Visual selection (TUI forwards press/drag/release), and multi-click (double =
> word, triple = line) timed by `'mousetime'` against an injectable server clock.
> Phases 4–8 remain. This is a design spec and a phased build order; the ✅/⬜
> markers below track what is built.

## Status legend

- ✅ done   🚧 in progress   ⬜ not started

---

## Why this document exists

nxvim today captures terminal mouse events but only wires them to **client-owned
chrome** — the message panel and the completion pmenu. The TUI hit-tests those
overlays itself and calls bespoke RPCs (`nxvim_panel_click`,
`nxvim_complete_select`/`_accept`), and the scroll wheel only nudges the panel /
pmenu. The actual **text area is mouse-dead**: clicking a buffer does nothing,
there is no drag-to-select, no wheel-scroll of a window, no split resize, no
tabline click.

Wiring the text area is not a matter of adding a few client branches. The
question that decides the whole shape of the feature is **where screen→buffer
hit-testing lives** — and the codebase has already answered it for every other
feature:

> *The server still owns which cells are in a group … the client is a dumb
> truecolor renderer.* — [architecture.md → View protocol](../architecture.md#view-protocol-ui)

So mouse follows the same split, and it happens to be exactly how real neovim
works too (single global grid, `grid = 0`, frame-tree hit-test server-side —
`vendor/neovim/src/nvim/mouse.c:mouse_find_win_inner`). **The client forwards a
raw screen cell; the server owns everything after that.** This keeps every front
end (TUI now, GUI later) identical, and keeps the geometry knowledge — gutter
width, tab expansion, horizontal scroll, wrap, folds, split layout — in the one
place that already computes it.

---

## The shape: one RPC, one core event, server-side hit-test

### Wire entry point — `nvim_input_mouse`

Add neovim's API verbatim so existing tooling/tests map 1:1
(`vendor/neovim/runtime/doc/api.txt`):

```
nvim_input_mouse(button, action, modifier, grid, row, col)
  button   "left" | "right" | "middle" | "wheel" | "move" | "x1" | "x2"
  action   buttons: "press" | "drag" | "release"
           wheel:   "up" | "down" | "left" | "right"
           move:    ignored
  modifier "C" / "S" / "A" / "D" in any order, "-" optional ("C-S", "cs", "CS")
  grid     0  (nxvim is single-grid; always 0 — the server hit-tests)
  row,col  zero-based screen cell, same coordinate space as redraw
```

This is the **primary** path and the one tests drive. The legacy notation forms
(`<LeftMouse>`, `<2-LeftMouse>`, `<ScrollWheelUp>`, `<C-LeftMouse>`, …) are
parsed by `nvim_input` so that **keymaps** can bind/remap them, but note the
neovim data-model fact: the notation keycode carries **no coordinates** — coords
ride a separate out-of-band token. In nxvim the encoded `nvim_input_mouse` call
*is* that out-of-band channel, so we don't reconstruct the deprecated
`<LeftMouse><col,row>` byte form. Notation without coords resolves against the
server's last-known mouse position (what real neovim does via the `mouse_row` /
`mouse_col` globals).

> **Wheel-direction trap to not re-introduce.** neovim's API inverts the names:
> `action = "up"` maps to the internal `KE_MOUSEDOWN` (scroll content up). We
> match the **observable** behavior (`"up"` scrolls toward the top of the
> buffer); the internal constant name is irrelevant since we don't share its
> enum.

### Core event — `MouseEvent` in `nxvim-core`

A new pure value type flows into `Editor`, parallel to how parsed `Key`s do
(`nxvim-core/src/input.rs`). It stays I/O-free and synchronous per the
[`nxvim-core` purity rule](../CLAUDE.md):

```rust
pub struct MouseEvent {
    pub button: MouseButton,   // Left | Right | Middle | Wheel | Move | X1 | X2
    pub action: MouseAction,   // Press | Drag | Release | WheelUp | WheelDown | WheelLeft | WheelRight
    pub modifiers: Mods,       // ctrl/shift/alt/super — reuse the Key modifier bits
    pub row: usize,            // global screen cell (grid 0)
    pub col: usize,
    pub stamp_ms: u64,         // server-stamped receive time, for multi-click — see Phase 3
}
```

The server (`dispatch.rs`) translates the RPC args into this, stamps `stamp_ms`
from an **injectable clock** (Phase 3), and hands it to a new
`Editor::mouse(event)` entry point alongside the existing `Editor::input`.

### Options (new, with neovim defaults)

Register four global options (`nxvim-core/src/editor/options.rs`), each gating
or tuning behavior — defaults match neovim exactly, including the two traps:

| option | default | role |
|---|---|---|
| `'mouse'` | `"nvi"` | per-mode enable: `n`/`v`/`i`/`c`/`a`/`r`/`h`. A gesture is acted on only if the current mode's flag (or `a`) is present. **Default is `nvi`, not `a`** — cmdline mouse is off out of the box. |
| `'mousemodel'` | `"popup_setpos"` | right-click semantics; decides whether the extend-gesture is right-click (`extend`) or `<S-LeftMouse>` (`popup*`). |
| `'mousescroll'` | `"ver:3,hor:6"` | wheel step: lines per vertical notch, columns per horizontal notch (`0` disables a direction). |
| `'mousetime'` | `500` | max ms between presses to count as a multi-click. |

When `'mouse'` doesn't enable the current mode, the event is a **silent no-op**
— this is the one place where "silent" is correct (it's vim-faithful, not a
hidden failure). An *unrecognized* button/action, by contrast, fails loud per
the [no-silent-stubs rule](../CLAUDE.md).

---

## The hit-test pipeline (the heart of it)

Global screen cell `(row, col)` → buffer position, entirely server-side. This is
the inverse of what `Editor::view(w, h)` already computes going the other way, so
the work is **factoring the layout out of `view()` so both directions share it**:

```
(row,col) global cell
   │  ① subtract chrome rows the client reserves
   │     (tabline at top if shown; cmdline/message + panel at bottom)
   ▼
windows-area cell
   │  ② walk the split tree (frame-tree hit-test, like mouse_find_win_inner):
   │     for each WindowView rect {x,y,width,height}, the point lands in exactly
   │     one tiled window — or on a `separator` (Phase 5) or the tabline (Phase 6).
   │     Floats are tested first, top-down, and opt out if not focusable.
   ▼
(window, win-relative row, win-relative col)
   │  ③ row  → buffer line:  topline(window) + rel_row   (nowrap; wrap = Phase 8)
   │     col  → subtract number_width gutter; if col is in the gutter it's a
   │            statuscolumn click (lands on the line, no horizontal move)
   ▼
(buffer line, screen col within text)
   │  ④ reverse virtcol: walk the line's graphemes accumulating display width
   │     (tabs→tabstop, wide chars via unicode-width) + leftcol horizontal
   │     scroll, until reaching screen col → byte offset, rounding to the
   │     nearest grapheme boundary (vim rounds a between-cells click left)
   ▼
(buffer line, byte col)  → set cursor, focus the window
```

Steps ③–④ are the exact inverse of the forward `virtcol`/`leftcol`/`number_width`
math the projection already does (architecture.md → *Text model* / *View
protocol*). The cleanest implementation extracts a `Layout` value from
`Editor::view` and adds `Layout::hit(row, col) -> Option<Hit>`, where `Hit` is
one of `Text { win, line, col }`, `Gutter { win, line }`, `Separator { … }`,
`Tabline { tab }`, `StatusLine { win }`, `BelowBuffer { win }`. Every phase below
consumes `Hit`.

---

## Phase 0 — RPC + event plumbing + options (no behavior) ✅

**Goal.** `nvim_input_mouse` exists, parses cleanly into a `MouseEvent`, reaches
`Editor::mouse`, and is gated by a real `'mouse'` option. No gesture does
anything yet except fail loud if the button/action is unknown.

**Why first.** Establishes the type and the wire contract so every later phase is
a self-contained behavior addition. Tests can already assert the gate (a click
with `mouse=` empty is a no-op; an unknown action errors).

**Scope.** `nxvim-server/src/dispatch.rs` (RPC), `nxvim-core/src/input.rs`
(`MouseEvent`, button/action/modifier parse), `nxvim-core/src/editor/options.rs`
(the four options), `nxvim-core/src/editor/mod.rs` (`Editor::mouse` stub that
matches on event and routes to per-phase handlers).

**Approach.** Map `(button, action)` to the `MouseEvent` enums; reject unknown
pairs with a named error. Add the options with neovim defaults + validation.
`Editor::mouse` checks the `'mouse'` gate for the current mode, then matches —
every arm `vim._notimpl`-style loud-fails for now.

**Test.** `nvim_input_mouse("left","press","",0,0,0)` with `mouse=` empty → no
state change; with a bad action → RPC error; option get/set round-trips.

**Landed as:** `MouseButton`/`MouseAction`/`MouseEvent` + `MouseEvent::parse`
(`nxvim-core/src/input.rs`); the four options in `nxvim-core/src/options.rs`
(parse/apply in `editor/options.rs` + `editor/windows.rs`); the
`nvim_input_mouse` RPC in `nxvim-server/src/dispatch.rs`. Tests:
`nxvim-server/tests/mouse.rs` (`mouse_gate_disabled_ignores_click`,
`unknown_mouse_action_errors`, `mousetime_option_roundtrips`).

---

## Phase 1 — Hit-test + left click places cursor & focuses window ✅

**Goal.** Left-press in any tiled window moves the cursor to the clicked
character and focuses that window (focus follows click). Clicking past a line's
end lands on the last char (or EOL); clicking below the last line lands on the
last line. The gutter is click-through to the line.

**Why.** This is the load-bearing slice — it builds the whole hit-test pipeline
that every other gesture reuses. After this, "click to move the cursor" works and
operators-then-click (`d`+click) deletes to the clicked spot, because the click
just sets cursor position like any motion.

**Scope.** Factor `Layout` out of `Editor::view` (`view.rs` /
`editor/windows.rs`); add `Layout::hit`; implement nowrap reverse-virtcol;
`Editor::mouse` Left/Press arm sets cursor + `set_current_win`.

**Approach.** Extract the per-window rect computation so hit-testing and
projection can't drift. Reverse-virtcol mirrors the forward `cursor_screen_col`
calc. Reuse `set_window_cursor` (`windows.rs`) for the placement; focus via the
existing current-window switch so saved cursor/scroll restore correctly.

**Test.** Two vertical splits; `nvim_input_mouse("left","press",…)` at a cell in
the right split → `nvim_get_current_win` is the right window and
`nvim_win_get_cursor` matches the clicked (line,col). Clicks across tab/wide-char
lines land on the right byte. Click in the gutter → cursor on that line, col 0.

**Landed as:** `Editor::mouse` + `hit_test` + `window_at` (new
`nxvim-core/src/editor/mouse.rs`), reusing `unicode::byte_at_virtcol` for the
reverse-virtcol and `window_scroll`/`window_content_size`/`number_width_for` for
the per-window geometry; the TUI forwards non-overlay left-clicks to
`nvim_input_mouse` (`nxvim-tui/src/lib.rs`). Tests: `left_click_moves_cursor`,
`left_click_past_eol_lands_on_last_char`, `left_click_in_gutter_lands_col0`,
`left_click_respects_tab_expansion`, `left_click_focuses_other_split`. Wrap-aware
hit-testing is deferred to Phase 8 (nowrap only for now).

---

## Phase 2 — Left drag → charwise Visual; release ends the drag ✅

**Goal.** Press-then-drag enters charwise Visual at the press position and
extends the selection to the drag position; release leaves you in Visual (vim
keeps the selection). Gated by `'mouse'` including `v`.

**Why.** "Clicking to select visual mode" — the headline ask. Reuses the Phase-1
hit-test for both endpoints; the only new state is a press-anchor.

**Scope.** `Editor::mouse` Left/Drag + Left/Release arms; a `mouse_anchor`
position on `Editor`; enter `Mode::Visual` with `visual_anchor` = press cell,
`cursor` = drag cell (the selection projection already exists, architecture.md →
*View protocol* `selection`).

**Approach.** First `Drag` after a `Press` sets the anchor (if not already in
Visual) and switches to Visual; subsequent drags just move `cursor`; the existing
selection-span projection paints it for free. Auto-scroll when the drag reaches
the top/bottom edge (reuse the scroll path; integrates with smooth-scrolling).

**Test.** Press at (1,0), drag to (1,5), release → `Mode::Visual`, selection
covers cols 0–5; `y` yanks those chars. Drag across lines selects multiline.

**Landed as:** `mouse_left_press` / `mouse_left_drag` + a `mouse_anchor: Option<Cursor>`
field on `Editor` (holds the press point until the first drag); a press also ends
any active Visual (vim's `<LeftMouse>`), reusing `record_visual_marks`. The TUI
forwards `Drag(Left)` / `Up(Left)` as `drag` / `release`. Tests:
`drag_enters_visual_and_selects`, `drag_selects_across_lines`,
`drag_release_keeps_visual`, `click_without_drag_stays_normal`,
`press_cancels_active_visual`. **Deferred:** auto-scroll when a drag reaches the
window's top/bottom edge (a drag past the viewport doesn't yet scroll to follow);
fold into the Phase 4 scroll work.

---

## Phase 3 — Multi-click: double=word, triple=line ✅ (quad=block deferred)

**Goal.** Repeated presses at the same cell within `'mousetime'` escalate:
double-click selects the word (Visual), triple selects the line (VisualLine),
quad selects blockwise. Drag after a multi-click keeps the unit (word-wise drag).

**Why.** Standard editor selection. It's isolated because it only adds a
click-counter in front of the Phase-2 machinery.

**Scope.** A multi-click state machine on `Editor` (last button/cell/stamp +
count, capped at 4), mirroring `vendor/neovim/src/nvim/os/input.c:check_multiclick`.

**The one real infra decision — the clock.** Multi-click is timing-based, and
core stays pure/sync, so the timestamp must be **injected**, not read inside
core. Plan: the server stamps `MouseEvent.stamp_ms` from a `Clock` it owns
(monotonic millis), and the test harness can set/advance that clock. This keeps
core deterministic and lets a test drive "two clicks 100 ms apart" vs "600 ms
apart" without wall-clock flake — the same discipline the
[redraw-take-latest race](../CLAUDE.md) taught us about not depending on real
timing in tests. Concretely: add a fakeable clock to the server and a harness
helper (e.g. `feed_mouse_at(ms, …)`); core just compares `stamp_ms` deltas.

**Test.** Two presses at the same cell `mousetime-1` ms apart → word selected;
`mousetime+1` ms apart → two separate single clicks. Triple → line. Word-wise
drag after double-click extends by whole words.

**Landed as:** `MouseEvent.stamp_ms` (`nxvim-core/src/input.rs`), stamped by the
server from `Server::mouse_stamp_ms` — the real monotonic clock, or an injectable
`Arc<AtomicU64>` fake (`ServerInit::mouse_clock`) tests advance for determinism.
The multi-click state machine is `MouseSelect` { row, col, stamp_ms, count,
anchor } on `Editor` (`nxvim-core/src/editor/mouse.rs`, replacing the Phase-2
`mouse_anchor` field): `next_click_count` escalates a same-cell press within
`'mousetime'`; `mouse_select_word` reuses `class_span` (the `iw` run) for the
double-click word, `mouse_select_line` enters `VisualLine` for the triple; drags
extend by the chosen unit (`mouse_extend_word` pivots its anchor on a backward
drag, `mouse_extend_line` grows whole lines). Harness: `TestClock` +
`feed_mouse_at(ms, …)` (`nxvim-test-harness`). Tests: `double_click_selects_word`,
`slow_second_click_is_not_a_double`, `second_click_elsewhere_is_not_a_double`,
`triple_click_selects_line`, `word_wise_drag_extends_by_words`,
`line_wise_drag_extends_by_lines`.

**Deferred:** **quad-click → blockwise Visual.** nxvim has no blockwise Visual
mode yet (the [`Mode`](../crates/nxvim-core/src/mode.rs) enum has `Visual` /
`VisualLine` but no `VisualBlock`, and `<C-v>` doesn't start one), so the count
caps at 3 rather than silently faking a block selection. Wire it once a blockwise
Visual mode exists — then bump the `next_click_count` cap to 4 and add a
`SelectAnchor::Block` arm.

---

## Phase 4 — Scroll wheel scrolls the window under the pointer ⬜

**Goal.** Vertical wheel scrolls the window **under the pointer** by `'mousescroll'`
ver lines (default 3) **without moving focus or the cursor**; Shift+wheel scrolls
a page. Horizontal wheel scrolls hor columns under `nowrap`. This replaces the
current panel/pmenu-only wheel for the text area.

**Why.** "Scrollwheel" — the other headline ask. Distinct from cursor motion:
the cursor only follows if the line would scroll off (`scrolloff`). Integrates
with the existing smooth-scrolling `scroll` gesture
([smooth-scrolling plan](2026-05-31-smooth-scrolling.md)).

**Scope.** `Editor::mouse` Wheel arms; hit-test to a window (no focus change);
adjust that window's `top` by the `'mousescroll'` step; emit the transient
`scroll` gesture so smooth-scroll animates. Horizontal: adjust `leftcol`.

**Approach.** Reuse the window-resolution half of the hit-test (we need the
window, not the buffer cell). Scroll an **inactive** window by mutating its saved
scroll position — wheel famously scrolls windows you're not in.

**Test.** Wheel-down over a tall buffer scrolls 3 lines, cursor unmoved, current
window unchanged. Wheel over the *other* split scrolls only that split. Shift =
page. `mousescroll=ver:0` disables vertical.

---

## Phase 5 — Drag the status line / separator to resize splits ⬜

**Goal.** Press on a window separator (vertical divider or a status line acting
as a horizontal divider) and drag → resize the adjacent splits by the drag delta.

**Why.** A natural extension once hit-test recognizes `Separator` / `StatusLine`
hits; the geometry (`separators[]`) is already in the View.

**Scope.** `Hit::Separator` / `Hit::StatusLine` cases; a `resize_drag` mode on
`Editor` capturing which frame edge is grabbed; map drag delta to the split
resize ops (`windows.rs`).

**Test.** Two vertical splits 40/40; press on the separator, drag 5 cells right →
left window 45, right 35. Horizontal split status-line drag resizes heights.

---

## Phase 6 — Tabline click switches tabs ⬜

**Goal.** With a tabline shown, clicking a tab label switches to that tab;
clicking the (optional) close affordance closes it.

**Why.** Tab pages and a statusline already exist
([tab-pages](2026-06-07-tab-pages.md), [statusline](2026-06-07-statusline.md));
this needs per-column **click regions** for the tabline row, the same mechanism
neovim's `tab_page_click_defs` uses.

**Scope.** Have the tabline projection emit a per-column → `tab_id` map (a
click-def table) in the View; `Hit::Tabline { tab }` consults it; `Editor::mouse`
switches the current tab.

**Test.** Three tabs; click the 2nd label's column → `nvim_get_current_tabpage`
is tab 2.

---

## Phase 7 — Right-click model, middle-click paste, insert-mode click ⬜

**Goal.** Fill in the remaining default gestures:
- **Right click** per `'mousemodel'`: `popup_setpos` (default) moves the cursor
  and would pop a context menu (menu UI deferred — for now: move cursor, and if
  inside a selection act on it / outside ends Visual); `extend` extends the
  selection toward the click. Under `popup*`, `<S-LeftMouse>` is the extend
  gesture.
- **Middle click** pastes (`gP` semantics) at the click position, from the
  primary/`*` register (reuse the register + `FakeClipboard` test seam).
- **Insert-mode left click** moves the insert caret without leaving insert
  (gated by `'mouse'` including `i`, which the default `nvi` provides).

**Why.** Rounds out parity; grouped because each is a small arm on the now-built
pipeline. The actual popup **menu** widget is out of scope (track separately).

**Scope.** `Editor::mouse` Right/Middle arms, `'mousemodel'` branch, insert-mode
gate; paste reuses the register path.

**Test.** `mousemodel=extend` + existing Visual + right-click extends to the
click. Middle-click pastes the `*` register at the click. Click in insert mode
moves the caret, mode stays Insert.

**Partially landed — `<S-LeftMouse>` extend.** The shift-click extend half of the
`popup*` model shipped early (it's the natural completion of the selection work):
shift+left-press keeps the existing anchor and moves the live end to the click —
starting a charwise Visual from the cursor when none is active, extending in place
when one is (charwise or linewise, matching the current mode), and a following
plain drag keeps extending from the same anchor. Landed as `Editor::mouse_left_extend`
(`nxvim-core/src/editor/mouse.rs`), gated on `MouseEvent.shift`; the TUI now
forwards crossterm modifiers via `mouse_modifier` (`nxvim-tui/src/lib.rs`) instead
of a hardcoded empty string. Tests: `shift_click_starts_selection_to_click`,
`shift_click_extends_active_visual`, `shift_click_extends_backward`,
`shift_click_extends_linewise_visual`, `drag_after_shift_click_keeps_extending`.
**Still in this phase:** right-click (`'mousemodel'` branch + cursor-move/act),
middle-click paste, insert-mode click.

---

## Phase 8 — Wrap-aware hit-testing ⬜

**Goal.** When `'wrap'` is on, one buffer line occupies several screen rows;
hit-testing maps a screen row to the right (buffer line, within-line screen row)
and the col within the wrapped segment.

**Why.** Deferred to last because the nowrap path (Phase 1) covers the common
case and wrap needs the screen-row→buffer-line walk to account for each line's
wrapped height ([horizontal-scrolling-and-wrap](2026-06-07-horizontal-scrolling-and-wrap.md)).

**Scope.** Extend `Layout::hit` step ③ to consume the same wrapped-row layout the
projection computes; reverse-virtcol per visual segment.

**Test.** A window narrower than a long line; clicking the 2nd visual row of that
line lands on the correct mid-line byte.

---

## Client side (TUI) — runs alongside, mostly Phase 0–1

The TUI already enables crossterm mouse capture (`nxvim-tui/src/lib.rs`,
`MouseCapture`). The change is to **translate and forward** instead of
interpreting:

- Map `crossterm::event::MouseEvent` → `nvim_input_mouse(button, action,
  modifier, 0, row, col)`, with `m.column`/`m.row` as the global cell. Down→press,
  Up→release, Drag→drag, ScrollUp/Down→wheel up/down, plus modifier bits.
- **Forward text-area / split / tabline / separator events to the server**; the
  server hit-tests. The existing **client-owned overlays** (message panel, the
  completion pmenu) keep their current client-side handling *for now*, because
  the client renders those and the server doesn't yet own their geometry —
  migrating them behind `nvim_input_mouse` is a clean follow-up once the server
  models those regions, not a blocker.

**Future GUI** ([architecture.md → Cross-platform & the future GUI](../architecture.md#cross-platform--the-future-gui))
converts pixel → cell (divide by the glyph cell size, with sub-cell precision
discarded) and calls the **same** `nvim_input_mouse`. No server changes — the
whole point of server-side hit-testing is that the GUI is the TUI with a
different rasterizer.

---

## Testing & deliverable conventions

- **Black-box only**, via the shared harness ([CLAUDE.md](../CLAUDE.md)): tests
  drive `nvim_input_mouse` over RPC and assert on `nvim_buf_get_lines` / cursor /
  the `redraw` selection spans. Add a `feed_mouse` / `feed_mouse_at(ms, …)`
  helper to `nxvim-test-harness` so suites share it; the `_at` variant threads
  the fake clock for Phase 3.
- **No silent stubs.** Unimplemented gestures fail loud with their name until
  their phase lands; the `'mouse'`-gated no-op is the deliberate exception
  (vim-faithful, documented).
- **Ship an example.** Per the example-config convention, land an
  `examples/mouse/` config + sample file demonstrating click-to-place,
  drag-select, wheel-scroll, and split-resize, verified end-to-end.

## Open questions (decide before the relevant phase)

1. **Multi-click clock injection (Phase 3).** Confirm the fakeable-server-clock
   approach over alternatives (e.g. an explicit `click_count` only via notation).
   Recommended: server-stamped `stamp_ms` + harness clock control — most faithful,
   keeps core pure.
2. **Panel/pmenu mouse migration.** Keep the current client-side handling, or
   fold those regions into server-side hit-testing too? Recommended: keep for
   now, migrate as a follow-up once the server models overlay geometry.
3. **Context-menu widget (Phase 7).** `popup`/`popup_setpos` imply a real popup
   menu UI. Scope that as its own feature; this plan only does the cursor-move /
   selection-act half of right-click.
