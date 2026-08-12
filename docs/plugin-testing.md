# Testing plugins

bemtvi plugins are pure Lua over the `btv.*` API ([ADR 0002](decisions/0002-native-plugin-system.md)),
so their tests are too. A plugin repo carries a `test/*_spec.lua` suite that drives
a **real** editor — feeds vim keys, then asserts on the resulting buffer, cursor, or
UI — run headlessly by **`bemtvi --test-plugin`**. No mocks, no stubs: the same
end-to-end philosophy as bemtvi's own Rust black-box harness
(`crates/bemtvi-test-harness`), reachable from your plugin's own repo and CI.

The framework is **`btv.test`** — `describe` / `it` / `expect` with a small async
context — shaped like a familiar BDD test runner (busted / Jest), so a spec reads
the way you'd expect.

## Quick start

Put specs under `test/` in your plugin repo (each file must end `_spec.lua`):

```lua
-- test/my_plugin_spec.lua
btv.test.describe("my-plugin", function()
  btv.test.before_each(function()
    require("my-plugin").setup({})
  end)

  btv.test.it("inserts text", function(t)
    t:feed("itext<Esc>")                          -- type in insert mode, then escape
    btv.test.expect(t:lines()).to_equal({ "text" })
    btv.test.expect(t:mode()).to_be("n")
  end)
end)
```

Run it — defaults to the current directory:

```sh
bemtvi --test-plugin                 # runs ./test/**/*_spec.lua
bemtvi --test-plugin path/to/plugin  # or an explicit plugin dir
```

The runner boots an embedded editor with your plugin on the runtimepath (so
`require("<your-plugin>")` resolves), runs every spec, prints a report, and exits
**`0`** (all pass) / **`1`** (any fail) — drop it straight into CI.

## The hermetic slate

Each plugin runs in isolation: **no** user `init.lua`, an in-memory clipboard, no
persistence (shada), and **your plugin as the sole runtimepath entry**. Every test
starts from a **fresh slate** — a new empty buffer in normal mode — so one test's
edits never bleed into the next. A test exercises *your* plugin against a clean
editor and nothing else.

## The tick model — why the context is async

The editor is tick-based: fed keys settle at the **end of a tick**, and the Lua
state mirrors refresh **before each Lua entry**. So a single synchronous chunk that
feeds *then* reads would see stale state (the Rust harness uses a fresh RPC
round-trip per assertion for exactly this reason).

`btv.test` handles it for you: every `it` body runs inside an `btv.async` coroutine,
and the context's **driving methods await internally**. `t:feed(...)` queues the
keys *and awaits one tick*, so by the next line the keys have drained and the reads
are current. You write straight-line code; the awaits are under the hood.

Deterministic (synchronous) input settles in one tick. **Asynchronous** effects — a
debounced popup, a timer, a file watch — won't be ready on the next line; await them
with `t:wait_for(predicate)`:

```lua
btv.test.it("shows a debounced popup", function(t)
  t:feed("<Space>")
  local float = t:wait_for(function() return t:float() end)
  btv.test.expect(float.text).to_contain("write")
end)
```

## API

### Structure

| Call | Meaning |
| --- | --- |
| `btv.test.describe(name, fn)` | A group; nestable. |
| `btv.test.it(name, fn)` | A test; `fn` receives the context `t`. |
| `btv.test.before_each(fn)` / `after_each(fn)` | Hooks, resolved per test along the describe chain (order-independent, busted-style — a hook declared after an `it` in the same block still applies to it). |

### Assertions — `btv.test.expect(value)`

Matchers are called with a dot; prefix any with `.never` to invert
(`btv.test.expect(x).never.to_equal(y)`):

| Matcher | Passes when |
| --- | --- |
| `.to_equal(x)` | `value` deep-equals `x`. |
| `.to_be(x)` | `value == x` (identity). |
| `.to_contain(x)` | `value` is a string containing substring `x`, or a list containing element `x`. |
| `.to_match(pat)` | `value` is a string matching the Lua pattern `pat`. |
| `.to_be_truthy()` / `.to_be_falsy()` / `.to_be_nil()` | The obvious. |
| `.to_error([substr])` | `value` is a function that raises when called (optionally with a message containing `substr`). |

### The context `t`

**Driving** methods are async — they settle before returning:

| Method | Does |
| --- | --- |
| `t:feed(keys[, opts])` | Type vim key-notation. `opts.remap` (default true), `opts.insert`, `opts.settle` (extra ticks). |
| `t:cmd(excmd)` | Run an ex-command. |
| `t:wait_for(pred[, opts])` | Await until `pred` is truthy (returns it). `opts = { tries, interval, message }`. |
| `t:sleep(ms)` | Await a wall-clock delay. |
| `t:exec(fn)` | Run `fn` now (it may itself await) and return its value. |

**Read** methods are plain (correct after an await):

| Method | Returns |
| --- | --- |
| `t:lines([first, last])` / `t:line(n)` | Buffer lines. |
| `t:cursor()` | `{ row, col }`. |
| `t:mode()` / `t:mode_info()` | The mode code (`"n"`, …) / the full table. |
| `t:current_line()` | The cursor's line. |
| `t:keymaps([mode])` | The defined maps (maparg shape). |
| `t:float()` | The content float — `{ text, lines, title }` — or nil. |
| `t:message()` / `t:statusline()` | The message / status line text. |

### Hermetic seams

For plugins that touch the clipboard or the filesystem:

- **`btv.test.clipboard.seed(text[, linewise])`** — put text on `"+` / `"*` as if an
  external app set it. **`btv.test.clipboard.peek()`** → `text, linewise` (what a
  plugin wrote). **`btv.test.clipboard.clear()`**.
- **`btv.test.tempdir()`** — a fresh, already-created unique directory; pair with
  `btv.fs` to exercise a plugin's file I/O without collisions.

## A real example

[`bemtvi-keys-helper`](https://github.com/bemtvi/bemtvi-keys-helper) (the
first-party which-key) ships a real suite,
[`test/popup_spec.lua`](https://github.com/bemtvi/bemtvi-keys-helper): it feeds a
leader prefix, `t:wait_for`s the debounced popup, and asserts on `t:float().text` —
group names, leaf descriptions, the built-in `z` grammar, and close-on-abort. It is
a compact model of a UI plugin tested entirely through its observable surface.

## Gating

The whole surface is **off** in a normal editor session: `btv.test` is `nil` and the
UI mirror it reads (`btv._ui`) is unpopulated. It is turned on only by the
`--test-plugin` runner (via the `btv_enable_test_mode` RPC), so a config or plugin
can't accidentally depend on the test API, and a normal session pays none of the
per-redraw mirror cost.

> **Note.** There is no virtual clock yet — tests use real wall-clock time plus
> `t:wait_for` / `t:sleep`, which covers debounce and timeout behavior. Faking the
> timer wheel is tracked as a follow-up.

## See also

- [Writing bemtvi plugins](plugin-authoring.md) — the anatomy a spec tests.
- [Async & promises](async.md) — the `btv.async` / `t:wait_for` machinery the context
  is built on.
- [Native plugin API design](specs/2026-06-11-native-plugin-api.md) — why plugins
  are pure Lua, hence testable as pure Lua.
