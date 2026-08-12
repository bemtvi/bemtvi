# Testing bemtvi plugins (`btv.test` + `bemtvi --test-plugin`)

Author guide for the native Lua test framework. bemtvi plugins are pure Lua over the
`btv.*` API ([ADR 0002](../decisions/0002-native-plugin-system.md)), so their tests
are too: a plugin repo carries a `test/*_spec.lua` suite that drives a **real**
editor and asserts on its state, run headlessly by `bemtvi --test-plugin`.

It is the Lua sibling of the Rust black-box harness (`crates/bemtvi-test-harness`):
same philosophy — feed vim keys, assert on the resulting buffer / cursor / UI —
reachable from a plugin's own repo.

## Quick start

Put specs under `test/` in your plugin repo (files must end `_spec.lua`):

```lua
-- test/my_plugin_spec.lua
btv.test.describe("my-plugin", function()
  btv.test.before_each(function()
    require("my-plugin").setup({})
  end)

  btv.test.it("does the thing", function(t)
    t:feed("itext<Esc>")
    btv.test.expect(t:lines()).to_equal({ "text" })
    btv.test.expect(t:mode()).to_be("n")
  end)
end)
```

Run it (defaults to the cwd):

```sh
bemtvi --test-plugin                 # runs ./test/**/*_spec.lua
bemtvi --test-plugin path/to/plugin  # or an explicit plugin dir
```

The runner boots an embedded editor with your plugin on the runtimepath (so
`require("<your-plugin>")` resolves), runs every spec, prints a report, and exits
`0` (all pass) / `1` (any fail) — drop it straight into CI.

## How it works

The runner boots an embedded server and drives it over the same msgpack-RPC a UI
uses — the harness pattern — then orchestrates the Lua framework. Each plugin runs
hermetically: no user `init.lua`, an in-memory clipboard, no persistence, your
plugin as the sole runtimepath entry. Each test starts from a **fresh slate** (a new
empty buffer in normal mode), so one test's edits never bleed into the next.

### The tick model — why the context is async

Fed keys settle at the **end of a tick**, and the Lua state mirrors refresh **before
each Lua entry** — so a single synchronous chunk that feeds then reads sees stale
state (the Rust harness uses a separate RPC round-trip per assertion for the same
reason). So every `it` body runs inside an `btv.async` coroutine, and the context's
driving methods `btv.await` internally: `t:feed` queues the keys then awaits one tick,
so the keys drain and the mirrors refresh before the next line runs.

Deterministic (synchronous) input settles in one tick. **Asynchronous** effects — a
debounced popup, a timer, a watch — need `t:wait_for(predicate)`:

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
| `btv.test.describe(name, fn)` | group; nestable |
| `btv.test.it(name, fn)` | a test; `fn` receives the context `t` |
| `btv.test.before_each(fn)` / `after_each(fn)` | hooks; resolved per test along the describe chain (order-independent, busted-style) |

### Assertions — `btv.test.expect(value)`

`.to_equal(x)` (deep), `.to_be(x)` (identity / `==`), `.to_contain(x)` (substring or
list element), `.to_match(pat)` (Lua pattern), `.to_be_truthy()`, `.to_be_falsy()`,
`.to_be_nil()`, `.to_error([substr])` (`value` is a function expected to raise).
Prefix any with `.never` to invert: `btv.test.expect(x).never.to_equal(y)`.

### The context `t`

Driving (async — they settle before returning):

| Method | Meaning |
| --- | --- |
| `t:feed(keys[, opts])` | type vim key-notation; `opts.remap` (default true), `opts.insert`, `opts.settle` (extra ticks) |
| `t:cmd(excmd)` | run an ex-command |
| `t:wait_for(pred[, opts])` | await until `pred` is truthy (returns it); `opts = { tries, interval, message }` |
| `t:sleep(ms)` | await a wall-clock delay |
| `t:exec(fn)` | run `fn` now (may itself await), return its value |

Reads (plain, correct after an await):

| Method | Returns |
| --- | --- |
| `t:lines([first, last])` / `t:line(n)` | buffer lines |
| `t:cursor()` | `{ row, col }` |
| `t:mode()` / `t:mode_info()` | mode code `"n"` / the full table |
| `t:current_line()` | the cursor's line |
| `t:keymaps([mode])` | the defined maps (maparg shape) |
| `t:float()` | the content float — `{ text, lines, title }` — or nil |
| `t:message()` / `t:cmdline()` / `t:statusline()` | the message / command / status line text |

### Hermetic seams

- `btv.test.clipboard.seed(text[, linewise])` — put text on `"+` / `"*` as if an
  external app set it. `btv.test.clipboard.peek()` → `text, linewise` (what a plugin
  wrote). `btv.test.clipboard.clear()`.
- `btv.test.tempdir()` — a fresh unique directory (already created); pair with
  `btv.fs` to exercise a plugin's file I/O without collisions.

## A real example

`bemtvi-keys-helper` (the which-key plugin) ships
[`test/popup_spec.lua`](https://github.com/bemtvi/bemtvi-keys-helper): it feeds a
leader prefix, waits for the debounced popup, and asserts on `t:float().text` —
group names, leaf descriptions, the built-in `z` grammar, close-on-abort.

## Gating

The whole surface is OFF in a normal editor session: `btv.test` is nil and the
`btv._ui` mirror is unpopulated. It is turned on only by the `--test-plugin` runner
(the `btv_enable_test_mode` RPC), so a config or plugin can't accidentally depend on
it, and a normal session pays none of the per-redraw mirror cost.

## Not yet supported

A virtual/deterministic clock for `btv.timer` — tests use real time plus
`t:wait_for` / `t:sleep`, which covers debounce/timeout behavior. (Faking the timer
wheel would be a larger change; tracked in
`docs/plans/2026-06-18-lua-plugin-testing.md`.)
