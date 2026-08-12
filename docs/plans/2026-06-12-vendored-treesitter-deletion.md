# Vendored `vim.treesitter` deletion — plan

> **Status: COMPLETE (2026-06-12).** Executed as Phase 4 of
> [the btv.* foundation plan](2026-06-12-btv-foundation-and-treesitter.md), with
> D1 = delete the Lua parser API and D2 = keep query execution / drop the merge.
> The native replacement is `btv.treesitter.set_query`; `vim.treesitter.start`/
> `stop` remain the bounded alias. Removes the vendored neovim
> `vim.treesitter` Lua surface — the API half of
> [ADR 0001](../decisions/0001-native-engines-vendored-lua-apis.md), superseded
> by [ADR 0002](../decisions/0002-native-plugin-system.md) ("there is no
> `vim.treesitter` surface … the vendored neovim Lua itself is deleted"). The
> **native engine** (`bemtvi-ts::Engine`, sync, in-core) is untouched and keeps
> driving highlight/indent/fold/`=`. What goes is the ~4000 lines of vendored
> Lua that existed only to serve plugins calling `vim.treesitter.*` by name, plus
> the bridges and the Lua-facing parser primitives that backed it. The two
> editor-side seams the bridges drove (`Editor::ts_override`,
> `Engine::set_query`) survive and are re-fronted by declarative `btv.*` —
> per the buffer-state model in
> [the native plugin API spec](../specs/2026-06-11-native-plugin-api.md)
> (*Treesitter highlighting is buffer state, not a verb*).
>
> **Context (2026-06-12):** the `300cdb0` "rip out neovim-plugin-compat runtime"
> refactor has landed on `main` — it removed the popup / fuzzy-finder / completion /
> prompt-buffer compat runtime but **left the vendored treesitter surface, the
> bridges, and `bemtvi-ts/src/lua.rs` fully intact**. This plan is the next cut,
> unaffected by that refactor; line anchors below are against `main` after it.

## Two decisions that gate the scope

Most of the work is mechanical; two design calls decide how much **deletes**
versus **migrates**. Resolve them first — the rest of the plan is written
against the recommended answers and flags where the alternative diverges.

### D1 — does `btv.treesitter` expose a *Lua* parser/query API?

Walking trees (`get_node`, `iter_matches`, `get_parser`) for plugins. Today
**nothing native needs it**: bemtvi's own highlight/indent/fold run in Rust on
`bemtvi-ts::Engine`, and the Lua-facing parser primitives (`btv._create_ts_parser`
& co. in [`bemtvi-ts/src/lua.rs`](../../crates/bemtvi-ts/src/lua.rs)) exist *solely*
to back the vendored surface. No shipped plugin walks trees (catppuccin doesn't).

- **Recommended — delete.** Per the no-silent-stubs rule, don't keep a surface
  with zero consumers. Rebuild a parser API under `btv.treesitter` when a real
  textobjects-class consumer lands, in bemtvi's own shape.
- **Alternative — expose.** The primitives survive but are re-homed under `btv.*`
  naming (not `btv._*`), and `treesitter_lua.rs` is rewritten rather than deleted.

### D2 — keep plugin query *customization*?

`query.set`, `after/queries` overlays, `;extends`/`;inherits` merge. The merge
logic lives entirely in the vendored `query.lua`. The **execution** seam is
native and survives: `Engine::set_query` / `set_query_overlay`
([`engine.rs:191`/`227`](../../crates/bemtvi-ts/src/engine.rs)) ←
`Editor::set_ts_query` ([`syntax.rs:194`](../../crates/bemtvi-core/src/editor/syntax.rs)).

- **Recommended — keep execution, drop the Lua merge.** Offer
  `btv.treesitter.set_query(lang, name, text)` straight to `Engine::set_query`.
  Loses neovim's `;extends`/runtimepath resolution; keeps override capability.
- **Alternative — drop entirely.** The native engine reads a single
  `highlights.scm` from its data dir; no plugin override at all.

**Plan assumes: D1 = delete, D2 = keep execution / drop merge** — the maximal
clean deletion consistent with both ADRs.

## What survives (do not touch)

- `bemtvi-ts::Engine` and the grammar loader/installer — the editor's real
  treesitter. Drives redraw highlighting, indent, fold, `=`.
- `Editor::ts_override` + `ts_start` / `ts_stop` / `ts_reset` / `ts_language_for`
  ([`syntax.rs`](../../crates/bemtvi-core/src/editor/syntax.rs)) — the per-buffer
  highlight-language override. **Mechanism unchanged; only its *writer* moves**
  (imperative `TsOp` → declarative option).
- `Engine::set_query` / `set_query_overlay`, `Editor::set_ts_query` — bridge #4's
  execution path (D2 keeps it, re-fronted by `btv.treesitter.set_query`).

## Delete set — the vendored *Lua surface* (not the submodule)

> **Scope note — do not confuse two `vendor` trees.** This plan deletes
> **`crates/bemtvi-lua/src/vendor/nvim/`** — copies of neovim's Lua runtime
> files compiled into the `bemtvi-lua` crate via `include_str!`. It does **not**
> touch the **`vendor/neovim`** git submodule (neovim's full source, kept as a
> behavioral/source reference, never built or linked — CLAUDE.md). The
> submodule stays.

The support files (`F`, `func`, `_memoize`, `_core.util`, `pos._util`) are
required **only** by the treesitter modules (verified — nothing else `require`s
them), so all of `crates/bemtvi-lua/src/vendor/nvim/` goes except its `LICENSE`,
which goes too once nothing vendored remains (no vendored `vim.lsp` exists — it
is prelude-only, out of scope). Paths below are under
`crates/bemtvi-lua/src/vendor/nvim/`.

| File (under `crates/bemtvi-lua/src/vendor/nvim/`) | Lines |
| --- | --- |
| `vim/treesitter.lua` | 548 |
| `vim/treesitter/query.lua` | 1163 |
| `vim/treesitter/languagetree.lua` | 1482 |
| `vim/treesitter/language.lua` | 206 |
| `vim/treesitter/_range.lua` | 181 |
| `vim/{F,func,func/_memoize,_core/util,pos/_util}.lua` | 454 |
| **vendored subtotal** | **~4034** |

## Wiring to remove

- **[`runtime.rs`](../../crates/bemtvi-lua/src/runtime.rs):** the `VENDORED_TS_LUA`
  `include_str!` table (287+), `register_vendored_modules` + its call (def 425,
  call 505), the `resolve_ts_query` method (590, which calls the prelude's
  `_resolved_query_string`), and the "engine data dir on the runtimepath for the
  query resolver" step (456–472).
- **[`prelude/treesitter.lua`](../../crates/bemtvi-lua/src/prelude/treesitter.lua)**
  (175 lines): deleted whole. Its three jobs split — `highlighter.new` notimpl
  stub → gone; the start/stop bridge → migrates (below); the
  `query.set`/`_resolved_query_string` bridge → gone (D2 replaces it with a
  direct setter).
- **[`bemtvi-ts/src/lua.rs`](../../crates/bemtvi-ts/src/lua.rs)** `install()` and its
  call at `runtime.rs:500` → **delete under D1** (keep + re-home under `btv.*` if
  D1 = expose).

## The two bridges

### Bridge #1 (start/stop) → declarative buffer state

Editor side survives untouched. Delete the imperative path feeding it:
`_ts_start` / `_ts_stop`
([`install.rs:1186`/`1194`](../../crates/bemtvi-lua/src/install.rs)), `TsOp::Start` /
`TsOp::Stop`, the `effects.rs:234–241` drain
([`effects.rs`](../../crates/bemtvi-server/src/effects.rs)). Replace with the
`btv.bo.filetype` / `btv.bo.ts_highlight` option-write path writing the same
override. `vim.treesitter.start` / `stop` become the alias desugaring already
recorded in [ADR 0002](../decisions/0002-native-plugin-system.md) point 4.

### Bridge #4 (query) → direct setter

Delete `_ts_set_query` ([`install.rs:1207`](../../crates/bemtvi-lua/src/install.rs)),
the `resolve_ts_query` resolver (`runtime.rs:590`) + the prelude's
`_resolved_query_string`, and the resolve-via-Lua step in the `effects.rs:244`
`SetQuery` handler. Keep `TsOp::SetQuery` → `Editor::set_ts_query` →
`Engine::set_query` as the execution path, now fed by `btv.treesitter.set_query`
directly (D2).

## Tests

- **[`treesitter_lua.rs`](../../crates/bemtvi/tests/treesitter_lua.rs)** (34 uses:
  `get_parser`, `get_node`, `iter_matches`, `query.parse`, `get_string_parser`,
  `language.inspect`) — exercises *only* the deleted surface → **delete the file**
  under D1 (rewrite against `btv.treesitter` if D1 = expose).
- **[`syntax.rs`](../../crates/bemtvi/tests/syntax.rs)** (30 uses): the 7 `start` +
  3 `stop` → **migrate** to `btv.bo.filetype` / `ts_highlight`; the 19 `query.*` +
  1 `get_parser` → migrate to `btv.treesitter.set_query` (D2). These exercise
  bemtvi's *own* highlighting, so they survive in migrated form — highlighting
  must keep working.
- **[`autocmds.rs`](../../crates/bemtvi-server/tests/autocmds.rs)** (1 use) —
  incidental; adjust inline.

## Order (failure-safe)

Step 2 verifies the native path carries the editor's highlighting *before*
step 3/4 remove the old path, so there is never a window where highlighting is
broken.

1. **Land the survivors' new front doors** — `btv.bo.filetype` / `ts_highlight`
   and `btv.treesitter.set_query` (thin contracts over `ts_override` /
   `Engine::set_query`), plus the `vim.treesitter.start` / `stop` aliases onto
   them.
2. **Migrate the tests** — point `syntax.rs` + `autocmds.rs` at the new front
   doors; confirm highlighting suites green. This proves the engine seam before
   any deletion.
3. **Delete the bridges** — `prelude/treesitter.lua`, the `runtime.rs` vendored
   wiring, and `TsOp::Start`/`Stop`/`SetQuery` + the `_ts_*` functions in
   `install.rs` / `effects.rs`.
4. **Delete the vendored Lua** — all of `crates/bemtvi-lua/src/vendor/nvim/`
   (its `LICENSE` too, since nothing vendored remains). **Leave the
   `vendor/neovim` submodule alone** — it is the reference, not vendored Lua.
5. **Under D1** — delete `bemtvi-ts/src/lua.rs` + `treesitter_lua.rs`.

Net removal ≈ 4034 (vendored) + ~1000 (`lua.rs` + `treesitter_lua.rs` + bridge
plumbing) lines.

## Out of scope

- The **`vendor/neovim` git submodule** — neovim's full source, kept as a
  behavioral/source reference (CLAUDE.md). Never built; not vendored Lua; stays.
- `bemtvi-ts::Engine` and grammar install/loading — the native engine stays.
- `vim.lsp` (prelude-only; its own deletion/refactor is a separate plan).
- Building a full `btv.treesitter` Lua parser API — deferred to D1's first real
  consumer; this plan only removes, plus the two thin `btv.treesitter` /
  `btv.bo` front doors needed to keep highlighting and query-override working.
