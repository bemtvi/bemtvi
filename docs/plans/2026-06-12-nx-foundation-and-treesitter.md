# `nx.*` foundation + two-noun treesitter — implementation plan

> **Status: COMPLETE (2026-06-12).** All five phases landed (nx.* foundation →
> two-noun TS core → nx.bo/nx.treesitter front doors → vendored deletion → this
> docs reconcile), plus the LuaJIT removal. Supersedes the scope of
> [the vendored-treesitter deletion plan](2026-06-12-vendored-treesitter-deletion.md)
> (now Phase 4 here). Stands up the real `nx.*` config namespace as the
> canonical surface — `vim.*` becomes the thin alias whitelist
> ([ADR 0002](../decisions/0002-native-plugin-system.md)) — then refactors
> treesitter highlight control onto declarative buffer state and deletes the
> vendored `vim.treesitter` Lua.

## What we found before starting (reality vs. the earlier docs)

Grounding that reshaped the plan:

- **`nx.*` does not exist yet.** The whole implemented Lua surface is `vim.*`
  (`install.rs` registers only the `vim` global). "Build `nx.*` first" therefore
  means actually standing up the namespace, not wiring thin contracts.
- **`filetype` is *already* the treesitter control noun in core.** `:set
  filetype=X` → `ts_start`, `:set ft=` (empty) → `ts_stop`, `:set ft&` →
  `ts_reset` ([`options.rs:149`](../../crates/nxvim-core/src/editor/options.rs)).
  The gap is only that the `vim.bo.filetype` *Lua proxy* writes a dead Lua store
  instead of the core. Core is the no-Lua path (works on the web build).
- **Core today is one-noun** (`filetype`, `""` = off); the committed spec/ADR
  say **two nouns**. Decision (David, 2026-06-12): build **two nouns** — add an
  orthogonal `ts_highlight`, so `filetype=rust` + `ts_highlight=false` keeps
  LSP/indent on rust while TS highlighting is dark.

Decisions locked: **D1 = delete** the Lua parser/query API (no consumer today);
**D2 = keep query execution, drop the vendored merge**; **two-noun** model;
**broader `nx.*` foundation** (not a treesitter-only slice).

## Inversion strategy (`nx.*` canonical, `vim.*` alias)

All machinery is already implemented under `vim.*`. Rather than rewrite it, a
new last-loaded `prelude/nx.lua` chunk builds the `nx` table whose members are
the **same objects/functions** the `vim.*` surface exposes, and then re-points
the whitelisted `vim.*` names at them (`vim.g = nx.g`, `vim.keymap = nx.keymap`,
…). Net behavior is identical; the canonical name flips to `nx`. Where `vim`
exposed only a partial API (e.g. `vim.api.nvim_create_autocmd`), `nx.on` /
`nx.command` are the canonical verbs and the `vim.api.*` entry forwards to them.

Surfaces in this slice (per the broader-foundation decision): `nx.g`/`b`/`w`,
`nx.o`/`opt`/`opt_local`/`bo`/`wo`, `nx.keymap.set`/`del`, `nx.on`,
`nx.command`, `nx.cmd`, `nx.notify`/`schedule` (callback-async as available),
plus `nx.treesitter` (Phase 3). Other surfaces (`nx.complete`, `nx.picker`, …)
are out of scope — separate roadmap slices.

## Phases (failure-safe order; build + test green at each)

### Phase 1 — `nx` namespace foundation
`prelude/nx.lua`, loaded last in `PRELUDE_MODULES`. Build the `nx` table over
the existing implementations; alias the whitelisted `vim.*` names onto it. No
behavior change. **Verify:** `cargo test --workspace` green (every `vim.*` call
site still works through the alias) + a smoke test that `nx.o`/`nx.keymap`/
`nx.on`/`nx.command` drive the same effects as their `vim.*` twins.

### Phase 2 — two-noun treesitter core
Restructure `Editor::ts_override` into independent *language* (filetype) and
*enabled* (`ts_highlight`) axes; add the `ts_highlight` buffer option + core
field; update `ts_language_for` and the highlight gate accordingly. Wire
`filetype` (str) and `ts_highlight` (bool) through
`set_buffer_option_str`/`_bool`. Keep `:set filetype`/`:setf` semantics.
**Verify:** highlighting + indentation suites green; add a test for the
orthogonal `filetype=rust, ts_highlight=false` case.

### Phase 3 — `nx.bo`/`nx.treesitter` front doors + migrate tests
`nx.bo.filetype` becomes a *wired* option (write → core str option, reaching the
existing `apply_set_str` filetype seam); add `nx.bo.ts_highlight`. Add
`nx.treesitter.set_query(lang, name, text)` → `Engine::set_query` directly (D2).
Reduce `vim.treesitter.start`/`stop` to aliases writing `nx.bo.filetype` /
`ts_highlight` (no TsOp, no vendored Lua). Migrate `syntax.rs` (7 `start` / 3
`stop`; 19 `query.*` / 1 `get_parser`) and `autocmds.rs` onto the front doors.
**Verify green BEFORE Phase 4** — proves the native path carries highlighting.

### Phase 4 — delete the vendored surface
Execute
[the deletion plan](2026-06-12-vendored-treesitter-deletion.md): all of
`crates/nxvim-lua/src/vendor/nvim/`, `prelude/treesitter.lua`,
`nxvim-ts/src/lua.rs` + its `runtime.rs:500` call (D1), `treesitter_lua.rs` (D1),
and the bridge plumbing (`VENDORED_TS_LUA` / `register_vendored_modules` /
`resolve_ts_query` in `runtime.rs`; `_ts_start`/`_ts_stop`/`_ts_set_query` in
`install.rs`; `TsOp::Start`/`Stop`/`SetQuery` in `effects.rs` + the enum
variants). **Leave the `vendor/neovim` submodule alone.** **Verify:** workspace
build + `clippy -D warnings` + test green.

### Phase 5 — reconcile docs
Update the spec + ADR 0002 so the built `nx.*` surfaces read as real (not
proposal) and confirm the two-noun model as shipped; mark the deletion plan
COMPLETE; update the `nx.*`/`nx.treesitter` roadmap entries in
`architecture.md`; ship an `examples/` config exercising `nx.bo.filetype` +
`ts_highlight` + `nx.treesitter.set_query` end to end.

## Out of scope

- The rest of the `nx.*` surface (`nx.complete`/`picker`/`statusline`/
  `snippet`/`tree`/`spawn`/`timer`/`fs`/`ui`) — separate slices.
- The `vendor/neovim` reference submodule — stays.
- `vim.lsp` deletion/refactor — its own plan.
