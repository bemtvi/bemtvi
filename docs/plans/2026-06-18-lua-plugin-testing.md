# Lua plugin testing: `btv.test` + `bemtvi --test-plugin`

Status: in progress (started 2026-06-18)

## Why

bemtvi's prime directive is that every feature that can be an `btv.*` Lua plugin
*is* one (ADR 0002, the dogfooding memory). But plugins are pure Lua living in
their own repos, and the only test infrastructure is the **Rust** black-box
harness (`crates/bemtvi-test-harness`) — unreachable from a plugin author's Lua
repo. Building `bemtvi-keys-helper` surfaced this: the plugin is real, but its
author has no way to write a regression test for it.

This adds first-class **Lua** testing affordances plus a headless runner, so a
plugin repo can carry its own suite and run it in CI with one command.

## Shape (decided)

- **API:** `btv.test.describe / it / expect`, native to `btv.*` (not a busted/
  nvim-compat clone). Each `it` body receives an async-aware **context** `t`
  whose driving methods settle the editor before returning, so test bodies read
  top-to-bottom with no explicit await ceremony.
- **Runner:** `bemtvi --test-plugin [dir]` (dir defaults to cwd). Discovers
  `<dir>/test/**/*_spec.lua`, auto-adds the plugin to the runtimepath, prints a
  per-test report, exits `0` (all pass) / `1` (any fail or error).
- **Scope of first cut:** state assertions *and* Lua UI/redraw inspection
  (floats, messages, statusline), so `bemtvi-keys-helper` ships a real suite.

## Author-facing example (target)

```lua
-- test/popup_spec.lua  (in the plugin repo)
btv.test.describe("bemtvi-keys-helper", function()
  btv.test.before_each(function()
    require("bemtvi-keys-helper").setup({
      delay = 0,
      spec = { { "<leader>f", group = "file" } },
    })
    btv.keymap.set("n", "<leader>ff", function() end, { desc = "find file" })
  end)

  btv.test.it("shows the leader menu on pause", function(t)
    t:feed("<Space>")
    t:wait_for(function() return t:float() ~= nil end)
    btv.test.expect(t:float().text).to_contain("+file")
    btv.test.expect(t:float().text).to_contain("find file")
  end)
end)
```

## The hard constraint: the tick model

Fed keys settle at the **end of a tick** (`drain_feedkeys` runs in the
convergence after the current Lua entry returns), and the Lua state mirrors
(`btv._bufs`, `btv._cur_cursor`, …) are refreshed by the server **before each Lua
entry**. So a single synchronous Lua chunk that feeds then reads sees stale
state — exactly why the Rust harness uses a separate round-trip per assertion.

The Lua API therefore runs each `it` body as an **`btv.async` coroutine**, and the
context's driving methods `btv.await` internally:

- `t:feed(keys)` → queue via `btv._feedkeys`, then `btv.await` one tick
  (`btv.on_next_tick`) so the keys drain and the mirrors refresh before the next
  line runs. Deterministic (synchronous) key handling settles in one tick;
  async effects (timers/debounce) need `t:wait_for`.
- `t:wait_for(pred[, opts])` → `btv.await(btv.wait_for(pred, opts))`.
- `t:exec(fn)` → run `fn` now (already on a fresh-mirror tick).
- reads (`t:lines`, `t:cursor`, `t:mode`, `t:float`, …) are plain mirror reads,
  correct because they run *after* an await.

This reuses the existing promise/async/timer machinery wholesale — no new
scheduling primitives.

## Phases

### Phase 1 — runner + framework + state asserts ✅ DONE (2026-06-18)

Landed: `prelude/test.lua` (describe/it/before_each/after_each/expect + async
context with feed/cmd/exec/wait_for/sleep + state reads), `btv.now_ms()` native,
`crates/bemtvi/src/test_runner.rs` + the `--test-plugin` flag, per-test fresh-slate
isolation (`enew!`), and `crates/bemtvi/tests/test_plugin_runner.rs` (pass/fail/
isolation coverage). UI reads (`t:float`/`t:message`/`t:statusline`) are present but
read a `btv._ui` mirror that Phase 2 populates.

Original scope:

1. **`btv.test` prelude module** (`crates/bemtvi-lua/src/prelude/test.lua`):
   - `describe(name, fn)`, `it(name, fn)`, `before_each`/`after_each`, nested.
   - `expect(value)` with `.to_equal` (deep), `.to_be`, `.to_contain`
     (string/list), `.to_be_truthy/falsy`, `.to_error`, each with `.never`.
   - The context `t`: `feed`, `type`, `wait_for`, `exec`, `lines`, `cursor`,
     `mode`, `current_line`, `keymaps(mode)`, `buf`.
   - `btv.test._run()` → runs every registered test as an async coroutine in
     sequence, captures `{name, status, ms, error}` into `btv.test._results`,
     resets registry. Failures are caught per-test (one bad test ≠ aborted run).
   - Gated to load only in test mode (cheap module, but keep `btv.test` absent in
     normal runs so it can't leak into plugin runtime).
2. **Headless runner** (`crates/bemtvi/src/test_runner.rs` + arg in `main.rs`):
   - Parse `--test-plugin [dir]`. Boot a server thread over a duplex + connect
     an RPC client (the harness `spawn`/`attach` pattern, on `bemtvi-rpc` so the
     binary needs no dev-dep). Config: no user init.lua, runtimepath =
     `[plugin_dir]`, clipboard disabled, no shada — hermetic.
   - Discover `<dir>/test/**/*_spec.lua` (sorted). For each, `exec_lua` to load
     it (registers tests). Then `exec_lua("btv.test._run()")` and poll
     `btv.test._results` to completion (each poll advances a tick; the server's
     own timer loop advances awaits autonomously) with a wall-clock cap.
   - Format the report; exit code from pass/fail counts.
3. **Self-test:** a tiny fixture plugin under `crates/bemtvi/tests/fixtures/` plus
   a Rust integration test that shells the runner (or calls its entry) and
   asserts the exit code + summary — so the runner itself is covered by the Rust
   suite (the runner is Rust; this is its black-box test).

### Phase 2 — Lua UI/redraw inspection ✅ DONE (2026-06-19)

Landed: `LuaRuntime::set_ui_mirror` populates `btv._ui = { float, message, cmdline,
statusline }` from the redraw projection (`chunk_runs_text` flattens the status
chunk runs); `t:float()` / `t:message()` / `t:cmdline()` / `t:statusline()` read it.
**Gated** behind an `EditHost.test_mode` flag flipped by a new `btv_enable_test_mode`
RPC (the runner sends it after attach), which also installs `btv.test` (kept out of a
normal session via `btv._install_test`) — so neither the API nor the per-redraw mirror
exists outside `--test-plugin`. `test_mode_gating.rs` covers the gate. Fixed a
`fresh_slate` bug (must feed `<Esc>` through the matcher, remap=true, to clear a
pending mapping-prefix between tests). Dogfood: `bemtvi-keys-helper/test/popup_spec.lua`
— 5 tests green via the runner (leader menu, group naming, descend, abort/close,
built-in `z` grammar).

Original scope:

1. Server mirrors the projected UI into a Lua-readable snapshot at redraw time
   (or before Lua entry): `btv._ui = { float = { lines, text, title, chunks },
   message, statusline, cmdline }`, sourced from the same projection
   `redraw.rs` already builds for clients (float at `redraw.rs:274`, message at
   `:237`, cmdline at `:224`).
2. Context reads: `t:float()` (nil when closed; `.text` joined, `.lines` chunk
   runs, `.title`), `t:messages()` / `t:message()`, `t:statusline()`.
3. Dogfood: write `test/popup_spec.lua` (+ group-naming, availability-filter
   cases) in the `bemtvi-keys-helper` repo; run `bemtvi --test-plugin` green; note
   it in that repo's README.

### Phase 3 — hermetic seams + docs ✅ DONE (2026-06-19)

Landed: `btv.test.clipboard.seed/peek/clear` (op over `Editor::clipboard_seed` +
`btv._ui.clipboard` mirror via `Editor::clipboard_contents`; the runner installs an
in-memory `MemClipboard` in test mode so `"+`/`"*` round-trip), `btv.test.tempdir()`
(`btv._test_tempdir` native), `btv.now_ms()` (phase 1). Author guide:
`docs/specs/2026-06-19-lua-plugin-testing.md`. Coverage: the
`clipboard_and_tempdir_seams` runner test. **Deferred**: a virtual/deterministic
clock for `btv.timer` (real time + `t:wait_for`/`t:sleep` cover debounce/timeout; a
fake timer wheel is a larger change, intentionally not stubbed).

Original scope:

1. Optional Lua seams: `btv.test.clipboard` (seed/peek, over the `"+`/`"*`
   registers), temp-dir helper, deterministic clock — for plugins that touch
   clipboard/fs/time.
2. Docs: a `docs/specs/2026-06-18-lua-plugin-testing.md` author guide, a mention
   in `architecture.md` (Testing philosophy), and a `test/` example in the
   plugin template.

## Open questions / risks

- **Poll vs. push for results:** start with polling (simplest, advances ticks);
  revisit a `btv_test_done` notification if latency matters.
- **`t:feed` settle depth:** one tick for synchronous input; document that async
  UI (debounce) requires `t:wait_for`. `delay = 0` in setup makes which-key
  deterministic for tests.
- **`btv.test` visibility:** only register it in test mode to avoid leaking a test
  API into normal plugin runtime.
```
