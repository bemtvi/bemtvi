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
   (`CLAUDE.md`, and `docs/lsp-completion-plan.md` Phase 0).

The per-function list is **generated from the code, not maintained here**:

```sh
# Silent approximations — each with its "why" and the fix, inline:
grep -rn 'INCOMPLETE:' crates/

# Loud "not implemented" gaps — the call sites:
grep -rn 'vim\._notimpl(' crates/nxvim-lua/src/prelude.lua
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

- **Multiple windows / splits / tabs.** There is one window onto one buffer.
  Everything window-keyed collapses to the single handle `1000`. This is the root
  cause behind the window-related `INCOMPLETE:` tags (`nvim_win_get_cursor`,
  `make_position_params`, `open_floating_preview` handles).
- **Treesitter Lua API.** nxvim highlights in a separate process, so
  `vim.treesitter.*` (parsers, queries, `get_node`, language registration,
  injections) is a near-empty shell — only the version-probe shape exists
  (tagged). Anything reaching for a parser hits nil.
- **`vim.uv` / `vim.loop` beyond timers.** `new_pipe`, TCP (`new_tcp` — the
  TCP transport behind the skipped gdscript `vim.lsp.rpc.connect`), and
  event-based `fs_*` watchers are absent.
- **Broad options surface.** `:set` honors only `number` / `relativenumber`, and
  options are global. Buffer-local options are *recorded* by `vim.bo` but do not
  drive behavior (tagged). Also: marks, folds, macros, registers beyond the
  unnamed register, and most `:s` flags.
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
| Core doesn't honor buffer-local options | every `vim.bo` write but `filetype`, `nvim_set_option_value` |
| No per-buffer command registry | `nvim_buf_create_user_command` registers globally |
| No virtual-text / signs / float surfaces | `vim.diagnostic.config` keys other than `underline` |
| No per-namespace highlight tables | `nvim_set_hl` non-zero namespace folded into global |

## Relationship to the LSP completion plan

[`docs/lsp-completion-plan.md`](lsp-completion-plan.md) is the **phased route**
that drove the LSP surface from "every hollow stub raises" (Phase 0) to today;
each phase notes the approximations it deliberately left behind. That document is
*history + plan*, not a live registry. The live registry is the `INCOMPLETE:` /
`vim._notimpl` tags in code (and this file's missing-features list above). If the
two ever disagree, the code wins.
