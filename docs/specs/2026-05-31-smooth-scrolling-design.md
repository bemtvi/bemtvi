# Smooth (animated) scrolling — design

**Date:** 2026-05-31
**Status:** Approved design, pending implementation plan

## Goal

Add neoscroll.nvim-style **animated jumps** to bemtvi: the scroll commands
`<C-d>`, `<C-u>`, `<C-f>`, `<C-b>` should *slide* the viewport over a short
duration (~80–160ms) instead of teleporting. The final editor state is identical
to today — only the visual transition is animated.

**In scope:** the four scroll commands above.
**Out of scope (for now):** animating cursor jumps (`gg`/`G`/search), mouse-wheel
scrolling, `scrolloff`, sub-cell pixel-smooth rendering (a future GUI concern),
and any configurability/`:set` toggle (there is no options system yet — it's on
the roadmap; duration is hardcoded until then).

## Decision: client-driven animation

Animation is a **presentation** concern, so it lives in the **client**, driven by
the client's local clock. The server stays authoritative and applies every scroll
**instantly** to its real state, then describes the gesture semantically in a
single `redraw`. The client renders the transition at its own native resolution.

This was chosen over server-driven animation (server emitting a stream of
intermediate frames) for two reasons that matter to bemtvi's roadmap:

1. **Remote seamlessness.** Server-driven animation would send one notification
   per frame; over a laggy link they arrive bunched and janky. Client-driven
   sends *one* redraw and animates against the local clock — smooth regardless of
   round-trip time.
2. **Future GUI pixel-smoothness.** A semantic "viewport moved from line A to
   line B" descriptor lets each client pick its own granularity: the TUI steps by
   whole cells, a future GUI interpolates by pixels. Server-driven animation would
   bake in line granularity that a GUI could never refine.

This respects the existing "server owns content, client owns presentation" split.
The client still owns **no authoritative editor state** — it already receives
transient render content (the visible lines) every frame; this design merely
**widens that render window** during a scroll gesture. The client mutates nothing
and persists nothing across the animation beyond the frames it is drawing.

Server-in-core timers were rejected outright: they would violate the
`bemtvi-core` "pure & synchronous" invariant the whole architecture rests on.

## Protocol changes (the `View` / `redraw`)

Today the `View` carries exactly the destination viewport, built over
`[top, top + height)`. This design **generalizes** the window so that normally it
is unchanged, and only a scroll widens it. Two new fields:

- **`base_line`** — the buffer-line index of `lines[0]`. Normally equal to `top`.
  On a scroll it may sit *above* the destination viewport (the over-scan starts at
  the topmost line visible during the slide).
- **`scroll`** — optional descriptor, `None` on every non-scroll redraw:
  ```
  scroll: {
      from_top:    usize,   // absolute buffer line
      to_top:      usize,
      from_cursor: usize,   // absolute buffer line
      to_cursor:   usize,
      duration_ms: u64,     // server's suggested pacing; client may clamp/ignore
  }
  ```

`lines` (and `selection`) are built over `[base_line, base_line + window_len)`:

- **Normal redraw:** `base_line == top`, `window_len == height` — byte-for-byte
  identical to today.
- **Scroll redraw:** `base_line = min(from_top, to_top)`,
  `window_len = |to_top − from_top| + height`. This is the union of every row
  visible during the slide. It is bounded: scroll distance ≤ one screen for
  half/full page, so `window_len ≤ ~2 × height` even on huge files.

`top` and the cursor fields always carry the **destination** (the authoritative
final state). A client that does not animate renders the instant jump correctly
via `lines[top − base_line ..][.. height]`.

The server's policy — *which* commands animate (only the four scroll commands) —
lives entirely server-side via the presence of the `scroll` flag. The client only
decides *how* to render the transition.

## Client animation loop (`bemtvi-tui`)

The client's `tokio::select!` gains a third arm (an animation tick) and a small
piece of animation state:

```
struct Anim {
    from_top: f32, to_top: f32,        // buffer-line units
    from_cursor: f32, to_cursor: f32,
    start: Instant, duration: Duration,
}
anim: Option<Anim>   // None when idle
```

**On redraw:**
- If the `View` carries `scroll` *and* the client animates (terminal supports it;
  a future reduce-motion/accessibility switch may disable it), seed
  `anim = Some(...)` from the descriptor and render the first interpolated frame
  (at `from_top`). The over-scanned `lines` window is held in `view`, so every
  frame slices out of it.
- Otherwise clear `anim` and render the destination directly. This is also how a
  mid-animation keypress lands: the superseding redraw has no `scroll` flag.

**Tick arm:** while `anim.is_some()`, `select!` includes a `tokio::time::sleep`
for the next frame (~16ms target; the terminal only moves on whole-cell
boundaries). Each tick:
1. `t = clamp(elapsed / duration, 0, 1)`, through an ease-out curve.
2. `interp_top = lerp(from_top, to_top, t)`; likewise the cursor.
3. Slice the viewport: `lines[round(interp_top) − base_line ..][.. height]`; place
   the cursor at `round(interp_cursor) − round(interp_top)`; render.
4. When `t >= 1.0`, render the exact destination frame and set `anim = None`.

**Terminal granularity:** `round(interp_top)` advances one whole line per visible
step, so a 12-line `<C-f>` reads as a quick eased line-stepped slide. A future GUI
keeps the same descriptor and interpolates sub-cell.

**Interrupt:** any keypress → server processes it instantly → fresh redraw with no
`scroll` flag → the redraw arm replaces `view` and clears `anim`. The animation
yields to the newest authoritative frame. No cancellation plumbing, no desync —
the server was never mid-anything.

`selection` is emitted over the same widened window and aligned to `base_line`, so
the client slices `lines` and `selection` identically per frame and visual-mode
highlighting slides correctly too.

## Core & server changes

**`bemtvi-core` (`editor.rs`):**
- New field `pending_scroll: Option<ScrollAnim>` (struct mirrors the protocol
  descriptor).
- `scroll_half` / `scroll_page` snapshot `top` + `cursor.line` before moving, then
  set `pending_scroll` **only if `top` actually changed**. A scroll at a boundary
  that moves the cursor but not `top` sets nothing (instant cursor update, no
  slide) — keeps scope tight.
- `duration_ms` = server-side function of distance, capped, e.g.
  `clamp(distance · 8, 80, 160)` ms. Hardcoded (no options system yet).

**`bemtvi-core` (`view.rs`):**
- `View` gains `base_line: usize` and `scroll: Option<ScrollAnim>`.
- The line builder and `selection_spans` iterate `[base_line, base_line + window_len)`
  instead of `[top, top + height)` — a small change that reduces to today's
  behavior when there is no pending scroll.
- `Editor::view(&mut self, …)` clears `pending_scroll` after projecting, so the
  animation fires exactly once.

**`bemtvi-server` (`lib.rs`):** `redraw()` serializes `base_line` and `scroll` into
the notification map. Still one `redraw` per input; nothing else changes.

## Edge cases

- **No `top` movement** (scroll at a boundary): no descriptor; cursor updates
  instantly.
- **Over-scan past EOF:** padded with `"~"` rows, exactly as the viewport does
  today.
- **Window bounded:** `window_len ≤ ~2 × height`; never unbounded.
- **Mid-animation keypress / resize:** the resulting fresh redraw (no `scroll`
  flag) supersedes; client clears `anim` and snaps to it.

## Testing

The contract under test is **the protocol**, not the wall-clock animation — which
fits the project's black-box, no-unit-test rule (test functionality through the
running server).

Add to `crates/bemtvi-server/tests/editing.rs` a small accessor for the last
redraw's `scroll` / `base_line`, then assert:

- `<C-d>` on a tall buffer → `scroll` present with correct
  `from_top`/`to_top`/`from_cursor`/`to_cursor`, `base_line == 0`,
  `lines.len() == height + half`, and window contents match buffer lines
  `[0 .. height + half)`.
- a plain `j` → **no** `scroll` field, `lines.len() == height` (proves the
  non-scroll path is unchanged).
- `<C-u>` already at the top → no descriptor (zero distance suppressed).
- `<C-f>` near EOF → window clamps to `line_count`, padded with `"~"`.

The **visual animation itself** (timing, easing, line-stepping) is client- and
time-based. Per the project's testing philosophy it is **not** asserted in the
integration suite; it is verified manually now and by the **planned PTY e2e
tests** later. This coverage boundary is intentional.
