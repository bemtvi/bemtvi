# Tree-sitter folds (and the general fold model)

Status: in progress · 2026-06-25 · Phases 1–5 landed (manual + navigation +
fold-column gutter; `foldmethod=indent`; native **and** web/wasm tree-sitter
`foldexpr`; generic Lua `foldexpr` + LSP `foldingRange`). Phase 6 mostly landed:
operator-over-fold semantics, manual-fold shada persistence, the `marker`
foldmethod, and docs + example config (+ the `vim.bo`/`vim.wo` fold-option wiring)
are done; `foldtext` customization and the `syntax`/`diff` foldmethods remain
deferred.

Implements code folding in nxvim. Scope (decided): full fold parity — a generic
fold model with **manual**, **indent**, **expr / tree-sitter**, and **LSP
foldingRange** sources, rendered with both the collapsed placeholder line *and* a
`foldcolumn` gutter. Tree-sitter is the headline source; the model, commands,
motion, and rendering are shared by every source.

This is "phase 6" of the in-process tree-sitter work
([docs/specs/2026-06-06-in-process-treesitter-and-indentation-design.md](../specs/2026-06-06-in-process-treesitter-and-indentation-design.md)
explicitly lists *folds* as pending), and it reuses that machinery wholesale.

## Background: what already exists (and what doesn't)

There is **no** fold implementation today, but the groundwork is unusually
favorable — three independent explorations confirmed:

- **Tree-sitter engine runs synchronous queries.** `nxvim-ts` (`engine.rs`,
  `loader.rs`) already loads/compiles `highlights` + `indents` + `injections`
  per language and runs them in-process on the keypress tick via the
  `SyntaxEngine` trait (`crates/nxvim-core/src/syntax.rs`). Adding `folds` is the
  same shape as `indents`: a new `Option<Query>` on `Grammar`, a new
  `is_engine_query()` arm, and a new trait method. The `folds` query file is
  *already fetched and cached* during grammar install
  (`nxvim-ts/src/install.rs:42` lists `"folds"`; web `grammars.js` `QUERY_KINDS`
  too) — it is simply never consumed.
- **The view model anticipated folds.** `RowKind` in
  `crates/nxvim-core/src/view.rs:68` (`Line` / `VirtLine` / `Filler`) carries the
  comment that a fold/diff-filler row "is just another arm here". The redraw
  projection (`crates/nxvim-server/src/redraw.rs`) emits **one entry per visible
  screen row** as parallel arrays (`lines` / `numbers` / `continuation` / signs /
  highlights …); hidden lines are simply *absent* from the row vector, which is
  exactly the collapsed-fold shape.
- **The `z`-prefix is already wired.** `command.rs` has a `Stage::ZPending`
  (`view_command()` around `command.rs:190`, prefix detection ~`command.rs:1076`)
  that today only completes viewport commands (`zt`/`zz`/`zb`). Fold commands are
  new continuations on the same stage. The continuation table even labels the `z`
  stage "Scroll / fold" (`command.rs:1493`).
- **A viewport-change signal exists.** `DecorViewport`
  (`crates/nxvim-core/src/editor/decor.rs:28`, drained via `take_decor_dirty()`)
  already fires `{win, buf, top, bot, generation}` on scroll/resize/edit — the
  natural trigger to (re)compute folds for the visible range.
- **LSP foldingRange is a stubbed placeholder.** `nxvim-lsp/src/dispatch.rs:609`
  routes `"textDocument/foldingRange"` to a `req_textDocument_foldingRange` that
  does not exist yet.

### Key existing touch-points

| Concern | File | Anchor |
| --- | --- | --- |
| `RowKind` / `RenderRow` / `WindowView` | `nxvim-core/src/view.rs` | `:68`, `:91`, `:347` |
| Redraw row projection / unbundle | `nxvim-server/src/redraw.rs` | `window_value` `:358`, `unbundle_rows` `:1594` |
| Protocol parse (clients) | `nxvim-view/src/view.rs` | `parse_window` `:975` |
| `z`-prefix dispatch | `nxvim-core/src/editor/command.rs` | `:190`, `:1076`, `:1493` |
| Cursor screen-row / scroll math | `nxvim-core/src/editor/cursor.rs` | `cursor_screen_row` `:406`, `line_text_rows` `:364`, `scroll_top_for_bottom` `:428` |
| Motions (j/k/gj/gk/G/gg) | `nxvim-core/src/editor/motions.rs` | `resolve_motion` `:134`, `display_motion` `:294` |
| Options struct | `nxvim-core/src/options.rs` | global `:11`, window `WindowOptions` |
| Window state (per-window) | `nxvim-core/src/editor/windows.rs` | `Window` `:356` |
| Syntax engine trait | `nxvim-core/src/syntax.rs` | trait `:57` |
| TS engine / grammar | `nxvim-ts/src/engine.rs`, `loader.rs` | `is_engine_query` `engine.rs:1532`, `Grammar` `loader.rs:47` |
| Viewport signal | `nxvim-core/src/editor/decor.rs` | `DecorViewport` `:28` |
| LSP folding stub | `nxvim-lsp/src/dispatch.rs` | `:609` |
| TUI / GUI gutter render | `nxvim-tui/src/render.rs`, `nxvim-gui/src/render.rs` | `render_gutter` `tui:1185`, gutter layout `gui:998` |

## Design

### The fold model (the spine — built once, reused by every source)

Folds are a **per-window** property (vim semantics: the same buffer in two
windows folds independently), but *fold structure* derives from buffer content.
We separate the two:

- **Fold structure** — a nested tree of ranges `Fold { start_line, end_line,
  level, children }`, derived from a **per-line fold-level array** (vim's model:
  each line has a level; `>N` opens a fold at level N, `<N` closes one). Every
  computed source (`indent`, `expr`, tree-sitter, LSP) produces the per-line
  level array; the tree is computed from it identically. `manual`/`marker` store
  explicit ranges and merge in.
- **Fold state** — per window: which folds are *closed*, plus the window's
  `foldlevel` threshold (folds deeper than `foldlevel` render closed when
  `foldenable`).

Stored on `Window` (`windows.rs`). Splitting a window copies fold state; the
structure is recomputed from buffer content (cached per `(buf, changedtick,
method)` so unchanged buffers don't recompute).

The single most important helper the rest of the editor consumes:
`closed_fold_at(win, line) -> Option<Fold>` and a visible-line iterator that
**skips lines hidden inside a closed fold** and yields one synthetic row per
closed fold. Motion, scroll, and redraw all go through these — so no caller hand-
rolls fold skipping.

### Options (vim names, added to `options.rs`)

Global/buffer: `foldmethod` (fdm: `manual`|`indent`|`expr`|`marker`|`syntax`?—
defer syntax), `foldexpr` (fde), `foldmarker` (fmr), `foldnestmax` (fdn),
`foldminlines` (fml). Per-window (`WindowOptions`): `foldenable` (fen),
`foldlevel` (fdl), `foldlevelstart` (fdls), `foldcolumn` (fdc, width or `auto:N`),
`foldtext` (fdt). Defaults match vim (`foldmethod=manual`, `foldenable=true`,
`foldlevel=0`, but `foldlevelstart=-1` ⇒ all open on open).

### Tree-sitter as a fold source

`SyntaxEngine::folds(buf, first, last) -> Vec<FoldRegion>` runs the compiled
`folds.scm` (`@fold` captures), mirroring `extract_spans`. `@fold` node ranges →
per-line levels by containment depth. Built-in `foldmethod=expr` with the
canonical `nx.treesitter.foldexpr` short-circuits to this native call (fast
path); an arbitrary Lua `foldexpr` is evaluated per line (vim-compatible, slower).
Recompute is driven by `DecorViewport` + `changedtick`, reusing the incremental
tree — no reparse.

**Web/wasm caveat:** the web build has no `nxvim-ts`; tree-sitter runs in JS
(`highlight.js`). Manual + indent folds are pure-core and work on web unchanged.
Tree-sitter folds on web need a JS `folds.scm` runner feeding ranges into the
core fold store across the edit-host seam (mirrors how `ts-indent.js` reimplements
indents). This is isolated to Phase 4b and may ship slightly behind native.

## Phases

Each phase is independently testable through the black-box harness (feed vim
keys, assert on `lines`/`cursor`/redraw rows). Bug-fix discipline applies: write
the failing test first.

### Phase 1 — Fold model + manual folds + collapse rendering (server-side)

The keystone. `manual` is self-contained (no engine), so it exercises the whole
spine end-to-end first.

- Fold store + `Fold` tree + per-window closed state on `Window`; the
  `closed_fold_at` / visible-row helpers.
- All fold options added to `options.rs` with vim defaults.
- Commands (`command.rs` `ZPending` continuations): `zf{motion}`/`zF` + `:fold`
  (create), `zd`/`zD`/`zE` (delete), `zo`/`zO`/`zc`/`zC`/`za`/`zA` (open/close),
  `zR`/`zM` (all), `zv` (view cursor), `zn`/`zN`/`zi` (fold-enable toggles).
- Redraw projection: new `RowKind::Fold { line, count }`, hidden lines dropped
  from the row vector, `foldtext` rendered into the placeholder row. Protocol
  carries it via the existing parallel-array shape (`nxvim-view` parse updated).
- **Tests:** create a manual fold, close it, assert hidden lines absent from
  redraw rows + placeholder text present; `zo` restores them; `zR`/`zM`;
  `foldlevel`/`foldenable` interplay.

### Phase 2 — Fold-aware navigation, cursor, scroll + client rendering + fold column

Make folds usable and visible.

- Motion/cursor (`motions.rs`, `cursor.rs`): `j`/`k`/`gg`/`G`/`gj`/`gk` and
  scrolling skip closed folds; a cursor landing inside a closed fold snaps to its
  first line; `cursor_screen_row`/`scroll_top_for_bottom` count a closed fold as
  one row. Fold motions `zj`/`zk`/`[z`/`]z`.
- Client rendering of the placeholder row + the `foldcolumn` gutter (markers:
  `+`/`-`/`│`, nested levels) in **TUI** (`render.rs` new column between sign and
  number gutter), **GUI** (new `fold_x0`/`fold_w` in the gutter layout math), and
  **web**.
- **Tests:** `j` over a closed fold lands past it; `G` with closed folds;
  fold-column markers appear in redraw; click-to-toggle on the fold column
  (mouse) optional.

### Phase 3 — `foldmethod=indent` ✅ done

First *computed* source — proves the per-line-level → tree pipeline without the
engine. **Landed:** the per-line-level → nested-range pipeline
(`ranges_from_levels`), the indent provider, the recompute cache, and the fold
options now exist and are the spine Phase 4 (tree-sitter) plugs into — it only
needs to supply a different per-line-level array.

- Indent provider (`fold.rs::compute_indent_folds`): per-line levels from leading
  indent / `shiftwidth` (tabs expand to `tabstop`), capped at `foldnestmax`; a
  blank line takes `min(prev_nonblank, next_nonblank)` so trailing blanks fall out
  of a fold while interior blanks stay in (vim's `fold-indent` rule).
- Options: `foldmethod` (fdm), `foldlevel` (fdl), `foldnestmax` (fdn),
  `foldminlines` (fml) added to `options.rs`, wired through `:set` (with a loud
  `E474` / "not supported yet" for unknown / unimplemented methods) and the
  `vim.wo`/`vim.bo` bridges.
- Recompute (`fold.rs::refresh_folds`): cache-keyed on
  `(changedtick, method, shiftwidth, foldnestmax, foldminlines)`, driven from the
  input loop and the fold-input option setters; preserves manual `zo`/`zc`
  overrides across an edit by matching ranges. `foldlevel` is applied separately
  (`apply_foldlevel`) without a structural rebuild. The cursor snaps out of a
  newly-closed fold onto its header.
- **Tests** (`tests/editing/folds.rs`): nested indented blocks fold at the right
  levels, `foldlevel=1` shows only the top level, editing reflows the fold span,
  and an unsupported `foldmethod` fails loud.

The closed-state model stays the per-fold `closed` flag (shared with manual
folds): a computed fold defaults to `closed = level > foldlevel`. Generalizing the
recompute to **all** visible windows (today only the focused window re-folds) and
honoring `foldminlines` as a pure *display* gate (rather than gating fold
existence) are the two knowingly-deferred simplifications — fine for single-window
indent/tree-sitter use, to revisit if a multi-window same-buffer fold divergence
shows up.

### Phase 4 — Tree-sitter folds (the headline)

**4a — native. ✅ done**
- `nxvim-ts`: `folds: Option<Query>` on `Grammar` (`loader.rs`), `"folds"` arm in
  `is_engine_query` + `recompile_query` (`engine.rs`), load/compile `folds.scm`.
- `SyntaxEngine::folds()` / `folds_available()` (trait + impl) extracting `@fold`
  node ranges; `Engine::folds` runs the query and trims a node ending at column 0
  of its last line (neovim's foldexpr guard). A new `FoldRange` carries the spans.
- `foldmethod=expr` (`FoldMethod::Expr` + a per-buffer `'foldexpr'` string, stored
  beside `commentstring` since it's not `Copy`). A `FoldSource` resolver classifies
  `expr` into `Treesitter` (the canonical foldexpr — recognized by
  `is_treesitter_foldexpr`, the `v:lua.`/`nx.`/`vim.` spellings) vs `GenericExpr`
  (Phase 5; produces no folds, warns at set-time — not a silent no-op).
  `compute_treesitter_folds` turns the engine's `@fold` ranges into per-line levels
  **by containment depth** (capped at `foldnestmax`), then reuses
  `ranges_from_levels` — the same Phase-3 spine. Cache key (`FoldKey`) now keys on
  the `FoldSource`. Recompute is `changedtick`-driven through the input loop;
  `DecorViewport`-scoped recompute is a deferred perf follow-up.
- `nx.treesitter.foldexpr` Lua surface (+ the `vim.treesitter.foldexpr` alias),
  authored in `prelude/nx.lua`. It is a **native marker** (the string reference the
  fold engine recognizes), so calling it directly fails loud (per-line Lua foldexpr
  eval is Phase 5).
- **Tests:** hermetic dispatch tests in `tests/editing/folds.rs` (expr accepted,
  the ts foldexpr recognized, a generic foldexpr warns, an expr source with no
  grammar is inert, the Lua marker is aliased + fails-loud-on-call); plus an
  `#[ignore]`d real-grammar e2e test (`tests/treesitter_folds.rs`) — installs the
  `lua` grammar, opens a function, and asserts the body folds — same opt-in posture
  (network + C compiler) as the other tree-sitter e2e tests.

**4b — web/wasm. ✅ done.** A JS `folds.scm` runner in the edit-host
(`web/ts-folds.js`, the sibling of `ts-indent.js`) loads grammars + `folds.scm`
(offline vendor bundle / OPFS `:TSInstall` cache) and answers `folds()`
synchronously inside the worker tick; the Rust `WasmSyntax::folds` reaches it over
a new `eh_js_ts_folds*` FFI bridge (`web/eh-lib.js`), writing the `@fold` ranges
into a Rust out-buffer (grow-and-retry if a fold-dense file overflows). The ranges
feed the **same** core fold store as native, so the browser folds identically. The
fold queries come from nvim-treesitter (the source native uses) — bundled by
`gen-treesitter` into `vendor/folds/` + `folds.json`, and fetched into OPFS by the
runtime `:TSInstall` (`highlight.js`, folds preferred from nvim-treesitter like
indents). A `:TSInstall` evicts both the indent and fold runner caches via the one
`eh_js_ts_reload` export. Playwright-verified (`web/verify-treesitter-folds.mjs`):
on the bundled `python` grammar, `foldmethod=expr` + the tree-sitter foldexpr
collapses a function body while the first line stays visible and the buffer is
unchanged. The native indent verifier still passes (no worker regression). Deferred
to a perf follow-up (shared with native 4a): recompute re-parses the whole buffer
per edit rather than scoping to the `DecorViewport` + reusing the incremental tree.

### Phase 5 — Generic `foldexpr` + LSP foldingRange ✅ done

Both halves are **server-pushed external fold sources**: nxvim-core can't run Lua
or talk to a language server, so the server computes the structure out-of-band and
pushes it into a new per-buffer `external_folds` store on the `Editor` (tagged with
the `changedtick` it was computed for); `refresh_folds`' `GenericExpr`/`Lsp` arms
build the fold tree from it (and a `set_*` push busts the structure cache so the
result is honored even when `changedtick` is unchanged — the async/first-eval case).
A stale push (for a since-edited buffer) is ignored until the server re-pushes.

- **Generic Lua `foldexpr`** (`fold.rs` `GenericExpr`): the server evaluates the
  expression once per line with `v:lnum` bound (`LuaRuntime::eval_foldexpr_lines`,
  driven from `redraw` before the view projects) and pushes the per-line value
  strings via `Editor::set_foldexpr_values`; core resolves the vim `fold-expr`
  grammar (`>N`/`<N`/`=`/`aN`/`sN`/numbers/`-1`) into per-line levels →
  `ranges_from_levels`. Cached by `changedtick`; a broken foldexpr fails loud on
  the message line once per edit. The tree-sitter foldexpr stays the native
  short-circuit. (Native only — the browser edit-host folds JS-side.)
- **LSP `foldingRange`** (`fold.rs` `Lsp`): a new `nx.lsp.foldexpr` marker (+
  `vim.lsp.foldexpr` alias, fail-loud on direct call) selects the source.
  `nxvim-lsp` gained the typed `LspRequest::FoldingRange`/`LspReply::Folds`, the
  `folding_range` client+provider capability, and the `dispatch`/`sync_client`
  legs (so it works on native **and** the wasm daemon path). The server requests
  ranges from `redraw` (`maybe_request_folding_range`, after `sync_lsp` flushes
  `didChange`) whenever the buffer wants LSP folds and lacks a fresh result — so a
  request fires on a content change *and* on the config change that selects the
  source, retrying until the server initializes; the reply is stale-dropped on a
  tick change and pushed via `Editor::set_lsp_folds` (containment depth → levels,
  the same `ranges_from_containment` helper tree-sitter uses). The mock LSP
  (`nxvim-lsp/src/mock.rs`) scripts `folding_ranges` + advertises the provider.
- **Tests:** generic foldexpr folds returned levels, reflows on a content-driven
  edit, and fails loud on a bad expr (`tests/editing/folds.rs`); the LSP markers
  alias + fail loud on call; an end-to-end mock-LSP `foldingRange` collapses the
  buffer (`crates/nxvim/tests/lsp_features.rs`).

Knowingly deferred (shared with the earlier phases): recompute is focused-window-
only and whole-buffer (no `DecorViewport` scoping / incremental reuse); generic
foldexpr supports the `v:lua.` Lua spelling, not arbitrary vimscript exprs; the
`-1` undefined level resolves to the lower defined neighbour (vim's rule).

### Phase 6 — Persistence, operator semantics, polish, docs, example config

**6a — operator-over-fold semantics. ✅ done.** A linewise operator (`dd`/`yy`/`cc`/
`d{motion}`) or a linewise-visual selection over a *closed* fold acts on the whole
fold range, via the existing `fold_line_start`/`fold_line_end` helpers in
`apply_operator`'s `Linewise` arm and `visual_range_lw`. (`>>`/`<<` shift operators
aren't implemented in nxvim at all — a separate gap, not a fold one.) Tests:
dd / yy / dj / linewise-visual over a closed fold each take the full range.

**6b — manual-fold shada persistence. ✅ done.** A file's **manual** folds (each
`(start, end, closed)`) round-trip through shada — a new `FileFolds` on
`PersistState`, exported per window keyed by its buffer path (manual-method only;
computed sources regenerate), seeded into the focused window's `FoldState` when the
file reopens (`seed_pending_folds`, hooked into `enter_buffer` + the import path,
guarded so a session's own folds win). Server: a `folds_file` redb table
(`StoredFolds`, recency-merged like the changelist); the wasm JSON-blob path gets it
free. Test: a closed fold survives a restart (reopen → `dd` removes the whole range).

**6c — docs + example config (+ option wiring). ✅ done.** Fixed the config-path gap
the LSP work surfaced: the fold buffer-options (`foldmethod`/`foldexpr`/`foldnestmax`/
`foldminlines`) and window-options (`foldcolumn`/`foldenable`/`foldlevel`) are now in
the `vim.bo`/`vim.wo` whitelists (`BUF_OPT_CANON`/`WIN_OPT_CANON`), so a config sets
folds without `:set` (the server-side setters already existed). Shipped
`examples/folds/` (a nested `sample.lua` + an `init.lua` that turns on the
fold-column gutter and uses the indent source out of the box, documenting the
tree-sitter / LSP upgrades), verified end-to-end by loading the shipped config
(`tests/folds_example.rs`). Updated `docs/architecture.md` (a *Folds* paragraph in
the view section) and the tree-sitter spec. Also tested: `vim.bo.foldmethod` reaches
the live fold engine.

**6d — `marker` foldmethod. ✅ done.** The fifth fold source: `foldmethod=marker`
folds bounded by the literal `'foldmarker'` start/end strings (default `{{{`/`}}}`).
A start marker opens a fold at its line (the line shown when closed); the matching
end marker's line is the fold's last line; markers nest by counting, and a number
after a marker (`{{{2`) sets an absolute level. Implemented as a pure-core computed
source (`fold.rs::compute_marker_folds` + the free `marker_line_levels`, a faithful
port of vim/neovim's `fold.c::foldlevelMarker` — `lvl`/`lvl_next` per line so an
end-marker line stays inside its fold), feeding the same `ranges_from_levels` spine
as indent/expr. `'foldmarker'` is a per-buffer `(start, end)` string pair stored
beside `'foldexpr'` (not a `Copy` `BufferOptions` slot); wired through `:set
foldmarker=…` (E474 on anything but a distinct non-empty `start,end` pair) and the
`vim.bo`/`nx.bo` bridge (`set_buffer_option_str` + the `BUF_OPT_CANON`/`fmr`
whitelist). Changing the markers busts the structure cache (they don't enter the
`FoldKey`). Being pure-core, it works on the web/wasm edit-host unchanged. Tests
(`tests/editing/folds.rs`): a marked block folds, nested markers nest, a numbered
marker sets an absolute level, a custom `'foldmarker'` changes the delimiters,
editing reflows the fold, an invalid `'foldmarker'` fails loud, and the `nx.bo`
write reaches the live engine.

**Deferred (not done):**
- **`foldtext` customization** via a Lua function (default vim-like
  `+-- N lines: <first line>`); `fillchars` `fold:`/`foldopen`/`foldclose`/`foldsep`.
  (`foldlevelstart` / `foldtext` options don't exist in core yet.)
- `syntax` / `diff` foldmethods (both still fail loud at set-time).
- Per-window *closed state* persistence for **computed** folds (only manual folds
  persist; matching computed ranges across a recompute on restore is the open work).

## Risks & decisions to watch

- **Per-window vs per-buffer fold state.** Vim is per-window; cheaper would be
  per-buffer. We commit to per-window to match vim and to keep split behavior
  correct — but it means window create/split/close must manage fold state. Keep
  the structure cache per-buffer, only the closed-set per-window.
- **Recompute cost.** Tree-sitter/indent recompute on every edit could thrash.
  Mitigate by computing only the visible range + margin off the `DecorViewport`
  signal and caching by `(buf, changedtick, method, foldlevel)`.
- **Interaction with wrap / virt_lines / signs.** A closed fold is one screen row
  regardless of wrap; diagnostics/signs inside hidden lines must surface on the
  fold row (vim shows the highest-severity sign). Handle in the row projection.
- **Web tree-sitter lag (4b).** Native and web compute folds via different
  engines; risk of divergence. Reuse the same `folds.scm` fixtures for both;
  fail loud if web folds are unavailable rather than silently showing none.
- **No silent stubs.** Any unsupported `foldmethod` (e.g. `syntax`, `diff`) must
  error at set-time naming what's missing, not silently no-op.

## Test surface summary

All black-box via `nxvim-test-harness`: manual create/close/open and redraw-row
assertions (P1), motion/scroll/fold-column (P2), indent nesting (P3), hermetic
grammar fold fixtures + Playwright web (P4), mock-LSP folding + custom foldexpr
(P5), shada round-trip + operator-over-fold (P6).
