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
  - **Decoration-provider highlighting** (`vim.treesitter.start` / the
    highlighter): nxvim highlights from the Rust engine, not a Lua-owned tree.
    `vim.treesitter.highlighter` is a small shim — legacy-API probes (`hl_map`)
    read nil and `active` is empty, but `highlighter.new` fails loud
    (`vim._notimpl`), so `vim.treesitter.start` raises rather than faking it.
  - **Injections.** A buffer's root tree parses; `LanguageTree` child languages /
    `language_for_range` are not wired, so an injected-language query returns only
    the host tree's captures.
  - **Live incremental buffer updates.** There is no `nvim_buf_attach`; a
    buffer-sourced parser re-reads the snapshot and **fully reparses** on each
    `:parse()` (correct, but pays the full cost — the "two parsers" tradeoff).
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
  options are still missing. Also: marks, folds, macros, registers beyond the
  unnamed register, and `:s` substitution (the interactive `/` / `?` cursor
  search is implemented — see the search design — but `:s` has no ex-command yet).
- **Legacy Vimscript (`eval.c`).** Deliberately **not** on the roadmap (guiding
  principle 2). `vim.fn.*` is a hand-written compatibility shim, not an
  interpreter — unimplemented `vim.fn.*` entries are loud gaps, not a TODO to
  build an evaluator.
- **`:TSInstall`-style grammar fetch/compile.** Grammars load from the data dir;
  installing them there is manual.
- **Synchronous prompts.** `vim.fn.input` / `vim.fn.confirm` must return the
  user's answer *inline*; nxvim's prompt surface is async-callback only, so both
  are loud gaps until a re-entrant input pump (nested loop or resumable coroutine)
  exists.

## Cross-cutting root causes

Many individual approximations share one root cause — fix the root and a batch
clears at once. (Run the `grep` above for the current, exact call-site list.)

| Root cause | Approximations it clears |
|---|---|
| Single-window model | `nvim_win_get_cursor(win)`, `make_position_params(window)`, `open_floating_preview` handles, per-window placement, the single completion-doc preview box (no separate preview-window handle / `completeopt` matrix) |
| No multi-buffer name/disk registry | `make_text_document_params` (non-current bufnr → empty URI), `locations_to_items` & `apply_workspace_edit` for unopened files |
| Core honors only the indentation buffer-local options | `vim.bo` / `nvim_set_option_value` writes other than `filetype` / `tabstop` / `shiftwidth` / `expandtab` are recorded but inert |
| No per-buffer command registry | `nvim_buf_create_user_command` registers globally |
| No virtual-text / signs / float surfaces | `vim.diagnostic.config` keys other than `underline` |
| No per-namespace highlight tables | `nvim_set_hl` non-zero namespace folded into global |

## Relationship to the LSP completion plan

[`docs/plans/2026-06-05-lsp-completion.md`](plans/2026-06-05-lsp-completion.md) is the **phased route**
that drove the LSP surface from "every hollow stub raises" (Phase 0) to today;
each phase notes the approximations it deliberately left behind. That document is
*history + plan*, not a live registry. The live registry is the `INCOMPLETE:` /
`vim._notimpl` tags in code (and this file's missing-features list above). If the
two ever disagree, the code wins.
