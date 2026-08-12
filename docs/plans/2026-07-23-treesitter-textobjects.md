# Tree-sitter text objects (`vif`, `vaf`, `dia`, …)

Status: **Phase 1 complete** (2026-07-23) — awaiting review before Phase 2.

Phase 1 landed all nine steps below. Verified: 5 hermetic engine tests
(`crates/bemtvi-ts/tests/textobjects.rs`), 5 hermetic server keystroke e2e tests
(`crates/bemtvi-server/tests/treesitter_textobjects.rs`), and the real-network
`:TSInstall rust` fetching a 429-line `textobjects.scm` with the
`@function/@parameter/@class/@comment` captures. `cargo fmt` + `clippy --all-targets
-D warnings` clean; existing text-object / key-pending / ts-install suites green.

## Goal

Add tree-sitter-driven text objects to the vim grammar, so an operator or a
visual selection can target a syntactic construct at the cursor:

| keys      | object                     | capture (base)      |
|-----------|----------------------------|---------------------|
| `if`/`af` | inside / around a function | `@function.inner/outer` |
| `ia`/`aa` | argument / parameter       | `@parameter.inner/outer` |
| `ic`/`ac` | comment                    | `@comment.inner/outer`  |
| `it`/`at` | type definition (class)    | `@class.inner/outer`    |

`vif` selects inside the enclosing function; `daf` deletes the whole function;
`ci a` changes the argument; `2if` targets the 2nd enclosing function. Works
identically in **operator-pending** and **visual** mode because both flow through
the existing `ObjectKind` → `text_object_range` → `apply_text_object` path
(`command.rs` / `motions.rs`), which we extend rather than fork.

Object-key mnemonics follow the Helix menu the request cited
(`f`/`a`/`c`/`t`), but the capture-name convention and query source are the
**nvim-treesitter** ecosystem (`.inner`/`.outer`), so users can drop in the large
existing corpus of `textobjects.scm` files unchanged.

### Deferred to Phase 2 (see bottom)
`m` (closest surrounding pair, tree-walk — no capture), `T` (test) and `e`
(entry) — the last two have **no** upstream tree-sitter-textobjects queries and
need hand-authored patterns. Plus: which-key hints listing the object menu, an
`examples/` config, and more bundled languages.

### Deferred to Phase 3
Web/wasm text objects (the JS-side `web-tree-sitter` highlighter would need to run
the `textobjects.scm` query). Native-only for now; the trait default returns "no
object", so web degrades gracefully (like tree-sitter folds did before 4b).

## Architecture (why these seams)

Tree-sitter is a `Box<dyn SyntaxEngine>` the core `Editor` owns
(`crates/bemtvi-core/src/syntax.rs`); the impl is `bemtvi-ts::Engine`. Core stays
pure — it never touches a `Tree`. The **exact** template is the `folds` query,
added the same way: a compiled `Option<Query>` on `Grammar`, an `Engine::folds`
method that runs it, a `SyntaxEngine::folds` trait method, and an
`Editor::ts_folds` wrapper that `sync_syntax_engine`s first. Text objects mirror
each of those.

Source of the queries: `nvim-treesitter/nvim-treesitter-textobjects`, at
`queries/<lang>/textobjects.scm` (NOT nvim-treesitter core — that repo 404s for
textobjects). Pinned commit `898ee307df58f854d11cd7edd06472574d48014e`. Files
carry `; inherits:` modelines (e.g. javascript → `ecma,jsx`), so the runtime
merge bridge must include `textobjects`.

## Phase 1 — the four upstream-backed objects, native

### 1. Fetch (`crates/bemtvi-ts/src/install.rs`)
- Add `const NVIM_TS_TEXTOBJECTS_REF` + `nvim_ts_textobjects_ref()` env override
  twin of `nvim_ts_ref()`.
- In `fetch_query_set`, after the nvim-treesitter loop, fetch
  `.../nvim-treesitter-textobjects/<ref>/queries/<lang>/textobjects.scm` (honoring
  `$BEMTVI_TS_MIRROR` via the same `fetch_opt`), write it to
  `<data>/queries/<lang>/textobjects.scm`, add `"textobjects"` to the written
  basenames, and fold its `; inherits:` modeline into the returned inherits set
  (so the inherit-walk in `install_queries` fetches `ecma`'s textobjects too).
- Do **not** add `"textobjects"` to `QUERY_FILES` (that constant is the
  nvim-treesitter-core repo path, which has no textobjects). Keep it separate.

### 2. Compile (`crates/bemtvi-ts/src/loader.rs`)
- Add `pub textobjects: Option<Query>` to `Grammar` (declare it among the query
  fields that drop before `_lib`).
- Load it in `Grammar::load` via the existing `load_optional_query(…, "textobjects", …)`.

### 3. Query it (`crates/bemtvi-ts/src/engine.rs`)
- `is_engine_query`: add `"textobjects"` (so runtimepath overlays can merge, like
  folds/injections).
- New method
  `pub fn text_objects_at(&mut self, buffer, capture: &str, byte: usize) -> Vec<(usize,usize)>`:
  run `grammar.textobjects` over the whole tree (folds pattern), keep captures
  whose name == `capture`, whose byte range **contains** `byte`
  (`start <= byte < end`), returning `(start_byte, end_byte)` sorted
  **innermost-first** (smallest span first). Empty on no grammar / no query / no
  match. Injections are ignored for now (host-tree only).
- `pub fn text_objects_available(&self, buffer) -> bool` — grammar loaded with a
  `textobjects` query (the `folds_available` twin).

### 4. Trait (`crates/bemtvi-core/src/syntax.rs`)
- `fn text_objects_at(&mut self, _buffer, _capture, _byte) -> Vec<(usize,usize)> { Vec::new() }`
- `fn text_objects_available(&self, _buffer) -> bool { false }`
  Both default so bare-core and the wasm highlighter compile unchanged.

### 5. Editor wrapper (`crates/bemtvi-core/src/editor/syntax.rs`)
- `pub(crate) fn ts_text_objects_at(&mut self, buf, capture, byte)` — call
  `sync_syntax_engine(buf)` then the engine method (the `ts_folds` twin).

### 6. Object alphabet (`crates/bemtvi-core/src/editor/command.rs`)
- `ObjectKind`: add `TsCapture(&'static str)` carrying the base capture name.
- `from_key`: `'f' => TsCapture("function")`, `'a' => TsCapture("parameter")`,
  `'c' => TsCapture("comment")`, `'t' => TsCapture("class")`.
- `text_object_continuations`: add the four hint rows.

### 7. Range resolution (`crates/bemtvi-core/src/editor/motions.rs`)
- `text_object_range` is `&self`, but the engine query needs `&mut self`. Add the
  ts branch in the **caller** instead: in `command.rs`'s
  `ResolvedCommand::TextObject` arm (and `apply_text_object_once` for multicursor),
  when `kind` is `TsCapture`, resolve via a new `&mut self`
  `ts_text_object_range(ia, base, count) -> Option<(usize,usize,bool)>` (in
  motions.rs), else the existing `text_object_range`. `ts_text_object_range`:
  `capture = format!("{base}.{}", if ia=='i' {"inner"} else {"outer"})`; take the
  `count`-th innermost containing range; charwise (`linewise=false`). If `inner`
  yields nothing, fall back to `outer` (upstream often omits `@x.inner`, e.g.
  rust `@comment`). Feeds the **unchanged** `apply_text_object`.

### 8. Runtime inherit merge (`crates/bemtvi-server/src/treesitter.rs`)
- Add `"textobjects"` to the query-name list in `resolve_runtimepath_queries`
  (the `["highlights","indents","injections"]` set) so js/ts merge `ecma`'s
  patterns and users can extend via `after/queries/<lang>/textobjects.scm`.

### 9. Tests
- **Hermetic engine test** — `crates/bemtvi-ts/tests/textobjects.rs`: use the
  `fixture` module (`install_rust_grammar` compiles `tree-sitter-rust` from the
  cargo registry, no network), `write_query(root,"rust","textobjects", …)` with a
  small real rust textobjects query, open a buffer, assert `text_objects_at`
  returns the right byte ranges for `function.inner/outer`, `parameter.*`,
  `class.*`, and that containment + innermost-first ordering hold for nested
  functions. Mutation-check by moving the cursor outside → empty.
- **Server e2e** — `crates/bemtvi-server/tests/treesitter_textobjects.rs`: compile
  the rust grammar into a temp `BEMTVI_DATA_DIR` (reuse the fixture-style compile
  helper), open a `.rs` buffer, and assert the keystroke path: `vif` selects the
  function body, `daf` deletes the whole function, `dia` deletes an argument,
  `2if` targets the outer of two nested functions. Serialized on `serial_lock`
  (process-global `BEMTVI_DATA_DIR`), like `treesitter_folds.rs`. Hermetic if `cc`
  is present; otherwise skip-if-missing per the external-dependency convention.

### Phase 1 acceptance
`vif`/`vaf`/`via`/`vaa`/`vic`/`vac`/`vit`/`vat` and their operator forms
(`d`/`c`/`y` + object) work on a rust (and, via fetch, python/lua/js/ts/go/c/cpp)
buffer with the grammar installed; count expands to enclosing scopes; no object
at cursor keeps the selection (visual) / is a no-op (operator). `cargo fmt` +
`clippy -D warnings` clean; hermetic engine test green.

## Phase 2 (scoped down 2026-07-23) — **complete**

Just the two polish items — `m` / `T` / `e` are dropped (the last two have no
upstream nvim-treesitter queries; Helix has them under `@test.*`/`@entry.*` with
`.inside`/`.around` naming, a possible future fetch source, but out of scope now).

- **which-key hints for the object menu** — DONE. It turned out Phase 1 already
  wired this: `text_object_continuations()` (with the four ts rows) is emitted for
  the `TextObjectPending` stage at `command.rs:1605`, so the pending-key oracle
  (`btv.on_key_pending`) already lists `f`/`a`/`c`/`t` in both operator (`di`) and
  visual (`vi`) mode. Like the bracket/quote objects they are shown as the object
  *alphabet* — always offered, not gated on grammar availability (consistent with
  `di(` showing with no paren at the cursor). Locked in by two new tests in
  `crates/bemtvi-server/tests/key_pending.rs`
  (`{,visual_}text_object_introducer_lists_treesitter_kinds`).
- **`examples/treesitter-textobjects/`** — DONE. `init.lua` (header + numbered
  sections + a TRY-IT list + a `:TextObjects` cheatsheet command) and `sample.rs`
  (nested fns, args, a struct, comments). Run with
  `BEMTVI_CONFIG=examples/treesitter-textobjects cargo run -p bemtvi -- examples/treesitter-textobjects/sample.rs`
  (then `:TSInstall rust` once). Config load verified end-to-end via a throwaway
  harness check (not committed, per the examples convention).

## Phase 3 — user-extensible registry (`btv.textobject.map`) — **complete** (2026-07-23)

Let users add their own tree-sitter objects, without being forced into bemtvi's
`.inner`/`.outer` convention — the mapping is `full i/a + key` → **exact capture**:

```lua
btv.textobject.map("il", "@loop.inner")            -- vil / dil
btv.textobject.map({ ik = "@call.inner", ak = "@call.outer" })
btv.textobject.map("if", "@function.inside")        -- override a built-in (Helix naming)
btv.textobject.unmap("il")
```

- Core: `Editor.textobject_map: HashMap<String,String>` (lhs → capture) +
  `set_textobject_map` / `textobject_map_entries`. `ResolvedCommand::TextObject`
  now carries the raw `key: char` (not a pre-resolved `ObjectKind`); `parse_step`
  accepts **any** char after `i`/`a` (an unknown key resolves to nothing and
  cancels, same as the old `AbortObject`). `resolve_text_object(ia, key, count)` is
  the single dispatch: **registry first** (so it can add keys *and* override a
  built-in), then the built-in alphabet, then `None`. Registry captures are used
  **verbatim** (`ts_text_object_range_capture`, strips a leading `@`, no
  inner/outer suffixing, no fallback) — so Helix `.inside`/`.around` or any custom
  capture works. `command_pending` appends registered entries to the object menu
  (overriding a built-in key replaces its row).
- Plumbing: `TextObjectOp` (ops.rs) → `textobject_ops` queue (runtime.rs) →
  `btv._textobject_map` (install.rs) → drained **unconditionally** in effects.rs
  (`set_textobject_map`) — plain editor state, so it applies in every build, not
  gated behind the native `ts_ops` path. Prelude `btv.textobject.map`/`.unmap`
  (`btv.lua`) with validation + table form.
- Tests: `user_registered_object_key_resolves`,
  `user_registry_overrides_a_builtin_verbatim`, `unmap_reverts_to_the_builtin`
  (server e2e), `text_object_menu_lists_user_registered_objects` (which-key). The
  example maps `l`/`k`/`r` → loop/call/return.

Query customization was *already* possible before this (drop
`queries/<lang>/textobjects.scm` / `after/queries/...` on the runtimepath, merged
by the bridge; or `btv.treesitter.set_query(lang, "textobjects", …)`). Phase 3 adds
the missing half: binding keys to captures.

## Phase 4 — web/wasm — **complete** (2026-07-23)

Text objects now work in the browser build. On web tree-sitter runs JS-side
(`web-tree-sitter`, `.wasm` grammars) — the native `bemtvi-ts::Engine` can't run in
wasm (it dlopens `.so`) — so this mirrors the **folds** seam exactly, over the
synchronous `eh_js_ts_*` FFI bridge (the one place the wasm tick calls into JS).

- **Rust** (`bemtvi-edithost/src/lib.rs`): `WasmSyntax::text_objects_at` /
  `text_objects_available` (the `folds` twin, grow-and-retry i32 out-buffer) over two
  new `extern "C"` imports `eh_js_ts_textobjects(lang, text, capture, byte, out, cap)`
  / `_available`. The core's shared `resolve_text_object` logic is unchanged — it just
  gets ranges from `WasmSyntax` instead of `bemtvi-ts::Engine`.
- **JS**: `eh-lib.js` implements the two imports (forward to `globalThis.__bemtviTs…`).
  `ts-textobjects.js` is the new worker-thread runner (mirrors `ts-folds.js`): loads
  the grammar + `textobjects.scm`, runs the query synchronously, unions per match,
  keeps regions containing the cursor, innermost first — a JS port of
  `engine.rs::text_objects_at`. It converts **UTF-8 byte offsets (core) ↔ UTF-16
  units (web-tree-sitter)** at the boundary (`byteToU16`/`u16ToBytes`), the one wrinkle
  folds/indent avoid by being line-based. `worker.mjs` installs it, warms it per frame,
  counts its pending loads, and evicts it on `:TSInstall`.
- **Query sourcing**: `grammars.js` `textobjectsSource` + `NVIM_TS_TEXTOBJECTS_REF`
  (the separate nvim-treesitter-textobjects repo) + `textobjects` in `QUERY_KINDS`;
  `highlight.js` install fetches + caches `textobjects.scm` to OPFS; `gen-treesitter.mjs`
  vendors it offline (`vendor/textobjects/<lang>.scm` + `textobjects.json`, sanitized).
- **Pre-existing fix**: `bemtvi-server`'s wasm (`--no-default-features`) build was broken
  at `extmarks.rs:359` (unconditional `self.syntax_states`, a native-only field) —
  cfg-gated it (on wasm, highlighting is JS-side, so there are no block-bg lines). This
  was failing at HEAD before this work; fixing it was required to build the web at all.
- **Verified**: `web/verify-treesitter-textobjects.mjs` (Playwright, headless Chromium,
  bundled python) — `daf` deletes a whole function, `dia` deletes an argument. Folds +
  indent web verifies still green (shared worker wiring intact).

## Phase 5 — merge `; inherits:` on the web query path — **complete** (2026-07-23)

The web query-fetch path (offline vendor + `:TSInstall`) fetched only each language's
own `<kind>.scm`, ignoring its `; inherits:` modeline — while native follows it. So
`javascript`'s `indents`/`folds`/`textobjects` (all just `; inherits: ecma,jsx`) came
up **empty** on web. Now a shared follower merges the chain, matching native.

- `grammars.js`: `parseInherits` (mirrors native `parse_inherits`) + `fetchQueryMerged(lang,
  kind, fetchText, base)` — fetches the language's file, recursively fetches each
  inherited language's same-kind file (via an un-registry-guarded `queryUrl`, since
  `ecma`/`jsx` aren't grammars), concatenates **inherited-first, own-last** so own wins.
  `textobjects` pulls from the nvim-treesitter-textobjects repo, the rest from
  nvim-treesitter core. The old `indentSource`/`foldSource`/`textobjectsSource` now
  delegate to `queryUrl`.
- Applied in **both** web sourcing paths: `gen-treesitter.mjs` (offline vendor) and
  `highlight.js` install (runtime OPFS) — for `indents`, `folds`, AND `textobjects`, so
  the fix also filled in js/ts's previously-empty indents/folds (the second half of the
  report: "the other languages don't include folds/indents").
- Result (bundled vendor regen): `javascript` 0→22 indents, 0→2 folds, 0→62 textobjects;
  `typescript` textobjects 12→72. Verified: `verify-treesitter-textobjects.mjs` now
  includes a **javascript** `daf` case (its objects come entirely from merged `ecma`);
  folds + indent web verifies still green.

`@test`/`@entry` objects remain out (would need Helix's queries — not sourced, per
decision).
