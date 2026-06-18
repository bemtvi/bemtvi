# Treesitter query bridge — resolve on the server, execute in the engine — design

**Status:** accepted; **implemented — but not as this document first proposed.**
The original plan (below) routed query *resolution* through the vendored neovim
`query.lua` (`query.get` + a `_resolved_query_string` wrapper). The `nx.*`
migration ([ADR 0002](../decisions/0002-native-plugin-system.md)) **deleted** that
vendored Lua, so the resolver is now a small **Rust resolver on the server**, not a
Lua one. The historical "Lua resolves" sections are kept below for rationale, but
the as-built shape is:

> **As built.** The engine query seam (`Engine::set_query` / `set_query_overlay`,
> the `(lang, name)` override map consulted by `Grammar::load`) is exactly as
> designed and unchanged. What differs is *who resolves*:
> - `Engine::base_query(lang, name)` exposes the on-disk base text.
> - `EditHost::resolve_runtimepath_queries(lang)`
>   ([`treesitter.rs`](../../crates/nxvim-server/src/treesitter.rs)) runs once per
>   language at its buffer's first highlight (guarded by `resolved_ts_langs`). For
>   each engine query (`highlights` / `indents` / `injections`) it gathers, via
>   `collect_query_parts`, the engine base + every runtimepath
>   `queries/<lang>/<name>.scm` and `after/queries/<lang>/<name>.scm`, and —
>   following `; inherits: a,b` modelines (`parse_inherits`) — the same for each
>   inherited language first, then installs the concatenation through
>   `Editor::set_resolved_ts_query` → `Engine::set_query_overlay` (a no-op when the
>   result equals the base).
> - Resolution is **additive concatenation**, not a faithful reimplementation of
>   `query.lua`'s precedence. `;; extends` / `; inherits:` are valid query comments,
>   so they concatenate harmlessly; this covers the overlay and inherits cases real
>   configs ship (e.g. a config `queries/ecma/injections.scm` reaching `javascript`,
>   whose bundled `injections.scm` is just `; inherits: ecma,jsx`). The
>   `vim.treesitter.query.set` in-memory path is **not** wired (no `nx`-public query
>   setter; the native `nx._nx_set_ts_query` raw-replace exists but is unused).
> `:TSInstall` follows the same `; inherits:` modelines and fetches the inherited
> query sets to disk (`javascript` → `ecma`,`jsx`), so the resolver has them to merge
> and base js/ts highlighting carries the `ecma` patterns. See
> [known-approximations.md](../known-approximations.md) for the remaining edge —
> injected *child* grammars load their queries raw (no inherits) until that child
> language is opened as a buffer.

> **Historical implementation note — getting the resolved *string* (obsolete).**
> The plan below assumed `query.get(lang, name)` (vendored Lua) and a
> `_resolved_query_string` wrapper to capture the merged string. That vendored file
> no longer exists; the Rust resolver above reads the files directly instead.

## Problem

The native engine ([`nxvim-ts`](../../crates/nxvim-ts)) compiles its highlight
and indent queries from **one file each** —
[`loader.rs::Grammar::load`](../../crates/nxvim-ts/src/loader.rs) does
`read_to_string(query_path(data_dir, lang, "highlights.scm"))` and the same for
`indents.scm`. It does **not** run neovim's query-resolution logic. So three
things the ecosystem routinely uses to customize highlighting are inert against
the paint:

1. **In-memory overrides** — `vim.treesitter.query.set(lang, name, text)` (stores
   into the Lua `explicit_queries` table; see
   [`query.lua`](../../crates/nxvim-lua/src/vendor/nvim/vim/treesitter/query.lua)).
2. **`after/queries/<lang>/*.scm` overlays** — the standard "add rules on top of
   the base grammar's query" mechanism most highlight tweaks ship as.
3. **`;extends` / `;inherits` modeline merges** and runtimepath ordering across
   multiple `queries/<lang>/` dirs.

All three share one root cause: the engine reads a single file, blind to the
resolution logic that knows how to merge. A drop-in *base* `queries/<lang>/` tree
is honored (it's the one file the engine reads); **layering on top of it is not.**

## Why not teach the loader to merge

The obvious fix — parse `;extends`/`;inherits` modelines, walk `after/queries`,
merge runtimepath order in Rust — means reimplementing `query.lua`'s
`get` / `get_files` / `read_query_files` in Rust. That is exactly the bug-for-bug
divergence ADR 0001 exists to avoid: the merge rules are upstream's, they drift,
and a subtly different Rust merge would mishighlight in ways no test of ours would
predict. **Rejected.**

## Design — Lua resolves, Rust executes

The vendored Lua already *is* the faithful resolver: `query.get(lang, name)`
merges `explicit_queries` + `;extends` base files + disk/runtimepath into one
final query **string** (and memoizes it, busting on `query.set`). So:

> The engine stops reading `highlights.scm` / `indents.scm` itself. Instead the
> server asks Lua for the resolved string and **pushes** it to the engine, which
> **compiles, caches, and executes** it. Lua owns resolution (faithful); Rust owns
> execution (sync, fast). No query-merge logic is duplicated in Rust.

This subsumes all three cases at once — `query.set`, `after/queries`, and
`;extends` are just inputs to the same `query.get` the bridge already consumes.

### The one real constraint: push-on-change, never pull-in-redraw

The engine runs **synchronously** during `redraw` (queried from
[`treesitter.rs::refresh_highlights`](../../crates/nxvim-server/src/treesitter.rs)),
so it cannot call Lua mid-parse. Resolution therefore happens on the server's
**async side, ahead of redraw**, at well-defined moments, and the engine only ever
executes a *cached* compiled query:

- when a buffer's language is first determined (before its first sync highlight);
- on a `query.set` effect (and, later, on runtimepath/`after` changes);

— and **never** lazily from inside the sync highlight path (that would add
per-frame cost and Lua re-entrancy). Query text changes at config time, not per
keystroke, so a push model is the natural fit.

## Components

Same effect-queue → engine shape as the other bridges:

1. **Lua seam** — in [`prelude/treesitter.lua`](../../crates/nxvim-lua/src/prelude/treesitter.lua),
   wrap `vim.treesitter.query.set` (like the existing snapshot seams) to emit a
   `ts_set_query(lang, name)` effect after updating `explicit_queries`. The server
   resolves the *string* via `query.get` rather than trusting the raw `set` text,
   so `;extends` base-merging is included.
2. **Resolution + push** — on buffer-open (language known) and on the
   `ts_set_query` effect, the server calls `query.get(lang, name)`, gets the
   resolved string, and hands it to the engine. Only the paint-relevant names
   (`highlights`, `indents`) push to the engine; other names (`folds`,
   `injections`, `textobjects`, …) update only the Lua side — they don't drive the
   paint.
3. **Engine API** — `Engine::set_query(lang, name, resolved_text)` compiles a
   `Query` against the loaded `Language` and stores it in an override map keyed by
   `(lang, name)`, consulted in place of the disk load. A compile failure **echoes
   loud** (no-silent-stubs), exactly like a broken on-disk query today.
4. **Invalidation** — after a push, drop the highlight memo for that language's
   open buffers (the per-buffer memo in
   [`treesitter.rs`](../../crates/nxvim-server/src/treesitter.rs)) so the next
   redraw repaints; indents recompute naturally on next query.

## Edge cases

- **Set before the grammar loads.** A plugin may `query.set` before any buffer of
  that language is open, so there is no `Language` to compile against yet. Store
  the resolved *text* in the override map and compile it at grammar-load time
  (`Grammar::load` consults the override before falling back to disk). The override
  store holds text; the compiled `Query` is derived.
- **Default path unchanged.** With no override for `(lang, name)`, the engine reads
  the single disk file exactly as today — the common, no-customization case pays
  nothing and stays byte-identical.
- **Compile failure is loud, once per `(lang, name)`** — surfaced via the editor
  echo like the existing broken-query path, not swallowed.
- **`set(lang, name, nil)`-style clear** — drop the override; revert to disk.

## Testing

Black-box, per [the testing conventions](../../CLAUDE.md): feed Lua that calls
`vim.treesitter.query.set('rust', 'highlights', <text capturing something the base
query doesn't>)`, then assert the redraw `highlights` payload reflects the new
capture; assert an `after/queries` overlay with `;extends` adds to (not replaces)
the base; assert a deliberately broken query text echoes loud and does not paint;
assert the no-override path is unchanged.

## Sequencing — why deferred behind `vim.treesitter.start`

`vim.treesitter.start` (bridge #1) unblocks the far more common case — getting any
treesitter highlighting onto buffers the extension table misses — and establishes
the start/stop → native-engine plumbing this bridge's invalidation reuses. This
query bridge is a *refinement* on top: it matters only once a config is already
highlighting and wants to customize *how*. Build order: `start`/`stop` first, then
this. Injections remain separately scoped (they change resolution inputs again, via
`LanguageTree` children, and layer on top of both).
