# Global-local options — a buffer/window option's *global value*

Status: **done** — Phases 1, 2, 3 and 4 shipped. Author-date: 2026-08-01.

Phase 1 (`cb3ac0cc`) put the buffer tier in the core and split `:set` / `:setlocal` /
`:setglobal`. Phase 2 (`994e4944`) put the Lua surface on top: `vim.opt` / `vim.o` write
both tiers, `vim.bo` / `vim.opt_local` only the buffer, `vim.go` / `vim.opt_global` only
the tier (`nx.opt_local` and `nx.opt_global` were aliases of `nx.opt` before). Phase 3
finished the model: window options got the same two tiers, and the three map-backed buffer
nouns (`commentstring`, `foldexpr`, `foldmarker`) got a read-time global fallback.

Two deviations from the plan as written below. `:setglobal` of an option with no global
value **fails loud** (`E5100`) rather than silently writing the local one — after Phase 3
that is only the read-derived slots plus `filetype` / `ts_highlight`. And the map-backed
nouns resolve their tier as a **fallback at read time** rather than a seed at creation,
because their `HashMap<BufferId, _>` storage already encodes "unset" as absence — which
also means a late `:setglobal` reaches buffers that are already open.

## The bug this closes

```lua
-- init.lua
vim.opt.tabstop = 3
vim.opt.expandtab = true
```

```
:e other.py     " tabstop=4, noexpandtab — the config never reached this buffer
```

A config's buffer-local `vim.opt` settings apply **only to whatever buffer was current
while `init.lua` ran** (the startup `[No Name]`). Every file opened afterwards is born at
`BufferOptions::default()`. Measured, not theorized — `session.rs` scratch run,
config `tabstop=3 expandtab`:

```
boot:                    buf ts=3 et=true
after :vsplit other.txt: buf ts=3 et=true | buf ts=4 et=false   <-- the new buffer
```

The same gap made a restored workspace lose its window options
(`98872c65`); windows only *looked* healthy because `Editor::split` copies the current
window's options ("as vim does", `windows.rs:2210`), so config settings propagate to later
windows by inheritance. Buffers have no such copy step, so nothing propagates.

## Why it doesn't already work

There is no tier to fall back to. nxvim's option model is three structs of **concrete**
values with no "unset" state:

| scope | storage | written by |
| --- | --- | --- |
| global | `Editor::global_base` → merged into `Editor::options` (with the `nx.wso` overlay on top) | `set_global_option_*` |
| window | `Window::options: WindowOptions`, one per window | `windows.cur_mut().options.*` |
| buffer | `Buffer::options: BufferOptions`, one per buffer | `buffer_mut().options.*` |

`:set tabstop=3` has exactly one place to write — the current buffer — and
`Buffer::empty()` / `Buffer::from_file` always start from `BufferOptions::default()`. So:

- **`:setglobal` is not a command.** `ex.rs:1156` routes `"set" | "se" | "setlocal" |
  "setl"` to `ex_set`; `:setglobal` is an unknown ex command.
- **`:setlocal` is `:set`.** `options.rs:22` — *"`:setlocal`, which is identical here"*.
- **`vim.go.tabstop` is inert.** `go_set` (`prelude/state.lua`) only reaches the core for
  names the catalog scopes `Global`; a buffer/window name falls into the `nx._o_store`
  catch-all — readable back, never honored.
- **`vim.opt_local` / `vim.opt_global` are aliases of `vim.opt`** — *"the forced-scope
  distinction neovim draws is collapsed"*.
- `vim.g.tabstop` is not a thing at all: `vim.g` is the `g:` **variable** namespace.

## The model (vim's, not a new one)

Every buffer-local and window-local option gains a **global value** alongside its per-
instance local value:

- `:set {opt}={v}` — writes the global value **and** the current buffer's/window's local.
- `:setlocal {opt}={v}` — writes only the current instance's local.
- `:setglobal {opt}={v}` — writes only the global value.
- a **new buffer** is born from the global values;
- a **new window** copies the current window's locals (vim, and what nxvim already does);
  the global value is what `:setglobal` reads/writes and what seeds a window with no
  source to copy from.
- reads: `:set {opt}?` / `vim.o` / `vim.bo` / `vim.wo` report the **local** value;
  `:setglobal {opt}?` / `vim.go` / `vim.opt_global` report the **global** one.

Deliberately **not** the alternative design (each local field becomes `Option<T>`, unset ⇒
resolve through the global at read time). That one retro-applies a late `:set` to buffers
already open, but it is not vim's semantics and it touches every read site of every
option. The tier model touches the *write* sites and one creation funnel.

## Phase 1 — the core buffer tier + `:setlocal` / `:setglobal` (shipped)

Scope: the 13 `BufferOptions` **struct slots** that a user sets and a new buffer should
inherit.

**Inherited** (the global tier seeds them at buffer creation): `tabstop`, `shiftwidth`,
`softtabstop`, `expandtab`, `autoindent`, `smartindent`, `autopairs`, `indentemptylines`,
`regexsyntax`, `fixendofline`, `foldmethod`, `foldnestmax`, `foldminlines`.

**Buffer-born** (the *read* or the buffer's identity decides them; a global value would
clobber a fact about the file): `fileencoding`, `bomb`, `fileformat`, `endofline` — all
detected from the bytes — and `modifiable`, which the read-only scratch listings set at
creation.

The classification lives in one function that **destructures the whole struct**, so adding
a `BufferOptions` field fails to compile until it is classified.

1. `Editor::buf_opts_global: BufferOptions` — the global values, `Default` at startup.
2. `BufferOptions::inherit_global(&mut self, global: &BufferOptions)`, called from
   `Editor::add_buffer` — the sole `buffers.insert` funnel in the crate.
3. A `SetScope { Local, Global, Both }` threaded from `ex_set` into
   `apply_set_{bool,num,str}`: `:set` → `Both`, `:setlocal` → `Local`, `:setglobal` →
   `Global`. Global-scope options ignore it (they have one value); window-scope options
   take `Local` for all three in this phase (Phase 3 gives them their tier).
4. `:setglobal` / `:setg` registered in `ex.rs`, with `:setglobal {opt}?` reading the tier.
5. The map-backed per-buffer nouns (`filetype`, `commentstring`, `ts_highlight`,
   `foldexpr`, `foldmarker`) have no tier in this phase: `:setglobal` on one **fails loud**
   (`E5xx: {opt} has no global value`), never a silent store. Phase 3 gives the three that
   want one (`foldexpr`, `foldmarker`, `commentstring`) a real fallback.

Tests (`crates/nxvim-server/tests/options.rs`): a new file opened after `:set tabstop=3`
carries it; `:setlocal` doesn't leak to the next buffer; `:setglobal` doesn't touch the
current one but does reach the next; `:set {opt}?` vs `:setglobal {opt}?` disagree after a
`:setlocal`; and a `:setglobal` twin of the catalog-driven
`every_known_option_is_wired_not_silent` guard.

## Phase 2 — the Lua surface (shipped)

- `nx.o` / `vim.o`: a buffer-scoped name currently forwards to `vim.bo` (local only). Route
  it to a **both-tiers** setter instead, matching `:set`.
- `vim.bo` / `nx.opt_local`: unchanged, local only — un-alias `nx.opt_local` from `nx.opt`.
- `vim.go` / `nx.opt_global`: reach the tier for buffer-scoped names instead of dropping
  into `nx._o_store`.
- A `nx._bo_global` mirror pushed beside `BoMirror` so a `vim.go` read is honest.
- Book/prelude docstrings for the three surfaces (markdown rules in CLAUDE.md).

## Phase 3 — windows, and the map-backed buffer nouns (shipped)

- `Editor::win_opts_global: WindowOptions` for `:setglobal` / `vim.go` on window names;
  seeds windows created with a bare `WindowOptions::default()` today (`quickfix.rs:787`).
  New splits keep copying the current window (vim).
- Global fallback for `foldexpr` / `foldmarker` / `commentstring` — cheap, because their
  `HashMap<BufferId, _>` storage *already* encodes "unset" as absence, so the tier is a
  read-side fallback rather than a seed. Closes the `vim.opt.foldmethod = "expr"` +
  `vim.opt.foldexpr = …` config pattern, which needs both halves to inherit.

## Phase 4 — the Lua routing tables, derived (shipped)

Phase 3 gave the *core* every tier the pattern above needs, but the `vim.opt` /
`vim.o` spelling a config actually writes still didn't reach them: the prelude's
scope-routing tables (`O_WIN` / `O_BUF` in `prelude/state.lua`) were hand-kept name
lists that had drifted from the catalog, so `vim.opt.foldmethod = "marker"` — and
`foldexpr`, `foldmarker`, `commentstring`, `indentemptylines`, `foldnestmax`,
`foldminlines`, `foldcolumn`, `foldenable`, `foldlevel`, `breakindent`, `showbreak`,
`breakindentopt`, `sidescroll`, `sidescrolloff`, `padding` — fell into the unmodeled
`nx._o_store` and silently did nothing while `:set foldmethod=marker` worked.

Every table that routes an option name is now **derived from core's option catalog**
(`nx._set_options_catalog`, fed by `options_catalog()` before any config runs), the
same list `:set` resolves against, so the Lua and ex surfaces can no longer disagree:

- `O_WIN` / `O_BUF` come from the catalog's `scope` column; `WO_GLOBAL_TIER` /
  `BO_GLOBAL_TIER` from a new `global_tier` column, filled by
  `options::has_global_tier` — one home for "does this option have a global value",
  shared with the ex path's `E5100` rejection.
- `'scrollanim'` is a **global** option with a per-window override, so it has no
  window tier; aliasing `WO_GLOBAL_TIER = WIN_OPT_CANON` had made `vim.go.scrollanim`
  read a tier nothing populates and always answer `true`.
- `'regexsyntax'` is the one genuinely global-local option here: its global value is
  the editor-wide `Options::regexsyntax` every `Inherit` buffer already resolves
  through, so `:set`/`:setglobal` write **that** rather than a second `buf_opts_global`
  slot nothing resolves against.
- The core gained the write half the newly-routed window options were missing
  (`breakindent`, `showbreak`, `breakindentopt`, `sidescroll`, `sidescrolloff` in
  `set_window_option_*`), a catalog row for `'winhighlight'` (honored by the core,
  absent from the catalog, so `:set winhl=` was `E518`), and the mirror fields
  `vim.wo` / `vim.bo` needed to read any of them back honestly.
- `:setglobal` reached `nx.cmdline_complete` (neither the command name nor its
  option-name argument completed).

Guarded by `every_scoped_option_is_routed_by_vim_opt` — the Lua twin of
`every_known_option_is_wired_not_silent`, walking the same catalog.
