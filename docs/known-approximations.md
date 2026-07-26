# Known approximations & missing features

nxvim tracks vim/neovim's **observable editing behavior**, but it is a fresh
Rust implementation, not a port — so some surfaces are only *approximate*, and
some subsystems simply aren't built yet. This page maps where nxvim **diverges
from neovim**, so a config or plugin you bring over holds no surprises.

Two principles shape what follows:

- **No silent stubs.** Anything unimplemented **fails loud** — it raises
  `nxvim: not implemented: <name>` rather than quietly returning a fake value, so
  a half-working feature never masquerades as a whole one. You find out at
  runtime exactly what tripped, instead of chasing mysterious wrong behavior.
- **Honest approximations.** Where nxvim does something plausible but not fully
  faithful, that is flagged in the source right beside the code, and the larger
  ones are listed below.

## Seeing what your own config hits

Every loud gap a running config trips is collected in `nx._notimpl_hits`, so you
can see precisely which gaps *you* actually hit — not just which ones exist:

```lua
:lua print(vim.inspect(nx._notimpl_hits))
```

The precise, always-current per-function detail lives **in the code**: greppable
`INCOMPLETE:` comments mark the silent approximations (each with its "why" and
the fix), and `nx._notimpl` raises mark the loud gaps. The sections below cover
the larger divergences and the whole subsystems that have no single line to point
at. If this page and the code ever disagree, **the code is right** — treat this
as a guide, not the registry.

## Missing or partial subsystems

These are whole areas where the subsystem itself is absent or only partly built,
so a config that leans on one meets a nil value or a generic error rather than a
named gap — worth knowing up front. (Subsystems that are now **fully built** — the
native tree-sitter engine with its `nx.bo.filetype` / `nx.bo.ts_highlight` nouns,
on-disk and customized queries, injections, incremental updates, and `:TSInstall`
fetch/compile — aren't listed; only the edges that still diverge are.)

- **Treesitter — a native engine, not neovim's Lua platform.** The vendored
  `vim.treesitter` Lua platform (`get_parser`, `LanguageTree`, `highlighter.*`,
  `query.get`) was deleted (see
  [the deletion plan](plans/2026-06-12-vendored-treesitter-deletion.md) and
  [ADR 0001](decisions/0001-native-engines-vendored-lua-apis.md)) — a plugin
  reaching for it hits a loud nil index. Highlighting is driven by the in-core
  Rust engine through the declarative buffer nouns `nx.bo.filetype` /
  `nx.bo.ts_highlight` (the one `vim.treesitter` survivor is the `foldexpr`
  marker), and query customization rides the runtimepath `queries/` /
  `after/queries/` overlay (next bullet).
  **Lua-driven indent** (`indentexpr=v:lua…` / `indent.lua`) is unwired — it
  wants the live buffer mid-keystroke, which fights the snapshot bridge, so the
  Rust indent stays.
- **Treesitter query resolution — additive, host-only.** The query bridge
  ([design](specs/2026-06-08-treesitter-query-bridge-design.md)) merges a language's
  bundled base with runtimepath `queries/` + `after/queries/` and the `; inherits:`
  chain — and `:TSInstall` fetches the inherited query sets too (`javascript` →
  `ecma`,`jsx`), so base js/ts highlighting carries the `ecma` patterns. Two edges
  remain: (1) the merge is **additive concatenation**, not neovim's full
  replace-vs-extend precedence; (2) it resolves only the **buffer's own** language —
  an **injected child** grammar still loads its query off disk raw, so an
  `; inherits:`-based child (e.g. `javascript` injected into markdown) paints only
  its own non-inherited captures until that child language is itself opened as a
  buffer (which installs its resolved overlay).
- **No `vim.uv` / `vim.loop`.** neovim exposes libuv as a public Lua API; nxvim
  does not — the `vim.uv` / `vim.loop` table does not exist, so a plugin reaching
  for it hits a loud nil index. Both the libuv **handle** surface (`new_timer` /
  `new_check` / `new_fs_event` / `spawn` / `new_pipe`, the plugin event-loop
  primitives) and the synchronous `fs_*` / scalar primitives (`fs_realpath`,
  `cwd`, `os_homedir`, `os_uname`, `hrtime`, `now`) are gone. There are no
  synchronous filesystem `vim.fn` builtins either (no blocking I/O ever runs on
  the editor thread): async lives in the `nx` API (`nx.run` / `nx.timer` /
  `nx.fs`), and the LSP-config root search runs on the same async `nx.fs` seam.
- **Broad options surface.** The set of honored options has grown well past the
  indentation knobs (the authoritative list is `crates/nxvim-core/src/options.rs`).
  `:set` (and `:setlocal` / `vim.bo` / `nvim_{set,get}_option_value`) honors: the
  **search** booleans (`ignorecase` / `smartcase` / `wrapscan` / `hlsearch` /
  `incsearch`); the **window-local rendering** options (`number` /
  `relativenumber` / `cursorline` / `numberwidth` / `signcolumn` / `wrap` /
  `breakindent` / `showbreak` / `sidescroll` / `sidescrolloff` / `winhighlight` /
  `fillchars`); the **fold** options (`foldmethod` / `foldenable` / `foldcolumn` /
  `foldlevel`); the **buffer-local indentation** options (`tabstop` / `shiftwidth`
  / `softtabstop` / `expandtab` / `commentstring`); and a set of **nxvim-native**
  options (`scrollanim` / `scrollanimduration`, `qfdock`, `imagepreview`,
  `history` / `persisthistory`, `regexsyntax`, `switchbuf`,
  `laststatus` / `showtabline`, …).
  nxvim breaks with vim's defaults on indentation: `tabstop` defaults to **4**,
  with `shiftwidth=0` ("follow tabstop") and `softtabstop=-1` ("follow
  shiftwidth") so the one `tabstop` knob drives the whole indent width.
  `tabstop`, `softtabstop`, and `expandtab` drive rendering and `<Tab>`;
  `shiftwidth` drives the `>>`/`<<` shift operators and the LSP indent width.
  `commentstring` backs the `gc`/`gcc` comment operator and defaults from the
  filetype (the ~20 most common languages) when unset. Still, the **bulk** of
  vim's hundreds of options are missing — a write to an unmodeled option is
  recorded but inert — as are **macros**.
- **Legacy Vimscript (`eval.c`).** Deliberately **not** on the roadmap (guiding
  principle 2). `vim.fn.*` is a hand-written set of helper aliases, not an
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
  **no `highlight_token` hook** (neovim's per-token Lua callback on the decode
  hot path — the name is absent, a loud nil index);
  `get_at_pos` reads the cached mirror even for a `stop`ped
  buffer; no per-client granularity (one cache per buffer); repaints mid-insert
  (`update_in_insert` always on). See `docs/plans/2026-06-08-lsp-semantic-tokens.md`.
- **LSP inlay hints approximations.** Painted inline, opt-in
  (`crates/nxvim-server/src/lsp/inlay.rs`). `inlayHint/resolve` (lazy per-hint
  label fill) and `vim.lsp.inlay_hint.get` (with a line-range filter) have landed.
  What still diverges: **one `LspInlayHint` group** for all kinds (no
  Type/Parameter split); the fetch is **whole-document** — the viewport-scoped
  `range` request is deferred; **per-buffer enable only** (no per-client
  granularity); horizontal-scroll (`leftcol>0`) + inline hints is best-effort;
  repaints mid-insert. See `docs/plans/2026-06-08-lsp-inlay-hints.md`.
- **No synchronous prompts.** The blocking `vim.fn.input` / `vim.fn.confirm`
  are omitted by design (loud gaps like the rest of unimplemented `vim.fn.*`) —
  prompting is promise-only: `nx.ui.input` / `nx.ui.select` / `nx.ui.confirm`,
  awaited inside `nx.async`. A config expecting an inline return must be
  reshaped onto the promise. See `examples/ui-prompt/`.
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
  Mouse-click segment regions have **landed**: a segment can carry an
  `on_click` handler, lowered to the `%@func@…%X` statusline syntax (with `%nT`
  tabline labels and `laststatus=3`), so clicks dispatch back to Lua. What it
  does **not** do yet: (1) The custom segment `ctx` carries `{ buf, win,
  focused }` but **no `width`** (the server doesn't mirror the per-window
  statusline width to Lua). (2) `git` / `lsp_progress` are *plugin* segments
  (custom-segment examples), not built-ins. The built-in set is `mode` /
  `filename` / `filepath` / `filetype` / `encoding` / `location` / `modified` /
  `readonly` / `diagnostics`.

## Shared limitations

A handful of the approximations above trace back to the same underlying
limitation, so they tend to surface together. The notable ones:

| Limitation | What it affects |
|---|---|
| The completion menu is server-owned chrome, not a window | The completion-doc preview is a single bespoke box with no separate preview-window handle / `'completeopt'` matrix. (Most of the `vim.lsp.util` helper surface — `make_position_params`, `open_floating_preview`, `make_text_document_params`, `locations_to_items` — is gone with the compat layer: LSP goes through the native `nx.lsp.*` verbs and hover/signature through the cursor float. Only `apply_workspace_edit` and `show_document` survive there, as aliases of `nx.lsp.apply_workspace_edit` / `nx.lsp.show_document`; the former loads unopened files into buffers on the spot. Splits, floats, and tab pages themselves are implemented — see architecture.md *Windows*.) |
| Core honors a fixed set of buffer-local options | `vim.bo` / `nvim_set_option_value` writes outside the wired set — `filetype` / `ts_highlight` / `tabstop` / `shiftwidth` / `softtabstop` / `expandtab` / `autoindent` / `smartindent` / `autopairs` / `indentemptylines` / `commentstring` / `regexsyntax` / `fileencoding` / `bomb` / `fileformat` / `modifiable` / `modified` / the fold options (`foldmethod` / `foldexpr` / `foldmarker` / `foldnestmax` / `foldminlines`) — are recorded but inert (`buftype` is mirrored read-only) |
| Diagnostic-display surfaces are approximations, not gaps — all four ship. `underline`, `virtual_text` (inline end-of-line message), `signs` (gutter glyph), and the on-demand float (`vim.diagnostic.open_float`) are implemented — see `docs/plans/2026-06-08-diagnostic-display-surfaces.md`. | `vim.diagnostic.config` keys other than `underline` / `virtual_text` / `signs` (`virtual_lines`, `severity_sort`, and the `config.float` pre-style defaults). `open_float` ignores its `opts` (scope/severity filters, `format`/`header`/`prefix`/`border`) — the default cursor-line scope shows, in the bottom panel (plain lines, like hover) not a cursor-anchored bordered popup. The `virtual_text` table honors `prefix` and the `signs` table its `text` glyph map; their `format` / `severity` filters and sign `priority`/`culhl` are not applied, the line's most-severe diagnostic wins the one inline slot / sign cell, and the sign column is client-side only (a fixed 2 cells not subtracted from `nxvim-core`'s text width, so a full-width line under `nowrap` can clip its last two cells). |
