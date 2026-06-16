# `nx.decor` — viewport-scoped decoration providers — phased plan

> **Status: proposed (2026-06-15).** Build-order **step 5** of the
> [native plugin API](../specs/2026-06-11-native-plugin-api.md) (§6, *"Viewport
> decorations (the decoration-provider shape) — `nx.decor`"*). The last named
> `nx.*` UI surface that isn't built: the statusline segment registry and tree
> docks (step 4) are the only other gaps. Lives at
> `docs/plans/2026-06-15-nx-decor-viewport-decorations.md` alongside the other
> `docs/plans/*.md`.

## Context

Some decorations are **expensive *and* viewport-dependent**: rainbow parens,
indent guides, inline git-blame, semantic tokens on a huge file. neovim serves
these with a **decoration provider** — `on_win`/`on_line` callbacks the renderer
invokes per visible row, *every frame*. That is exactly the
re-enter-Lua-every-redraw model [ADR 0002](../decisions/0002-native-plugin-system.md)
rule 4 forbids, and the PUC-5.4 backend cannot host a per-row hot loop anyway.

`nx.decor` keeps the **useful kernel** — *only decorate what is visible;
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
nx.decor.provider {
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
practice the three substrates `nx.decor` needs are all built:

1. **The extmark / decoration layer** — `crates/nxvim-core/src/extmark.rs`
   (`docs/specs/2026-06-07-extmark-decoration-layer-design.md`). Byte-offset
   anchored marks partitioned by namespace
   (`HashMap<u32, BTreeMap<u64, Extmark>>`), priorities, automatic edit-shift via
   `Buffer::record → ExtmarkStore::shift`. Lua surface in `prelude/api.lua`
   (`nx.ns.create`, `nx.buf.set_extmark`, `nx.buf.clear_namespace`), funnelled
   through `nx._extmark_{set,del,clear}` (`install.rs`) → `ExtmarkOp::{Set,Del,
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
   by `nx.complete` / `nx.picker`. `EditHost::run_pending` (`effects.rs:1562`) is
   a fixpoint that drains Lua queues **once after every key**, calls back into Lua
   with a generation (`run_complete_run(gen, …)` / `run_picker_run(gen, query)`),
   and gen-gates the pushes that come back (`if p.gen == live { … }`). The
   callback registry (`nx._cb_fns` / `nx._run_cb(id, keep, …)`), the Lua-side
   debounce (`nx.timer(dispatch, ms)` cancelling the prior timer), and the
   Shared-queue drain (`take_*` on `nxvim-lua/src/runtime.rs`) are all reusable.

3. **Per-window viewport state** — `Editor::{top, leftcol}` (focused) /
   `Window::{saved_top, saved_leftcol}` (inactive), `Window::rect` (assigned on
   `relayout`), exposed per window by `window_layouts()`
   (`windows.rs:1472`). The **treesitter highlighter** is the working precedent
   for *viewport-keyed recompute*: `refresh_highlights(height)` (`treesitter.rs`)
   memoises on `(changedtick, first, last, language)` with a one-screen overscan.
   `nx.decor` reuses the *idea* (viewport in the key) but — per the spec — drives
   the signal **off `run_pending`, not the redraw projection**, because the
   provider is Lua and cannot run during a frame.

### The one new primitive: the viewport-changed signal

Treesitter recomputes *inside* `redraw()` because its work is synchronous Rust.
`nx.decor`'s work is **Lua, off-frame**, so the trigger must be detached from the
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
                                         lua.run_decor_provide(pidx, gen, ctx) ───▶ nx._decor_provide:
                                                                                      on_range(ctx, publish)
                                                                                        publish(marks):
                                   ◀── take_decor_publishes() ◀───────────────────       nx._decor_publish(pidx,gen,marks)
                                     drop if gen != decor_gen[win]            (queues DecorPublish on Shared)
                                     else: clear provider ns on buf,
                                           set marks (ExtmarkOp) → extmark layer
                                                                                   (next redraw paints them)
```

Generation semantics: `decor_gen[win]` is a monotonic counter bumped on **every**
viewport change for that window. A provider dispatch carries the gen current at
detection; the publish it produces carries the same gen; at apply time the server
drops it unless `gen == decor_gen[win]` (i.e. no newer scroll superseded it). This
is the `nx.complete` `menu_generation()` gating, re-keyed per window.

## Design decisions

1. **Signal off `run_pending`, not redraw.** Core stamps the dirty list when input
   settles (alongside `finalize_scroll_gesture`); the server drains it in the
   `run_pending` fixpoint that already runs once per key. Keeps Lua off the frame
   (rule 4) and reuses the exact drain point completion/picker use.

2. **Per-window generation, coalesced within a batch.** One `decor_gen` per
   `WindowId`. Multiple viewport changes between two `run_pending` drains collapse
   to one dirty entry per window (latest wins) — natural debounce for held
   `Ctrl-E`. A *further* time-debounce (coalesce a fast scroll *gesture* into one
   provider run) is a `nx.timer`-backed Phase-4 polish, not core.

3. **One namespace per provider; publish = clear-then-set, wholesale.** Each
   provider owns a namespace (`nx.ns.create("nx.decor:"..name)`). `publish(marks)`
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
   docs and the `nx.decor` example state hl-only, and a mark with *no* renderable
   field is reported via the provider-error path. virt_text rendering is an extmark
   -layer follow-up; when it lands, indent-guides/blame work with no `nx.decor`
   change.

7. **Provider errors fail loud, disable after repeated failures.** An `on_range`
   that errors is reported `E5108`-style (loud) and the provider is disabled after
   N consecutive failures (neovim's `CB_MAX_ERROR = 3` analog) — matching the
   no-silent-stubs convention. Phase 4.

8. **Async providers are fine.** `on_range` may `nx.spawn`/`nx.lsp` and call
   `publish` from the callback; the gen token makes a late response safe to fold or
   drop. The publish queue + gen-gate already handle out-of-order arrival; no extra
   machinery. Phase 4 adds the test.

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

> Landed: `prelude/decor.lua` (`nx.decor.provider{ name, bufs, on_range }` +
> `nx._decor_dispatch` + mark normalization + the Phase-2 `publish` that records the
> latest marks Lua-side); the `nx._decor_register` gate bridge (`install.rs`) +
> `Shared.decor_active`; `LuaRuntime::{has_decor_providers, run_decor_dispatch}`
> (`runtime.rs`); and the server dispatch in `EditHost::run_pending` / `dispatch_decor`
> (`effects.rs`) — drains `take_decor_dirty()`, builds the `ctx` slice from the rope,
> and dispatches matching providers off-tick. Gated on `has_decor_providers()` so a
> no-provider config never slices lines or re-enters Lua on scroll. Tested black-box
> in `nxvim-server/tests/decor.rs` (provider dispatched with the visible slice; `top`
> tracks scroll; `bufs.filetype` skips non-matching buffers; publish normalization).
> Builds clean on `native` and `--no-default-features`; `fmt` / `clippy -D warnings`
> clean. `ctx` is `{ win, buf, top, bot, lines, filetype, gen }`.

Original plan for this phase:

- `crates/nxvim-lua/src/prelude/decor.lua` (new): `nx.decor.provider{ name, bufs,
  on_range }` validates + registers into `nx._decor.providers` (each gets a
  namespace + a `publish` closure factory). Wire it into the prelude loader.
- Bridge: `nx._decor_provide(pidx, gen, ctx)` (Lua) runs `on_range(ctx, publish)`;
  `publish(marks)` → `nx._decor_publish(pidx, gen, marks)` (Rust funnel in
  `install.rs`) queues `DecorPublish { pidx, gen, win, buf, marks }` onto a new
  `Shared.decor_publishes` (drained by `take_decor_publishes` in `runtime.rs`).
- Server: in `run_pending`, drain `take_decor_dirty()`; for each dirty vp build
  `ctx = { win, buf, top, bot, lines, tick, gen }` (lines sliced from the rope,
  `tick = changedtick`) and, for each provider whose `bufs.filetype` matches, call
  `lua.run_decor_provide(pidx, gen, ctx)` (new `runtime.rs` method →
  `nx._decor_provide`).
- **Test (black-box):** a provider that stashes `ctx.top`/`#ctx.lines` into a Lua
  global; feed scroll keys; `exec_lua` reads the global back → proves the snapshot,
  the dispatch, and per-window coalescing. (Publishes are queued but not yet
  applied.)

### Phase 3 — The publish path → render (end-to-end)
- Server drains `take_decor_publishes()` in `run_pending` (after the provide pass,
  same fixpoint round): drop if `pub.gen != editor.decor_gen(pub.win)`; else
  `apply_extmark_op(Clear{ buf, ns, whole })` then one `Set` per mark
  (`row,col → byte_of`, `hl_group`, `priority` default `DEFAULT_PRIORITY`).
- Marks fold into the existing extmark projection → painted next redraw, merged at
  priority order with treesitter/semantic/static extmarks.
- **Flagship + test:** `examples/rainbow/` (the spec's provider, verified
  end-to-end) + `nxvim-server/tests/decor.rs`: open nested brackets, assert the
  rainbow `hl` spans land on the right cells via the redraw highlight map; scroll,
  assert the newly-revealed brackets get colored and the old viewport's marks are
  replaced; assert a hand-raced stale publish (gen bumped mid-flight) paints
  nothing.

### Phase 4 — Async, robustness, polish, wasm parity
- Async provider test (publish from an `nx.spawn`/`nx.lsp` callback; gen gates a
  late response). Time-debounce a scroll gesture (Decision 2) via `nx.timer`.
  Provider-error → `E5108` loud + disable-after-N (Decision 7). `bufs` `buftype` /
  per-buffer opt-in. Verify the **wasm / serverless** `EditHost` tick drives the
  same loop (viewport detection is core; `run_pending` is shared — confirm both
  `native` and `--no-default-features` build + behave). Docs + a second example
  (indent-guides, gated on the extmark virt_text follow-up — shipped only if that
  lands; otherwise documented as pending, not stubbed).

## Out of scope (named, not silently dropped)
- **`virt_text` / `virt_lines` / `sign` / `conceal` rendering** — an extmark-layer
  follow-up (Decision 6); `nx.decor` consumes it for free when it lands.
- **`on_line` / per-frame providers** — deliberately absent (the whole point).
- **RPC-twin out-of-process providers** — build-order step 6, later.
