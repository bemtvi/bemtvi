# `btv.decor` — viewport-scoped decoration providers — phased plan

> **Status: proposed (2026-06-15).** Build-order **step 5** of the
> [native plugin API](../specs/2026-06-11-native-plugin-api.md) (§6, *"Viewport
> decorations (the decoration-provider shape) — `btv.decor`"*). The last named
> `btv.*` UI surface that isn't built: the statusline segment registry and tree
> docks (step 4) are the only other gaps. Lives at
> `docs/plans/2026-06-15-btv-decor-viewport-decorations.md` alongside the other
> `docs/plans/*.md`.

## Context

Some decorations are **expensive *and* viewport-dependent**: rainbow parens,
indent guides, inline git-blame, semantic tokens on a huge file. neovim serves
these with a **decoration provider** — `on_win`/`on_line` callbacks the renderer
invokes per visible row, *every frame*. That is exactly the
re-enter-Lua-every-redraw model [ADR 0002](../decisions/0002-native-plugin-system.md)
rule 4 forbids, and the PUC-5.4 backend cannot host a per-row hot loop anyway.

`btv.decor` keeps the **useful kernel** — *only decorate what is visible;
recompute when the viewport moves* — and drops the frame coupling. The engine
wakes a provider **once per visible-range change** (scroll, resize, edit
reflow), **off the frame path**, hands it a snapshot of the visible slice, and
the provider **publishes** marks carrying a **generation token**; a publish from
a viewport the user already scrolled past is dropped. There is no `on_line`, no
per-frame call, and no single-frame "ephemeral" mark — a published range stands
until the next publish supersedes it or the viewport invalidates its generation.

The spec's flagship example (a whole rainbow-delimiters plugin) and the target
shape:

```lua
btv.decor.provider {
  name = "rainbow",
  bufs = { filetype = { "lua", "rust", "json" } },   -- engine skips non-matching windows
  on_range = function(ctx, publish)                  -- off the frame, once per range change
    -- ctx is a snapshot, never live state:
    --   { win, buf, top, bot, lines, tick, gen }    -- top/bot 0-based inclusive; lines = that slice
    local marks, depth = {}, 0
    for i, line in ipairs(ctx.lines) do
      local row = ctx.top + i - 1
      for col = 1, #line do
        local c = line:sub(col, col)
        if c:match("[%(%[{]") then
          marks[#marks+1] = { row, col-1, end_col = col, hl = RAINBOW[depth % 6 + 1] }
          depth = depth + 1
        elseif c:match("[%)%]}]") then
          depth = math.max(0, depth - 1)
          marks[#marks+1] = { row, col-1, end_col = col, hl = RAINBOW[depth % 6 + 1] }
        end
      end
    end
    publish(marks)        -- carries ctx.gen; folded into the next frame, or dropped if scrolled past
  end,
}
```

Marks are the **same shape as a static extmark**:
`{ row, col, end_row?, end_col?, hl?, virt_text?, virt_lines?, sign?, conceal?, priority? }`.

## What already exists (so this is mostly wiring three known mechanisms together)

The spec is explicit that *"the decoration-provider drive already exists; the new
piece is the debounced viewport-changed signal off the scroll/resize path."* In
practice the three substrates `btv.decor` needs are all built:

1. **The extmark / decoration layer** — `crates/bemtvi-core/src/extmark.rs`
   (`docs/specs/2026-06-07-extmark-decoration-layer-design.md`). Byte-offset
   anchored marks partitioned by namespace
   (`HashMap<u32, BTreeMap<u64, Extmark>>`), priorities, automatic edit-shift via
   `Buffer::record → ExtmarkStore::shift`. Lua surface in `prelude/api.lua`
   (`btv.ns.create`, `btv.buf.set_extmark`, `btv.buf.clear_namespace`), funnelled
   through `btv._extmark_{set,del,clear}` (`install.rs`) → `ExtmarkOp::{Set,Del,
   Clear}` → `effects.rs::apply_extmark_op`, projected live each frame in
   `server/src/extmarks.rs` and merged at priority order in the highlight path.
   **A published decoration lowers straight to this** — `clear_namespace` then a
   batch of `set_extmark` into the provider's namespace.
   - **v1 render limit (inherited, not new):** the extmark redraw projection
     renders `hl_group` only; `virt_text` / `virt_lines` / `sign` / `conceal` are
     *accepted but unrendered*. The flagship **rainbow** example is hl-only, so
     it renders fully. Indent-guides / inline-blame (virt_text) light up for free
     once the extmark layer renders virt_text — tracked as that layer's follow-up,
     **out of scope here** and called out loud, not silently dropped (§Decision 6).

2. **The off-tick drain + generation-token + debounce pattern** — already proven
   by `btv.complete` / `btv.picker`. `EditHost::run_pending` (`effects.rs:1562`) is
   a fixpoint that drains Lua queues **once after every key**, calls back into Lua
   with a generation (`run_complete_run(gen, …)` / `run_picker_run(gen, query)`),
   and gen-gates the pushes that come back (`if p.gen == live { … }`). The
   callback registry (`btv._cb_fns` / `btv._run_cb(id, keep, …)`), the Lua-side
   debounce (`btv.timer(dispatch, ms)` cancelling the prior timer), and the
   Shared-queue drain (`take_*` on `bemtvi-lua/src/runtime.rs`) are all reusable.

3. **Per-window viewport state** — `Editor::{top, leftcol}` (focused) /
   `Window::{saved_top, saved_leftcol}` (inactive), `Window::rect` (assigned on
   `relayout`), exposed per window by `window_layouts()`
   (`windows.rs:1472`). The **treesitter highlighter** is the working precedent
   for *viewport-keyed recompute*: `refresh_highlights(height)` (`treesitter.rs`)
   memoises on `(changedtick, first, last, language)` with a one-screen overscan.
   `btv.decor` reuses the *idea* (viewport in the key) but — per the spec — drives
   the signal **off `run_pending`, not the redraw projection**, because the
   provider is Lua and cannot run during a frame.

### The one new primitive: the viewport-changed signal

Treesitter recomputes *inside* `redraw()` because its work is synchronous Rust.
`btv.decor`'s work is **Lua, off-frame**, so the trigger must be detached from the
frame: core detects "the visible range of window *W* changed" when input settles,
stamps a fresh generation for *W*, and queues a dirty entry the server drains in
`run_pending`. That detached, generation-stamped signal is the net-new piece.

## Architecture (the loop)

```
       core (sync)                          server (off-tick, run_pending)              Lua
 ─────────────────────────────   ───────────────────────────────────────────   ──────────────────────
 input()/resize()/win-ops settle
   └─ recompute_decor_dirty():
        diff each visible win's
        (buf, top, bot) vs last
        on change: bump decor_gen[win],
        push DecorViewport{win,buf,top,bot,gen}
                                   take_decor_dirty() →
                                     for each dirty vp:
                                       build ctx.lines slice from rope
                                       for each provider matching bufs:
                                         lua.run_decor_provide(pidx, gen, ctx) ───▶ btv._decor_provide:
                                                                                      on_range(ctx, publish)
                                                                                        publish(marks):
                                   ◀── take_decor_publishes() ◀───────────────────       btv._decor_publish(pidx,gen,marks)
                                     drop if gen != decor_gen[win]            (queues DecorPublish on Shared)
                                     else: clear provider ns on buf,
                                           set marks (ExtmarkOp) → extmark layer
                                                                                   (next redraw paints them)
```

Generation semantics: `decor_gen[win]` is a monotonic counter bumped on **every**
viewport change for that window. A provider dispatch carries the gen current at
detection; the publish it produces carries the same gen; at apply time the server
drops it unless `gen == decor_gen[win]` (i.e. no newer scroll superseded it). This
is the `btv.complete` `menu_generation()` gating, re-keyed per window.

## Design decisions

1. **Signal off `run_pending`, not redraw.** Core stamps the dirty list when input
   settles (alongside `finalize_scroll_gesture`); the server drains it in the
   `run_pending` fixpoint that already runs once per key. Keeps Lua off the frame
   (rule 4) and reuses the exact drain point completion/picker use.

2. **Per-window generation, coalesced within a batch.** One `decor_gen` per
   `WindowId`. Multiple viewport changes between two `run_pending` drains collapse
   to one dirty entry per window (latest wins) — natural debounce for held
   `Ctrl-E`. A *further* time-debounce (coalesce a fast scroll *gesture* into one
   provider run) is a `btv.timer`-backed Phase-4 polish, not core.

3. **One namespace per provider; publish = clear-then-set, wholesale.** Each
   provider owns a namespace (`btv.ns.create("btv.decor:"..name)`). `publish(marks)`
   lowers to `clear_namespace(buf, ns)` + N×`set_extmark(buf, ns, …)`. Republishing
   for a new viewport replaces the prior set. Marks persist (and edit-shift) until
   the next publish — exactly the spec's "a published range stands until the next
   publish supersedes it." (A per-mark batch op may be added later; v1 reuses the
   existing per-mark `ExtmarkOp::Set` — the picker/completion already issue Ns of
   them per keystroke without trouble.)

4. **Stale-drop at apply time.** `decor_gen[win]` is readable by the server
   (`editor.decor_gen(win)`); a publish with a stale gen is dropped before any
   `ExtmarkOp` is issued. A viewport the user scrolled past never paints.

5. **`bufs` matching skips non-matching windows.** v1 matches `bufs.filetype`
   (a list) against the window's buffer filetype; a window whose buffer doesn't
   match runs no provider (and is never even dispatched). `buftype` / per-buffer
   opt-in are Phase 4.

6. **v1 renders `hl` only — loud about the rest.** A mark carrying only
   `virt_text`/`sign`/`conceal` lowers to an extmark the layer accepts-but-doesn't-
   render *today*; rather than silently no-op (CLAUDE.md: no silent stubs), the
   docs and the `btv.decor` example state hl-only, and a mark with *no* renderable
   field is reported via the provider-error path. virt_text rendering is an extmark
   -layer follow-up; when it lands, indent-guides/blame work with no `btv.decor`
   change.

7. **Provider errors fail loud, disable after repeated failures.** An `on_range`
   that errors is reported `E5108`-style (loud) and the provider is disabled after
   N consecutive failures (neovim's `CB_MAX_ERROR = 3` analog) — matching the
   no-silent-stubs convention. Phase 4.

8. **Async providers are fine.** `on_range` may kick off async work
   (`btv.run(...):next(…)` for a one-shot, `btv.lsp`, a `btv.run_stream` +
   `btv.await_each` loop) and call `publish` from the continuation; the gen token makes
   a late response safe to fold or drop. The publish queue is drained every
   `run_pending` round (in `apply_lua_effects`, not only on the dispatch round), and
   the gen-gate handles out-of-order arrival — so a publish from a later tick already
   works with no extra machinery. (`btv.spawn` was retired in the promise-only async
   move; providers use the promise/async-iterator surface.) Phase 4 adds the test.

## Phases

### Phase 1 — The viewport-changed signal (core, pure/sync)
- New `Editor` state: `decor_viewports: HashMap<WindowId, (BufferId, usize, usize)>`
  (last-seen `(buf, top, bot)` per window), `decor_gen: HashMap<WindowId, u64>`,
  `decor_dirty: Vec<DecorViewport>` where
  `DecorViewport { win: WindowId, buf: BufferId, top: usize, bot: usize, gen: u64 }`.
- `recompute_decor_dirty(&mut self)`: walk `window_layouts()`, compute each visible
  tiled window's `(buf, top, bot)` (`bot = top + text_height - 1`, clamped to line
  count); on a diff vs `decor_viewports`, bump `decor_gen[win]`, update the snapshot,
  push a `DecorViewport`. Call it at the tail of `input()` (by `finalize_scroll_gesture`),
  `resize()`, and the window open/close/focus paths.
- Public accessors: `take_decor_dirty() -> Vec<DecorViewport>`, `decor_gen(win) -> u64`.
- No Lua, no render yet → no end-to-end test in this phase; its behavior is asserted
  through Phases 2–3. (A focused internal check can ride along in Phase 2's test.)

### Phase 2 — Lua surface + registry + off-tick dispatch (no render) ✅ DONE (2026-06-16)

> Landed: `prelude/decor.lua` (`btv.decor.provider{ name, bufs, on_range }` +
> `btv._decor_dispatch` + mark normalization + the Phase-2 `publish` that records the
> latest marks Lua-side); the `btv._decor_register` gate bridge (`install.rs`) +
> `Shared.decor_active`; `LuaRuntime::{has_decor_providers, run_decor_dispatch}`
> (`runtime.rs`); and the server dispatch in `EditHost::run_pending` / `dispatch_decor`
> (`effects.rs`) — drains `take_decor_dirty()`, builds the `ctx` slice from the rope,
> and dispatches matching providers off-tick. Gated on `has_decor_providers()` so a
> no-provider config never slices lines or re-enters Lua on scroll. Tested black-box
> in `bemtvi-server/tests/decor.rs` (provider dispatched with the visible slice; `top`
> tracks scroll; `bufs.filetype` skips non-matching buffers; publish normalization).
> Builds clean on `native` and `--no-default-features`; `fmt` / `clippy -D warnings`
> clean. `ctx` is `{ win, buf, top, bot, lines, filetype, gen }`.

Original plan for this phase:

- `crates/bemtvi-lua/src/prelude/decor.lua` (new): `btv.decor.provider{ name, bufs,
  on_range }` validates + registers into `btv._decor.providers` (each gets a
  namespace + a `publish` closure factory). Wire it into the prelude loader.
- Bridge: `btv._decor_provide(pidx, gen, ctx)` (Lua) runs `on_range(ctx, publish)`;
  `publish(marks)` → `btv._decor_publish(pidx, gen, marks)` (Rust funnel in
  `install.rs`) queues `DecorPublish { pidx, gen, win, buf, marks }` onto a new
  `Shared.decor_publishes` (drained by `take_decor_publishes` in `runtime.rs`).
- Server: in `run_pending`, drain `take_decor_dirty()`; for each dirty vp build
  `ctx = { win, buf, top, bot, lines, tick, gen }` (lines sliced from the rope,
  `tick = changedtick`) and, for each provider whose `bufs.filetype` matches, call
  `lua.run_decor_provide(pidx, gen, ctx)` (new `runtime.rs` method →
  `btv._decor_provide`).
- **Test (black-box):** a provider that stashes `ctx.top`/`#ctx.lines` into a Lua
  global; feed scroll keys; `exec_lua` reads the global back → proves the snapshot,
  the dispatch, and per-window coalescing. (Publishes are queued but not yet
  applied.)

### Phase 3 — The publish path → render (end-to-end) ✅ DONE (2026-06-16)

> Landed: `publish(marks)` (in `prelude/decor.lua`) now splits the normalized marks
> into parallel arrays and calls the new `btv._decor_publish(ns, gen, win, buf, rows,
> cols, end_rows, end_cols, hls, priorities)` funnel (`install.rs`), which queues a
> [`DecorPublish`] (`ops.rs`) on `Shared.decor_publishes` (drained by
> `take_decor_publishes`, `runtime.rs`). The server drains it in `apply_lua_effects`
> (so a sync *or* a later async publish both land) → `EditHost::apply_decor_publish`
> (`effects.rs`): drop if `publish.gen != editor.decor_generation(win)`, else
> `apply_extmark_op(Clear{ ns, whole })` + one `Set` per mark (ids restart at 1,
> `priority` defaults to `DEFAULT_PRIORITY`). Marks fold into the existing extmark
> projection and paint next redraw, merged at priority order with treesitter/semantic
> spans. **One Phase-1 gap fixed in passing:** the viewport key now carries the
> buffer `changedtick` (`editor/decor.rs`), so an on-screen edit that leaves `top`/`bot`
> unchanged (typing a bracket) still re-dispatches — without it a fresh bracket stayed
> uncoloured until the next scroll. **Startup paint fixed:** `nvim_ui_attach` /
> `nvim_ui_try_resize` now drive `run_pending` after the resize — the resize assigns the
> first window rect (so a provider's viewport is only then known), and without the drain
> the providers weren't dispatched until the first keystroke, so a fresh session opened
> uncoloured until you pressed a key. **v1 hl-only made loud:** `normalize_mark` rejects
> a mark with no `hl` (Decision 6) and defaults a same-line `end_col` to `end_row =
> row` (the spec's rainbow shape). Flagship `examples/rainbow/` (init + bracket-dense
> sample) verified end-to-end. Tested black-box in `bemtvi-server/tests/decor.rs`:
> a fresh session colours on the **first frame with no keypress** (the startup path,
> file opened at boot — times out without the attach drain); rainbow `hl` spans land
> on the right cells; an on-screen edit re-colours without scrolling; scrolling colours
> newly-revealed lines; a hand-issued stale-gen publish paints nothing while the
> live-gen one does; the real example colours its sample.
> Builds + tests clean on `native` and `--no-default-features`; `fmt` / `clippy -D
> warnings` clean.

Original plan for this phase:

- Server drains `take_decor_publishes()` in `run_pending` (after the provide pass,
  same fixpoint round): drop if `pub.gen != editor.decor_gen(pub.win)`; else
  `apply_extmark_op(Clear{ buf, ns, whole })` then one `Set` per mark
  (`row,col → byte_of`, `hl_group`, `priority` default `DEFAULT_PRIORITY`).
- Marks fold into the existing extmark projection → painted next redraw, merged at
  priority order with treesitter/semantic/static extmarks.
- **Flagship + test:** `examples/rainbow/` (the spec's provider, verified
  end-to-end) + `bemtvi-server/tests/decor.rs`: open nested brackets, assert the
  rainbow `hl` spans land on the right cells via the redraw highlight map; scroll,
  assert the newly-revealed brackets get colored and the old viewport's marks are
  replaced; assert a hand-raced stale publish (gen bumped mid-flight) paints
  nothing.

### Phase 4 — Async, robustness, polish, wasm parity ✅ DONE (2026-06-16)

> Landed, all in `prelude/decor.lua` + tests (no core/server changes needed — the loop
> was already complete after Phase 3):
> - **Async provider** — the `publish` closure + the gen-gate already handled a late
>   publish (Decision 8); Phase 4 adds the proof: a provider that publishes from an
>   `btv.promise.delay(…):next(…)` continuation renders, the late response folded by the
>   live gen. Test `an_async_provider_publishes_from_a_promise_continuation`.
> - **Scroll-gesture debounce** — opt-in `debounce = <ms>` on `btv.decor.provider`; a
>   per-window trailing `btv.utils.debounce` re-armed on each viewport change, so a fast
>   continuous scroll fires `on_range` once after it settles (Decision 2). Default off
>   (rainbow stays instant). Test `a_debounced_provider_coalesces_a_burst_to_one_run`
>   (a synchronous burst arms once, fires once — deterministic, no wall-clock race).
> - **Provider errors → loud + disable-after-N** (Decision 7) — a throwing `on_range` is
>   reported `E5108`-style via `btv.notify` and, after `MAX_DECOR_ERRORS = 3` consecutive
>   failures (neovim's `CB_MAX_ERROR` analog), the provider is disabled (skipped until
>   re-registered, which rebuilds the provider table). A clean run resets the counter.
>   Test `a_provider_is_disabled_after_three_consecutive_errors`.
> - **`bufs` per-buffer + buftype opt-in** — `bufs.buf = id | { id, … }` scopes a
>   provider to specific buffer(s); `bufs.buftype = { "quickfix", "" , … }` scopes by
>   buffer *kind*. AND-combined with `bufs.filetype`. bemtvi models the buftypes it
>   distinguishes via the new core accessor `Editor::buffer_buftype` (`quickfix.rs`):
>   `"quickfix"` (quickfix **or** location-list display buffer), `"terminal"`, and `""`
>   (ordinary file/scratch); the dispatch passes `ctx.buftype` alongside `ctx.filetype`.
>   Tests `a_buffer_scoped_provider_runs_only_for_its_buffer` (real buf id, input-driven)
>   and `buftype_scopes_a_provider_to_buffer_kind` (runs in `:copen`'s quickfix window,
>   not an ordinary buffer). Other vim buftypes (`help`/`nofile`/`prompt`) read as `""`
>   until modelled.
> - **Undo no longer flashes the decorations** — decor marks are *ephemeral* viewport
>   state (republished off-tick), not document history, but the undo tree snapshotted the
>   whole extmark store, including the decor namespace. The **root** undo node is captured
>   at buffer load, *before* any provider runs, so undoing back to it restored an empty
>   decor namespace — wiping the live marks for one frame until the re-dispatch
>   republished them (the user-visible flash, seen on the first undo back to that state).
>   Fix: a namespace a decor publish targets is registered ephemeral
>   (`Editor::mark_extmark_namespace_ephemeral`), and `Editor::restore_snapshot`
>   (`undo.rs`) **carries the live marks for those namespaces across the restore**
>   (`ExtmarkStore::move_namespace_into`) instead of swapping in the snapshot's. Test
>   `custom_highlights_survive_undo_without_flashing` (a publish-once provider turns the
>   flash into a permanent loss, so the guard is deterministic — it fails without the fix).
> - **Off-tick viewport changes re-detect** — `recompute_decor_dirty` is now driven from
>   `run_pending` (gated on a registered provider), not only the `Editor::input` tail. A
>   viewport change made *off* the input tick — a `:e` run via a queued command-line action
>   (the widget-keys cmdline path), a buffer switch from a Lua callback — wouldn't otherwise
>   re-run the input-tail detector, so the file opened uncoloured until the next keystroke.
>   **Worth keeping in mind as widget-keys grows more queued actions:** any new path that
>   mutates the buffer/viewport off the input tick is already covered by this chokepoint —
>   it does *not* need to call `recompute_decor_dirty` itself. (Caught when the rebase onto
>   the widget-keys cmdline work turned 9 render tests red.)
> - **wasm / serverless parity** — viewport detection is core (`editor/decor.rs`) and the
>   dispatch/publish lives in the shared `EditHost::run_pending` fixpoint with **no
>   `native` gate**, so the serverless tick drives the identical loop (only the transport
>   differs). Confirmed: `cargo build`/`clippy` clean on both `native` and
>   `--no-default-features`; the black-box suite exercises the same shared path.
> - **Docs + second example** — `examples/decor-todo/` (a debounced TODO/FIXME/HACK/XXX/
>   NOTE keyword highlighter — hl-only, so it renders fully), verified end-to-end by
>   `the_todo_example_colours_its_keywords_end_to_end`. **indent-guides stays pending**,
>   not stubbed: it needs `virt_text` rendering (Decision 6, an extmark-layer follow-up),
>   so it is documented as pending rather than shipped broken — it works with no
>   `btv.decor` change once that layer grows `virt_text`.
>
> Builds + the 16-test `decor.rs` suite clean on `native` and `--no-default-features`
> (undo + quickfix + multicursor suites still green); `fmt` / `clippy -D warnings` clean.

Original plan for this phase:

- Async provider test (publish from an `btv.run(...):next(…)` / `btv.lsp` continuation —
  the promise-only async surface, `btv.spawn` is gone; gen gates a late response).
  Time-debounce a scroll gesture (Decision 2) — **reuse
  `btv.utils.debounce`** (landed upstream 2026-06-16, `prelude/utils.lua`) rather than
  hand-rolling the cancel-prior-timer dance over `btv.timer`; the per-window
  coalescing in Phase 1 already collapses changes between two drains, so this only
  needs to add a trailing delay before the provider re-runs on a fast continuous
  scroll. Provider-error → `E5108` loud + disable-after-N (Decision 7). `bufs`
  `buftype` / per-buffer opt-in. Verify the **wasm / serverless** `EditHost` tick
  drives the same loop (viewport detection is core; `run_pending` is shared — confirm
  both `native` and `--no-default-features` build + behave). Docs + a second example
  (indent-guides, gated on the extmark virt_text follow-up — shipped only if that
  lands; otherwise documented as pending, not stubbed).

## Out of scope (named, not silently dropped)
- **`virt_text` / `virt_lines` / `sign` / `conceal` rendering** — an extmark-layer
  follow-up (Decision 6); `btv.decor` consumes it for free when it lands.
- **`on_line` / per-frame providers** — deliberately absent (the whole point).
- **RPC-twin out-of-process providers** — build-order step 6, later.


---

## Follow-up (2026-08-14): the mark vocabulary is the extmark vocabulary

Decision 6 above shipped `publish` as an **hl-only** mark shape, with everything else
"accepted but unrendered". That narrowing was the wrong seam, and the evidence was in
the plugin tree: of the plugins that paint signs or virtual text — `bemtvi-dap`,
`bemtvi-diff`, `bemtvi-tree` — **none** used a decor provider. Each called
`btv.buf.set_extmark` directly and hand-rolled its own clearing. The only provider in
the wild (`bemtvi-colored-pairs`) was the rainbow-parens case v1 was designed around.

The extmark layer had rendered `virt_text` / `virt_lines` / `sign_text` /
`line_hl_group` / `line_fill` end to end for some time; only `publish` refused them.
Worse, `normalize_mark` enforced the refusal *loud*, so the narrowing was maximally
visible to the one surface that most wanted the payloads — a viewport-scoped provider
is exactly where an inline blame or a per-hunk sign belongs.

What `publish` legitimately adds over placing marks yourself is **lifecycle**, not
vocabulary: the generation gate that drops a publish from a scrolled-past viewport, the
wholesale clear-and-reset of the provider's namespace, one bridge crossing per publish,
and the ephemeral-namespace marking that keeps undo from flashing decorations. None of
that has anything to do with which fields a mark carries.

So the second vocabulary was deleted rather than grown:

* `btv._extmark_split_opts` (in `prelude/api.lua`) now holds the key partitioning,
  validation and defaulting that `btv.buf.set_extmark` used to inline. Both surfaces
  call it, so an option one accepts the other accepts, and a decoration added to the
  extmark layer reaches viewport providers the same day.
* `normalize_mark` splits a published mark into `{ row, col, opts }` and validates
  `opts` through that shared splitter. `hl` stays as the decor-native shorthand for
  `hl_group`. The old "needs an `hl`" error becomes "would draw nothing" — a mark
  carrying no `hl_group` / `virt_text` / `virt_lines` / `sign_text` / `line_hl_group` /
  `line_fill` still fails loud, so a sign-only mark is legal and an empty one is not.
  The splitter runs *first*, so a typo'd key names itself instead of reporting as
  "draws nothing".
* `btv._decor_publish(ns, gen, win, buf, marks)` takes a list of per-mark tables
  carrying the same split payload `btv._extmark_set` takes (the parallel arrays are
  gone — `virt_text` is a ragged chunk list and never fit them). `DecorMark` grew
  `decor` / gravity, and `apply_decor_publish` forwards the payload instead of
  hardcoding `decor: None`.

Note this changed the **internal** bridge signature, so `a_stale_publish_paints_nothing`
— which hand-issues `btv._decor_publish` to control the generation exactly — was updated
to the new shape. The public `publish` contract is backward compatible: `hl` still works,
positional `{ row, col }` still works.

**Plugin follow-through.** The three plugins above were examined and deliberately *not*
converted: their decorations are persistent, edit-tracked buffer state (breakpoints, diff
hunks, a fully re-rendered tree), not viewport-recomputed, so extmark lifecycle is the
right one and a decor provider would be a regression. What the audit did surface was a
real bug in `bemtvi-dap`, caused by a *false* belief this narrowing helped spread: its
`signs.lua` recorded that "a whole-line `line_hl_group` is stored-but-unpainted" and drew
the stopped line as a ranged `hl_group` spanning the line's text instead. That form had to
read the line to compute `end_col`, stopped at the end of the text rather than the window
edge, and — being a char-range span — joined the winner-takes-cell resolution and lost
every cell a syntax span covered, so the stopped line was tinted only in its uncoloured
gaps. It now uses `line_hl_group`, with an e2e assertion pinning the shape.
