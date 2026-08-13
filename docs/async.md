# Async & promises

The editor is **single-threaded and tick-based**: all Lua runs on the editor's own
loop, and nothing may block it — a blocking read would freeze every keystroke,
every repaint, every client. So bemtvi has no blocking I/O at all. Instead it gives
plugins a **browser-shaped async runtime**: Promises/A+ (`btv.promise`) plus
`async`/`await` sugar (`btv.async` / `btv.await`), layered over the editor's event
loop. If you've written `fetch(...).then(...)` or `await fetch(...)` in JavaScript,
you already know this API.

One rule shapes the whole surface (ADR 0002):

> **`btv` async is promise-only.** Every "do a thing, get a result later" API
> returns a **promise** — never takes a callback. Streaming is an async-iterator
> over promises. (The `vim.*` muscle-memory aliases keep neovim's callback shapes;
> new `btv` code uses promises.)

So `btv.run`, `btv.fs.read`, `btv.ui.input`, `btv.lsp.hover` — anything that waits —
hands you a promise you `:next()` / `await`.

## Promises

A **promise** is a value that arrives later. `btv.promise.new(executor)` is the
browser constructor — `executor(resolve, reject)` runs now, and you call `resolve`
(or `reject`) when the result is ready:

```lua
local function after(ms)
  return btv.promise.new(function(resolve)
    btv.timer(function() resolve("done") end, ms)   -- settle later, off the tick
  end)
end

after(500)
  :next(function(v) btv.notify(v) end)     -- "done", ~500ms later
  :catch(function(e) btv.notify("failed: " .. tostring(e), "warn") end)
```

The three chaining methods are the browser's, with one spelling change:

| Method | Does |
| --- | --- |
| `:next(on_fulfilled[, on_rejected])` | The spine. Returns a **new** promise resolved with the handler's return (adopting it if it's itself a promise), or rejected if the handler throws. Spelled `:next`, not `:then`, because `then` is a Lua keyword. |
| `:catch(on_rejected)` | Sugar for `:next(nil, on_rejected)`. A bare `:catch` at the end of a chain catches a rejection from **anywhere** earlier in it. |
| `:finally(on_finally)` | Runs whichever way the promise settles, then passes the original value/reason through untouched. |

Ready-made promises: `btv.promise.resolve(v)` (already fulfilled), `btv.promise.reject(e)`
(already rejected), and `btv.promise.try(fn, ...)` — run `fn` *inside* a promise so a
**synchronous** throw becomes a rejection and a returned promise is adopted, folding
"might fail either way" into one chain.

Two semantics worth knowing:

- **Reactions are always asynchronous.** A `:next` callback runs as a *microtask*
  (deferred to the end of the current tick via `btv.schedule`), never inline — even
  on an already-settled promise. Same as the browser running `.then` off the stack.
- **Unhandled rejections are reported, not swallowed.** A promise that rejects with
  nothing subscribed surfaces a loud `bemtvi: unhandled promise rejection: …`
  (CLAUDE.md: no silent failures). Attach a `:catch` — or handle it via `await`.

## async / await

Chains are fine for one step, but nest badly. `btv.async` + `btv.await` flatten them
into straight-line code — the same trade the JS `async`/`await` keywords make.

`btv.async(fn)` returns a **function** that, when called, runs `fn` as a coroutine
and returns a **promise** for its result. Inside, `btv.await(p)` suspends until `p`
settles and evaluates to its value (or re-raises its rejection as a Lua error):

```lua
local load_config = btv.async(function(path)
  local exists = btv.await(btv.fs.exists(path))
  if not exists then return {} end
  local text = btv.await(btv.fs.read_text(path))
  return parse(text)                       -- becomes the promise's fulfilment
end)

load_config("init.lua")
  :next(use_config)
  :catch(function(e) btv.notify("config load failed: " .. tostring(e), "warn") end)
```

Error handling works either way: a rejected `await` *raises* inside the coroutine,
so you can `pcall` it to handle locally (PUC 5.4 yields across a `pcall`), or let it
propagate to the coroutine edge — the returned promise rejects, caught by `:catch`
on the result.

> `btv.await` must be called **inside** an `btv.async` function (there's nothing to
> suspend otherwise — it errors loudly). It's also what `setup` / `render` run
> inside on an [`btv.component`](features/ui-primitives.md#components--reactive-uis),
> so you can `btv.await(...)` straight inside those too.

## The async APIs you get

Everything that waits is one of these promise-shaped surfaces — all awaitable, all
non-blocking:

| Surface | Returns | For |
| --- | --- | --- |
| `btv.run{ cmd, args, cwd, env, stdin }` | promise of `{ code, stdout, stderr }` | Run a subprocess to completion. |
| `btv.run_stream{ … }` + `btv.await_each` | a `Stream` (async-iterator) | Stream a subprocess's stdout as it arrives. |
| `btv.fs.read` / `read_text` / `stat` / `readdir` / `exists` / … | promise of the result | Filesystem, never blocking. |
| `btv.ui.input` / `select` / `confirm` / `open` | promise of the user's answer | Prompts and choosers. |
| `btv.lsp.hover` / `references` / … | promise | Language-server requests. |
| `btv.promise.delay(ms[, v])` | promise that fulfils after `ms` | An await-able sleep. |
| `btv.wait_for(pred[, opts])` | promise of the truthy value | Poll a cross-tick condition (below). |

`btv.run` resolves rather than rejects on a non-zero exit — the exit `code` is part
of the value, so you decide what counts as failure:

```lua
btv.async(function()
  local r = btv.await(btv.run({ cmd = "git", args = { "rev-parse", "HEAD" } }))
  if r.code == 0 then btv.notify("HEAD is " .. r.stdout:gsub("%s+$", "")) end
end)()                                       -- note the trailing () — call it to run
```

### Streaming

For output you want *as it arrives* (not after the process exits), `btv.run_stream`
returns a `Stream` you iterate with `btv.await_each`. Each step awaits the next batch
of lines; the loop ends when the stream is exhausted. `:kill()` reaps the child
early. This is how the picker's `files` / `live_grep` sources stream `rg`:

```lua
btv.async(function()
  local stream = btv.run_stream({ cmd = "rg", args = { "--files" } })
  for batch in btv.await_each(stream) do
    for _, line in ipairs(batch) do
      -- … push each result as it streams in …
    end
  end
end)()
```

The contract is **sequential**: at most one outstanding `:next()` at a time — which
is exactly what the `for` loop does. Batches arriving between steps buffer
internally.

### Combinators

The browser's, on `btv.promise` — for fanning out concurrent work:

| Combinator | Settles when | With |
| --- | --- | --- |
| `all(list)` | all fulfil (or any rejects) | the array of values, in input order |
| `all_settled(list)` | every one settles | an array of `{ status, value }` / `{ status, reason }` (never rejects) |
| `race(list)` | the first settles | that outcome (fulfil or reject) |
| `any(list)` | the first **fulfils** (or all reject) | the first value (else an aggregate error) |

```lua
btv.async(function()
  -- read three files concurrently, wait for all
  local texts = btv.await(btv.promise.all({
    btv.fs.read_text("a.lua"),
    btv.fs.read_text("b.lua"),
    btv.fs.read_text("c.lua"),
  }))
  btv.notify(("read %d files"):format(#texts))
end)()
```

## Scheduling: schedule, on_next_tick, timer

These run a function *later* but aren't one-shot results, so they stay
**callback-shaped** (a promise models one eventual value — the wrong shape for a
microtask or a repeating timer). Reach for the right one:

| Primitive | Runs `fn`… | Use for |
| --- | --- | --- |
| `btv.schedule(fn)` | at the **end of the current tick** (a microtask) | defer off the current call stack, same convergence |
| `btv.on_next_tick(fn)` | on the **next event-loop tick** | observe state the server only refreshes *between* ticks |
| `btv.timer(fn, ms)` | after `ms` **wall-clock** ms (returns a `:stop()` handle) | delays, polling, self-rescheduling timers |
| `btv.wait_for(pred, opts)` | polls `pred` **between ticks**, fulfils a promise with its truthy value | await a cross-tick condition |

> **The cross-tick gotcha.** `btv.schedule` runs within the *same* convergence, so it
> **cannot observe** anything that only refreshes between server ticks — a
> freshly-mounted window's id, a server-repopulated mirror. Re-arming
> `btv.schedule(self)` to "wait" for such a value spins forever. For anything
> cross-tick use `btv.on_next_tick` or `btv.wait_for` — the latter is the await-able
> form:

```lua
btv.async(function()
  local v = btv.view.create({}); v:mount({ float = { width = 40, height = 8 } })
  local win = btv.await(btv.wait_for(function() return v:winid() end))  -- settles when the winid exists
  vim.wo[win].number = false
end)()
```

`btv.wait_for(pred, { tries = 200, interval = nil, message = … })` checks once
immediately, then once per tick, and **rejects** (so an `await` fails loud) if the
condition never holds within `tries`.

## Helpers

- **`btv.utils.debounce(fn, ms)`** — coalesce a *burst* of calls into one trailing
  invocation `ms` after the last (which-key's show-delay, on-change/resize/scroll
  reactions). Callback-shaped, not a promise — a different job — but they compose:
  pass an `btv.async` function as `fn` to kick await-able work after the quiet period.
- **`btv.promise.wrap(fn)`** — lift a single-callback async function into a
  promise-returning one (it appends a resolver as the last argument). The bridge for
  a `vim.*`-style callback API into the promise world.
- **`btv.schedule_wrap(fn)`** — a function that, when called, defers `fn` to the end
  of the tick — the `btv.schedule` analogue of a wrapped callback.

## Try it

| Example | Shows |
| --- | --- |
| [`async-runtime`](https://github.com/bemtvi/bemtvi/tree/main/examples/async-runtime) | The bare loop — `btv.schedule` vs `btv.timer`, and a self-rescheduling timer firing on wall-clock time while the editor stays responsive. |
| [`ui-picker`](https://github.com/bemtvi/bemtvi/tree/main/examples/ui-picker) | `btv.run_stream` + `btv.await_each` streaming `rg` results into a source. |

```sh
BEMTVI_CONFIG=examples/async-runtime cargo run -p bemtvi -- examples/async-runtime/sample.txt
```

## How it works (in brief)

`btv.promise` is **pure Lua** layered on the editor's existing async runtime: a
promise's reactions run as microtasks via `btv.schedule`, exactly the way the browser
runs `.then` callbacks off the current stack, so `:next` is always asynchronous and
the ordering matches Promises/A+. `btv.async` drives a coroutine one resume at a time —
each `btv.await` yields the awaited promise, and the runner re-enters the coroutine
with the value (or re-raises the reason) once it settles.

Under the hood a background **actor owns timers and process I/O** and wakes the
editor loop when something completes, so deferred work and subprocesses run *off* the
input tick without ever stalling the editor. The transports stay native
(`btv._system_async` / `btv._spawn_stream` for process, the `btv._fs_*` bridge for
files, the daemon on wasm); only the *shape* is promises.

## See also

- [Writing bemtvi plugins](plugin-authoring.md) — where this fits in a plugin.
- [UI primitives](features/ui-primitives.md) — `btv.component`'s `setup`/`render` are
  `btv.async` contexts; widgets return promises.
- [ADR 0002 — native plugin system](decisions/0002-native-plugin-system.md) — why
  promise-only, and the `vim.*` alias whitelist.
