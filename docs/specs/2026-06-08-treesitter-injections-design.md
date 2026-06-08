# Treesitter injections — Lua resolves the query, the engine executes the layers — design

**Status:** **implemented (Phases 0–4).**
*Phase 0* — the `injections` query name resolves through the bridge and is compiled
+ stored on the grammar (`loader.rs` `Grammar.injections`, `engine.rs`
`is_engine_query`, `treesitter.rs` `resolve_ts_queries_for`, the prelude `query.set`
seam).
*Phase 1* — single-level injection highlighting: `engine.rs` `BufferState.injections`
+ `InjectionLayer`, layer building run after every reparse and on an
injection-query change, `collect_injection_regions`, and a layered `extract_spans`
where injected layers paint over the host.
*Phase 2* — faithful child parsing: the child grammar parses the buffer through
`Parser::set_included_ranges` (`ts_range`/`point_at`), so child trees are in
buffer coordinates; incremental child reparse (`update_injection_layers` replays
the edits onto the surviving child trees, keyed by language); the dynamic
`@injection.language` node-text language form; and a per-pass `INJECTION_DEADLINE`
budget (stale-tree fallback).
*Phase 3* — combined injections + nesting + the remaining directive vocabulary.
`collect_injection_regions` now returns `(language, ranges)` region-sets (combined
patterns accumulate all matches' ranges into one set), resolves the language via
`injection.self` / `injection.parent` (threaded `self_lang`/`parent_lang`), and
masks named children unless `injection.include-children` (`content_ranges`).
`build_injection_layers` is a breadth-first walk that recurses into each layer's
own injections to `MAX_INJECTION_DEPTH`; the painter clips a layer's captures to
its `ranges` (so a combined node spanning the gap paints only within them). A
second fixture grammar, `tree-sitter-md`, drives cross-language / nested / combined
tests. Tested in [`syntax.rs`](../../crates/nxvim/tests/syntax.rs) "injections
bridge, Phase 0/1/2/3" (markdown → rust fence, markdown → rust → rust nesting,
`injection.self`, combined split-comment).
*Phase 4* — the platform half needed no new plumbing: the snapshot parser binding
already honors `set_included_ranges` and `LanguageTree.new` resolves its injection
query via the bridge's `query.get`, so the vendored `LanguageTree:parse(true)`
builds injected child trees over nxvim's snapshot. Verified end to end —
`children()` / `language_for_range` / `get_node(…, ignore_injections=false)` resolve
the injected language (`treesitter_lua.rs`), and a **drift oracle** asserts the
engine's paint agrees with the vendored `_get_injections` for the same buffer
(`syntax.rs` `the_engine_paint_agrees_with_the_platform_injection_resolution`).
This is the fifth worked instance of
[ADR 0001](../decisions/0001-native-engines-vendored-lua-apis.md)'s bridge
pattern, and the deepest: it extends the
[query-resolution bridge](2026-06-08-treesitter-query-bridge-design.md) one name
further (`injections`, alongside `highlights`/`indents`) and then teaches the
native engine to *act* on the resolved query — parse injected sub-languages and
paint them. It closes the gap recorded in
[known-approximations.md](../known-approximations.md): *"A buffer's root tree
parses; `LanguageTree` child languages / `language_for_range` are not wired, so an
injected-language query returns only the root tree."*

The build is split into five phases (0–4), each independently shippable, each with
its own black-box tests. Phases 0–3 deliver the **paint** (the native engine
highlighting injected regions — the visible win). Phase 4 delivers the **platform**
(plugins seeing injected child trees through `vim.treesitter`).

## Problem

Injections are how one buffer holds more than one language: SQL inside a Rust
string literal, Lua inside `vim.cmd[[ ... ]]`, a fenced code block inside markdown,
JavaScript inside an HTML `<script>`. Tree-sitter models this with an
**injection query** (`queries/<lang>/injections.scm`) whose captures mark a node's
text as belonging to another grammar:

```scheme
; rust/injections.scm — regex inside Regex::new("…")
((call_expression
   function: (scoped_identifier path: (identifier) @_re (#eq? @_re "Regex"))
   arguments: (arguments (string_literal (string_content) @injection.content)))
 (#set! injection.language "regex"))
```

The native engine ([`nxvim-ts`](../../crates/nxvim-ts)) parses **one tree per
buffer with one grammar** ([`engine.rs::BufferState`](../../crates/nxvim-ts/src/engine.rs)
holds a single `tree: Option<Tree>` and one `language: String`). It never runs the
injection query, never spawns a child parser, and `extract_spans` runs only the
host grammar's highlights query over that single tree. So every injected region is
painted as whatever the *host* grammar thinks it is — the string body of a regex is
one flat `@string`, the Lua in `vim.cmd[[…]]` is one flat string, a markdown code
block is undifferentiated prose.

The vendored Lua side already has the full machinery — `LanguageTree` in
[`languagetree.lua`](../../crates/nxvim-lua/src/vendor/nvim/vim/treesitter/languagetree.lua)
has `_get_injections` / `_add_injections` / `set_included_regions` / child trees —
but nothing drives it onto the paint, and (per the constraint below) it *can't* drive
the synchronous redraw.

## The central tension — and why it resolves like the query bridge

The query-resolution bridge established the discipline: **the engine runs
synchronously during `redraw` and must never call Lua mid-parse**
([the bridge design](2026-06-08-treesitter-query-bridge-design.md) §"push-on-change,
never pull-in-redraw"). Resolution that needs Lua happens on the async side, ahead
of redraw, at config-time moments; the engine only ever executes a *cached* compiled
artifact.

Injections look like they violate this, because **injected regions are
content-dependent and move on every keystroke** — type a character inside a
`vim.cmd[[…]]` block and the Lua region's byte range shifts. If region detection
required Lua, it would have to run per-redraw, which the constraint forbids.

The resolution is the same split the query bridge already uses, applied one level
deeper:

> **Lua resolves the injection *query*** (which patterns capture what, with
> `;extends`/runtimepath merging) — config-time, pushed once through the **existing**
> bridge as a third query name. **The engine executes injections** — it runs that
> compiled query over the live tree each parse, extracts `(language, ranges)`,
> parses the child layers, and highlights them. All synchronous, all per-edit, no
> Lua in the hot path.

What the engine must port from neovim is **not** query merging (rejected in the
query bridge for good reason — it drifts) but the much smaller, far more stable
**injection directive vocabulary**: how to turn injection-query *captures* into
`(language, ranges, combined?)`. That logic lives in
[`languagetree.lua::_get_injections`](../../crates/nxvim-lua/src/vendor/nvim/vim/treesitter/languagetree.lua)
and is a fixed handful of directives — `injection.language`, `injection.content`,
`injection.combined`, `injection.include-children`, `injection.self`,
`injection.parent`, and the `@injection.<lang>` capture-name shorthand. Unlike the
query-merge rules, this vocabulary is tree-sitter-stable and tiny; porting it is the
acceptable cost, and it is exactly mirrored by a Lua test oracle (Phase 4) so drift
is caught.

This keeps the invariant that defines the whole architecture: **the synchronous
treesitter floor is always underneath** — a missing child grammar, an injection
query that fails, or a parse-budget exhaustion degrades the injected region to "host
grammar's flat span," never to dark or to a hang.

## Design overview

```
                 config-time (async, ahead of redraw)          per-edit (sync, in engine)
  injections.scm ──Lua query.get merges──▶ Engine.set_query ──compile──▶ grammar.injections: Query
                    (existing bridge, +1 name)                                    │
                                                                                  ▼
  root Tree ──run injections query──▶ [(lang, ranges, combined)] ──▶ child parsers + child Trees
                                                                                  │
                                                                                  ▼
  extract_spans gathers host captures + each child layer's captures, child layers paint OVER host
```

Three structural additions to the engine:

1. **A third pushed query name.** `injections` joins `highlights`/`indents` in
   `Engine::set_query` / `set_query_overlay` and in `Grammar` (a new
   `injections: Option<Query>` field). The buffer-open trigger
   ([`treesitter.rs::resolve_ts_queries_for`](../../crates/nxvim-server/src/treesitter.rs))
   resolves `injections` too. **Phase 0**, and it's nearly free — the bridge
   plumbing already exists.

2. **Per-buffer injection layers.** `BufferState` grows a set of child layers, each
   `{ language, parser, tree, ranges }`, derived from the root tree after each
   reparse. **Phases 1–3.**

3. **Layered span extraction.** `extract_spans` gathers captures from the host tree
   *and* each child layer, with child layers winning inside their ranges. **Phase 1**,
   refined through Phase 3.

## Phases

### Phase 0 — push `injections` through the existing bridge (no paint change)

Make `injections` a paint-relevant query name end to end, stored on the grammar,
consumed by nothing yet. This is pure plumbing reuse and proves the resolution
half in isolation.

- `loader.rs`: add `pub injections: Option<Query>` to `Grammar`; `Grammar::load`
  reads/compiles `injections.scm` (optional, like `indents.scm`), consulting the
  override map first.
- `engine.rs`: in `set_query` / `set_query_overlay` / `recompile_query`, accept
  `"injections"` as a third name (today they early-return `Ok(())` for anything but
  `highlights`/`indents`). `read_disk_query` already generalizes over name.
- `treesitter.rs::resolve_ts_queries_for`: iterate `["highlights", "indents",
  "injections"]`.

**Test:** a `query.set('rust','injections', '(...) @injection.content (#set!
injection.language "rust")')` and a buffer-open overlay both compile without echoing
an error; a deliberately broken injections query echoes loud (no-silent-stubs),
exactly like a broken highlights query does today. No paint assertion yet.

> **Why ship 0 alone:** it's the seam the rest stands on, it exercises the bridge's
> generality (the design always said "only `highlights`/`indents` push *today*"),
> and it can't regress the paint because nothing reads `grammar.injections` yet.

### Phase 1 — single-level injection highlighting (static language, non-combined)

The first visible win, covering the common case: one injected region, one named
language, painted with that language's grammar.

After the root reparse, the engine builds **injection layers**:

1. Run `grammar.injections` over the root tree (whole tree — regions can be
   anywhere; the QueryCursor isn't range-limited here, but see the per-frame budget
   below).
2. For each match, interpret directives → `(language, content-range)`:
   - `@injection.content` capture marks the injected node(s); its byte range is the
     region.
   - language from, in priority order: a `(#set! injection.language "lua")`
     directive; else the `@injection.language` capture's **node text** (dynamic —
     e.g. a markdown fence's `info_string`); else the `@injection.<lang>`
     capture-name shorthand (`@injection.lua`).
   - **Phase-1 scope:** static language only (the `#set!` and capture-name forms).
     The dynamic `@injection.language` text form lands in Phase 2 alongside
     combined/included-ranges, because it shares the same region-collection path.
3. For each region, lazily load the child grammar via the existing `grammar(lang)`
   cache (a missing child grammar is silently skipped — best-effort, like a missing
   host grammar). Parse the region. **Phase 1 takes the simplest correct parse: a
   standalone parse of the region's substring**, with the child's own parser. (Phase
   2 upgrades this to `included_ranges` on a buffer-wide child parser, which is more
   faithful for position-sensitive grammars and required for combined/multi-range.)
4. Store the child layer `{ language, tree, range }` on the `BufferState`.

`extract_spans` becomes layer-aware:

- Collect host captures as today, tagged layer 0.
- For each child layer intersecting `[first_line, last_line)`, run the child
  grammar's highlights query over the child tree, **offset** every capture's byte
  range back into buffer coordinates by the region start, and tag it with a deeper
  layer rank.
- Merge: the existing "broadest-first, narrower overwrites" resolution runs per
  line as today, but the per-line `groups[]` fill applies **layer order** — a deeper
  (injected) layer's captures overwrite shallower ones within the overlap. This is
  neovim's "injected language paints over host" rule, expressed as a layer rank in
  the existing sort/fill.

**Self-injection is the load-bearing Phase-1 test fixture** (host == injected ==
`rust`, the one grammar the suite already compiles): an `injections.scm` that injects
`rust` into some host node exercises the *entire* pipeline — region detection,
child-grammar load (cache hit on the host), child parse, child-highlight, byte
offset, layered merge — with **no second grammar required**. Assert that a node which
the host paints flat gets the injected grammar's finer captures inside it.

A true cross-language test (e.g. markdown injecting rust, or rust injecting regex)
follows once a second grammar is added to the test fixture as a dev-dependency
(today only `tree-sitter-rust` is present; see Testing).

**Deferred past Phase 1, explicitly:** combined injections, `included_ranges`
parsing, incremental child reparse (Phase 1 re-derives child layers from scratch
each root reparse — correct, just not yet incremental), nested injections, and
injection-aware indent.

### Phase 2 — faithful child parsing: `included_ranges` + incremental child reparse

Two upgrades that make injected highlighting correct and cheap, not just present.

- **`included_ranges` instead of substring parse.** A child parser is set to parse
  the whole buffer shadow but *restricted* to the injected ranges
  (`Parser::set_included_ranges`). This is faithful for grammars whose lexer is
  position- or boundary-sensitive, lets one child tree own **multiple** ranges
  (the foundation combined injections need), and avoids materializing the substring.
  The dynamic `@injection.language` *node-text* language form lands here too (it
  shares region collection).
- **Incremental child reparse.** Child layers persist on `BufferState` across edits
  keyed by language; on a root edit the engine re-runs the injection query, diffs the
  region set, and `tree.edit` + incrementally reparses surviving child layers rather
  than rebuilding them. New regions spawn layers; vanished regions drop. The root's
  `PARSE_DEADLINE` discipline extends to a **per-frame injection budget** (total
  child-parse wall-clock bounded; on exhaustion, keep last-good child trees and paint
  stale — one frame of lag, never a hang, mirroring the root's cancelled-parse rule).

**Test:** type inside an injected region and assert the injected captures track the
edit (incremental correctness); assert a position-sensitive case the substring parse
would have gotten wrong now paints correctly; assert two sibling regions of the same
language both paint.

### Phase 3 — combined injections and nesting

- **`(#set! injection.combined)`** — all regions of one match-set parse as a *single*
  child tree with multiple `included_ranges` (e.g. every `<script>` block in an HTML
  file as one JS document, so a `function` opened in one block and closed in another
  parses). Built directly on Phase 2's multi-range child parser: combined = one layer
  owning N ranges; non-combined = N layers each owning one range.
- **Nesting** — a child layer may itself carry an injection query (markdown → fenced
  `rust` block → a `regex` inside that rust). The layer build recurses, with a
  **bounded depth** (a small constant, echoed-and-stopped past the limit per
  no-silent-stubs) to cap pathological configs.
- **`injection.include-children` / `injection.self` / `injection.parent`** — the
  remaining directive vocabulary, completing faithful parity with
  `_get_injections`.

**Test:** a combined-injection fixture where a construct spans two regions parses as
one tree; a two-level nested injection paints the innermost grammar's captures.

### Phase 4 — the platform half: `language_for_range` / child trees on `vim.treesitter`

Phases 0–3 are the **paint** (engine-internal). Phase 4 is the **plugin platform**:
make `vim.treesitter.get_parser(buf)` expose the injected child `LanguageTree`s so
`get_node({ bufnr, lang })`, `language_for_range`, and injection-aware plugins (and
the `nvim-treesitter-textobjects` path) resolve the *injected* language at a
position, not just the root.

The vendored `languagetree.lua` already implements `_get_injections` /
`_add_injections` / child trees in pure Lua, over the snapshot primitives
(`vim._create_ts_parser` & co.). The work here is plumbing the snapshot side so a
`LanguageTree:parse(true)` actually runs injections over the pushed snapshot and
builds children — i.e. ensuring `included_ranges` / `set_included_regions` are honored
by the primitive parser binding. This is independent of the engine's internal layers
(different consumer, different lifetime), but it is the **drift oracle** for Phases
1–3: a black-box test asserts the engine's painted injected captures agree with what
the vendored `_get_injections` resolves for the same buffer, so a divergence between
the ported Rust directive logic and upstream's Lua is caught.

> **Why last:** the paint is the user-visible win and stands alone; the platform half
> matters only for plugins that introspect injected trees, and it reuses the snapshot
> machinery already vendored. Doing it last also lets it serve as the cross-check on
> the ported directive vocabulary.

## Components (where the code lands)

| Concern | File | Change |
| --- | --- | --- |
| Third query name | [`loader.rs`](../../crates/nxvim-ts/src/loader.rs) | `Grammar.injections: Option<Query>`; load/compile it |
| Push `injections` | [`engine.rs`](../../crates/nxvim-ts/src/engine.rs) | `set_query`/`set_query_overlay`/`recompile_query` accept `"injections"` |
| Resolve at open | [`treesitter.rs`](../../crates/nxvim-server/src/treesitter.rs) | `resolve_ts_queries_for` iterates `injections` too |
| Injection layers | [`engine.rs`](../../crates/nxvim-ts/src/engine.rs) | `BufferState` child layers; build after reparse; directive interpreter |
| Layered paint | [`engine.rs`](../../crates/nxvim-ts/src/engine.rs) | `extract_spans` gathers host + child captures with layer rank |
| Platform children | [`languagetree.lua`](../../crates/nxvim-lua/src/vendor/nvim/vim/treesitter/languagetree.lua) (driven via primitives) | honor `included_ranges` on the snapshot parser binding |

No new RPC, no new effect: injections ride the **existing** `TsOp::SetQuery` effect
and the existing buffer-open resolve path. The engine API surface gains only internal
layer state — `highlights()` keeps its `(buffer, first, last) -> Vec<Span>` signature,
now returning host + injected spans merged.

## Edge cases (no-silent-stubs throughout)

- **Missing child grammar** — silently skipped (best-effort), exactly like a missing
  host grammar; the region keeps the host's flat paint.
- **Broken injection query** — echoes loud at push time (Phase 0), like a broken
  highlights query; the engine keeps the prior compiled injection query.
- **Dynamic language names nothing installed** — a markdown fence ```` ```nonsuch ````
  resolves to a language with no grammar → skipped, host paint kept.
- **Injection budget exhausted** — keep last-good child trees, paint one frame stale;
  never hang (mirrors the root `PARSE_DEADLINE`).
- **No `injections.scm`** — `grammar.injections` is `None`; the layer build is a
  no-op; the buffer is byte-identical to today's single-tree paint. The common,
  no-injection case pays nothing.
- **Nesting depth limit** — past the bound, stop and echo once (don't silently
  truncate a deep config into looking complete).
- **Self-injection** — host == child language: handled by the same path; the grammar
  cache returns the already-loaded host grammar.

## Testing

Black-box, per [the conventions](../../CLAUDE.md): drive Lua / set up an on-disk
`injections.scm`, then assert the redraw `highlights` payload shows injected captures
the single-tree paint could not produce. Reuses the bridge suite's helpers in
[`syntax.rs`](../../crates/nxvim/tests/syntax.rs) (`row0_has_group`,
`wait_for_highlights`, the `query_overlay_runtimepath` fixture).

- **Phase 0:** `query.set('rust','injections', …)` and an on-disk `injections.scm`
  overlay compile silently; a broken one echoes loud. (No paint assertion.)
- **Phase 1:** *self-injection* fixture (rust-in-rust) — a host node painted flat
  gains the injected grammar's finer captures inside it. **Needs only
  `tree-sitter-rust`**, already a dev-dep.
- **Cross-language (Phases 1–3):** requires a **second grammar in the fixture.**
  Today only `tree-sitter-rust` is a dev-dependency
  ([`Cargo.toml`](../../Cargo.toml) `[workspace.dependencies]`). Add one whose
  source compiles in the same `cc` fixture path the rust grammar uses
  (`treesitter_lua.rs::fixture_data_dir`) — a small grammar like `tree-sitter-regex`
  (rust injects regex) or a markdown/rust pair. Pin it `=x.y.z` like every other dep.
- **Phase 2:** edit-inside-region tracks incrementally; a position-sensitive case the
  substring parse mis-painted now paints right.
- **Phase 3:** combined injection parses split regions as one tree; nested injection
  paints the innermost grammar.
- **Phase 4:** `get_node({ bufnr, lang })` inside an injected region returns the
  *child* language's node; and the drift oracle — engine-painted injected captures
  agree with the vendored `_get_injections` resolution for the same buffer.

## Sequencing rationale

The query-resolution bridge ([its design](2026-06-08-treesitter-query-bridge-design.md))
deliberately scoped injections out: *"Injections remain separately scoped (they
change resolution inputs again, via `LanguageTree` children, and layer on top of
both)."* This doc is that follow-up. It depends on the query bridge (Phase 0 *is* a
one-name extension of it) and on `start`/`stop` (the per-buffer engine-enable the
layers ride on). Within this doc: **0 → 1 → 2 → 3** is a strict dependency chain
(each phase's child-parse model is the next's foundation); **4** depends only on the
vendored snapshot primitives and can be built in parallel with 1–3, but is sequenced
last so it can double as the directive-vocabulary drift oracle.

## Alternatives considered

- **Port neovim's full injection resolution into Rust, Lua-free.** Rejected for the
  same reason the query bridge rejected porting query-merge: it reimplements upstream
  semantics that drift. *But note the asymmetry* — we **do** port the injection
  *directive vocabulary* (`#set! injection.*`), because unlike query-merge it is tiny,
  tree-sitter-stable, and oracle-checked against the vendored Lua in Phase 4. We do
  **not** port query resolution (the `;extends`/runtimepath merge that produces the
  `injections.scm` string) — that stays Lua's job, pushed through the existing bridge.
- **Run injections in Lua per-redraw.** Rejected: violates "never pull Lua
  in-redraw"; injected regions move per-keystroke, so this would put Lua on the hot
  path.
- **Substring parse forever (skip Phase 2's `included_ranges`).** Rejected as the
  end state (position-sensitive grammars and combined injections need true ranges),
  but accepted as the **Phase 1** simplification so the visible win ships before the
  incremental machinery.

## References

- [ADR 0001 — native engines, vendored Lua APIs](../decisions/0001-native-engines-vendored-lua-apis.md)
  (this is bridge #5)
- [Treesitter query bridge](2026-06-08-treesitter-query-bridge-design.md) (the bridge
  this extends)
- [In-process treesitter + indentation](2026-06-06-in-process-treesitter-and-indentation-design.md)
- [The `vim.treesitter` Lua platform](2026-06-07-vim-treesitter-lua-platform.md)
- [known-approximations.md](../known-approximations.md) — the injection gap this closes
