# Making nvim-cmp and lualine.nvim *actually* work — remaining-work plan

> **Status: PLANNED (not started).** This document scopes the two plugin
> compatibility gaps left after the 2026-06-10 plugin smoke-test sweep. That
> sweep fixed five plugins/clusters by adding the small `vim.*` primitives they
> were missing (see [Context](#context)); the two below need real *subsystems*,
> not one-line shims, so they were deferred rather than half-stubbed (nxvim's
> rule: a gap must fail loud, never quietly succeed).

## Context

A sweep booted a real server with each plugin in
`~/.config/nxvim/pack/plugins/start/` on the runtimepath and ran
`require(plugin).setup()`. Most plugins already worked; five were fixed by adding
genuinely-missing surface, one commit each, each with a black-box regression test
in `crates/nxvim-server/tests/plugin_compat.rs`:

| Commit | Fix | Plugins |
|---|---|---|
| `0ed0cc6` | `vim.split` honors the legacy boolean `plain` arg | nvim-treesitter |
| `633e509` | `vim.fn.hlexists` | LuaSnip |
| `480adb3` | `vim.w` / `vim.b` scoped variables | trouble.nvim |
| `9ed3516` | sign-definition registry + `vim.fn.trim` | nvim-dap, nvim-dap-virtual-text, dap-python |
| `705fd61` | `vim.uv.now`, `nvim_get_current_line`, `vim.str_utfindex`/`str_byteindex`, `vim.fn.exists` | nvim-cmp surface (partial — see below) |

Two remain. Both are blocked on subsystems nxvim does not yet have.

---

## 1. nvim-cmp — needs a decoration-provider redraw hook

### Where it stands

`705fd61` already added the four primitives cmp reaches for while building a
completion `context` and its float windows (`vim.uv.now`,
`nvim_get_current_line`, `vim.str_utfindex`/`str_byteindex`, `vim.fn.exists`).
With those, `require('cmp')` / `cmp.setup{}` now gets all the way to cmp's
default *custom entries* view, which registers a decoration provider:

```
nvim-cmp/lua/cmp/view/custom_entries_view.lua:57:
  vim.api.nvim_set_decoration_provider(custom_entries_view.ns, { on_win = ..., on_line = ... })
```

`vim.api.nvim_set_decoration_provider` does not exist in nxvim, so the view
errors at construction — which happens during `setup`, so cmp (and the four cmp
*sources* that `require('cmp')`: `cmp_luasnip`, `cmp_nvim_lua`, `cmp_path`, and
transitively the buffer/lsp sources) fail to load.

### Why it is not a one-liner

In neovim a decoration provider is a *per-redraw callback set*. The editor calls
`on_start(tick)` once per frame, then `on_buf` / `on_win(ns, win, buf, top, bot)`
/ `on_line(ns, win, buf, row)` as it lays out each visible window and line, and
the provider places **ephemeral** extmarks (highlights / virtual text) that live
for exactly that frame and are cleared before the next. cmp uses it to highlight
the matched characters of each entry in its menu buffer.

nxvim has none of that machinery:

- `redraw()` (`crates/nxvim-server/src/redraw.rs`) projects each window's band
  synchronously from the core (`project_band` → `highlights_for`), with
  `&self.editor` borrowed for the whole projection. There is no point at which
  Lua provider callbacks are invoked, and invoking them mid-projection would
  fight that borrow.
- The core extmark model (`crates/nxvim-core/src/extmark.rs`) has no notion of an
  *ephemeral* (single-frame, auto-cleared) extmark.
- There is no registry of decoration providers and no lifecycle to drive them.

A registration-only stub that stores the callbacks and never fires them is
**not acceptable** — it would make cmp look configured while its menu silently
renders unstyled, exactly the "quietly succeeds" failure nxvim forbids. And it
would diverge from the precedent that nxvim callbacks (`on_lines`, autocmds,
timers) do fire.

### Proposed implementation

Build a minimal but *real* decoration-provider path:

1. **Lua registry + API.** Add `vim.api.nvim_set_decoration_provider(ns, opts)`
   storing `{ on_start, on_buf, on_win, on_line, on_end }` per namespace in a
   `vim._decoration_providers` table (callbacks held in the existing
   `vim._cb_fns`-style registry so the server can invoke them by id). Clearing
   (`opts = {}` / empty) removes the provider.
2. **Ephemeral extmarks in core.** Extend `extmark.rs` with an ephemeral set that
   is rebuilt each frame: cleared at the start of a redraw, populated by provider
   callbacks, read by `highlights_for`, and dropped after the frame. Keep it
   separate from the persistent extmark store so namespaces and `on_line`
   re-entrancy stay clean.
3. **Drive the callbacks from redraw.** Before projecting a window's band, invoke
   the registered providers' `on_win` / `on_line` for that window/buffer/visible
   range. The hard part is the borrow: factor the provider-invocation step *out*
   of the `&self.editor`-borrowed projection — compute the visible
   window/buffer/range up front, run the Lua callbacks (which only need to *read*
   that info and *write* ephemeral extmarks via the queue), drain the queued
   ephemeral marks into core, then project. This matches nxvim's existing "Lua
   queues, server drains" model rather than calling Lua mid-borrow.
4. **Respect the generation/memoization** that `refresh_highlights` and
   `project_band` rely on so ephemeral decorations invalidate the per-frame
   cache correctly (otherwise a stale band hides them, or every frame
   recomputes).

Scope control: implement `on_win` + `on_line` (what cmp uses) first; `on_start` /
`on_buf` / `on_end` can be no-op-but-invoked initially. Anything not yet wired
must still **fail loud** if a provider relies on it.

### Files

- `crates/nxvim-lua/src/install.rs` or `prelude/api.lua` — the
  `nvim_set_decoration_provider` surface + registry.
- `crates/nxvim-core/src/extmark.rs` — ephemeral extmark support.
- `crates/nxvim-server/src/redraw.rs` (+ maybe `effects.rs`) — the per-frame
  provider invocation and ephemeral-mark drain.
- `crates/nxvim-lua/src/runtime.rs` — invoking the stored callbacks by id.

### Test plan

Extend `plugin_compat.rs`:

- `nvim_cmp_loads`: `require('cmp')` + `cmp.setup{}` returns `"OK"` (the load
  gate the sources cascade from) — promote the current `cmp_vim_surface_primitives`
  primitive test once cmp loads.
- An end-to-end menu test (mirroring `telescope_e2e`): open a buffer, trigger a
  completion, assert the entries render *and* the matched-character highlight
  spans appear in the `redraw` projection — proving the provider actually fires,
  not just registers.

### Risk

Medium-high. Touches the redraw hot path and the core extmark model; the borrow
restructuring and per-frame cache invalidation are the delicate parts. Land it
behind the existing redraw tests (`tests/redraw*`, `tests/extmarks.rs`) to catch
regressions in the no-provider path.

---

## 2. lualine.nvim — needs highlight read-after-write + defaults

### Where it stands

`require('lualine').setup{}` errors at:

```
lualine.nvim/lua/lualine/highlight.lua:54: attempt to index local 'base_color' (a nil value)
```

`base_color = extract_highlight_colors('Normal')` returns `nil` because, in
`lualine/utils/utils.lua`:

```lua
if vim.fn.hlexists(color_group) == 0 then return nil end
color = vim.api.nvim_get_hl_by_name(color_group, true)
```

`hlexists('Normal')` is `0` and `nvim_get_hl_by_name` is absent. There are three
distinct gaps, none a compat one-liner.

> Note: lualine is **installed but not configured** in the user's setup
> (`lua/plugins/init.lua` only sets up which-key), so this is latent, not a live
> breakage for them. Still worth fixing for general compatibility.

### Root causes

1. **No default highlight groups.** nxvim deliberately ships no built-in
   colorscheme — `Highlights::new()` (`crates/nxvim-core/src/highlight.rs`)
   starts empty, "the client owns the no-theme fallback look." So `Normal` (and
   the other standard groups neovim always defines) simply do not exist until a
   colorscheme loads. lualine's `theme = 'auto'` derives its statusline palette
   from `Normal`, so with no theme it has nothing to read.
2. **Highlight mirror lag (read-after-write).** Even *with* a colorscheme,
   `nvim_set_hl` (`install.rs`) only **queues** group definitions; the server
   folds them into the `vim._hl_defs` Lua mirror **between turns**
   (`effects.rs`, gated on `highlights.generation()`). So a typical
   `init.lua` that does `vim.cmd.colorscheme(...)` and then
   `require('lualine').setup{}` in the *same chunk* can't see `Normal` —
   `hlexists` / `nvim_get_hl` read a stale, empty mirror. (Options and registers
   already *write through* their mirrors for exactly this reason — see the
   `o_set` / `setreg` comments in `prelude/stdlib.lua`; highlights do not.)
3. **`nvim_get_hl_by_name` missing.** The pre-0.9 reader lualine calls; trivially
   a shim over `nvim_get_hl`, but moot until 1 and 2 are addressed.

### Proposed implementation

1. **Write-through `nvim_set_hl` → `vim._hl_defs`.** On each `nvim_set_hl`, in
   addition to queuing for the core, update the Lua mirror entry immediately so a
   same-turn `nvim_get_hl` / `hlexists` reflects it. The subtlety is *format*:
   `nvim_get_hl` returns `fg`/`bg`/`sp` as `0xRRGGBB` integers and boolean attrs
   (or `{ link = ... }`), whereas `nvim_set_hl` accepts `"#rrggbb"` strings,
   integers, or color names. Reuse the Rust color parser (`color_field` /
   `parse_color`) so the write-through produces the *same* canonical row the
   between-turn mirror push (`set_hl_mirror`) would — otherwise same-turn and
   next-turn reads disagree. Cleanest is to have the Rust `nvim_set_hl` closure
   also write the canonical mirror row (it already has `lua` in scope), rather
   than re-implementing the parse in Lua.
2. **`vim.api.nvim_get_hl_by_name(name, rgb)`** — a thin wrapper over
   `nvim_get_hl(0, { name = name })` mapping to the legacy shape
   (`foreground`/`background`/`special` ints + attrs).
3. **(Decision needed) Default highlight groups.** To make *bare*
   `lualine.setup{}` with **no** colorscheme work, nxvim would need to seed the
   standard default groups (at least `Normal`). This contradicts the current
   "no built-in colorscheme" design, so it is a **product decision**, not just an
   implementation one. Options:
   - (a) Leave it: lualine works once any colorscheme is loaded (the normal
     case), and bare-no-theme `setup{}` is documented as unsupported (faithful to
     nxvim's no-default-theme stance).
   - (b) Seed a tiny default group set (`Normal` fg/bg from the client's fallback,
     plus the handful colorschemes always define) so plugins that read `Normal`
     degrade gracefully. Lower-risk than it sounds if scoped to a read-only
     default that any real colorscheme immediately overrides.

   Recommendation: ship 1 + 2 (which make lualine work with a colorscheme, the
   realistic path), and treat 3 as a separate, explicitly-decided change.

### Files

- `crates/nxvim-lua/src/install.rs` — `nvim_set_hl` write-through.
- `crates/nxvim-lua/src/prelude/api.lua` — `nvim_get_hl_by_name` shim.
- `crates/nxvim-core/src/highlight.rs` — *only if* default groups (option 3b) are
  chosen.

### Test plan

Extend `plugin_compat.rs`:

- `lualine_loads`: load a real colorscheme (e.g. tokyonight, present in the pack)
  **then** `require('lualine').setup{}` in the *same* chunk → `"OK"`, proving the
  write-through makes `Normal` visible same-turn.
- A focused surface test: `nvim_set_hl(0, 'X', { fg = '#112233', bold = true })`
  immediately followed by `nvim_get_hl(0, { name = 'X' })` returns
  `fg == 0x112233` and `bold == true` in the *same* chunk (the read-after-write
  guarantee), plus the legacy `nvim_get_hl_by_name('X', true)` shape.

### Risk

Low–medium. The write-through is localized and matches an established pattern;
the only real care is format parity between the same-turn write-through and the
between-turn mirror push (cover both with the surface test above). Option 3b, if
taken, has broader blast radius (every highlight consumer + the redraw fallback
look) and should be gated behind the existing highlight/redraw tests.
