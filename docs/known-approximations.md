# Known approximations & missing features

A curated registry of where nxvim's `vim.*` / editor surface **diverges from
neovim** — both *silent approximations* (it does something plausible, but not
fully faithfully) and *loud gaps* (it refuses to fake it and raises `not
implemented`). The scope is what we have **actually bumped into** running real
configs and plugins, plus the whole subsystems we know are still absent.

This file is for planning a sweep. The authoritative, per-function detail lives
**in the code**, next to each call site — this doc points you at it and records
only what has *no* call site to tag.

## Two kinds of divergence, and where each is tracked

1. **Silent approximation** — returns/does something that looks right but isn't
   fully faithful (the dangerous kind: it makes a half-working feature look
   whole). Tagged in code with a greppable `INCOMPLETE:` comment directly above
   the call site, stating what's wrong and what a faithful implementation needs.

2. **Loud gap** — not implemented, and it says so: raises
   `nxvim: not implemented: <name>` through `vim._notimpl(name)` rather than
   returning a fake value. Self-documenting at the call site (the name is in the
   raise) and at runtime (see below). This is the project's no-silent-stubs rule
   (`CLAUDE.md`, and `docs/plans/2026-06-05-lsp-completion.md` Phase 0).

The per-function list is **generated from the code, not maintained here**:

```sh
# Silent approximations — each with its "why" and the fix, inline:
grep -rn 'INCOMPLETE:' crates/

# Loud "not implemented" gaps — the call sites:
grep -rn 'vim\._notimpl(' crates/nxvim-lua/src/prelude/
```

At runtime, every loud gap a real config trips is recorded in the
`vim._notimpl_hits` set and enumerated by `vim.lsp._report()` (and a future
`:checkhealth`) — so you can see exactly which gaps *a given config* hit, not
just which exist.

When you implement one: delete its `INCOMPLETE:` tag (or its `vim._notimpl`
raise). If it's one of the subsystems below, update this file too.

## Missing features not yet in code

These have **no single call site to tag** because the subsystem itself is
absent — a config touching them hits a nil index or a generic error, not a named
gap. Recorded here so the sweep doesn't lose them.

- **Treesitter Lua API — now a real platform, with a few deferred edges.** The
  `vim.treesitter` plugin API is implemented: `get_parser(buf):parse()`,
  `get_string_parser`, `get_node`/`get_node_text`, and `query.parse` +
  `iter_captures`/`iter_matches` with predicates/directives all run neovim's
  vendored Lua on bespoke Rust primitives (see the
  [platform design](specs/2026-06-07-vim-treesitter-lua-platform.md)). Remaining
  gaps, each a *deliberate* deferral rather than a silent stub:
  - **`vim.treesitter.start` / `stop` bridge to the native engine** (ADR 0001,
    bridge #1 — *implemented*). nxvim does not run neovim's decoration-provider
    highlighter on the redraw hot path; instead `start(buf, lang)` enables the
    in-core Rust engine for that buffer at `lang` (forcing highlighting even for an
    extension the built-in table misses), and `stop(buf)` disables it (even for a
    recognized extension). `vim.treesitter.highlighter` stays a small shim —
    legacy-API probes (`hl_map`) read nil, `active[buf]` reflects the bridge's
    on/off state, and the real decoration-provider entry point `highlighter.new`
    still fails loud (`vim._notimpl`). The one approximation that remains: a
    highlight-only `start` does not create a Lua-side `LanguageTree`, so a config
    that calls `start` and then reaches for `vim.treesitter.highlighter.active[buf]
    .tree` (rare) won't find one until something calls `get_parser`.
  - **Customized queries — `query.set` *and* on-disk overlays both change the
    paint.** The query-resolution bridge (ADR 0001, #4 — Lua resolves, the engine
    executes) is **built**, via two triggers. (1) `vim.treesitter.query.set(lang,
    name, text)`: the server pulls the merged string back through the vendored
    `query.get` (so a `;extends` modeline in the set text merges onto the base) and
    pushes it to the engine, which recompiles in place. (2) The **buffer-open
    trigger**: the first time a buffer of some language is highlighted, the server
    resolves `highlights`/`indents` through the same `query.get` and offers them to
    the engine, which keeps the override only when it differs from the base file —
    so a *pure on-disk* `after/queries/<lang>/*.scm` overlay or a `;extends` /
    `;inherits` file dropped in with **no** `query.set` call also reaches the paint,
    while an un-customized language stays byte-identical on the disk-read path. The
    engine's data dir is on the Lua runtimepath, so the resolver sees the same base
    `queries/<lang>/` the engine reads. A broken query echoes loud and keeps the
    prior paint. See
    [ADR 0001](decisions/0001-native-engines-vendored-lua-apis.md) and
    [the query-bridge design](specs/2026-06-08-treesitter-query-bridge-design.md).
  - **Injections — built** (ADR 0001, #5 — Lua resolves the injection query, the
    engine executes the layers). The engine runs the resolved `injections` query
    over the root tree each parse, parses each injected region with its child grammar
    through `included_ranges` (buffer-coordinate child trees, incremental reparse,
    a per-frame parse budget), and paints child captures over the host — single,
    combined (`#set! injection.combined`), and nested (to a depth bound) injections,
    with the full `injection.language` / `self` / `parent` / `include-children`
    directive vocabulary. On the platform side the vendored `LanguageTree` builds the
    same injected child trees over nxvim's snapshot primitives, so `children()` /
    `language_for_range` / `get_node(…, ignore_injections=false)` resolve the injected
    language; a drift oracle test asserts the engine's paint agrees with the vendored
    `_get_injections`. A missing child grammar degrades to the host's flat paint. See
    [the injections design](specs/2026-06-08-treesitter-injections-design.md).
  - **Live incremental buffer updates — built** (`nvim_buf_attach` `on_bytes`).
    A buffer-sourced `LanguageTree` attaches through the vendored `_create_parser`
    and edits its trees from real `on_bytes`/`on_reload` callbacks, so `:parse()`
    reparses **incrementally** (re-lexing only the changed ranges) instead of
    re-reading and fully reparsing the snapshot. Two `on_bytes` sources keep the
    tree in lockstep with the snapshot the parser reads: the server drains each
    buffer's byte-delta journal (the same `BufferEdit` stream the native engine
    reparses from) and fires `on_bytes` before any plugin Lua runs, for core edits
    (keystrokes, ex commands); and the Lua `nvim_buf_set_lines` write-through fires
    `on_bytes` synchronously for plugin edits within an entry (the server then
    suppresses its own fire for that edit so the tree is never double-edited). A
    whole-rope replacement (undo/redo, `:e`) arrives as `on_reload` (a full
    reparse), not as meaningless deltas. String parsers keep their cached trees as
    before.
  - **Lua-driven indent** (`indentexpr=v:lua…` / `indent.lua`) fights the
    snapshot bridge (it wants the live buffer mid-keystroke); the Rust indent
    stays. `query.get` needs `io` for on-disk `queries/<lang>/*.scm`; a missing
    query file returns nil.
- **`vim.uv` / `vim.loop` beyond timers.** `new_pipe`, TCP (`new_tcp` — the
  TCP transport behind the skipped gdscript `vim.lsp.rpc.connect`), and
  event-based `fs_*` watchers are absent.
- **Broad options surface.** `:set` honors the search/number booleans plus the
  buffer-local indentation options `tabstop` / `shiftwidth` / `softtabstop` /
  `expandtab` (also via `:setlocal`, `vim.bo`, and `nvim_{set,get}_option_value`).
  nxvim breaks with vim's defaults here: `tabstop` defaults to **4**, with
  `shiftwidth=0` ("follow tabstop") and `softtabstop=-1` ("follow shiftwidth") so
  the one `tabstop` knob drives the whole indent width. `tabstop`, `softtabstop`,
  and `expandtab` drive rendering and `<Tab>`; `shiftwidth` only feeds the LSP
  indent width until the `>>`/`<<` operators land. The rest of vim's hundreds of
  options are still missing. Also: folds and macros (registers — named/numbered/
  special + the system clipboard — and marks — buffer-local, global, and the
  special marks — are both implemented). (`:s` substitution *is* implemented — ex-range parsing, the
  `g`/`i`/`I`/`n`/`c` flags with confirm, pattern/replacement reuse and repeat;
  it speaks the same canonical-regex dialect as `/` search, not vim magic. See
  `docs/plans/2026-06-07-substitute-command.md`.)
- **Legacy Vimscript (`eval.c`).** Deliberately **not** on the roadmap (guiding
  principle 2). `vim.fn.*` is a hand-written compatibility shim, not an
  interpreter — unimplemented `vim.fn.*` entries are loud gaps, not a TODO to
  build an evaluator.
- **`:TSInstall`-style grammar fetch/compile.** Grammars load from the data dir;
  installing them there is manual.
- **LSP semantic tokens — complete (Phases 1–3), with approximations.** A server
  that advertises `semanticTokensProvider` has its whole-buffer
  `textDocument/semanticTokens/full`(/`delta`) set decoded and painted *over* the
  treesitter floor (ADR 0001 bridge #2 — `crates/nxvim-server/src/lsp/semantic.rs`),
  and the `vim.lsp.semantic_tokens` control surface
  (`start`/`stop`/`force_refresh`/`get_at_pos`/`enable`) plus
  `client.server_capabilities.semanticTokensProvider` are live. The approximations:
  **one resolvable group per cell** — a token paints its most-specific `@lsp.*`
  capture that resolves, not neovim's stack of `@lsp.type.<t>` + per-modifier
  `@lsp.mod.<m>`/`@lsp.typemod.<t>.<m>` (the merge layer picks one winner, it doesn't
  blend); **theme-gated** — a token whose group is undefined is dropped so the floor
  shows (no semantic paint without `@lsp.*` definitions); **no `range`** — only
  `full`/`full/delta`, the whole document is tokenized; **`highlight_token` is a loud
  gap** (`vim._notimpl` — it would put a Lua callback on the decode hot path);
  `get_at_pos` reads the cached mirror even for a `stop`ped buffer (the cache
  survives a stop), and the per-buffer `start`/`stop` has no per-client granularity
  (one semantic cache per buffer); repaints on every reply including mid-insert
  (`update_in_insert` always on). See
  `docs/plans/2026-06-08-lsp-semantic-tokens.md` and `examples/semantic-tokens/`.
- **LSP inlay hints — Phase 1 complete, with approximations.** A buffer with
  `vim.lsp.inlay_hint.enable(true)` requests `textDocument/inlayHint` from a server
  that advertises the provider (on enable + after each change), decodes the hints
  against the negotiated encoding, and paints each one **inline** at its column —
  shifting the real glyphs and the cursor right
  (`crates/nxvim-server/src/lsp/inlay.rs`, the splice in
  `crates/nxvim-tui/src/render.rs::highlight_line`). Unlike semantic tokens it is
  **opt-in** (off by default). The approximations: **string labels only** — label
  *parts* are joined to their `value`s, dropping the per-part `location` (go-to on
  click) / `tooltip` / `textEdits`-on-accept (these need `inlayHint/resolve`,
  Phase 2); **one `LspInlayHint` group** for all kinds (no
  `LspInlayHintType`/`Parameter` split), with a dim built-in fallback when the
  group is undefined; **whole document, no `range`** (the viewport-scoped request
  is Phase 2 — the whole buffer is requested on every change); **per-buffer enable
  only** (no per-client granularity; `vim.lsp.inlay_hint.get` is Phase 2);
  horizontal-scroll (`leftcol>0`) + inline hints is best-effort (the cursor shift
  ignores scrolled-off hints); repaints on every reply including mid-insert
  (`update_in_insert` always on). See `docs/plans/2026-06-08-lsp-inlay-hints.md`
  and `examples/inlay-hints/`.
- **Synchronous prompts — now implemented.** `vim.fn.input` / `vim.fn.confirm`
  return the user's answer *inline*: a pumped Lua entry (`:lua` chunk, keymap, or
  user command) runs inside a coroutine via `vim._pump`, so the prompt
  `coroutine.yield`s to park the chunk on the command line and the result resumes
  it. See `examples/sync-prompts/`. (The remaining caveat: only *pumped* entry
  points can prompt — Lua sourced at startup or off a bare callback has no
  coroutine to yield from.)

## Cross-cutting root causes

Many individual approximations share one root cause — fix the root and a batch
clears at once. (Run the `grep` above for the current, exact call-site list.)

| Root cause | Approximations it clears |
|---|---|
| LSP helpers not window-arg-aware (always use the current window) | `make_position_params(window)` ignores its `window` arg, `open_floating_preview` returns placeholder handles, the single completion-doc preview box (no separate preview-window handle / `completeopt` matrix). Note: splits, floats, and tab pages themselves are implemented — see architecture.md *Windows*; it's these LSP-side helpers that still assume the current window. |
| No multi-buffer name/disk registry | `make_text_document_params` (non-current bufnr → empty URI), `locations_to_items` & `apply_workspace_edit` for unopened files |
| Core honors only the indentation buffer-local options | `vim.bo` / `nvim_set_option_value` writes other than `filetype` / `tabstop` / `shiftwidth` / `expandtab` are recorded but inert |
| No per-buffer command registry | `nvim_buf_create_user_command` registers globally |
| Diagnostic-display surfaces are approximations, not gaps — all four ship. `underline`, `virtual_text` (inline end-of-line message), `signs` (gutter glyph), and the on-demand float (`vim.diagnostic.open_float`) are implemented — see `docs/plans/2026-06-08-diagnostic-display-surfaces.md`. | `vim.diagnostic.config` keys other than `underline` / `virtual_text` / `signs` (`virtual_lines`, `severity_sort`, and the `config.float` pre-style defaults). `open_float` ignores its `opts` (scope/severity filters, `format`/`header`/`prefix`/`border`) — the default cursor-line scope shows, in the bottom panel (plain lines, like hover) not a cursor-anchored bordered popup. The `virtual_text` table honors `prefix` and the `signs` table its `text` glyph map; their `format` / `severity` filters and sign `priority`/`culhl` are not applied, the line's most-severe diagnostic wins the one inline slot / sign cell, and the sign column is client-side only (a fixed 2 cells not subtracted from `nxvim-core`'s text width, so a full-width line under `nowrap` can clip its last two cells). |
| No per-namespace highlight tables | `nvim_set_hl` non-zero namespace folded into global |

## Relationship to the LSP completion plan

[`docs/plans/2026-06-05-lsp-completion.md`](plans/2026-06-05-lsp-completion.md) is the **phased route**
that drove the LSP surface from "every hollow stub raises" (Phase 0) to today;
each phase notes the approximations it deliberately left behind. That document is
*history + plan*, not a live registry. The live registry is the `INCOMPLETE:` /
`vim._notimpl` tags in code (and this file's missing-features list above). If the
two ever disagree, the code wins.
