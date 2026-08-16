# Known approximations & missing features

bemtvi tracks vim/neovim's **observable editing behavior**, but it is a fresh
Rust implementation, not a port — so some surfaces are only *approximate*, and
some subsystems simply aren't built yet. This page maps where bemtvi **diverges
from neovim**, so a config or plugin you bring over holds no surprises.

Two principles shape what follows:

- **No silent stubs.** Anything unimplemented **fails loud** — it raises
  `bemtvi: not implemented: <name>` rather than quietly returning a fake value, so
  a half-working feature never masquerades as a whole one. You find out at
  runtime exactly what tripped, instead of chasing mysterious wrong behavior.
- **Honest approximations.** Where bemtvi does something plausible but not fully
  faithful, that is flagged in the source right beside the code, and the larger
  ones are listed below.

## Seeing what your own config hits

Every loud gap a running config trips is collected in `btv._notimpl_hits`, so you
can see precisely which gaps *you* actually hit — not just which ones exist:

```lua
:lua print(vim.inspect(btv._notimpl_hits))
```

The precise, always-current per-function detail lives **in the code**: greppable
`INCOMPLETE:` comments mark the silent approximations (each with its "why" and
the fix), and `btv._notimpl` raises mark the loud gaps. The sections below cover
the larger divergences and the whole subsystems that have no single line to point
at. If this page and the code ever disagree, **the code is right** — treat this
as a guide, not the registry.

## Missing or partial subsystems

These are whole areas where the subsystem itself is absent or only partly built,
so a config that leans on one meets a nil value or a generic error rather than a
named gap — worth knowing up front. (Subsystems that are now **fully built** — the
native tree-sitter engine with its `btv.bo.filetype` / `btv.bo.ts_highlight` nouns,
on-disk and customized queries, injections, incremental updates, and `:TSInstall`
fetch/compile — aren't listed; only the edges that still diverge are.)

- **Treesitter — a native engine, not neovim's Lua platform.** The vendored
  `vim.treesitter` Lua platform (`get_parser`, `LanguageTree`, `highlighter.*`,
  `query.get`) was deleted (see
  [the deletion plan](plans/2026-06-12-vendored-treesitter-deletion.md) and
  [ADR 0001](decisions/0001-native-engines-vendored-lua-apis.md)) — a plugin
  reaching for it hits a loud nil index. Highlighting is driven by the in-core
  Rust engine through the declarative buffer nouns `btv.bo.filetype` /
  `btv.bo.ts_highlight` (the one `vim.treesitter` survivor is the `foldexpr`
  marker), and query customization rides the runtimepath `queries/` /
  `after/queries/` overlay (next bullet).
  **Lua-driven indent** (`indentexpr=v:lua…` / `indent.lua`) is unwired — it
  wants the live buffer mid-keystroke, which fights the snapshot bridge, so the
  Rust indent stays.
- **Treesitter query resolution.** The query bridge
  ([design](specs/2026-06-08-treesitter-query-bridge-design.md)) merges a language's
  bundled base with runtimepath `queries/` + `after/queries/` and the `; inherits:`
  chain — and `:TSInstall` fetches the inherited query sets too (`javascript` →
  `ecma`,`jsx`), so base js/ts highlighting carries the `ecma` patterns. `;; extends`
  carries its upstream meaning: a runtimepath file with it is *added*, one without it
  *replaces* that language's bundled query (first in runtimepath order wins). Two
  deliberate deviations remain:
  (1) **extension ordering** — every extension lands after every base, including the
  bases of inherited languages, so an `after/queries` customization wins a tie
  against what it customizes; upstream interleaves per language, which lets a
  *bundled* pattern of the outer language beat a user's extension of an inherited
  one. (2) **`; inherits: (lang)`** — upstream's parenthesized form means "inherit
  only when this query is loaded for an *injected* language"; bemtvi doesn't model the
  condition, and such a name simply doesn't resolve, so the language is not
  inherited. No shipped nvim-treesitter query uses it.
  *(Two former edges are closed: the bundled `; inherits:` chain is now resolved by
  the engine's own query reader, so every grammar it loads gets it — **injected
  children** included, where `javascript` inside a markdown fence used to paint only
  its non-inherited captures; and resolution is no longer additive-only.)*
- **No `vim.uv` / `vim.loop`.** neovim exposes libuv as a public Lua API; bemtvi
  does not — the `vim.uv` / `vim.loop` table does not exist, so a plugin reaching
  for it hits a loud nil index. Both the libuv **handle** surface (`new_timer` /
  `new_check` / `new_fs_event` / `spawn` / `new_pipe`, the plugin event-loop
  primitives) and the synchronous `fs_*` / scalar primitives (`fs_realpath`,
  `cwd`, `os_homedir`, `os_uname`, `hrtime`, `now`) are gone. There are no
  synchronous filesystem `vim.fn` builtins either (no blocking I/O ever runs on
  the editor thread): async lives in the `btv` API (`btv.run` / `btv.timer` /
  `btv.fs`), and the LSP-config root search runs on the same async `btv.fs` seam.
- **Broad options surface.** The set of honored options has grown well past the
  indentation knobs (the authoritative list is `crates/bemtvi-core/src/options.rs`).
  `:set` (and `:setlocal` / `:setglobal` / `vim.bo` / `vim.go` /
  `nvim_{set,get}_option_value`) honors: the
  **search** booleans (`ignorecase` / `smartcase` / `wrapscan` / `hlsearch` /
  `incsearch`); the **window-local rendering** options (`number` /
  `relativenumber` / `cursorline` / `numberwidth` / `signcolumn` / `wrap` /
  `breakindent` / `showbreak` / `sidescroll` / `sidescrolloff` / `winhighlight` /
  `fillchars`); the **fold** options (`foldmethod` / `foldenable` / `foldcolumn` /
  `foldlevel`); the **buffer-local indentation** options (`tabstop` / `shiftwidth`
  / `softtabstop` / `expandtab` / `commentstring`); the **editing-feedback** pair
  `showcmd` / `report` and the undo bound `undolevels`; and a set of
  **bemtvi-native** options (`scrollanim` / `scrollanimduration`, `qfdock`,
  `imagepreview`, `history` / `persisthistory`, `regexsyntax`, `switchbuf`,
  `laststatus` / `showtabline`, `guiglyphoverflow`, …).
  Buffer- and window-local options are **global-local**, as in vim: each carries a
  global value (`:setglobal` / `vim.go` / `vim.opt_global`) alongside the per-instance
  one, `:set` / `vim.opt` write both, and a new buffer is born from the global value —
  which is what makes a config's `vim.opt.tabstop = 3` reach files opened later. A few
  buffer options have no global value (the read decides them) and `:setglobal` on one
  fails loud with `E5100`. One deliberate departure inside that model:
  `commentstring`, `foldexpr` and `foldmarker` live in a per-buffer map that already
  spells "unset" as absence, so their global value resolves as a **read-time fallback**
  rather than a creation seed — a `:setglobal` of one reaches buffers that are already
  open and carry no value of their own, where vim (which always holds a local value per
  buffer) would reach only buffers created afterwards. A `:setlocal` still pins a buffer
  against any later global write.
  bemtvi breaks with vim's defaults on indentation: `tabstop` defaults to **4**,
  with `shiftwidth=0` ("follow tabstop") and `softtabstop=-1` ("follow
  shiftwidth") so the one `tabstop` knob drives the whole indent width.
  `tabstop`, `softtabstop`, and `expandtab` drive rendering, `<Tab>` and the
  `<BS>`-over-blanks unit delete (bemtvi always has an effective soft-tab unit,
  so it has no `smarttab` and applies that unit wherever vim's `softtabstop`
  would);
  `shiftwidth` drives the `>>`/`<<` shift operators and the LSP indent width.
  `commentstring` backs the `gc`/`gcc` comment operator and defaults from the
  filetype (the ~20 most common languages) when unset. Still, the **bulk** of
  vim's hundreds of options are missing — a write to an unmodeled option is
  recorded but inert.
- **Keyboard macros are on `<F2>` / `<F3>`, not `q` / `@`, and hold key
  notation.** Recording and playback are complete (`<F2>{reg}` … `<F2>` records,
  `{count}<F3>{reg}` plays, `<F3><F3>` repeats the last, `<F3>:` re-runs the last
  ex command, a failed keystroke aborts the run) — but two deliberate departures.
  (1) The **keys**: `q` stays free for the user and for the `q`-to-close
  convention of bemtvi's read-only surfaces; `btv.keymap.set("n", "q", "<F2>")`
  and the same for `@` → `<F3>` restore the vim spelling, which works because a
  recording captures a mapping's LHS and replays through the keymap matcher.
  (2) The **storage**: a macro register holds bemtvi key notation
  (`ciwfoo<Esc>`), not vim's raw bytes — so it pastes, lists, persists, and can
  be hand-authored as readable text, at the cost that playing a register that
  holds ordinary *yanked text* parses any `<...>` in it as a key rather than as
  literal characters.
- **Filetype detection is a small, deliberate rule set — not vim's
  `filetype.vim`.** Detection is a *derive* from the path (see architecture.md
  → *Buffers*): the exact basename, ~7 globs, the extension, and a `#!` line as
  the last resort. vim ships thousands of rules; bemtvi ships the ones that pay
  for themselves, and the glob tier is kept small on purpose — a pattern that
  *guesses* (`*.conf`, `*rc`) is worse than no detection, because a wrong
  filetype is harder to notice than a missing one. A language bemtvi doesn't
  recognize is named with `:setf {lang}` (or `btv.bo.filetype`) — from a config,
  that means a `BufReadPost`/`BufNewFile` autocmd, since there is no
  `vim.filetype.add`-style API for extending the tables themselves.
- **Legacy Vimscript (`eval.c`).** Deliberately **not** on the roadmap (guiding
  principle 2). `vim.fn.*` is a hand-written set of helper aliases, not an
  interpreter — unimplemented `vim.fn.*` entries are loud gaps, not a TODO to
  build an evaluator.
- **`:TSInstall` approximations.** The command fetches/compiles grammars
  (`bemtvi_ts::install`), with a pinned, checksum-verified Zig fetched on demand
  when no system `cc`/`clang`/`gcc`/`zig` (or `$BEMTVI_CC`) is found — on macOS,
  Linux, and Windows alike. Remaining: (1) grammars needing `tree-sitter
  generate` (no committed `src/parser.c`) fail loud rather than generating;
  (2) the nvim-treesitter ref is pinned in source — no `:TSInstall`-from-`HEAD`;
  (3) in the **browser**, markdown can't be installed at all — no npm package publishes a
  markdown `.wasm` — so its two parsers (block + inline) ship as committed wasm in the
  offline bundle (`crates/bemtvi-edithost/treesitter/prebuilt/`) and `:TSInstall markdown`
  reports that instead of fetching.
- **Tree-sitter query-directive approximations.** `(#trim! @fold)` is ignored by **both**
  engines, so markdown's `(section)` fold runs to the blank line before the next section.
  The browser's injection runner (`web/highlight.js`) additionally parses each
  `@injection.content` range on its own — an approximation of `(#set! injection.combined)`,
  which asks for one parse over their union — and applies no `(#offset! …)`, so a `---`
  metadata fence reaches yaml/toml with its delimiter lines attached.
- **LSP semantic tokens approximations.** Painted over the treesitter floor
  (`crates/bemtvi-server/src/lsp/semantic.rs`): **one resolvable group per cell**
  (the merge picks the most-specific `@lsp.*` winner, it doesn't blend neovim's
  `@lsp.type.<t>` + per-modifier stack); **theme-gated** (an undefined group is
  dropped so the floor shows); **no `range`** (only `full`/`full/delta`);
  **no `highlight_token` hook** (neovim's per-token Lua callback on the decode
  hot path — the name is absent, a loud nil index);
  `get_at_pos` reads the cached mirror even for a `stop`ped
  buffer; no per-client granularity (one cache per buffer); repaints mid-insert
  (`update_in_insert` always on). See `docs/plans/2026-06-08-lsp-semantic-tokens.md`.
- **LSP inlay hints approximations.** Painted inline, opt-in
  (`crates/bemtvi-server/src/lsp/inlay.rs`). `inlayHint/resolve` (lazy per-hint
  label fill) and `vim.lsp.inlay_hint.get` (with a line-range filter) have landed.
  What still diverges: **one `LspInlayHint` group** for all kinds (no
  Type/Parameter split); the fetch is **whole-document** — the viewport-scoped
  `range` request is deferred; **per-buffer enable only** (no per-client
  granularity); horizontal-scroll (`leftcol>0`) + inline hints is best-effort;
  repaints mid-insert. See `docs/plans/2026-06-08-lsp-inlay-hints.md`.
- **No synchronous prompts.** The blocking `vim.fn.input` / `vim.fn.confirm`
  are omitted by design (loud gaps like the rest of unimplemented `vim.fn.*`) —
  prompting is promise-only: `btv.ui.input` / `btv.ui.select` / `btv.ui.confirm`,
  awaited inside `btv.async`. A config expecting an inline return must be
  reshaped onto the promise. See `examples/ui-prompt/`.
- **`btv.statusline` segment registry — v1 deferrals.** The lualine-shaped surface
  is built (`btv.statusline.setup`/`segment`/`invalidate`; built-ins composed in
  `bemtvi_core::statusline::compose_segments`, custom segments rendered per window
  and cached server-side; see
  `docs/plans/2026-06-15-btv-statusline-segments.md`). Custom segments **are now
  per-window**: each is rendered once per window against that window's
  `{ buf, win, focused }`, cached by `(window, name)`, and re-rendered when the
  segment is invalidated or the window layout changes (split/close, focus move, or
  a window swapping its buffer) — so `ctx.focused` and `ctx.buf` are correct in
  every window. The server orchestrates the re-render from `run_pending` with a
  fresh window mirror (`EditHost::refresh_statusline_segments`), so an invalidate
  fired from an autocmd that ran before the transition still renders against the
  settled layout. Layouts are also **per-window / `setlocal`-able**:
  `btv.statusline.setup{win=…}` sets a window-local layout that overrides the
  global one, `setup{win=…, format=true}` opts a window back to the `'statusline'`
  `%`-format even under a global segment layout (the per-region mix), and
  `btv.statusline.reset(win)` drops the override (`EditHost::resolve_window_layout`).
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
| The completion menu is server-owned chrome, not a window | The completion-doc preview is a single bespoke box with no separate preview-window handle / `'completeopt'` matrix. (Most of the `vim.lsp.util` helper surface — `make_position_params`, `open_floating_preview`, `make_text_document_params`, `locations_to_items` — is gone with the compat layer: LSP goes through the native `btv.lsp.*` verbs and hover/signature through the cursor float. Only `apply_workspace_edit` and `show_document` survive there, as aliases of `btv.lsp.apply_workspace_edit` / `btv.lsp.show_document`; the former loads unopened files into buffers on the spot. Splits, floats, and tab pages themselves are implemented — see architecture.md *Windows*.) |
| Core honors a fixed set of buffer-local options | `vim.bo` / `nvim_set_option_value` writes outside the wired set — `filetype` / `ts_highlight` / `tabstop` / `shiftwidth` / `softtabstop` / `expandtab` / `autoindent` / `smartindent` / `autopairs` / `indentemptylines` / `commentstring` / `regexsyntax` / `fileencoding` / `bomb` / `fileformat` / `endofline` / `fixendofline` / `modifiable` / `modified` / the fold options (`foldmethod` / `foldexpr` / `foldmarker` / `foldnestmax` / `foldminlines`) — are recorded but inert (`buftype` is mirrored read-only) |
| Diagnostic-display surfaces are approximations, not gaps — all four ship. `underline`, `virtual_text` (inline end-of-line message), `signs` (gutter glyph), and the on-demand float (`vim.diagnostic.open_float`) are implemented — see `docs/plans/2026-06-08-diagnostic-display-surfaces.md`. `update_in_insert` is honored too, and **extended**: it takes a number of milliseconds as well as neovim's two booleans, defaulting to `3000` — an update landing while you type is held and applied once typing has been quiet that long (or immediately on `InsertLeave`), where neovim offers only per-keystroke (`true`) or nothing-until-`InsertLeave` (`false`). Both booleans still mean exactly what they do in neovim. The hold also sits one layer deeper than neovim's: bemtvi holds at the store every surface reads, so `vim.diagnostic.get` reports the held set for LSP-published diagnostics instead of staying fresh as it does in neovim. Displayed positions **track edits** the way neovim's do — each applied diagnostic is anchored by an extmark in the reserved `DIAGNOSTIC_NS`, so a squiggle, sign, inline message, `]d` target and loclist row follow the text they flag as you type around it, rather than sitting at the coordinates the publish named. `vim.diagnostic.get` still reports the *published* `lnum`/`col` (as neovim does); it is the rendering and navigation that follow the buffer. | `vim.diagnostic.config` keys other than `underline` / `virtual_text` / `signs` / `update_in_insert` (`virtual_lines`, `severity_sort`, and the `config.float` pre-style defaults). `open_float` ignores its `opts` (scope/severity filters, `format`/`header`/`prefix`/`border`) — the default cursor-line scope shows, in the bottom panel (plain lines, like hover) not a cursor-anchored bordered popup. The `virtual_text` table honors `prefix` and the `signs` table its `text` glyph map; their `format` / `severity` filters and sign `priority`/`culhl` are not applied, the line's most-severe diagnostic wins the one inline slot / sign cell, and the sign column is client-side only (a fixed 2 cells not subtracted from `bemtvi-core`'s text width, so a full-width line under `nowrap` can clip its last two cells). |
