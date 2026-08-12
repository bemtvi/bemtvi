# The `vim.treesitter` Lua platform — design

**Status:** phases 1–4 shipped — the plugin-facing API is complete. The vendored
neovim `vim.treesitter` Lua runs on bemtvi's bespoke primitives:
`get_parser(buf):parse()` parses real buffers off the pushed snapshot,
`get_string_parser` parses literals, and `query.parse` + `iter_captures` /
`iter_matches` evaluate predicates (`#eq?`, `#match?`, …) and directives over a
faithful, **unfiltered** query cursor ported from neovim's `treesitter.c`. The
vendored Lua is embedded under `crates/bemtvi-lua/src/vendor/nvim/` and registered
into `package.preload`; the Rust primitives live in `crates/bemtvi-ts/src/lua.rs`;
the snapshot/`nvim_buf_attach` seam is adapted in
`crates/bemtvi-lua/src/prelude/treesitter.lua`; all tested end to end in
`crates/bemtvi/tests/treesitter_lua.rs`. Phase 5 (injections / decoration-provider
highlighting / Lua-driven indent) remains deferred. Builds the
`vim.treesitter` Lua
API as a **plugin platform** — Lua-owned parsers/trees/queries exposed as
userdata over the in-process tree-sitter trees — so treesitter-consuming plugins
(textobjects, context/sticky-scroll, incremental selection, AST/query tools)
run unmodified, the same way bemtvi already runs real catppuccin and
nvim-lspconfig.

This **does not supersede** the in-process treesitter engine
([`2026-06-06-in-process-treesitter-and-indentation-design.md`](2026-06-06-in-process-treesitter-and-indentation-design.md)).
That doc's Rust `Engine` stays the editor's **hot path** for highlighting and
indentation. This platform is *additive and parallel*: it gives **Lua** its own
view of the same grammars. Whether the editor's own highlight/indent later move
to ride on it is explicitly out of scope here (see *Coexistence* and *Non-goals*).

The phase-6 line item "the `vim.treesitter` Lua API" in that doc is what this
spec expands into a real plan.

---

## Why a platform, and why *not* re-route core editing through it

The original ask was "don't reimplement nvim-treesitter's indent — run theirs."
Investigating that surfaced a **load-bearing constraint** that reshapes the goal.

### The constraint: bemtvi's Lua bridge is a snapshot, not live editor access

Unlike neovim — where Lua holds a *live* handle to the editor and the C core
calls *into* Lua synchronously mid-keystroke (how `indentexpr=v:lua…` works) —
bemtvi's Lua bridge is a **snapshot + effect-queue** (`runtime.rs`, `nvim_api.lua`):

- Before running a Lua chunk/callback, the server **pushes a snapshot** of buffer
  state into Lua (`btv._bufs[bufnr] = { lines, name, loaded }`).
- Lua getters (`nvim_buf_get_lines`, `nvim_win_get_cursor`, …) read **that
  snapshot**, not the live editor.
- Lua mutations **queue effects** (`buf_ops`, `window_ops`, …) that the server
  drains and applies to the live editor *after* the chunk.

This is exactly what lets `bemtvi-core` stay pure and synchronous with no Lua
dependency. It is a feature, not an accident — and it draws a hard line:

> **Lua can read a buffer snapshot and return data/effects. Lua cannot be a
> synchronous oracle that `bemtvi-core` consults mid-edit.**

### What that line allows and forbids

| Capability | Fits the architecture? | Why |
|---|---|---|
| Plugins that **query** a tree and return data/effects (textobjects, context, AST tools, query-based motions) | ✅ yes | They read the snapshot tree and queue effects — the normal chunk/callback flow. **This is the platform.** |
| **Highlighting** sourced from a Lua-owned tree | ✅ feasible later | Redraw is server-side (owns editor *and* Lua); it can refresh the snapshot and query. But "real `vim.treesitter.start`" = decoration-providers + extmarks, a separate subsystem bemtvi lacks. Out of scope here. |
| **Indent via real `indent.lua`** during `o`/`O`/insert-`<CR>` | ⚠️ fights the architecture | `get_indent(lnum)` wants the *live* buffer mid-keystroke; indent is computed deep in pure `bemtvi-core`, which has no Lua handle. Would require lifting insert-mode indent into the server or giving Lua live access. **Out of scope.** |

So the platform delivers the strategic prize — the **plugin ecosystem** — and the
Rust hot path keeps owning the latency-sensitive, core-side decisions it already
does well. Re-routing the editor's *own* indent/highlight through Lua is a
separate, larger effort (a bridge change) and is **not** part of this spec.

---

## Strategy: low-level primitives in Rust, neovim's real treesitter Lua on top

neovim's `vim.treesitter` is itself **Lua over a thin C binding**. The Lua half
(`runtime/lua/vim/treesitter/{init,languagetree,query,language,…}.lua`) holds the
high-value, fiddly logic: predicate evaluation (`#eq?`, `#match?`, `#any-of?`,
`#has-ancestor?`, …), directive handling (`#set!`, `#offset!`), injection
resolution, query memoization, range math. The C half is a small set of
primitives over the tree-sitter library.

We mirror that split rather than hand-write the high-level API:

1. **Implement the low-level primitives in Rust/mlua** — the
   `src/nvim/lua/treesitter.c` equivalent. This is the only bespoke code.
2. **Vendor neovim's `runtime/lua/vim/treesitter/*.lua` verbatim** and run it on
   top — predicates, injections, query handling come for free and stay
   bug-for-bug compatible with upstream.

This is the same posture as nvim-lspconfig: vendor the real Lua, satisfy the
primitives it stands on. The required primitive surface (confirmed against
upstream `languagetree.lua` / `query.lua` / `language.lua`) is small:

```text
btv._create_ts_parser(lang)            -> TSParser userdata
btv._ts_has_language(lang)             -> bool
btv._ts_add_language(path, lang)       -> (via vim.treesitter.language.add)
btv._ts_inspect_language(lang)         -> symbols/fields table (language.inspect)
btv._ts_parse_query(lang, query_str)   -> TSQuery userdata

TSParser:  :parse(old_tree|nil, source, include_ranges?) -> {TSTree,...}
           :set_included_ranges(ranges)  :reset()  :_set_logger(...)
TSTree:    :root() -> TSNode   :copy()   :edit(...)   :included_ranges(bytes?)
TSNode:    :type() :symbol() :id() :range(bytes?) :start() :end_()
           :parent() :child(i) :child_count() :named_child(i) :named_child_count()
           :iter_children() :field(name) :child_by_field_name(name)
           :descendant_for_range(...) :named_descendant_for_range(...)
           :next_sibling() :prev_sibling() :next_named_sibling() :prev_named_sibling()
           :has_error() :is_named() :is_missing() :is_extra() :byte_length() :equal(o)
TSQuery:   query iteration consumed by query.lua's iter_captures/iter_matches
           (raw captures + per-pattern metadata; predicates evaluated in Lua)
```

**`source`** passed to `:parse()` is where the snapshot plugs in. neovim's C
parser reads the buffer directly; ours reads the **pushed snapshot**
(`btv._bufs[bufnr].lines`, or a literal string for `get_string_parser`). The
adapter materializes the snapshot text and hands its bytes to tree-sitter's read
callback — no live editor access required.

`vim.api.nvim_get_runtime_file` (parser/query discovery) and the grammar load
already exist (`bemtvi-ts::loader`, `host::get_runtime_file`); `language.add`
routes to the existing loader instead of neovim's `.so` resolution.

---

## The ownership & lifetime model (the crux)

tree-sitter's `Node<'tree>` borrows its `Tree`; mlua userdata must be `'static`.
We reconcile this exactly as established bindings do:

```rust
// 'static userdata: the co-stored Rc keeps the C tree's heap alive; the Node is
// a small POD whose borrow we erase. Sound because (a) the Rc outlives the Node
// (same struct, dropped together; dropping a Node is a no-op), and (b) the Tree
// is never mutated while nodes reference it.
struct LuaNode { tree: Rc<Tree>, node: tree_sitter::Node<'static> }
struct LuaTree { tree: Rc<Tree> }
```

Invariants that make the lifetime erasure sound, stated so the impl upholds them:

- **Trees are immutable snapshots.** A `TSTree` userdata wraps `Rc<Tree>` and is
  never edited in place. Incremental reparse does **clone → edit clone → reparse
  → new `Rc<Tree>`** (`ts_tree_copy` is cheap/refcounted), so any outstanding
  nodes keep pointing at a still-valid, unchanged tree.
- **Nodes co-own their tree.** Deriving a child/parent/sibling clones the
  `Rc<Tree>` into the new `LuaNode`, so a node handed to Lua can never outlive its
  tree even if the parser moves on.
- **Single-threaded.** The runtime is already `Rc<RefCell<…>>` / non-`Send`
  (lua51); `Rc<Tree>` fits. No `Send`/`Sync` is introduced.

A `TSParser` userdata owns a `tree_sitter::Parser` + the loaded `Grammar`
(`Language`, queries) from `bemtvi-ts::loader`, plus the last `Rc<Tree>` for
incremental reuse.

---

## Crate seam

Heavy C deps (tree-sitter, libloading) must stay in **`bemtvi-ts`**
(architecture invariant). So the binding lives there, behind an optional Lua
feature, reusing the existing `loader`/`Engine` internals:

- `bemtvi-ts/Cargo.toml`: add `mlua` (optional, `lua` feature, workspace-pinned to
  match `bemtvi-lua`'s version + `lua51`). No new heavy dep; tree-sitter already
  present.
- `bemtvi-ts/src/lua.rs`: `pub fn install(lua: &mlua::Lua, data_dir: &Path)` —
  registers the userdata types and the `btv._ts_*` / `btv._create_ts_parser`
  primitives onto the shared VM.
- `bemtvi-lua` gains a dep on `bemtvi-ts` (feature `lua`) and calls
  `bemtvi_ts::lua::install(&lua, …)` during runtime construction, then loads the
  vendored `vim/treesitter/*.lua` off the runtimepath. tree-sitter stays out of
  `bemtvi-lua`'s own code; only the binding crate links it.
- `vendor/nvim-treesitter-runtime/` (or reuse `vendor/neovim/runtime/lua/vim/`):
  the vendored upstream treesitter Lua, added to the runtimepath like
  nvim-lspconfig.

`bemtvi-core` is untouched — it never sees Lua or tree-sitter, per its invariant.

---

## Coexistence with the Rust hot path

This platform is **parallel** to the editor's Rust `Engine`, not a replacement:

- The Rust `Engine` keeps owning the tree used for **highlighting at redraw** and
  **indent on keystroke** — fast, synchronous, core-side, already shipped.
- A **Lua `LanguageTree` is created lazily**, only when a plugin calls
  `vim.treesitter.get_parser(buf)`. Buffers with no treesitter-consuming plugin
  pay **zero** extra cost.
- A buffer that *does* have such a plugin pays a **second parse** (Rust engine's +
  Lua's). This is the known "two parsers" tradeoff the in-process design called
  out; it is acceptable for v1 because it is opt-in and lazy.

**Unification is a deliberate follow-up, not this spec:** later, the Rust redraw
highlighter could query the Lua-owned tree (single tree, server-queried), or the
engines could share one parse. Doing that now would entangle the working
highlighter with the new platform and risk a shipped feature for no user-visible
gain. Keep them separate until a concrete consumer forces the merge.

---

## Non-goals (explicitly deferred)

- **`vim.treesitter.start` / decoration-provider highlighting** — needs an
  extmark/decoration layer bemtvi doesn't have. The Rust highlighter stays.
- **Lua-driven indent** (`o`/`O`/`<CR>`/`=` via `indent.lua`) — fights the
  snapshot bridge; the Rust indent stays. (`@indent.align` fidelity, if wanted,
  is a separate Rust-side task per the in-process spec's phase 6.)
- **LSP semantic tokens** (`@lsp.*`, the "pyright recolors" effect) — an LSP
  feature on a different axis; tracked separately. Shares only the future
  highlight-layering primitive.
- **Live incremental updates via `nvim_buf_attach`.** v1 parses from the snapshot
  on each `:parse()`; incremental reuse is an internal optimization (clone-edit
  from buffer deltas) added once correctness is proven.

---

## Testing (black-box, per the no-unit-test rule)

All coverage is end-to-end through the server via `:lua`, asserting on values the
chunk writes back (e.g. `print`/`vim.g`/a scratch buffer), reusing the existing
grammar-fixture machinery from `crates/bemtvi/tests/{syntax,indent}.rs` (compile
`tree-sitter-rust`, point `BEMTVI_DATA_DIR` at it):

- **Primitives & lifetime:** a `:lua` chunk parses a string, walks
  `root → children`, asserts node `:type()`/`:range()`; holds a node across a
  reparse and asserts it stays valid (lifetime model).
- **Parser/LanguageTree:** `vim.treesitter.get_parser(0):parse()` over a real
  buffer returns a tree whose root type matches; re-parse after an edit reflects
  the change (snapshot refresh).
- **Query + predicates:** `vim.treesitter.query.parse(...)` +
  `:iter_captures(root, 0)` returns expected captures; a `#eq?`/`#match?`
  predicate filters correctly (proves the vendored query.lua runs).
- **A real consumer:** a small user-Lua routine that counts/selects function
  nodes via the API end-to-end — the platform's acceptance test.

---

## Implementation phases

1. **Userdata + lifetime model.** ✅ **Done.** `bemtvi-ts/src/lua.rs`: `TSNode`/
   `TSTree`/`TSQuery` userdata over tree-sitter with the `Rc<TreeInner>` erasure
   (the `Rc` co-owns the `Tree` *and* the `LoadedLanguage` whose dylib the node
   types live in); the `btv._create_ts_parser` / `btv._ts_has_language` /
   `btv._ts_parse_query` primitives; `TSParser:parse(source)` reading a
   **string** and returning `(tree, changed_ranges)` (matching neovim's
   `parser_parse`, not the `{TSTree,…}` sketch above). The full upstream
   `TSNode` method surface is implemented so phases 2–3 vendor the Lua against no
   Rust gaps. A new `bemtvi-ts::loader::LoadedLanguage` loads a grammar's parser
   *without* requiring `highlights.scm` (the platform needs only the `Language`).
   mlua wired behind bemtvi-ts's optional `lua` feature; `install()` called from
   `bemtvi-lua::LuaRuntime::new`. Black-box tests (`tests/treesitter_lua.rs`):
   parse a string, walk nodes, hold a node across a reparse *and* a GC of its
   source tree, incremental `tree:edit` reparse, query compile + loud error,
   missing-grammar loud error. *No high-level API yet — proves the hardest part.*
2. **Vendor neovim treesitter Lua + `get_parser` over buffers.** ✅ **Done.**
   The vendored `vim/treesitter/*.lua` (plus the `vim.func` / `vim.F` /
   `btv._core.util` / `vim.pos._util` helpers it stands on) is embedded under
   `crates/bemtvi-lua/src/vendor/nvim/` and registered into `package.preload` by
   `runtime.rs` (hermetic — no runtime dependency on the `vendor/neovim`
   submodule). `prelude/treesitter.lua` wires it: defines `btv._defer_require`,
   sets `vim.func`/`vim.F`, requires `vim.treesitter`, and adapts the two snapshot
   seams — `TSParser:parse(bufnr)` reads `btv._bufs[bufnr].lines`, and a
   buffer-sourced `LanguageTree` re-reads that snapshot each `:parse()` (full
   reparse, no `nvim_buf_attach`). New primitives: `_ts_get_language_version` /
   `_ts_get_minimum_language_version` / `_ts_inspect_language` / parse-from-bufnr /
   `set_included_ranges`. `get_parser(buf):parse()` works end-to-end.
3. **Query surface.** ✅ **Done.** `query.parse` / `iter_captures` /
   `iter_matches`, predicates (`#eq?`, `#match?` via `vim.regex`, `#any-of?`, …),
   directives, and metadata all run via the vendored `query.lua`. The bespoke core
   is `btv._create_ts_querycursor` + `TSQuery:inspect()`, ported from neovim's
   `treesitter.c` over the raw tree-sitter cursor FFI so matches are returned
   **unfiltered** (predicate evaluation stays in Lua, bug-for-bug with upstream —
   the safe Rust iterator's text-predicate filtering would diverge on `#match?`'s
   regex dialect). `query.get` (loading `queries/<lang>/<name>.scm`) works where
   query files exist; the parser-only fixture returns `nil` for a missing query.
4. **Coexistence hardening + consumer acceptance.** ✅ **Done.** A `LanguageTree`
   is created only on `get_parser`; buffers without a treesitter consumer pay
   nothing. The real-consumer acceptance test (select every function name in a
   buffer via a query) passes, alongside `#eq?`/`#match?`, `iter_matches`,
   buffer-edit reflection, and `language.inspect`. Docs updated here +
   `architecture.md` + `known-approximations.md`.
5. **(Later, separately scoped)** injections (LanguageTree children /
   `language_for_range`), then — only if pursued — the bridge work for
   decoration-provider highlighting (`vim.treesitter.start`; the highlighter is a
   loud not-implemented stub today) and Lua-driven indent.

Each phase is independently testable and leaves the tree green. Phase 1 front-
loaded the only genuinely novel risk (the node/tree lifetime over the Lua
boundary); phases 2–4 were integration of vendored Lua against those primitives,
plus the one further bespoke piece — the unfiltered query cursor.

---

## Risks & edge cases

- **Lifetime erasure soundness** — mitigated by the immutable-snapshot +
  co-owned-`Rc` invariants above; phase 1's hold-across-reparse test guards it.
- **mlua version skew** — the binding must use the *same* pinned mlua + `lua51`
  feature as `bemtvi-lua`, or the shared `Lua` userdata registration won't link.
  Pin in `[workspace.dependencies]`; never `--all-features` (the existing
  `lua51`/`luajit` exclusivity rule).
- **Vendored Lua reaching for absent `vim.*`** — upstream treesitter Lua may call
  `vim.func._memoize`, `vim.validate`, `vim.deprecate`, `vim.iter`, etc. Most
  exist; any gap fails *loud* (`btv._notimpl`) per the no-silent-stubs rule and is
  filled as found, exactly like the lspconfig bring-up.
- **Snapshot staleness** — a plugin that parses then reads stale lines sees the
  snapshot at chunk entry. This matches how every other bemtvi Lua getter already
  behaves; documented, not worked around.
- **Grammar segfault** — unchanged posture (neovim's): user-installed grammars,
  ABI-probed on load; a poison grammar can crash the process.
