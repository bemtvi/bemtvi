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
   `nxvim: not implemented: <name>` through `nx._notimpl(name)` rather than
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

At runtime, every loud gap a real config trips is recorded in the global
`nx._notimpl_hits` set (populated by `nx._notimpl` wherever it fires —
`vim.fn`, the LSP layer, …), so you can see exactly which gaps *a given
config* hit, not just which exist: inspect it directly
(`:lua print(vim.inspect(nx._notimpl_hits))`), or via the future `:checkhealth`.
The runtime scoreboard `nx._report()` also surfaces it as its `notimpl_hits`
field, alongside the LSP-specific status.

When you implement one: delete its `INCOMPLETE:` tag (or its `nx._notimpl`
raise). If it's one of the subsystems below, update this file too.

## Missing features not yet in code

These have **no single call site to tag** because the subsystem itself is
absent — a config touching them hits a nil index or a generic error, not a named
gap. Recorded here so the sweep doesn't lose them. Subsystems that are now
**fully built** (the treesitter platform — `start`/`stop`, customized/on-disk
queries, injections, incremental `on_bytes` updates; `:TSInstall` fetch/compile)
are no longer listed here — only the edges that still diverge are.

- **Treesitter — two edges remain.** The `vim.treesitter` platform is built (see
  the [platform design](specs/2026-06-07-vim-treesitter-lua-platform.md) and
  [ADR 0001](decisions/0001-native-engines-vendored-lua-apis.md)). What still
  diverges: (1) the decoration-provider highlighter `highlighter.new` fails loud
  (`nx._notimpl`) — nxvim's `start`/`stop` drives the in-core Rust engine
  instead, so a highlight-only `start` never builds a Lua-side `LanguageTree` and
  `highlighter.active[buf].tree` reads nil until something calls `get_parser`;
  (2) **Lua-driven indent** (`indentexpr=v:lua…` / `indent.lua`) is unwired — it
  wants the live buffer mid-keystroke, which fights the snapshot bridge, so the
  Rust indent stays. `query.get` returns nil for a missing on-disk query file.
- **No `vim.uv` / `vim.loop`.** neovim exposes libuv as a public Lua API; nxvim
  does not — the `vim.uv` / `vim.loop` table does not exist, so a plugin reaching
  for it hits a loud nil index. Both the libuv **handle** surface (`new_timer` /
  `new_check` / `new_fs_event` / `spawn` / `new_pipe`, the plugin event-loop
  primitives) and the synchronous `fs_*` / scalar primitives (`fs_realpath`,
  `cwd`, `os_homedir`, `os_uname`, `hrtime`, `now`) are gone. Async lives in the
  `nx` API (`nx.run` / `nx.timer` / `nx.fs`); the synchronous host info the
  LSP-config paths need is read through `vim.fn` (`executable` / `exepath` /
  `glob` / `filereadable` / `resolve` / …) instead.
- **Broad options surface.** `:set` honors the search/number booleans plus the
  buffer-local indentation options `tabstop` / `shiftwidth` / `softtabstop` /
  `expandtab` (also via `:setlocal`, `vim.bo`, and `nvim_{set,get}_option_value`).
  nxvim breaks with vim's defaults here: `tabstop` defaults to **4**, with
  `shiftwidth=0` ("follow tabstop") and `softtabstop=-1` ("follow shiftwidth") so
  the one `tabstop` knob drives the whole indent width. `tabstop`, `softtabstop`,
  and `expandtab` drive rendering and `<Tab>`; `shiftwidth` only feeds the LSP
  indent width until the `>>`/`<<` operators land. The rest of vim's hundreds of
  options are still missing, as are **folds** and **macros**.
- **Legacy Vimscript (`eval.c`).** Deliberately **not** on the roadmap (guiding
  principle 2). `vim.fn.*` is a hand-written compatibility shim, not an
  interpreter — unimplemented `vim.fn.*` entries are loud gaps, not a TODO to
  build an evaluator.
- **`:TSInstall` approximations.** The command fetches/compiles grammars
  (`nxvim_ts::install`), with a pinned, checksum-verified Zig fetched on demand
  when no system `cc`/`clang`/`gcc`/`zig` (or `$NXVIM_CC`) is found — on macOS,
  Linux, and Windows alike. Remaining: (1) grammars needing `tree-sitter
  generate` (no committed `src/parser.c`) fail loud rather than generating;
  (2) the nvim-treesitter ref is pinned in source — no `:TSInstall`-from-`HEAD`.
- **LSP semantic tokens approximations.** Painted over the treesitter floor
  (`crates/nxvim-server/src/lsp/semantic.rs`): **one resolvable group per cell**
  (the merge picks the most-specific `@lsp.*` winner, it doesn't blend neovim's
  `@lsp.type.<t>` + per-modifier stack); **theme-gated** (an undefined group is
  dropped so the floor shows); **no `range`** (only `full`/`full/delta`);
  **`highlight_token` is a loud gap** (`nx._notimpl` — a Lua callback on the
  decode hot path); `get_at_pos` reads the cached mirror even for a `stop`ped
  buffer; no per-client granularity (one cache per buffer); repaints mid-insert
  (`update_in_insert` always on). See `docs/plans/2026-06-08-lsp-semantic-tokens.md`.
- **LSP inlay hints approximations.** Painted inline, opt-in
  (`crates/nxvim-server/src/lsp/inlay.rs`): **string labels only** (per-part
  `location`/`tooltip`/`textEdits` need `inlayHint/resolve`, Phase 2); **one
  `LspInlayHint` group** for all kinds (no Type/Parameter split); **whole
  document, no `range`** (Phase 2); **per-buffer enable only** (no per-client
  granularity; `vim.lsp.inlay_hint.get` is Phase 2); horizontal-scroll
  (`leftcol>0`) + inline hints is best-effort; repaints mid-insert. See
  `docs/plans/2026-06-08-lsp-inlay-hints.md`.
- **Synchronous prompts — one caveat.** `vim.fn.input`/`confirm` return inline
  via a pumped coroutine (`nx._pump`), but only *pumped* entry points (`:lua`
  chunk, keymap, user command) can prompt — Lua sourced at startup or off a bare
  callback has no coroutine to yield from. See `examples/sync-prompts/`.
- **`nx.statusline` segment registry — v1 deferrals.** The lualine-shaped surface
  is built (`nx.statusline.setup`/`segment`/`invalidate`; built-ins composed in
  `nxvim_core::statusline::compose_segments`, custom segments rendered per window
  and cached server-side; see
  `docs/plans/2026-06-15-nx-statusline-segments.md`). Custom segments **are now
  per-window**: each is rendered once per window against that window's
  `{ buf, win, focused }`, cached by `(window, name)`, and re-rendered when the
  segment is invalidated or the window layout changes (split/close, focus move, or
  a window swapping its buffer) — so `ctx.focused` and `ctx.buf` are correct in
  every window. The server orchestrates the re-render from `run_pending` with a
  fresh window mirror (`EditHost::refresh_statusline_segments`), so an invalidate
  fired from an autocmd that ran before the transition still renders against the
  settled layout. Layouts are also **per-window / `setlocal`-able**:
  `nx.statusline.setup{win=…}` sets a window-local layout that overrides the
  global one, `setup{win=…, format=true}` opts a window back to the `'statusline'`
  `%`-format even under a global segment layout (the per-region mix), and
  `nx.statusline.reset(win)` drops the override (`EditHost::resolve_window_layout`).
  What it does **not** do yet: (1) The custom segment `ctx` carries
  `{ buf, win, focused }` but **no `width`** (the server doesn't mirror the
  per-window statusline width to Lua). (2) No mouse-click segment regions (shared
  with the deferred tabline `%@…@` work). (3) `git` / `lsp_progress` are *plugin*
  segments (custom-segment examples), not built-ins. The built-in set is `mode` /
  `filename` / `filepath` / `filetype` / `encoding` / `location` / `modified` /
  `readonly` / `diagnostics`.

## Cross-cutting root causes

Many individual approximations share one root cause — fix the root and a batch
clears at once. (Run the `grep` above for the current, exact call-site list.)

| Root cause | Approximations it clears |
|---|---|
| LSP helpers not window-arg-aware (always use the current window) | `make_position_params(window)` is now window-aware (reads the passed window's buffer + cursor), and `open_floating_preview` returns real float handles (a `relative="cursor"` float over a scratch buffer, auto-closing on cursor move). **Remaining:** the completion-doc preview box is still a single bespoke box with no separate preview-window handle / `completeopt` matrix (the completion menu is server-owned chrome, not a window). Note: splits, floats, and tab pages themselves are implemented — see architecture.md *Windows*. |
| No multi-buffer name/disk registry | `make_text_document_params` (non-current bufnr → empty URI), `locations_to_items` & `apply_workspace_edit` for unopened files |
| Core honors only the indentation buffer-local options | `vim.bo` / `nvim_set_option_value` writes other than `filetype` / `tabstop` / `shiftwidth` / `expandtab` are recorded but inert |
| Diagnostic-display surfaces are approximations, not gaps — all four ship. `underline`, `virtual_text` (inline end-of-line message), `signs` (gutter glyph), and the on-demand float (`vim.diagnostic.open_float`) are implemented — see `docs/plans/2026-06-08-diagnostic-display-surfaces.md`. | `vim.diagnostic.config` keys other than `underline` / `virtual_text` / `signs` (`virtual_lines`, `severity_sort`, and the `config.float` pre-style defaults). `open_float` ignores its `opts` (scope/severity filters, `format`/`header`/`prefix`/`border`) — the default cursor-line scope shows, in the bottom panel (plain lines, like hover) not a cursor-anchored bordered popup. The `virtual_text` table honors `prefix` and the `signs` table its `text` glyph map; their `format` / `severity` filters and sign `priority`/`culhl` are not applied, the line's most-severe diagnostic wins the one inline slot / sign cell, and the sign column is client-side only (a fixed 2 cells not subtracted from `nxvim-core`'s text width, so a full-width line under `nowrap` can clip its last two cells). |

## Relationship to the LSP completion plan

[`docs/plans/2026-06-05-lsp-completion.md`](plans/2026-06-05-lsp-completion.md) is the **phased route**
that drove the LSP surface from "every hollow stub raises" (Phase 0) to today;
each phase notes the approximations it deliberately left behind. That document is
*history + plan*, not a live registry. The live registry is the `INCOMPLETE:` /
`nx._notimpl` tags in code (and this file's missing-features list above). If the
two ever disagree, the code wins.
