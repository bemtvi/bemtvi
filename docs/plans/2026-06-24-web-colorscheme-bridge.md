# Web colorscheme bridge — make catppuccin (and any colorscheme) apply in the browser build

Date: 2026-06-24

## Problem

In the in-browser python demo (and the plain `nxvim-web`/edithost wasm build), the
catppuccin colorscheme is loaded by `demo-seed/init.lua`
(`require("catppuccin").load("mocha")`) and **populates the core highlight registry**
correctly — but the editor still renders in the hardcoded One Dark look. The
colorscheme never reaches the renderer:

- **Chrome** (Normal bg/fg, gutter, status line, selection, cursor line, …) is read
  from hardcoded `:root` CSS variables in `web/index.html`. The real palette path,
  `applyChrome()`, is gated behind `serverStyled`, which is **false on the wasm build**
  (the renderer stays in JS-highlighting mode so it keeps painting code itself — see the
  `js_highlight` frame flag, `redraw.rs`).
- **Syntax** (code token colors) is read from the hardcoded `FG` map in
  `web/highlight.js` (a One Dark family), applied by the JS tree-sitter highlighter — not
  from the colorscheme's `@capture` / legacy syntax groups.

So whatever colorscheme you `load()` in the browser, you see One Dark.

## Key insight

The chrome data is **already on the wire**: `redraw.rs::chrome_styles()` runs
unconditionally, resolving the standard chrome groups (`Normal`, `LineNr`, `Visual`, …)
into the per-frame `styles` palette and shipping them as `view.chrome` (a
`key -> style_id` map). The only thing missing is *applying* it on the wasm path. Syntax
colors are the genuinely new export.

## Approach — full theme bridge

Make the browser renderer drive its look from the active colorscheme, so **any**
colorscheme works (not a catppuccin special-case), consistent with the project's
port-natively / no-special-casing ethos.

### Phase 1 — chrome (mostly wiring already-shipped data)

- `index.html`: apply chrome whenever there's a colorscheme to honor — i.e. on the
  server-styled `:connect` path **or** the local wasm build (`serverStyled || js_highlight`)
  — without flipping `serverStyled` (JS highlighting + JS-synthesized chrome stay on).
- Rewrite `applyChrome()` to resolve each chrome key through the per-frame palette via
  the existing `chromeStyle()` helper (chrome values are palette **ids** on every build,
  not inline objects), and to cover the full var set: `--bg/--fg` (Normal),
  `--gutter` (LineNr), `--gutter-cur` (CursorLineNr), `--cursorline` (CursorLine),
  `--sel` (Visual), `--search` (Search), `--status-bg/--status-fg` (StatusLine),
  `--eob` (EndOfBuffer), `--cursor` (Cursor). Each var is only overridden when its group
  resolves, so a build with no colorscheme keeps the One Dark CSS defaults (no regression
  for the plain web build).
- `redraw.rs`: add `("cursor", "Cursor")` to the `CHROME` table so the block cursor color
  tracks the theme.

### Phase 2 — syntax (new export)

- `redraw.rs`: on the wasm build only (`cfg!(not(feature = "native"))`), ship a `theme`
  map — `capture-name -> style_id` resolved via `highlights.resolve_capture()` over the
  same key set the JS `FG` table covers (so each key gets the colorscheme's faithful
  fallback: `@function.builtin` → `@function` → `Function`). Also ship `theme_gen`
  (`highlights.generation()`) so the client rebuilds the JS color map only when the
  colorscheme actually changes.
- `highlight.js`: add a module-level runtime theme override consulted by `colorFor()`
  before the static `FG` table (same dotted-fallback walk), plus a `setHlTheme(map)`
  export to set it. Code spans are capture *names*, resolved to colors at paint time, so
  updating the theme + repainting is enough — no span recompute.
- `index.html`: when `view.theme_gen` changes, resolve `view.theme` ids against the
  current `view.styles` palette into `{name: cssColor}` and call `ts.setHlTheme(...)`,
  then repaint.

### Phase 3 — verify

- `web/verify-colorscheme.mjs`: boot the demo (or load catppuccin via `execLua`), assert
  the rendered `--bg` CSS var equals catppuccin mocha base (`#1e1e2e`) and that a code
  token (e.g. a Python keyword) renders in catppuccin mauve (`#cba6f7`), not the One Dark
  values. Build wasm + run under the Playwright harness (see
  `edithost-web-verify-harness-howto`).

## Files

- `crates/nxvim-server/src/redraw.rs` — `CHROME` table; new `theme` / `theme_gen` export.
- `crates/nxvim-edithost/web/index.html` — `applyChrome()` rewrite + call site; theme
  hook in `setFrame`.
- `crates/nxvim-edithost/web/highlight.js` — runtime theme override + `setHlTheme`.
- `crates/nxvim-edithost/web/verify-colorscheme.mjs` — new verify.
