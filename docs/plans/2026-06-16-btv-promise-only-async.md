# Promise-only `btv` async — drop raw `btv.spawn`, add `btv.run` / `btv.run_stream`

> **Status: LANDED (2026-06-16).** All four phases done — `btv.run` / `btv.run_stream`
> / `btv.await_each` primitives (prelude/process.lua), `btv.spawn` removed, the picker
> and completion source contracts migrated to `ctx.push` + return-a-promise (`done`
> removed; lazy-docs `resolve` now returns a promise too), and all consumers + docs
> updated. Process is the first promise-only cut; the one-shot `btv.ui.*` / `btv.lsp`
> / `btv.fs` surfaces (see *Out of scope*) are follow-ups on the same principle.

`btv` is greenfield (no compat constraints — [[ADR 0002]]). Goal: **no callback-shaped
one-shot async on `btv`** — every "do a thing, get a result later" API returns a
promise ([the promise system](../../crates/bemtvi-lua/src/prelude/promise.lua)).
Streaming becomes an **async-iterator**, not a single promise. Event subscriptions
(`btv.autocmd`, keymap rhs) and the microtask primitive (`btv.schedule`) **stay** —
they fire repeatedly / underpin promises, so a promise is the wrong shape.

This first cut covers the **process** surface, which is the only callback-shaped
one-shot async with real call sites today.

## The shape

There are two transports under the hood (both stay): `btv._system_async` (collect
stdout, fire once on exit) and `btv._spawn_stream` (incremental stdout batches).
The Lua surface maps onto them as:

```lua
-- one-shot: btv._system_async transport
btv.run { cmd, args, cwd, env, stdin } -> promise of { code, stdout, stderr }

-- streaming: btv._spawn_stream transport, consumed as an async-iterator
local stream = btv.run_stream { cmd, args, cwd, env }   -- a Stream: :next() / :kill()
for batch in btv.await_each(stream) do ... end          -- inside btv.async; batch = {lines}
```

`btv.spawn` (the raw `{ on_stdout, on_exit }` callback surface) is **removed**.

### The Stream + async-iterator

`btv.run_stream` returns a `Stream` with `:next()` → a promise that resolves to the
next batch of stdout lines, or `nil` at end-of-stream, plus `:kill()`. Batches that
arrive between `:next()` calls buffer in a queue; a `:next()` with an empty queue
parks a single waiter the next batch (or exit) wakes. **Sequential consumption**
(one outstanding `:next()` at a time — exactly what a `for` loop does) is the
contract. `btv.await_each(stream)` is the sugar:

```lua
function btv.await_each(stream)
  return function() return btv.await(stream:next()) end  -- nil ends the for-loop
end
```

### Source contract change (picker + completion)

Today a source is `items/complete = function(ctx, push, done)`. Promise-only means
**`done` goes away** — the source is a function returning a promise (or nothing, for
a synchronous source), and the engine treats resolution as completion. `push` stays
as `ctx.push` (it's the emit *sink*, the generator-`yield` analogue, not async
control flow); `ctx.on_cancel` stays (a cleanup hook). New shape:

```lua
btv.picker.source {
  name = "files",
  items = btv.async(function(ctx)
    for batch in btv.await_each(btv.run_stream { cmd = "rg", args = { "--files" }, cwd = ctx.cwd }) do
      for _, l in ipairs(batch) do if l ~= "" then ctx.push { text = l, path = l } end end
    end
  end),
  confirm = btv.picker.edit,
}
```

The engine drives it with `btv.promise.resolve(source.items(ctx)):next(finish):catch(...)`
— so a plain synchronous source (the `buffers` source: push in a loop, return) and an
`btv.async` streaming source unify on one code path, and the multi-source completion
"all done" reduces to `btv.promise.all(...)` instead of a manual remaining-counter.

## Phases

### Phase 1 — primitives (`prelude/process.lua`, new) ✅ first

`btv.run`, `btv.run_stream` (+ the `Stream`), `btv.await_each`. New prelude module
loaded **after** `promise.lua` (needs `btv.promise`/`btv.async`). Remove `btv.spawn`
from `runtime.lua` (keep the `_spawn_stream` / `_system_async` / `_stdout_fns`
transport). Tests: a `btv.run` of a real command resolves `{code,stdout}`; a
`btv.run_stream` consumed via `btv.await_each` inside `btv.async` yields batches then
ends; `:kill()` reaps. (Use a portable command — `printf` / `sh -c` — kept hermetic.)

### Phase 2 — picker engine + built-in sources

`btv._picker_run`: drop the positional `push`/`done`; put `push` on `ctx`; drive the
returned promise (`btv.promise.resolve(...)` → finish, `:catch` → notify + finish);
keep the gen/identity gating and `ctx.on_cancel`. Migrate `files` / `live_grep`
(→ `btv.run_stream` + `btv.async`) and `buffers` (sync, `ctx.push` + return). Update
`tests/picker.rs` (the streaming-source test uses `btv.spawn`).

### Phase 3 — completion engine + source contract

`btv._complete_run`: same contract change (`ctx.push`, source returns a promise);
replace the manual per-source done-counter with `btv.promise.all(active sources)` →
one `btv._complete_finish(gen)`. Built-in sources are `buffer`/`lsp`/`snippets`
(server-native, no Lua spawn), so only the *plugin* source contract + the driver
change; keep debounce + gen-gating.

### Phase 4 — consumers, removal, docs

Migrate `examples/btv-statusline/` git segment to `btv.run` + `btv.async`. Migrate any
remaining `btv.spawn` reference. Confirm `btv.spawn` is gone (grep). Update
`examples/ui-complete` comment, `examples/ui-picker`, the picker/complete specs, and
`known-approximations.md` / the source-contract docs. `vim.fn.system` / `vim.system`
are unaffected (separate `_system_async` path; `vim.*` is the compat layer, exempt).

## Out of scope (later promise-only cuts)

`btv.lsp` request verbs → promise, `btv.fs.*` → promise, one-shot `btv.timer` (already
`btv.promise.delay`). Each is its own small unit on the same principle; this plan is
the **process** surface only, because it's where the callback shape actually has
call sites today.

### Follow-up cut: `btv.ui.*` — LANDED (2026-06-16)

The `btv.ui.select` / `input` / `confirm` chooser/prompt surfaces are now
promise-only on the same principle (`on_choice`/`on_confirm` → a returned promise):

- `btv.ui.input(opts)` → promise of the entered text (`nil` on cancel).
- `btv.ui.select(items, opts)` → promise of the chosen **item** (`nil` on cancel);
  the 1-based index is dropped from the promise.
- `btv.ui.confirm(message, opts)` → promise of a boolean.
- `btv.ui.float` is unchanged (fire-and-forget content float — no result).

Passing the old callback argument errors loudly (names the migration). The
`vim.ui.input` / `vim.ui.select` muscle-memory aliases keep neovim's callback
signatures (the bounded compat layer is exempt) — `vim.ui.select` still hands its
callback `(item, index)`, adapted from the shared `select_into` core. As part of
this cut, `prelude/promise.lua` moved **earlier** in the prelude load order (right
after `runtime.lua`): it is the async foundation every later surface builds on, and
its only load-time need is `btv.schedule` — the `btv.timer` dependency cited in its
old header is call-time only (`btv.promise.delay`).
