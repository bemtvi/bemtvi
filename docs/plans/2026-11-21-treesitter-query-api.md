# A treesitter query API for plugins

**Status:** done. Phases 1–4 landed in the editor in `adfa41db` (2026-11-24); phase 5
landed in bemtvi-md-table in `7cf8fee` (2026-11-24). What was left on purpose is under
*Known limitations*.

**Decisions** (David, 2026-11-21):

- **Plain ranges for now.** Results are node ranges and kinds, not node handles to walk.
- **Root language only for now.** No injected layers.
- **No fallback in bemtvi-md-table.** With no markdown grammar installed the plugin
  does nothing: the editor doesn't consider the buffer structured markdown, so there
  is nothing to align. The Lua table finder and the fence scan are deleted, not kept
  as a fallback.
- **Grammar-dependent tests run when the grammar is installed and are skipped when it
  isn't.**

**Goal.** Let a `btv.*` plugin ask what the parse tree of an open buffer says: run a
treesitter query over a buffer (or a row range of it) and get the captured nodes back.
That is the canonical way for a plugin to learn the structure of a buffer. Without it
each plugin rebuilds that structure by scanning text.

The motivating consumer is
[bemtvi-md-table](https://github.com/bemtvi/bemtvi-md-table) (local:
`~/work/nxvim-plugins/bemtvi-md-table`). It aligns markdown pipe tables with inline
virtual text. Today it parses tables in Lua and scans the **whole buffer** for code
fences once per change whenever a table is on screen. That scan is the one cost in the
plugin that grows with the buffer, so it goes against the never-freeze rule. The
markdown grammar already knows where every `pipe_table`, `pipe_table_cell` and
`fenced_code_block` is. Asking it removes the fence scan and most of the Lua table
parser.

## Background

### What exists today

`btv.treesitter` (`crates/bemtvi-lua/src/prelude/btv.lua`) offers:

- `highlight(lang, text)`: a promise of highlight spans for a string, not a buffer;
- `set_query(lang, name, text)`: override one of a language's query files;
- `installed()`, `reload(lang)`, `fragment_context(lang, templates)`;
- `foldexpr`: a marker string for `'foldexpr'`, an error if called.

Highlighting is switched with `btv.bo.filetype` and `btv.bo.ts_highlight`.

Nothing lets a plugin see a buffer's tree.

### Why there is no tree API

Decision D1 in [the vendored-treesitter deletion plan](2026-06-12-vendored-treesitter-deletion.md)
deleted the Lua parser and query primitives (`btv._create_ts_parser` and co., in
`bemtvi-ts/src/lua.rs`, removed in `654de958`). They existed only to back the vendored
`vim.treesitter` surface, and nothing else used them. The plan said to rebuild a
parser API under `btv.treesitter`, in bemtvi's own shape, when a real consumer arrived.
bemtvi-md-table is that consumer.

### Where trees live

| Build | Who parses | How the core reaches it |
|---|---|---|
| native (and daemon sessions) | `bemtvi_ts::Engine`, in process, incremental, one tree per buffer with its own shadow text | `Editor.syntax: Option<Box<dyn SyntaxEngine>>`, called synchronously |
| web (wasm edit-host) | web-tree-sitter in the JS worker | `WasmSyntax` keeps a shadow copy of the text and calls JS through synchronous FFI (`eh_js_ts_indent`, `eh_js_ts_textobjects*`, folds) |

Daemon sessions need nothing special: the editing core, syntax included, runs on the
client side, and the daemon only ships bytes.

`Engine::text_objects_at` (`crates/bemtvi-ts/src/engine.rs`) is the closest existing
code. It runs a query over a buffer's tree and returns byte ranges. `web/ts-textobjects.js`
is its web twin, including the UTF-16 to UTF-8 offset conversion. A buffer query is
the same machinery with the query text supplied by the caller.

### How Lua reaches the editor

The Lua runtime does not hold the editor. It reads mirrors the server pushes in
(`btv._bufs`, windows, mode), and anything else is an op queued to the server and
answered through a callback (`btv._ts_highlight` → `settle_ts_highlight` →
`CallbackArgs::TsHighlight`). So a general query from Lua is **asynchronous**: a
promise, answered when the server drains the op.

The one place the server calls Lua *with* a snapshot is `btv.decor` dispatch
(`EditHost::dispatch_decor` in `crates/bemtvi-server/src/effects.rs`). It builds
`ctx` from the rope, on the server, with the editor in hand. A query computed there
reaches the provider in the same call, with no extra round trip.

## Design

Two surfaces over one engine method.

### 1. The engine method

Add to `SyntaxEngine` (`crates/bemtvi-core/src/syntax.rs`):

```rust
/// Run `query` (treesitter query source) over `buffer`'s current tree, restricted
/// to rows `[first, last)` when given. Matches are in tree order.
fn query(
    &mut self,
    buffer: BufferId,
    query: &str,
    rows: Option<(usize, usize)>,
) -> Result<Vec<QueryMatch>, QueryError>;
```

```rust
pub struct QueryMatch {
    pub pattern: usize,
    pub captures: Vec<QueryCapture>,
}
pub struct QueryCapture {
    pub name: String,          // without the leading `@`
    pub start: (usize, usize), // (row, byte col), 0-based
    pub end: (usize, usize),   // exclusive
    pub node_kind: String,
}
pub enum QueryError {
    NoTree,                    // buffer has no grammar / not opened in the engine
    Pending,                   // grammar still loading, or the parse is incomplete
    Compile(String),           // the query doesn't compile, with treesitter's message
    Unsupported,               // this engine has no query support
}
```

The default implementation returns `Err(QueryError::Unsupported)`: an engine that
can't answer says so instead of returning an empty list (no silent stubs).

Native (`bemtvi_ts::Engine`):

- compile with the buffer's root language and cache compiled queries per
  `(language, query text)` in a small cache (cleared when full), so a provider that
  sends the same query on every scroll compiles it once;
- `QueryCursor::set_point_range` for the row range, and a cap on the total number of
  matches as a backstop (`TooManyMatches` past 100,000). Not `set_match_limit`: when
  exceeded it drops matches silently, which would hand back a wrong list. With the
  row range, a query bounded to the viewport stays bounded;
- `Pending` when `parse_pending(buffer)` is true or the language is still loading.
  Answering from a half-parsed tree would be confidently wrong.

Coordinates match extmarks (0-based row, byte column, end exclusive), so a capture
can be passed straight to `btv.buf.set_extmark` or `publish`.

**Root language only** (decided). The query runs on the buffer's root language. For markdown that is the block grammar (`markdown`), which owns
`pipe_table`, `pipe_table_cell` and `fenced_code_block`. Inline nodes
(`markdown_inline`) and fence contents (rust inside a fence) live in injected layers.
A `lang =` option that selects an injected layer is a natural follow-up; see
*Not in scope*.

### 2. `btv.treesitter.query` (general, asynchronous)

```lua
-- `btv.treesitter.query(buf, query[, opts])` -> promise of matches.
btv.treesitter.query(0, "(pipe_table) @table", { rows = { top, bot + 1 } })
  :next(function(res)
    -- res = { tick = <changedtick the tree reflects>,
    --         matches = { { pattern = 1, captures = {
    --           table = { { row, col, end_row, end_col, kind = "pipe_table" } } } }, ... } }
  end)
```

- Captures are grouped by name within each match (a name can capture several nodes,
  as `_+ @function.inner` does), and every node is a 4-tuple plus `kind`.
- `res.tick` is the `changedtick` the tree corresponds to, so a caller can drop a
  result the buffer has already moved past.
- `Pending` is **parked** server-side and retried when the grammar loads or the parse
  converges (the same pattern as `parked_ts_highlights`), so a caller just waits. The
  park has a bounded number of retries, after which the promise rejects with a message
  naming the cause.
- `NoTree`, `Compile` and `Unsupported` **reject** with a message that names the cause
  (`"no treesitter grammar for filetype 'markdown'"`). A plugin that can work without
  a grammar catches the rejection and falls back; it is never handed an empty list it
  can't tell from "no matches".

Plumbing: an op on the existing Lua op queue (`ops.rs`), drained in `effects.rs`,
answered with a new `CallbackArgs::TsQuery`. That follows `btv._ts_highlight`, but
the query is answered in the drain itself when the tree is ready, so the promise
usually settles within the same tick.

### 3. `query` on a decor provider (synchronous, viewport-bound)

A decor provider runs on every viewport change and is the main consumer, so it gets
the query results in its `ctx` instead of a second round trip:

```lua
btv.decor.provider({
  name = "bemtvi-md-table",
  bufs = { filetype = { "markdown" } },
  query = [[
    (pipe_table) @table
    (fenced_code_block) @fence
  ]],
  on_range = function(ctx, publish)
    -- ctx.query = { matches = {...} } or { error = "<why>", pending = <bool> }
  end,
})
```

- `dispatch_decor` runs the query over `[top, bot]` while it builds `ctx`, through
  the same `SyntaxEngine::query`. It's bounded by the viewport, like `ctx.lines`.
- A node that **intersects** the viewport is returned with its full extent. A
  `pipe_table` that starts above the screen reports its real first row, which is what
  a table aligner needs to measure the whole table. Child nodes outside the range are
  not returned; a provider that needs them reads those rows (`btv.buf.lines`) or
  issues a `btv.treesitter.query` for the node's range.
- On failure `ctx.query.error` carries the reason and `matches` is absent.
  `ctx.query.pending = true` marks a "not yet" (grammar loading, parse unfinished):
  the provider is re-dispatched by itself once the answer is ready, so it should keep
  what it drew rather than clear it.
- A query that doesn't compile is reported once on the message line
  (`E5108`-style, like a throwing provider), so a typo can't fail silently.
  *Changed from the first draft*, which compiled the query at registration: Lua
  doesn't hold the engine, and the grammar to compile against is the buffer's, which
  only the dispatch knows. Registration validates that `query` is a non-empty
  string.
- *Found while building the example:* changing a buffer's filetype or
  `ts_highlight` didn't re-run its providers, since neither is a viewport change.
  That was already true for the `bufs.filetype` filter, and a `query` makes it
  visible. The core's filetype/enable setters now invalidate the buffer's decor.

### Web (wasm) parity

`WasmSyntax::query` calls a new synchronous FFI `eh_js_ts_query(lang, text, query,
first, last)` into a new `web/ts-query.js`. That module reuses ts-textobjects.js's
grammar loading and UTF-16 to byte conversion, and returns matches as JSON over the
FFI. The edit-host's tick runs in the worker, so the call is synchronous there, as
indent and text objects already are.

Cost: today the web indenter re-parses the shadow text on each call. A query on every
viewport change would do the same, which scales with the buffer. Phase 3 has the JS
side keep one tree per buffer, updated with `tree.edit` from the `BufferEdit` deltas
`WasmSyntax::edit` already receives. That keeps the web path incremental like the
native one. The same cache then serves indent and text objects too.

*As built:* the per-buffer store (`web/ts-buffers.js`, shared by queries, indent, folds
and text objects) keeps each buffer's text as lines, with a Fenwick tree of
line lengths, plus its own parser and tree. `WasmSyntax::edit` (whose shadow text is now
a rope) forwards every edit delta to it, and the runner converts the delta's byte columns
to UTF-16 within the touched lines only, edits its tree and re-parses incrementally,
reading straight from the lines. The whole text crosses the FFI only when the runner's
copy isn't at the core's version (first query, a re-open such as undo, a missed edit).
On an 80,000-line buffer an in-line edit costs about 0.03 ms and a query 0.02–0.2 ms.
The parse runs in 50 ms slices; one that doesn't finish answers "not yet" and continues
in the background, and edits that arrive mid-parse are queued until it finishes.

The browser needs one thing the native build gets for free: a wake when a grammar
finishes loading. Grammars load in the worker, so the core never hears of it. The
runner reports a load a query was waiting on, and the worker calls
`eh_ts_grammar_ready`, which re-runs parked queries and re-dispatches waiting decor
providers.

## Phases

Each phase lands, with tests, and pauses for review before the next. Phase 4 doesn't
depend on 1–3 and can land first.

### Phase 1: native engine method and `btv.treesitter.query`

**Done** (adfa41db).

- `SyntaxEngine::query`, `QueryMatch`/`QueryCapture`/`QueryError` in `bemtvi-core`;
  the default returns `Unsupported`.
- `bemtvi_ts::Engine::query`, with the compiled-query cache, the row range and the
  match cap.
- The Lua op, the server drain, parking `Pending` and retrying, and
  `CallbackArgs::TsQuery`; `btv.treesitter.query` in `prelude/btv.lua` with a
  book-ready docstring (markdown rules in CLAUDE.md).
- Tests: `crates/bemtvi-server/tests/treesitter_query.rs`. Like the other
  `treesitter_*` suites, it compiles a real grammar from the cargo registry into a
  temp `BEMTVI_DATA_DIR` (`tree-sitter-md` is already a dev-dependency; there is an
  `install_markdown_grammar` helper in `bemtvi-ts/tests/fixture`). Cases:
  - a pipe table outside a fence is matched, one inside a fence is not;
  - the row range is respected, and a node crossing the range edge comes back with
    its full extent;
  - after an edit the result follows the new text, with `tick` advanced;
  - a bad query rejects with treesitter's message;
  - a buffer with no grammar rejects with `NoTree`'s message;
  - a query issued while the grammar is still loading settles after it lands
    (parking);
  - mutation check: break the range restriction or the parking and watch a case fail.

  These follow the existing convention for an unavailable dependency: with no C
  compiler to build the grammar, the test prints `skip: ...` and returns.

### Phase 2: `query` on decor providers

**Done** (adfa41db).

- `query` key on `btv.decor.provider` (validated at registration, compiled at
  dispatch; see the design above).
- `dispatch_decor` fills `ctx.query`; `run_decor_dispatch` carries it into Lua.
- Tests, in `treesitter_query.rs` beside the grammar fixture: `ctx.query` matches
  the viewport, follows a scroll and an edit, keeps a table crossing the top whole,
  reports `error` for a buffer with no grammar, reports a bad query once, and is
  answered once an unfinished parse converges. In `decor.rs`: a filetype change runs
  the providers without a scroll.
- An `examples/decor-query/` config with its own `--test-plugin` spec (CLAUDE.md's
  example rule). It highlights each markdown table and fence with a sign, so the
  reader can see the query drive the paint.

### Phase 3: web parity

**Done** (adfa41db).

- `eh_js_ts_query` FFI (an import, from `web/eh-lib.js`), `web/ts-query.js`, and
  `WasmSyntax::query`; `eh_ts_grammar_ready` (in `EXPORTED_FUNCTIONS`) for the
  grammar-loaded wake. Parking moved into code both builds share.
- Incremental per-buffer trees (see *As built* above), in one store that indent,
  folds and text objects share too (`web/ts-buffers.js`).
- `web/verify-treesitter-query.mjs`: the queries and a decor provider in the browser
  build, an incremental-vs-fresh-parse equivalence check after structural edits, and a
  large buffer. The tree reuse was measured in the worker; a page-side timing check
  can't see it under the `execLua` round trips.

### Phase 4: skipped tests in `btv.test` (editor repo)

**Done** (adfa41db).

The `--test-plugin` runner keeps the user's config out, but its syntax engine reads
grammars from the real data dir (`bemtvi_ts::data_dir()`: `$BEMTVI_DATA_DIR`, else
`stdpath("data")`). So a spec can find out whether a grammar is installed
(`btv.treesitter.installed()`). What it can't do today is report a test as skipped:
returning early would count as a pass, which is a silent skip.

- Add `t:skip(reason)`: ends the test, which is reported as skipped with the reason
  (its own marker and colour, and a `N skipped` count in the summary). A skip is never
  a pass and never a failure; the run's exit status ignores it.
- Tests in `crates/bemtvi/tests/` for the runner: a skipped test shows in the output
  and summary with its reason, and doesn't change the exit status.
- Document it in `prelude/test.lua`. A spec gates grammar-dependent tests like this:

  ```lua
  local function needs_grammar(t, lang)
    if not btv.tbl.contains(btv.await(btv.treesitter.installed()), lang) then
      t:skip("no " .. lang .. " treesitter grammar installed")
    end
  end
  ```

### Phase 5: port bemtvi-md-table (in the plugin repo)

**Done** (bemtvi-md-table `7cf8fee`).

- Declare `query = "(pipe_table) @table"` on the provider. The table extent comes from
  the grammar. The fence scan and `layout.table_at` are deleted. Cell splitting stays
  in Lua over the table's rows. That's a few lines and doesn't need child-node ranges
  past the viewport.
- **No fallback.** When `ctx.query.error` reports no markdown grammar, the provider
  publishes nothing. The README and help say tables are aligned only where a markdown
  grammar is installed (`:TSInstall markdown`).
- Specs: every test that needs the grammar starts with `needs_grammar(t, "markdown")`
  and is skipped without it. The pure `layout_spec.lua` (cell splitting and padding)
  needs no grammar and always runs. Add a test that with the grammar installed, a
  table inside a fence gets no padding. It now proves the grammar's answer, not a Lua
  scan.
- Remove from the plugin docs the note about the whole-buffer fence scan, since that
  cost goes away.

## Risks

- **Pending trees.** Large files parse across frames under `PARSE_DEADLINE`. For the
  async API, parking hides this. For decor, `ctx.query.error = "pending"` on the first
  dispatch means a provider paints late on a big file. Mitigation: re-dispatch the
  window when the parse converges (the server already polls `parse_pending` to
  schedule frames, so it can mark the viewport dirty when the parse completes).
- **Grammar node names are not a stable API.** A query written against
  `tree-sitter-md` 0.5 node names breaks if a grammar renames them. That's inherent to
  query-based plugins (neovim's are the same); a compile error names it rather than
  failing quietly.
- **Match volume.** `(_) @n` over a big range returns a lot. The row range and the
  match cap bound it, and the docs steer providers to name the nodes they need.
- **Web JSON over FFI.** Serialization cost scales with the match count, not the
  buffer, so it's acceptable for viewport-sized queries. Measure it in Phase 3.

## Open questions

1. **`btv.treesitter.query` on a string.** A string variant (like `highlight(lang,
   text)`) is cheap once the engine method exists. Add it only if a consumer appears.

## Known limitations

Found in the branch's three review rounds and left, deliberately:

- **Web: adding or removing a line rebuilds the runner's line-length index**, a numeric
  pass over the lines (about 0.6 ms on 80,000 lines). Edits within a line cost
  O(log lines). (Fixed after the reviews: the runner used to re-read and diff the whole
  text on every edit; it now applies the core's edit deltas, forwarded by
  `WasmSyntax::edit` when the core next syncs the engine.)
- **Web: an edit still costs an incremental re-parse, and wasm tree-sitter's reuse is
  weak.** Indent, folds and text objects now share the query runner's buffer store
  (`web/ts-buffers.js`): no copy of the text, no parse when nothing changed, one
  incremental re-parse per edit for all four runners (about 20–30 ms on a 60,000-line
  Python file in the browser, against a ~220 ms copy and full parse per call before).
  They still finish that parse within the call, as they always did, since they must
  answer now; queries parse in slices. Undo and reload re-read the text and re-parse it
  fully, as natively.
- **Query compile time is unbounded.** tree-sitter offers no budget for `Query::new`; a
  pathological plugin query is compiled once (it's cached per language and source).
- **The example spec skips in CI.** `examples/decor-query`'s spec needs the markdown
  grammar and skips without it; CI installs none, so there it only checks that it skips.
  The Rust suite (`tests/treesitter_query.rs`) compiles the grammar and covers the same
  behavior.

## Not in scope

- Injected-language layers (`opts.lang`), for now. Natural follow-up: pick the layer
  by language and query only its ranges.
- Node handles and tree walking (`parent`, `child`, `named_children`), for now. They
  would need either a synchronous bridge into the engine or a serialized subtree.
  Revisit when a consumer needs to walk the tree.
- Query predicates beyond what the tree-sitter crate evaluates natively (`#eq?`,
  `#match?`, `#any-of?`). Custom Lua predicates would mean calling back into Lua
  per match.
- Changing highlighting, indent, folds or text objects. They keep their own paths.
  Phase 3's tree cache helps the web ones, but they don't move onto this API.
