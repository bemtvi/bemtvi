# Statusline (`'statusline'`) — the `%`-format engine

A custom, per-window status line driven by neovim's `'statusline'` option, with
**full neovim format semantics**: `%`-items (`%f %l %c %m %= %< …`), highlight
switches (`%#Group#`), and embedded expressions (`%{expr}`, `%!expr`,
`%{%expr%}`). This is the foundation for rich, user-configured status lines and
the **shared `%`-format engine the tabline will later reuse** —
the original request that kicked this off (custom tab labels via
`tabline = '%!v:lua...'`) is a follow-up that drops onto the same engine.

Status line first, because it exercises every hard part of the format language;
the tabline is a thin second customer.

## Why this is feasible now (de-risking facts)

Verified in the current tree before planning:

- **Synchronous Lua eval already exists** — `LuaRuntime::eval_to_value_pumped`
  (`crates/nxvim-lua/src/runtime.rs:238`) evaluates an expression and returns an
  `rmpv::Value`, pumping the prompt loop. So `%!v:lua…()` / `%{%…%}` can be
  evaluated *inline during redraw*; we do **not** need to invert the async
  callback registry (the earlier worry). This is the single biggest de-risk.
- **The highlight registry is done** — named groups, `nvim_set_hl`, links, and
  `@`-fallback resolution all live in `crates/nxvim-core/src/highlight.rs`, and
  the redraw path already resolves chrome regions to concrete `Style`s
  (`crates/nxvim-server/src/redraw.rs:223`, including a `StatusLine` group).
  `%#Group#` resolution is reusing machinery that already works. Segment
  highlights generated via `nvim_set_hl` are already supported.
- **The status line is the last client-composed chrome.** Today the TUI builds
  the status *text* itself in `render_status`
  (`crates/nxvim-tui/src/render.rs:796`) from projected `WindowView` fields
  (`mode_label`, `file_name`, `modified`, `cursor_line`, `cursor_col`). The rest
  of nxvim's philosophy is "server resolves style + content, client paints"
  (redraw.rs module docs). This work brings the status line in line with that:
  the server projects **pre-styled segments**, the client just paints them.

## Architecture

The `%`-format language splits cleanly along nxvim's purity boundary:

- **Parsing + field expansion + layout are pure** → a new
  `crates/nxvim-core/src/statusline.rs` module. No Lua, no I/O — exactly the kind
  of code core owns. It parses a format string into items, expands the built-in
  fields it can compute itself (cursor, filename, modified, filetype, …), and
  does the width-dependent `%=` alignment / `%<` truncation pass.
- **Expression evaluation needs Lua** → injected into core as a callback. Core's
  render entry point takes a `&mut dyn FnMut(ExprKind, &str) -> String`. Core
  stays Lua-free; the **server** supplies a closure wrapping
  `eval_to_value_pumped`. This is the same dependency-injection trick that keeps
  `nxvim-core` pure elsewhere.
- **Style resolution + projection** happen in the **server** redraw path, where
  the highlight registry already resolves chrome.

### Two-pass render (why)

`%=` alignment and `%<` truncation depend on the *final* text widths, which are
only known after `%{}`/`%!` expressions evaluate. So:

1. **Parse** (core, pure): `parse(fmt) -> Vec<Item>`. Items:
   `Literal(text)`, `Field(FieldKind)` (built-ins), `HlSwitch(Option<group>)`
   (`%#grp#`, `%*`, `%0*`), `Align` (`%=`), `Truncate` (`%<`),
   `Expr { kind, raw }` for `%{…}` / `%!…` / `%{%…%}`.
2. **Expand** (core, pure, with injected `eval`): walk items, compute each
   `Field` from a `StatuslineCtx`, and call `eval(kind, raw)` for each `Expr`.
   `%{%…%}` results are **re-parsed** as items (they may contain further
   `%`-items); `%{…}` results are literal text; a leading `%!…` means the whole
   statusline *is* the eval result, re-parsed. Output: a flat
   `Vec<(text, Option<group>)>` with concrete text + active highlight group.
3. **Layout** (core, pure): resolve `%=` (distribute remaining width between
   left/right groups) and `%<` (truncation point) against the window width →
   `Vec<StatusSegment { text, group: Option<String> }>`.
4. **Style + project** (server): resolve each segment's `group` to a concrete
   `Style` via the highlight registry; emit a per-window `status` segment array
   into the redraw notification.
5. **Paint** (client): `render_status` paints the pre-styled segments. No format
   knowledge in the client anymore.

### Expression scope (loud, not silent)

`%{}` / `%!` evaluate **Vimscript** in real neovim. nxvim has no Vimscript, so we
support **only `v:lua.…` expressions** (which is what a user's statusline
config uses: `%!v:lua.require('myutils').my_tab_line()`). Any non-`v:lua`
expression **errors loudly** at eval time (per the no-silent-stub rule in
CLAUDE.md) — naming the unsupported expression — rather than rendering empty.

### Testing note — granted exception for the parser

CLAUDE.md forbids `#[test]` unit tests; behavior is normally verified end-to-end
through the running server. **Exception granted for the statusline parser
(Phase 2):** because it is a pure, dense format language, it gets `#[test]` unit
tests inside `nxvim-core` — but every case is **derived from real neovim
behavior, never guessed**. The oracle is `nvim --headless` calling
`vim.api.nvim_eval_statusline(fmt, {maxwidth=…, highlights=true})`, which returns
the exact `{str, width, highlights}` neovim produces; each parser test asserts
nxvim matches that ground truth (capture the nvim output, encode it as the
expected value, cite the format string).

Everything *else* still follows the convention: option plumbing (Phase 1),
segment projection (Phase 3), and client paint (Phase 4) are verified end-to-end
by setting `'statusline'` and asserting on the `redraw` notification
(take-latest helper, per the documented harness race).

## Model

- `'statusline'` is a string option. In neovim it is window-local with a global
  default; v1 treats it as **global** (the common case — `vim.opt.statusline`),
  with per-window override deferred. Empty string ⇒ the built-in default look.
- The built-in default is expressed *as a format string* and rendered through
  the same engine, so there is one code path. The current look
  (` MODE  file [+]    line,col `) maps to a default format roughly
  `" %{mode} %f%m%=%l,%c "` (mode is an nxvim field, since neovim shows mode
  elsewhere; preserving today's look).
- Rendering is **live**: redraw re-evaluates the format every frame, so
  `%{}`/`%!` results that depend on editor state stay current with no extra
  invalidation machinery. (An external refresh timer is then mostly redundant.)

## Phases

### Phase 1 — String options foundation

Make the options system carry string-valued options end-to-end.

- `OptionValue::String` variant (`crates/nxvim-lua/src/ops.rs`); core `Options`
  gains `statusline: String`.
- `resolve_set` / `canonical` (`crates/nxvim-core/src/options.rs:219,285`): a
  `Str` `OptKind`; `:set statusline=…` keeps the raw value (no number coercion;
  handle vim's backslash escaping of spaces), `:set statusline?` echoes it,
  `:set statusline&` resets to default.
- `GlobalOptionOp` apply path stores the string; Lua bridge `_set_global_option`
  (`crates/nxvim-lua/src/install.rs:469`) gains a `String` arm; `vim.opt` /
  `vim.o` / `vim.go` `.statusline` read/write round-trips.
- **Tests** (editing.rs): set via `:set` and via `vim.opt.statusline`, read back
  via `:set statusline?` / `vim.o.statusline`.

### Phase 2 — The `%`-format engine (pure, core)

`crates/nxvim-core/src/statusline.rs`: `parse`, `StatuslineCtx`, `expand`,
`layout`. Built-in fields for v1: `%f %F %t %m %M %r %h %y %n %l %L %c %v %p %P`,
`%=`, `%<`, `%%`, and highlight switches `%#Group#` / `%*` / `%0*`. `expand`
takes the injected `eval` callback for `%{}`/`%!`/`%{%…%}`. No tests here yet
(exercised end-to-end in Phase 3 per the convention note above); internal
correctness is asserted through Phase 3's redraw tests.

### Phase 3 — Server eval + style resolution + redraw projection

- Server builds the `eval` closure over `eval_to_value_pumped`, enforcing the
  `v:lua`-only rule (loud error otherwise).
- Resolve each `StatusSegment.group` to a `Style` via the highlight registry;
  fall back to the `StatusLine` group / reverse-video when unset.
- Project per-window `status` as a styled-segment array in `redraw`
  (`crates/nxvim-server/src/redraw.rs` `window_value`), replacing the implicit
  field-based status.
- Default format rendered through the engine when `'statusline'` is empty.
- **Tests**: set a literal `statusline`, a field one (`%f %l,%c`), a `%#grp#`
  one, and a `%!v:lua…` one returning a string; assert the segment text + styles
  in the latest redraw.

### Phase 4 — Client paints styled segments

- `render_status` (`crates/nxvim-tui/src/render.rs:796`) paints the projected
  `status` segments (text + style); drop the client-side field composition.
- `crates/nxvim-tui/src/view.rs` parses the new `status` segment array.
- Keep a reverse-video fallback if the array is absent (older server).
- **Tests** (`crates/nxvim/tests/screen.rs`): end-to-end paint of a custom
  statusline.

### Phase 5 — `vim.fn` / `vim.api` surface for real configs ✅ done

Add the functions a real statusline calls from inside `%{}`/`%!`:
`mode()`, `line('.')`, `col('.')`, `winnr()`, `bufnr()`, `fnamemodify()`,
`expand('%:…')`, `getbufvar`, `nvim_get_current_line` width helpers, etc. Each
added only when exercised by a test (no speculative stubs). Confirm live refresh:
a `%{}` reading editor state updates as the state changes across redraws.

**Shipped:** the editor mode is now threaded into the Rust→Lua mirror
(`nx._cur_mode`, via `set_buf_mirror`), and `redraw` refreshes that mirror
before evaluating a statusline that contains a `%{}`/`%!` expression — so a
`%{v:lua.vim.fn.mode()}` tracks the mode live across frames. New / enhanced
`vim.fn`: `mode`, `line('.'/'$')`, `col('.'/'$')`, `winnr('.'/'$')`,
`winwidth`/`winheight` (nvim_api.lua), plus a vim-faithful `fnamemodify`
(`:p :~ :. :h :t :r :e`, consecutive-`:e` widening, loud error on unsupported
modifiers — replacing fs.lua's coarser version) and `bufnr('$')`. `expand`
routes `%:<mods>` through `fnamemodify`. fnamemodify cases are derived from real
neovim. `getbufvar` deferred (no test demands it yet). The window-relative
`line('w0'/'w$')` forms error loud (the mirror has no scroll position yet).

### Phase 6 (later) — `laststatus`, then tabline reuse

- `laststatus` option ✅ done — `0` never, `1` only with ≥2 windows, `2` always
  (the default), `3` a single global statusline row.

  **Shipped:** `Options.laststatus` (`crates/nxvim-core/src/options.rs`), wired
  through the shared `set_global_option_num` (0..=3, loud `E474`/`E487` out of
  range) so the `:set laststatus=…`/`?`/`&` ex path and the `vim.o`/`vim.opt`
  Lua bridge validate, echo, and relayout identically (`ls` abbreviation in the
  Lua `O_GLOBAL` map + the `_go_mirror`). Per-window visibility is the new
  `Editor::window_statusline_visible(floating)` gate, projected onto each
  `WindowView` as `status_visible`; the core view reserves the text-area status
  row only when shown, so a hidden status reclaims its row as text. Mode 3 docks
  one global row in `relayout` (`global_statusline_rows`, the bottom analogue of
  the tabline) and `View.global_statusline` carries the *focused* window's
  `%`-context; the server renders it full-width via the shared
  `render_statusline` helper (`redraw.rs`) into a top-level `global_status`
  segment array, and the TUI paints it on the row above the command line while
  carving no per-window status. Floats are unaffected (always carry their own
  status). Verified end-to-end (`editing.rs` modes 0/1/2/3 + round-trip + the
  shipped `examples/laststatus/` config; `screen.rs` paints the global bar / no
  bar at mode 0).
- **Tabline reuse** — the original request: a `tabline` string option that runs
  the *same* engine, with `%nT` tab-select / `%T` / `%X` close items. At this
  point `tabline = '%!v:lua.require("myutils").my_tab_line()'` works verbatim.
- Full-config readiness pass.

## Phase 7 — Statusline click regions (`%@handler@…%X`) ✅ done

The deferred `%@click@` handlers, now implemented for the per-window
`'statusline'` `%`-format. `%@handler@text%X` (and `%N@handler@…%X` carrying a
`minwid`) makes the wrapped cells clickable; a left-click calls the handler with
neovim's arguments `(minwid, clicks, button, modifiers)`. The handler is a
`v:lua.…` reference, consistent with the `%{}`/`%!` bridge.

**Shipped:**
- **Parse/expand/layout** (`crates/nxvim-core/src/statusline.rs`):
  `Item::ClickStart { handler, minwid }` / `Item::ClickEnd` (and `Piece` twins);
  `%X`/`%nX` now parse as `ClickEnd` (still no-op text, so render-only `%T…%X`
  tablines are unaffected), `%T`/`%nT` stay `TabRegion`. `flatten` tags each
  `Cell` with the click region in force; the new `layout_with_clicks` returns the
  laid-out segments **and** a `Vec<ClickRegion>` (the surviving display-column
  spans, tracked through truncation/`%=` fill). The plain `layout` is unchanged
  for the callers / parser tests that don't need regions.
- **Mouse** (`crates/nxvim-core/src/editor/mouse.rs`): `MouseTarget::StatusLine`
  carries the window-relative column; a left-press on a status line records a
  `StatuslineClick` (win, col, click count, button, modifiers) on
  `Editor::statusline_clicks`, with its own multi-click tracker so it never seeds
  a text selection.
- **Server** (`redraw.rs` / `effects.rs` / `dispatch.rs`):
  `statusline_click_at` recomputes the clicked window's regions on demand and
  resolves the column to a handler; `dispatch_statusline_clicks` (drained right
  after `editor.mouse`) fires each via `LuaRuntime::run_statusline_click`
  (`nx._statusline_click` resolves the `v:lua.` reference loud), then
  `apply_lua_effects` + `run_pending` so a handler's `vim.cmd`/`:lua` settles on
  the same gesture.
- **Tests** (`tests/mouse.rs`): fire-with-args, outside-region no-op,
  column-resolves-the-region, effects-settle, double-click count, and a loud
  error for a non-`v:lua` handler. **Example**: `examples/statusline/` wraps the
  file block in a `%@v:lua.on_name_click@…%X` region.

**Follow-ons (done, 2026-06-16):**
- Click regions also work for `nx.statusline` **segment** layouts — a segment/cell
  `on_click = "v:lua.<fn>"` fires through the same dispatch (see
  `docs/plans/2026-06-15-nx-statusline-segments.md`).
- The single **global bar** (`laststatus=3`) is clickable too: `hit_test` resolves
  its chrome row to `MouseTarget::GlobalStatusLine { col }`. Both the `%`-format and
  segment surfaces are covered. (`examples/laststatus/` makes its mode block
  clickable; tests in `nxvim-server/tests/mouse.rs`.)
- The **custom tabline** (`'tabline'` `%`-format) is now clickable: `%nT` opens a
  tab-select region (→ `Editor::select_main_tab(n)`), `%@…%X` a Lua handler. A click
  region carries a `ClickAction` (`Handler { handler, minwid }` | `Tab(n)`), and a
  `StatuslineClick` carries a `ClickSurface` (`Window` | `Global` | `Tabline`) so the
  server re-runs the right format at the right width. The built-in (structured)
  tabline keeps its own click path; `hit_test` routes a custom-tabline press to the
  server. (`examples/tabline/` is now click-to-switch; tests in `mouse.rs`.)

**Out of scope (still):** right/middle-button click regions (v1 fires on
left-click only); `%nX` close-button regions still just terminate (no per-tab close
action).

## Out of scope (for now)

- Full Vimscript `%{}` expressions — only `v:lua.*` is supported (loud error
  otherwise). Covers the user's config.
- Per-window (`setlocal statusline`) overrides — global-only in v1.
- `'rulerformat'`, `'winbar'`.
