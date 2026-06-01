# Running the catppuccin colorscheme plugin

**Status:** plan / not started · **Date:** 2026-06-01

Goal: take the real [catppuccin/nvim](https://github.com/catppuccin/nvim) Lua
plugin, unmodified, load it inside nxvim, and have it actually recolor the
editor — text (treesitter groups), the line-number gutter, the visual
selection, and the status/command lines — with catppuccin's palette.

This is the first *real* Lua plugin nxvim will run, and catppuccin is a good
forcing function: it is pure-Lua (no Vimscript), it is a colorscheme (so the
payoff is visible and testable on screen), and it exercises a broad slice of the
`vim.*` API — `require` over a runtimepath, `vim.api.nvim_set_hl`, user
commands, autocmds, `vim.fn.stdpath`, options, and a Lua→file compile step.

---

## How catppuccin actually loads (what we must satisfy)

Traced from the plugin source (`init.lua`, `colors/catppuccin.lua`,
`lib/compiler.lua`):

1. **User entry.** A config does `require("catppuccin").setup({…})` then
   `vim.cmd.colorscheme("catppuccin")` (or `:colorscheme catppuccin`).
2. **`:colorscheme catppuccin`** sources `colors/catppuccin.lua` off the
   runtimepath, whose entire body is `require("catppuccin").load()`.
3. **`setup(conf)`** merges the user table over defaults with
   `vim.tbl_deep_extend`, optionally auto-detects integrations, hashes the
   config (`+ vim.fn.getftime` of the install + `vim.o.winblend`/`pumblend`),
   and **recompiles** if the hash changed, writing the hash to `compile_path`.
4. **`compile()`** calls `require("catppuccin.lib.mapper").apply(flavour)` to
   build the full highlight table, serializes it to Lua source of the form
   `h(0, "<Group>", { fg = "#…", bg = "#…", bold = true, … })` (where
   `h = vim.api.nvim_set_hl`), runs it through `loadstring`/`string.dump`, and
   writes the **bytecode** to `compile_path/<flavour>` via `io.open`/`file:write`
   (creating the dir with `vim.fn.isdirectory` / `vim.fn.mkdir`).
5. **`load(flavour)`** `loadstring`s that compiled chunk and runs it; the chunk
   sets `vim.o.termguicolors = true`, `vim.o.background`, `vim.g.colors_name`,
   and fires several hundred `vim.api.nvim_set_hl(0, group, opts)` calls. Groups
   include both legacy syntax groups (`Comment`, `String`, `Function`,
   `Keyword`, `Type`, …), editor groups (`Normal`, `LineNr`, `CursorLineNr`,
   `Visual`, `StatusLine`, …), and treesitter groups (`@comment`, `@string`,
   `@keyword`, `@function.call`, …), many defined as **links** to others.

So "make catppuccin work" decomposes into four capabilities nxvim lacks today:

- **A plugin runtime**: `require` resolving modules off a runtimepath, a place
  to drop the plugin, and an `init.lua` sourced at startup.
- **A broad-enough `vim.*` surface**: the table/string/option/fn/api helpers the
  load path calls (most are pure Lua and can ship as a Lua prelude).
- **A highlight-group registry**: `nvim_set_hl` storing fg/bg/sp/attrs/links, a
  `:colorscheme` command that sources `colors/<name>.lua`, and link resolution.
- **A theme→screen pipeline**: today the **client** owns colors
  (`nxvim-tui::group_style` hardcodes ANSI per treesitter family). catppuccin
  moves color ownership to the plugin, so the **server** must resolve each
  capture/region to a concrete (truecolor) style and the client must render it.

That last point is a deliberate architectural shift — see *Architecture note*.

---

## Architecture note: where colors live

Per `docs/architecture.md`, the current split is "server owns *which* cells are
in a group; client owns *how* the group looks." catppuccin inverts the *how*:
the theme (server-side Lua) now decides the concrete color of every group. We
keep the *which* on the server unchanged; we move *resolution of group → style*
to the server too, and the redraw payload carries **concrete styles** (RGB +
attributes) instead of bare capture names. The client becomes a dumb truecolor
renderer with a built-in fallback theme for when no colorscheme is loaded
(preserving today's behavior out of the box).

This matches real neovim (highlight groups + `termguicolors` live in the
editor, the UI just paints attrs) and must be written up in `architecture.md`
(Phase 6) so the *View protocol* and *Syntax highlighting* sections stay honest.

---

## Phasing

Six phases. Each is independently shippable, ends with black-box integration
tests (per the repo's testing philosophy — no unit tests; drive a real server
over RPC and assert on `nvim_buf_get_lines` / cursor / the `redraw` view, or
paint a `View` with the real client and assert on cells), and leaves `main`
green. A fresh context can pick up any phase from its **Done when** checklist
plus the **Handoff notes** without re-reading the others.

Phases 1–2 are plumbing (no visible change). Phase 3 makes the theme *exist* in
the server. Phase 5 is where the screen finally turns purple. Keep that order:
each later phase is testable only because the earlier one landed.

---

### Phase 1 — Plugin runtime: `require`, runtimepath, and `init.lua` ✅ DONE (2026-06-01)

**Landed:** `ServerInit` gained `config_dir` + `runtimepath`;
`nxvim_server::default_runtime()` resolves them from `$NXVIM_CONFIG` /
`$XDG_CONFIG_HOME` / `$HOME` + `$NXVIM_RUNTIMEPATH` + `pack/*/start/*` discovery
(binary calls it; tests pass explicit paths to avoid env races).
`LuaRuntime::new(runtimepath)` seeds `package.path` (`<rt>/lua/?.lua`,
`<rt>/lua/?/init.lua`) and exposes `runtimepath()` for Phase 3's `colors/` search.
The server sources `<config_dir>/init.lua` at startup via a new `source_init`
(reuses the `:lua` drain path; missing file = silent). Tests:
`init_lua_runs_at_startup_and_require_resolves_runtimepath_modules` and
`missing_init_lua_is_harmless` in `editing.rs`.
**Phase 4 de-risked:** verified under the live VM that `loadstring`, `io`, `os`,
`require`, and crucially the `string.dump`→`loadstring` **bytecode round-trip**
all work — so Phase 4 can pursue strategy A (run catppuccin's real compiler).



**Why first:** nothing about catppuccin can run until `require("catppuccin")`
resolves a file on disk. `Lua::new()` already loads the safe stdlib (so
`package`/`require`/`io`/`os` exist), but `package.path` points nowhere useful
and nxvim has no concept of a runtimepath, a plugin directory, or a startup
script.

**Scope**
- Define nxvim's runtimepath and config story (smallest thing that works):
  - A config dir (XDG: `$XDG_CONFIG_HOME/nxvim` → `~/.config/nxvim`, override
    with `$NXVIM_CONFIG`) containing `init.lua`.
  - A pack/plugin dir for installed plugins (e.g. `<config>/pack/*/start/*` or a
    flat `<config>/plugins/*`), each plugin contributing its `lua/` to the
    module search path and its root to the runtimepath (so `colors/*.lua` is
    findable). Mirror neovim's layout closely enough that a real catppuccin
    checkout is drop-in.
  - Allow `$NXVIM_RUNTIMEPATH` (a list) so tests can point at a fixture/checkout
    without touching the user's home.
- In `nxvim-lua`: on `LuaRuntime::new`, seed `package.path` from the runtimepath
  (`<rt>/lua/?.lua;<rt>/lua/?/init.lua;…`) and record the runtimepath so later
  phases can search `colors/`, `after/`, etc. Decide modules-via-`package.path`
  vs. a custom `package.loaders` searcher; `package.path` is enough for
  catppuccin and simpler.
- In `nxvim-server`/`nxvim`: at startup, after the Lua runtime is built, source
  `init.lua` if present (run it through the same `drain_lua` path so its queued
  commands apply). Thread the runtimepath from the binary into the server init
  (extend `ServerInit`).
- Confirm `io`/`os`/`string.dump`/`loadstring` are actually present under the
  current `Lua::new()` stdlib set; if any are missing, widen the stdlib (still
  excluding `debug`). Phase 4 (compile) depends on this; verify it now.

**Done when**
- A fixture module on the runtimepath can be `require`d from Lua and its return
  value observed (e.g. an `init.lua` does `require("probe")` and the probe sets a
  status message asserted via the `redraw` view, or writes via an `nvim_command`
  that the test reads back).
- `init.lua` in the config dir runs at server startup.
- A test points `$NXVIM_RUNTIMEPATH` at a temp dir and proves both the module
  search and the `init.lua` sourcing.

**Handoff notes**
- Touch points: `crates/nxvim-lua/src/lib.rs` (package.path + runtimepath
  storage), `crates/nxvim-server/src/lib.rs` (`ServerInit`, startup sourcing),
  `crates/nxvim/src/main.rs` (resolve config/runtimepath, pass down).
- Don't expand `vim.*` here — that's Phase 2. Just get files loading.
- Keep the runtimepath as data on `LuaRuntime` (Phase 3 reads it to find
  `colors/<name>.lua`).

---

### Phase 2 — Broaden the `vim.*` surface (no highlights yet) ✅ DONE (2026-06-01)

**Landed:** a bundled Lua prelude (`crates/nxvim-lua/src/prelude.lua`,
`include_str!`-loaded at VM init) provides the pure-Lua surface —
`tbl_deep_extend`/`tbl_extend`/`tbl_filter`/`tbl_keys`/`tbl_values`/`tbl_map`/
`tbl_contains`/`tbl_isempty`, `deepcopy`, `list_extend`, `split`, `startswith`/
`endswith`, `inspect`, a minimal chainable `vim.iter`, `vim.log.levels`,
`vim.notify`, `vim.schedule` (runs immediately — no event loop yet), `vim.g`
(plain store), `vim.o` (`background`/`termguicolors`/`winblend`/`pumblend`
defaults), `vim.opt` (scalar proxy over `vim.o`), `vim.env` (read-through to
`os.getenv`), and the registration APIs `nvim_create_user_command` /
`nvim_create_augroup` / `nvim_create_autocmd` (stored in `vim._user_commands` /
`_augroups` / `_autocmds`) plus a no-op `nvim_set_hl` stub. `vim.cmd` became
callable **and** indexable (`vim.cmd.set("number")` → `:set number`). Rust-backed
`vim.fn` (`stdpath`, `getftime`, `isdirectory`, `mkdir`, `has`) covers what needs
real FS/env. **Command resolution seam:** the core now defers unknown
ex-commands to `Editor::deferred_commands`; the server's new `run_pending`
fixpoint loop runs `:lua` chunks and dispatches deferred commands to Lua user
commands (`LuaRuntime::has_user_command`/`run_user_command`), else the standard
`E492`. Tests in `editing.rs`: `vim_tbl_deep_extend_*`, `vim_g_round_trips_*`,
`vim_cmd_is_callable_and_indexable`, `vim_fn_stdpath_*`,
`user_command_registers_and_dispatches`, `unknown_command_still_reports_*`, and
`colorscheme_style_plugin_load_runs_clean` (a mini-plugin mimicking catppuccin's
setup→load→nvim_set_hl/link/user-command/autocmd shape, run clean end to end).
**Deferred to when catppuccin is on disk (Phase 6 fixture):** running the *real*
plugin's `setup()` — the mini-plugin proves the surface shape instead.



**Why:** `setup()` and the load path call a wide but shallow set of helpers. Get
them all present so catppuccin's Lua *executes* to completion (highlights still
no-op until Phase 3). Most are pure Lua — ship them as a **Lua prelude** loaded
at runtime init (the nxvim analogue of neovim's `runtime/lua/vim/shared.lua`),
not as Rust, so they stay faithful and cheap to maintain.

**Scope** — provide at least what the traced load path uses:
- Pure-Lua prelude (`vim.tbl_deep_extend`, `vim.tbl_extend`, `vim.tbl_filter`,
  `vim.tbl_keys`, `vim.tbl_contains`, `vim.startswith`, `vim.split`,
  `vim.list_extend`, `vim.iter` (minimal), `vim.inspect`, `vim.log.levels`,
  `vim.deepcopy`). Lift these from neovim's `shared.lua` where licensing allows
  or reimplement.
- `vim.g` — a metatable-backed proxy over server-held global vars
  (`vim.g.colors_name`, `vim.g.catppuccin_flavour`, `vim.g.catppuccin_debug`).
- `vim.o` / `vim.opt` — at minimum readable/writable `background`,
  `termguicolors`, `winblend`, `pumblend`. Back them by real editor options
  where they exist (`background`, `termguicolors`), stub the rest as stored
  values. `vim.opt` can be a thin wrapper over `vim.o` for the fields used.
- `vim.env` — a proxy over `os.getenv`/process env (e.g. `KITTY_WINDOW_ID`).
- `vim.fn` — `stdpath("cache"|"config"|"data")`, `getftime`, `isdirectory`,
  `mkdir`, `has`(stub). Back `stdpath` by the dirs resolved in Phase 1.
- `vim.api` additions used *before* highlights: `nvim_create_user_command`
  (register name → Lua callback; dispatch from the ex-command path),
  `nvim_create_augroup` / `nvim_create_autocmd` (store handlers; only
  `ColorScheme` needs to actually fire, in Phase 3), `nvim_command` (already
  present), `nvim_set_hl` (stub now → real in Phase 3).
- `vim.cmd` — make it callable *and* indexable: `vim.cmd("…")` queues an
  ex-command (today's behavior) and `vim.cmd.colorscheme("catppuccin")` maps to
  `:colorscheme catppuccin`. A metatable over the existing queue.
- `vim.notify` / `vim.schedule` — `notify` routes to the message line;
  `schedule(fn)` can run synchronously after the current chunk drains (no real
  event loop needed for catppuccin).

**Done when**
- A test runs `require("catppuccin").setup({})` with no error (highlights are
  stubbed; assert no `E5108` lua error reaches the message line).
- Targeted tests for the load-bearing helpers: `vim.tbl_deep_extend` merge,
  `vim.cmd.colorscheme` queuing `:colorscheme catppuccin`, `vim.g.colors_name`
  round-trip, `vim.fn.stdpath("cache")` returning the Phase-1 dir, a user
  command registered via `nvim_create_user_command` then invoked as `:Catppuccin`.

**Handoff notes**
- Touch points: `crates/nxvim-lua/src/lib.rs` + a new bundled prelude `.lua`
  (embed with `include_str!`, run at init). `crates/nxvim-core/src/editor.rs` and
  `options.rs` for `background`/`termguicolors` options + user-command dispatch
  from `execute_ex`.
- Anything editor-affecting must round-trip through the existing
  `lua_queue`/`drain_lua` mechanism — don't let Lua mutate the editor directly
  (keeps `nxvim-core` pure).
- Leave `nvim_set_hl` a no-op stub that *accepts* the full arg shape
  (`(ns, name, { fg, bg, sp, bold, italic, underline, undercurl, reverse,
  link, … })`) so Phase 3 only has to add storage.

---

### Phase 3 — Highlight-group registry + `:colorscheme` + link resolution ✅ DONE (2026-06-01)

**Landed:** a pure `Highlights` registry (`crates/nxvim-core/src/highlight.rs`)
on `Editor` — `HlDef` (fg/bg/sp as 24-bit `Rgb` + the six boolean attrs +
`link`), `set`/`clear`/`get`, a cycle-guarded `resolve(group) -> Style` that
follows link chains, and `resolve_capture(capture)` that walks the standard
fallback chain (`function.call` → `@function.call` → `@function` → `Function`,
then a legacy-group map for the captures nxvim-ts emits). `parse_color` handles
`#rrggbb`, a small named-color set, and `NONE`. `nvim_set_hl` is now Rust-backed
in `nxvim-lua` (captures the opts shape — incl. integer→`#rrggbb` colors — into
`Shared.highlights`, exposed via `take_highlights()`/the new `HlSet`); the
server folds them into the registry through the existing `apply_lua_effects`
drain, so the core stays the sole mutator. `:colorscheme <name>` sources
`colors/<name>.lua` off the runtimepath, records `g:colors_name`, and fires the
`ColorScheme` autocmd (new prelude `vim._fire` + `LuaRuntime::fire_autocmd`);
missing → `E185`. `:hi clear` empties the registry. New RPCs: `nvim_get_hl(0,
{name})` (link-resolved style as RGB ints + attr flags) and the
`nxvim_resolve_capture` debug hook. Tests in `editing.rs`:
`nvim_set_hl_stores_resolved_colors_and_attrs`,
`nvim_get_hl_follows_links_to_the_target_color`,
`capture_resolves_through_the_group_fallback_chain`,
`colorscheme_sources_the_file_and_fires_the_autocmd`,
`colorscheme_missing_file_reports_e185`, `hi_clear_empties_the_registry`.
**Deliberately not touched:** the `View`/redraw and the TUI — the registry is
fully resolvable but nothing is repainted yet (that lands in Phase 5).



**Why:** this is where the theme starts to *exist* server-side. After this
phase the full catppuccin highlight table lives in the server, queryable, with
links resolved — even though nothing is repainted yet (Phase 5).

**Scope**
- A `Highlights` registry (new module in `nxvim-core`, kept pure — it's just a
  map + resolver, no I/O). Stores per-group attrs: `fg`/`bg`/`sp` as 24-bit RGB
  (parse `"#rrggbb"` and the small set of named colors catppuccin uses),
  booleans (`bold`, `italic`, `underline`, `undercurl`, `strikethrough`,
  `reverse`), and `link` (group → group). Namespace `0` (global) is enough.
- `nvim_set_hl(ns, name, opts)` (real now) → writes the registry via the
  `lua_queue`/drain path. `:hi`/`:highlight` parsing is *not* required for
  catppuccin (it uses the API), but `:hi clear` / reset-to-default should empty
  the registry back to the built-in defaults.
- Link resolution: `resolve(group) -> ResolvedStyle` follows `link` chains
  (cycle-guarded) to a concrete style; unresolved/empty → none.
- Treesitter capture → highlight group mapping. nxvim-ts emits capture names
  like `keyword`, `string`, `function.call`. Map to neovim's `@`-group
  convention (`@keyword`, `@string`, `@function.call`) and walk the standard
  fallback chain (`@function.call` → `@function` → `Function`) so a theme that
  only sets `Function` still colors function calls. This mapping table is the
  heart of the phase — model it on neovim's `runtime/lua/vim/treesitter/`
  defaults.
- `:colorscheme <name>` ex-command: locate `colors/<name>.lua` on the
  runtimepath (Phase 1), source it, set `g:colors_name`, and fire the
  `ColorScheme` autocmd (Phase 2 handlers). Add `nvim_get_hl` (or a debug RPC
  hook) so tests can read resolved styles back.

**Done when**
- A test sources catppuccin (`:colorscheme catppuccin`, or directly
  `require("catppuccin").load()`), then queries via `nvim_get_hl`:
  `Normal`/`Comment`/`Function`/`@keyword` return the expected catppuccin
  mocha hex values, and a linked group resolves to its target's color.
- A capture-name → resolved-style lookup test: `string` resolves to catppuccin's
  green, `function.call` resolves (via fallback) to blue, an unknown capture
  resolves to none.
- `:hi clear` empties back to defaults.

**Handoff notes**
- Touch points: new `crates/nxvim-core/src/highlight.rs` (registry + resolver +
  capture map), `editor.rs` (`:colorscheme`, `:hi clear`, owns the registry),
  `nxvim-lua` (`nvim_set_hl` → queued registry op; `nvim_get_hl` read path),
  `nxvim-server` (`nvim_get_hl` RPC, `:colorscheme` sourcing via runtimepath).
- Keep the registry in `nxvim-core` and mutate it only via the drain path, so
  core stays pure and synchronous.
- This phase deliberately does **not** touch the `View`/redraw or the TUI — the
  theme is fully resolvable but not yet painted. That keeps the diff reviewable
  and lets the (harder) rendering change land on its own in Phase 5.

---

### Phase 4 — catppuccin's compile step (run it, or bypass it) ✅ DONE (2026-06-01, strategy A)

**Landed:** strategy A confirmed — catppuccin's real compile mechanics work
under nxvim's vendored Lua 5.1 with **zero new Rust** (the Phase 1/2 surface
already sufficed). Verified by fetching the actual plugin and reading its real
load path: `lib/compiler.lua` serializes the highlight table to Lua source,
`loadstring`s it, `string.dump(fn, true)`s the result to bytecode, and
`io.open(path, "wb")`/`file:write`s it to `compile_path/<flavour>` (creating the
dir via `vim.fn.isdirectory`/`mkdir`); `init.lua`'s `load()` then **`loadfile`s
the cached bytecode** (not `loadstring`) and calls it, firing `nvim_set_hl`. The
open risk was whether mlua permits loading binary chunks from disk via
`loadfile` — it does. A hermetic test (`editing.rs`,
`colorscheme_compiles_to_bytecode_then_reuses_the_cache`) drives a fixture
mirroring those exact mechanics: first `:colorscheme` compiles once
(serialize→`string.dump`→`io.write`), the bytecode cache file lands on disk,
`loadfile`+run populates the registry (asserted via `nvim_get_hl`, incl. a link
resolving through the compiled table), and a second load **reuses the cache**
(an observable `vim.g._compiles` counter stays at 1). Per the Phase 2 precedent,
the fixture stands in for the real checkout; running the *actual* catppuccin is
Phase 6's job.
**Remaining for Phase 6:** the real plugin's default
`compile_path = vim.fn.stdpath("cache") .. "/catppuccin"` — Phase 6 tests must
redirect `stdpath("cache")` (or pass `compile_path` via `setup`) to a temp dir.
The fixture here passes `compile_path` explicitly, so it needs no stdpath
redirect.



**Why isolated:** the compile path is the one piece that touches the real
filesystem and Lua bytecode (`io.open`, `file:write`, `string.dump`,
`loadstring`, `vim.fn.mkdir`), and is the most likely to misbehave under
vendored Lua 5.1. Keeping it as its own phase means a fresh context can make
`load()` work end-to-end (or swap the strategy) without disturbing Phases 1–3.

**Scope** — pick one strategy (recommend trying A first, B as the fallback):
- **A — run the real compiler.** Provide the exact surface the compiler needs:
  `vim.fn.stdpath("cache")` + `mkdir`/`isdirectory` (Phase 2), a writable cache
  dir, and a working `string.dump`/`loadstring` round-trip in mlua's Lua 5.1
  (same VM dumps and loads, so the bytecode format matches — verify with a small
  dump→load test). Then `require("catppuccin").load()` compiles on first run and
  loads the cached chunk thereafter.
- **B — bypass compilation.** If the bytecode round-trip is unreliable, call
  `require("catppuccin.lib.mapper").apply(flavour)` directly to get the highlight
  table and feed it through `nvim_set_hl`, skipping `compile()`/`loadstring`.
  This still runs the *real* plugin logic (palette, integrations, groups) — only
  the serialize-to-disk optimization is replaced. Implement as a thin nxvim-side
  shim, not a patch to the vendored plugin.

**Done when**
- `require("catppuccin").load()` (via `:colorscheme catppuccin`) completes with
  no Lua error and the registry is fully populated (re-run the Phase 3
  `nvim_get_hl` assertions, now through the *real* end-to-end load rather than a
  hand-driven `nvim_set_hl`).
- If strategy A: a second load reuses the cache (assert the cache file exists);
  a `dump→load` smoke test passes. If strategy B: document why and where the
  shim lives.

**Handoff notes**
- Decide A vs B with a 30-minute spike on the `string.dump`/`loadstring`
  round-trip before committing.
- The compiler writes to `vim.fn.stdpath("cache")` — make sure tests point that
  at a temp dir (via the Phase-1/2 stdpath plumbing) so they don't litter
  `~/.cache`.
- Catppuccin's default flavour is `mocha`; pin the test flavour explicitly via
  `setup({ flavour = "mocha" })` so assertions are stable.

---

### Phase 5 — Paint it: resolved styles through the View into the TUI (truecolor) ✅ DONE (2026-06-01)

**Landed:** color resolution moved into the redraw and the client became a
truecolor renderer. The server now resolves every highlight span's capture
through the Phase-3 registry and the chrome groups (`Normal`, `LineNr`,
`CursorLineNr`, `Visual`, `StatusLine`, `EndOfBuffer`) to concrete styles,
deduping them into a per-frame `styles` palette (a new `StyleTable` in
`nxvim-server`); the redraw map gained a `styles` array, a `chrome` map of
`name -> style_id`, and each `highlights` span grew a 4th element (a palette id,
`Nil` when unresolved). Core `Rgb`/`Style` gained `Eq`/`Hash` for the dedup. The
TUI parses the palette into `ratatui::Style`s, paints the `Normal` background as
a `Block` across the whole text area (so token spans patch their fg onto it),
themes the gutter via `LineNr`/`CursorLineNr`, the selection via `Visual`
(replacing reverse-video), the status line via `StatusLine`, and the `~` rows
via `EndOfBuffer`; the scroll-animation band shares the same palette. The
client's `group_style` stays the **fallback theme** used per-span when no
resolved style is sent, so default startup is byte-for-byte unchanged. Tests:
Tier-1 `crates/nxvim-tui/tests/paint.rs`
(`a_resolved_style_paints_its_truecolor_foreground`,
`the_normal_background_fills_the_text_area`,
`the_visual_style_replaces_reverse_video_when_themed`,
`no_colorscheme_falls_back_to_the_builtin_theme`) and Tier-2
`crates/nxvim/tests/syntax.rs`
(`a_loaded_colorscheme_paints_resolved_styles_truecolor` — sources a
catppuccin-shaped `colors/` fixture via `:colorscheme` and asserts keyword
mauve, string green, `Normal` background, `CursorLineNr` gutter, and a `Visual`
selection on the real painted grid).
**Deviation from the handoff note:** chrome styles and per-span ids are resolved
entirely server-side (the syntax spans already live in the server), so
`view.rs` was left untouched — the resolution layers onto the redraw map, not
the `View` struct. **Left for Phase 6:** the Tier-3 PTY test confirming real
crossterm 24-bit escapes, and the `architecture.md` write-up of the
color-ownership shift.



**Why:** the visible payoff. Everything before this resolves a theme the user
can't see. This phase moves color resolution into the redraw and teaches the
client to render 24-bit color, gutter, selection, and status with the theme.

**Scope**
- **Redraw payload change.** Today `highlights` carries
  `[start_col, end_col, group]` (a bare capture name) and the client maps it to
  ANSI. Change the server to resolve each span's capture → `@`-group → concrete
  style (Phase 3 resolver) and send a **style** instead: either inline RGB +
  attr flags per span, or an index into a per-frame style table (dedup; cheaper
  on the wire). Recommend a small per-redraw palette: `highlights` spans carry a
  style id, plus a `styles` array of `{ fg, bg, sp, attrs }`.
- **Editor-chrome groups.** Add resolved styles for `Normal` (the editor
  fg/bg — the big visible win), `LineNr` + `CursorLineNr` (gutter), `Visual`
  (selection — replace the hardcoded reverse-video), `StatusLine`, and the
  `~` end-of-buffer (`EndOfBuffer`/`NonText`). Carry these on the `View`/redraw.
- **TUI render.** In `nxvim-tui`: render truecolor (`ratatui::style::Color::Rgb`)
  from the sent styles; apply `Normal` bg to the whole text area, theme the
  gutter via `LineNr`/`CursorLineNr` (replacing the `DIM` modifier), the
  selection via `Visual`, and the status line via `StatusLine`. Keep
  `group_style` as a **fallback theme** used only when the redraw carries no
  resolved style (no colorscheme loaded) so default-startup behavior is
  unchanged.
- Mind the existing scroll-animation band: it also carries `highlights`
  (`ScrollAnim`/`ScrollData`) — migrate it to the new style payload too so
  smooth-scroll stays colored.

**Done when**
- A Tier-2 full-stack screen test (`crates/nxvim/tests/screen.rs`): open a
  small source file, `:colorscheme catppuccin`, drive a real `redraw`, paint
  with the real client into a `TestBackend`, and assert specific cells carry
  catppuccin RGB — a keyword cell is mauve, a string cell green, the text-area
  background is base, the cursor-line gutter is the `CursorLineNr` color, a
  visual selection uses `Visual`.
- A Tier-1 paint test (`crates/nxvim-tui/tests/paint.rs`) feeding a known
  styled `View` asserts the RGB mapping and the no-colorscheme fallback.
- Default startup (no colorscheme) still renders exactly as before.

**Handoff notes**
- Touch points: `crates/nxvim-core/src/view.rs` (carry chrome styles + per-span
  style ids), `crates/nxvim-server/src/lib.rs` (`highlights_for` resolves to
  styles; build the per-frame `styles` table; add chrome groups), all of
  `nxvim-tui/src/lib.rs`'s render path (`render`, `render_text`,
  `highlight_line`, `cell_style`, `render_gutter`, `render_status`, `group_style`
  fallback), and the `View`/redraw parse on both sides.
- This is the largest single diff. Land the payload/protocol change and the TUI
  change together (they're coupled) but keep chrome groups (`Normal`, `Visual`,
  …) as a clearly separable second commit if it helps review.
- truecolor requires the terminal to support it; crossterm emits 24-bit
  escapes — the Tier-3 PTY test in Phase 6 confirms the real bytes.

---

### Phase 6 — Wire-up, defaults, and docs ✅ DONE (2026-06-01)

**Landed:** the **real, unmodified catppuccin** now loads end to end. Driving the
actual plugin surfaced (and fixed) three gaps the fixtures had hidden: the VM now
loads the full safe stdlib **plus `debug`** (catppuccin's `debug.getinfo` locates
its own install dir — done via mlua's `unsafe_new_with`, matching neovim's
trusted-config model); the prelude ships a pure-Lua **LuaJIT-compatible `bit`
library** (PUC Lua 5.1 has neither `bit` nor `bit32`, and catppuccin hashes its
config with djb2/xor); and a minimal **`vim.treesitter`/`vim.notify_once`** stub
(catppuccin's core `semantic_tokens` module probes `vim.treesitter.highlighter`).
With those, `setup({flavour="mocha"})` compiles the highlight table to Lua
bytecode under `stdpath("cache")` and `:colorscheme catppuccin` populates the
registry with the exact mocha palette (Normal `#cdd6f4`/`#1e1e2e`, Keyword mauve,
Function blue, Comment overlay+italic). Catppuccin is installed the user-config
way — cloned into `~/.config/nxvim/pack/plugins/start/catppuccin` (no vendoring) —
and a user `init.lua` loads it; since init.lua is sourced before the first frame,
the editor is themed from the moment it opens. Docs: `docs/architecture.md`
updated (View protocol → server-resolved styles + `styles`/`chrome` payload; Lua
section grown to runtimepath/`require`/`init.lua`/`nvim_set_hl`/`:colorscheme`
+ stdlib notes; roadmap line for the remaining `vim.*` gaps), and a new
`docs/getting-started.md` documents the repeatable setup. Tests: Tier-3
`crates/nxvim/tests/e2e.rs::catppuccin_repaints_the_editor_in_truecolor` drives
the real binary in a PTY and asserts catppuccin's truecolor reaches the `vt100`
screen (skips cleanly when no checkout is present, since we don't vendor it);
Tier-2 `editing.rs::init_lua_colorscheme_themes_the_first_frame` proves the
startup frame's `chrome` is already resolved.
**Decisions:** per the owner's call, catppuccin is **not vendored** — it's cloned
into the real user-config plugin path so it loads like any user plugin; the e2e
test reuses that checkout (or `$NXVIM_CATPPUCCIN`) and skips when absent.

**Why:** make it usable and keep the design docs honest.

**Scope**
- A realistic `init.lua` example (in `docs/` or an `examples/` dir) doing
  `require("catppuccin").setup({ flavour = "mocha" })` +
  `vim.cmd.colorscheme("catppuccin")`, and instructions for dropping the
  catppuccin checkout onto the runtimepath. Optionally vendor catppuccin as a
  submodule under a test/fixtures path so CI can run the full load.
- Make the `ColorScheme` autocmd observable and confirm `:colorscheme` at
  startup (from `init.lua`) colors the first frame.
- **Docs:** update `docs/architecture.md` — the *View protocol* and *Syntax
  highlighting* sections now describe server-resolved styles (the color-ownership
  shift from *Architecture note* above), and the *Lua* section grows from "narrow
  bridge" to "runtimepath + `require` + `nvim_set_hl` + colorscheme." Add a
  roadmap line for remaining plugin-API gaps.
- A Tier-3 PTY smoke test (`crates/nxvim/tests/e2e.rs`): launch the binary with a
  config that loads catppuccin and assert the parsed `vt100` screen shows the
  expected foreground/background colors (proves real crossterm truecolor escapes
  end to end).

**Done when**
- A documented, repeatable setup loads catppuccin on startup and the editor is
  visibly themed.
- `architecture.md` matches the new pipeline; `cargo test --workspace`,
  `cargo fmt --check`, and `cargo clippy -D warnings` are green.

---

## Risks & open decisions

- **Lua bytecode round-trip (Phase 4).** `string.dump`/`loadstring` under
  vendored Lua 5.1 must agree on format. Spike it early; strategy B is the
  escape hatch.
- **Surface creep in `vim.*` (Phase 2).** catppuccin's integrations can pull in
  more API than the core load path. Mitigate by running with
  `setup({ integrations = {} })`/default integrations first, expanding only what
  the actual error trail demands.
- **Color ownership shift (Phase 5).** This changes the client/server contract
  described in `architecture.md`. It's the right model (matches neovim) but must
  be documented, and the no-colorscheme fallback must preserve today's look.
- **Scope of "highlight groups."** Aim for the editor + treesitter groups that
  visibly matter (text, gutter, selection, status). Full parity with every
  neovim group (LSP, diagnostics, plugins) is out of scope until those
  subsystems exist.

## Out of scope

LSP/diagnostics highlight groups, statusline/tabline plugins, `:hi`-command
parsing, multiple namespaces beyond `0`, light/dark auto-switching beyond what
catppuccin reads from `background`, and Vimscript colorschemes (Lua-only, per
guiding principle 2).
