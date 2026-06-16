# Promise-only `nx` async — drop raw `nx.spawn`, add `nx.run` / `nx.run_stream`

> **Status: LANDED (2026-06-16).** All four phases done — `nx.run` / `nx.run_stream`
> / `nx.await_each` primitives (prelude/process.lua), `nx.spawn` removed, the picker
> and completion source contracts migrated to `ctx.push` + return-a-promise (`done`
> removed; lazy-docs `resolve` now returns a promise too), and all consumers + docs
> updated. Process is the first promise-only cut; the one-shot `nx.ui.*` / `nx.lsp`
> / `nx.fs` surfaces (see *Out of scope*) are follow-ups on the same principle.

`nx` is greenfield (no compat constraints — [[ADR 0002]]). Goal: **no callback-shaped
one-shot async on `nx`** — every "do a thing, get a result later" API returns a
promise ([the promise system](../../crates/nxvim-lua/src/prelude/promise.lua)).
Streaming becomes an **async-iterator**, not a single promise. Event subscriptions
(`nx.autocmd`, `nx.on_key`, keymap rhs) and the microtask primitive (`nx.schedule`)
**stay** — they fire repeatedly / underpin promises, so a promise is the wrong shape.

This first cut covers the **process** surface, which is the only callback-shaped
one-shot async with real call sites today.

## The shape

There are two transports under the hood (both stay): `nx._system_async` (collect
stdout, fire once on exit) and `nx._spawn_stream` (incremental stdout batches).
The Lua surface maps onto them as:

```lua
-- one-shot: nx._system_async transport
nx.run { cmd, args, cwd, env, stdin } -> promise of { code, stdout, stderr }

-- streaming: nx._spawn_stream transport, consumed as an async-iterator
local stream = nx.run_stream { cmd, args, cwd, env }   -- a Stream: :next() / :kill()
for batch in nx.await_each(stream) do ... end          -- inside nx.async; batch = {lines}
```

`nx.spawn` (the raw `{ on_stdout, on_exit }` callback surface) is **removed**.

### The Stream + async-iterator

`nx.run_stream` returns a `Stream` with `:next()` → a promise that resolves to the
next batch of stdout lines, or `nil` at end-of-stream, plus `:kill()`. Batches that
arrive between `:next()` calls buffer in a queue; a `:next()` with an empty queue
parks a single waiter the next batch (or exit) wakes. **Sequential consumption**
(one outstanding `:next()` at a time — exactly what a `for` loop does) is the
contract. `nx.await_each(stream)` is the sugar:

```lua
function nx.await_each(stream)
  return function() return nx.await(stream:next()) end  -- nil ends the for-loop
end
```

### Source contract change (picker + completion)

Today a source is `items/complete = function(ctx, push, done)`. Promise-only means
**`done` goes away** — the source is a function returning a promise (or nothing, for
a synchronous source), and the engine treats resolution as completion. `push` stays
as `ctx.push` (it's the emit *sink*, the generator-`yield` analogue, not async
control flow); `ctx.on_cancel` stays (a cleanup hook). New shape:

```lua
nx.picker.source {
  name = "files",
  items = nx.async(function(ctx)
    for batch in nx.await_each(nx.run_stream { cmd = "rg", args = { "--files" }, cwd = ctx.cwd }) do
      for _, l in ipairs(batch) do if l ~= "" then ctx.push { text = l, path = l } end end
    end
  end),
  confirm = nx.picker.edit,
}
```

The engine drives it with `nx.promise.resolve(source.items(ctx)):next(finish):catch(...)`
— so a plain synchronous source (the `buffers` source: push in a loop, return) and an
`nx.async` streaming source unify on one code path, and the multi-source completion
"all done" reduces to `nx.promise.all(...)` instead of a manual remaining-counter.

## Phases

### Phase 1 — primitives (`prelude/process.lua`, new) ✅ first

`nx.run`, `nx.run_stream` (+ the `Stream`), `nx.await_each`. New prelude module
loaded **after** `promise.lua` (needs `nx.promise`/`nx.async`). Remove `nx.spawn`
from `runtime.lua` (keep the `_spawn_stream` / `_system_async` / `_stdout_fns`
transport). Tests: a `nx.run` of a real command resolves `{code,stdout}`; a
`nx.run_stream` consumed via `nx.await_each` inside `nx.async` yields batches then
ends; `:kill()` reaps. (Use a portable command — `printf` / `sh -c` — kept hermetic.)

### Phase 2 — picker engine + built-in sources

`nx._picker_run`: drop the positional `push`/`done`; put `push` on `ctx`; drive the
returned promise (`nx.promise.resolve(...)` → finish, `:catch` → notify + finish);
keep the gen/identity gating and `ctx.on_cancel`. Migrate `files` / `live_grep`
(→ `nx.run_stream` + `nx.async`) and `buffers` (sync, `ctx.push` + return). Update
`tests/picker.rs` (the streaming-source test uses `nx.spawn`).

### Phase 3 — completion engine + source contract

`nx._complete_run`: same contract change (`ctx.push`, source returns a promise);
replace the manual per-source done-counter with `nx.promise.all(active sources)` →
one `nx._complete_finish(gen)`. Built-in sources are `buffer`/`lsp`/`snippets`
(server-native, no Lua spawn), so only the *plugin* source contract + the driver
change; keep debounce + gen-gating.

### Phase 4 — consumers, removal, docs

Migrate `examples/nx-statusline/` git segment to `nx.run` + `nx.async`. Migrate any
remaining `nx.spawn` reference. Confirm `nx.spawn` is gone (grep). Update
`examples/ui-complete` comment, `examples/ui-picker`, the picker/complete specs, and
`known-approximations.md` / the source-contract docs. `vim.fn.system` / `vim.system`
are unaffected (separate `_system_async` path; `vim.*` is the compat layer, exempt).

## Out of scope (later promise-only cuts)

`nx.ui.select`/`input`/`confirm` (`on_choice`/`on_confirm` → promise), `nx.lsp`
request verbs → promise, `nx.fs.*` → promise, one-shot `nx.timer` (already
`nx.promise.delay`). Each is its own small unit on the same principle; this plan is
the **process** surface only, because it's where the callback shape actually has
call sites today.
