# 0001 — Native engines underneath, vendored neovim Lua APIs on top

**Status:** accepted (2026-06-08). Records a cross-cutting boundary that several
feature designs already assume but none states in one place: who owns the
treesitter / LSP *engine* (nxvim, natively) versus who owns the `vim.treesitter`
/ `vim.lsp` *API surface* (vendored neovim Lua), and where the two are wired
together. This is an ADR — the *why* of a standing decision — not a build plan;
the feature specs it references say *how*.

## Context

Two constraints pull in opposite directions:

- **nxvim-core is pure and synchronous** — no async, no Lua, no I/O beyond
  `Buffer` read/write (CLAUDE.md; [`architecture.md`](../architecture.md)). Every
  front end shares identical editing behavior because the editing state machine
  takes no detour through a scripting runtime. So anything that drives a
  *synchronous editing decision* — the highlight floor, indentation, folding, the
  `=` family — must run in-core, in Rust, with no Lua and no socket.

- **The plugin ecosystem is the product.** nxvim's value is running unmodified
  neovim plugins, and the corpus already in the repo proves the target:
  catppuccin, lspconfig, plenary, which-key, nvim-treesitter, telescope,
  nvim-treesitter-textobjects, gitsigns. Those plugins call `vim.treesitter.*`
  and `vim.lsp.*` *by name*, and depend on the **exact** behavior of those APIs —
  the `#match?` regex dialect, `#lua-match?`, query directives/metadata,
  `LanguageTree` injection semantics, the shape of what `iter_matches` yields.

These cannot be satisfied by a single implementation. The editor needs treesitter
*sync, in-core, Lua-free*; plugins need the API *faithful to neovim, in Lua*. And
LSP is socket I/O to an external process — it structurally cannot live in core at
all, so it can never drive a synchronous decision; it can only enrich,
asynchronously, after the fact.

## Decision

**Ship native engines and vendor the neovim Lua APIs on top of them. The engine
is what nxvim needs; the API is what the ecosystem needs. Bridges wire a vendored
API to the native engine underneath.**

|                | Native engine (nxvim's)                              | Lua API surface (vendored neovim)                      |
| -------------- | ---------------------------------------------------- | ------------------------------------------------------ |
| Treesitter     | [`nxvim-ts`](../../crates/nxvim-ts) — sync, in-core; drives highlight floor, indent, fold, `=` | [`vim.treesitter.*`](../../crates/nxvim-lua/src/prelude/treesitter.lua) — snapshot, for plugin queries |
| LSP            | [`nxvim-lsp`](../../crates/nxvim-lsp) + [`server/src/lsp/`](../../crates/nxvim-server/src/lsp) — async, server-side | [`vim.lsp.*`](../../crates/nxvim-lua/src/prelude/lsp.lua) — for lspconfig/mason/cmp |

The vendored Lua does **not** exist to serve nxvim — nxvim's editor behavior runs
on the native engine. It exists to serve *plugins*, a customer the native engine
cannot serve: plugins call Lua by name and depend on its exact semantics. That is
why both can run over one buffer (the "double parse"): **native engine for the
editor, faithful API for the ecosystem.** They only both run when a plugin opts
in by calling the Lua API; a buffer nobody queries from Lua parses once.

### Why *vendor* the API rather than hand-write it

The ecosystem depends on the long tail of behavior, not the happy path. The
phase-3 query work chose the unfiltered query cursor *specifically* to stay
bug-for-bug with upstream, because a safe-but-divergent reimplementation breaks
plugins on exactly those edges (see
[the vim.treesitter Lua platform spec](../specs/2026-06-07-vim-treesitter-lua-platform.md)).
Vendoring buys the long tail for free, and a neovim update is a *re-vendor*, not a
re-derivation. The same logic drives the `vim.lsp` surface: faithfully implement
the API so lspconfig/mason/cmp run, backed by nxvim's own async server.

### The bridge pattern — three instances of one shape

Where a vendored API must affect what nxvim actually paints or edits, the result
is **projected into nxvim's own highlight layer at the right priority**, never
allowed into core's sync path. The
[extmark / decoration layer](../specs/2026-06-07-extmark-decoration-layer-design.md)
is the shared substrate ([`extmark.rs`](../../crates/nxvim-core/src/extmark.rs):
`TS_HL_PRIORITY = 100` < `DEFAULT_PRIORITY = 4096`), so an async/plugin mark rides
*over* the synchronous treesitter floor:

1. **`vim.treesitter.start` / `stop`** (*implemented*) — a plugin/config toggling
   highlighting. Bridges to *enabling the native engine* for that buffer with a
   `lang` override, rather than running neovim's Lua highlighter on the redraw hot
   path. `start(buf, lang)` forces highlighting (even for an extension the table
   misses); `stop(buf)` disables it (even for a recognized extension). The prelude
   overrides `start`/`stop` to emit a `TsOp` the server forwards to
   `Editor::ts_start` / `ts_stop` (a per-buffer override consulted ahead of the
   extension table); `highlighter.new` (the real decoration provider) stays a loud
   `_notimpl`. A highlight-only `start` does *not* create a Lua `LanguageTree`, so
   such a buffer still parses once — the double parse begins only on `get_parser`.
2. **LSP semantic tokens** — async, server-side. Cached on arrival and projected
   as extmarks *above* the treesitter floor. Request plumbing exists in
   [`nxvim-lsp`](../../crates/nxvim-lsp/src/dispatch.rs); the response→extmark
   projection is the open wire.
3. **LSP on-type formatting** (if pursued) — an async edit that must use the
   arrive-late / apply-as-follow-up pattern, never blocking the keystroke.
4. **Query resolution → execution** — a plugin customizing a highlight/indent
   query (in-memory `query.set`, an `after/queries` overlay, or a `;extends`
   merge). The native engine reads a *single* `highlights.scm`; it does not run
   neovim's query-merge logic. The bridge goes one level deeper than the others:
   **Lua resolves** the final query string (`query.get` already merges explicit +
   `;extends` + runtimepath, faithfully), the server **pushes** it to the engine
   on buffer-open / `query.set`, and the engine **compiles + caches + executes** it
   (loud on a bad query). Lua owns resolution, Rust owns execution; it subsumes the
   in-memory-query leak in *Consequences* below. **Deferred behind bridge #1**;
   design: [treesitter query bridge](../specs/2026-06-08-treesitter-query-bridge-design.md).

Same shape each time: an enrichment that lives outside core's sync path is wired
into the projection layer; the synchronous treesitter floor is always underneath,
so a missing or slow server degrades to "syntactic but correct," never to blank.

## Consequences

**Positive.**
- Editor behavior is instant and always available; LSP is *allowed* to be lazy
  and absent precisely because the sync treesitter floor covers the gap.
- Plugins get bug-for-bug `vim.treesitter` / `vim.lsp`, updated by re-vendoring.
- One projection seam (`*_for(buffer, numbers, styles)` →
  [`window_value`](../../crates/nxvim-server/src/redraw.rs)) absorbs every
  highlight source — treesitter, extmarks, diagnostics — arbitrated by priority.

**Costs (accepted).**
- **Opt-in double parse.** A buffer a plugin queries from Lua is parsed by both
  the native engine and the Lua `LanguageTree`. Acceptable for v1; sharing the
  native tree into the Lua API is a *later* optimization (phase-1 already solved
  node lifetime across the boundary), gated on the cost actually biting.
- **Vendoring couples us to upstream internals.** The vendored Lua reaches for
  `vim.func._memoize`, `vim.validate`, `vim.iter`, `vim.regex`, `nvim_buf_attach`,
  decoration providers; each must be supplied or fail loud. That is the prelude's
  job and the same fill-as-found tax paid bringing up lspconfig — a standing
  maintenance surface, not a one-time cost.
- **One ecosystem assumption we cannot honor:** that a single tree both highlights
  the buffer *and* answers queries. Most plugins only query, so they don't notice.
  Where it leaks, it is a bridge or a documented approximation: `start()` →
  bridge (#1 above); a *customized* highlight/indent query — in-memory `query.set`,
  an `after/queries` overlay, or a `;extends` merge — doesn't change the paint
  today, because the engine reads a single `highlights.scm` rather than running
  neovim's query-merge → bridge (#4 above, deferred) and recorded in
  [known-approximations.md](../known-approximations.md); injection-aware plugins →
  the deferred injections work.

## Alternatives considered

- **Un-vendor — hand-write a thin `vim.treesitter` over the native engine.** One
  stack, full control, but forfeits the bug-for-bug fidelity that is the entire
  reason to vendor, and turns every neovim release into a chase. Rejected: the
  ecosystem *is* the product.
- **Route editor behavior through the vendored Lua to get "one stack."** Would
  drag Lua (and, for LSP, async I/O) into core's synchronous path, breaking the
  identical-behavior-across-front-ends guarantee and adding per-keystroke latency
  and blank frames. Rejected: it inverts the constraint that makes the async
  enrichment model viable in the first place.

## Related

- [In-process treesitter + indentation](../specs/2026-06-06-in-process-treesitter-and-indentation-design.md)
  — the native engine; why the worker decision was reversed to make it sync.
- [The `vim.treesitter` Lua platform](../specs/2026-06-07-vim-treesitter-lua-platform.md)
  — the vendored API and the snapshot seams.
- [Extmark / decoration layer](../specs/2026-06-07-extmark-decoration-layer-design.md)
  — the shared projection substrate the bridges target.
- [LSP support](../specs/2026-06-02-lsp-support-design.md) — the async server side.
