# Retire the bottom panel: loclists + read-only scratch buffers

> **Superseded in part (2026-06-17):** the scratch-listing half of this plan was reverted by
> [`2026-06-17-panel-as-focus-locked-overlay.md`](2026-06-17-panel-as-focus-locked-overlay.md),
> which brings the panel back as a *focus-locked transient overlay over an ordinary buffer*
> (no bespoke API). The **loclist** half below stands. The text listings (`:messages` /
> `:registers` / `:ls` / `:marks` / …) are now panels again, not permanent bottom-split windows.

Status: **DONE** — 2026-06-17 (all 5 phases landed; workspace green default +
`--no-default-features`, clippy + fmt clean). The final step of the special-buffer unification
(`2026-06-16-unify-special-buffer-kinds.md`): the bottom **panel** — the last grabbing,
bespoke-rendered "non-ordinary buffer" mechanism — is **deleted entirely**. No `panel`
concept survives. Every listing it served moves to one of the two general mechanisms
nxvim already has:

- **Location-bearing lists** (`:marks`, `:jumps`, `:changes`, LSP references /
  definitions / diagnostics-with-targets) → real **location lists** (`filetype=qf`,
  the existing `qf_set_items` + `:lopen` machinery). `<CR>` jumps via the buffer-local
  qf map already installed by the `FileType qf` autocmd.
- **Pure-text listings** (`:messages`, `:registers`, `:LspInfo`, `:command`,
  `:TSInstall` info, plain diagnostics text, …) → **read-only scratch buffers** in a
  bottom window (a new `modifiable` buffer option, set false; vim's `nomodifiable`).

Two listings are *select-with-callback*, not jumps, so they get explicit homes:

- **`:ls`** (switch to the picked buffer) → a read-only scratch buffer whose
  buffer-local `<CR>` parses the buffer number off the cursor line and switches to it.
- **LSP code actions** (apply the picked action) → **`nx.ui.select`** (the select
  menu) — exactly where neovim puts them (`vim.ui.select`), retiring the
  `CODE_ACTION_PANEL_TITLE` routing hack.

The public **`vim.panel.*` Lua API and `nxvim_panel_*` RPC are retired** (breaking;
they were nxvim-only and never a neovim concept). Scripts that want a custom bottom-dock
list use `nx.view`, the documented generalization.

## Why

The panel was the last widget keeping a `KeyContext` variant, a keymap bucket (`'L'`),
a bespoke `PanelView` chrome projection, and an all-keys input grab. Per the
[panel analysis](2026-06-16-unify-special-buffer-kinds.md#later--separate--the-bottom-panel)
it has neither property that *justifies* a widget: it has no prompt (doesn't reinterpret
keys) and — once it lives in a window — it is a buffer-in-a-window, not a transient
overlay. So it converges. Word-wrap (the one feature `PanelView` had that an ordinary
buffer lacks) is **explicitly dropped here** — it's a buffer-wide gap that will be
implemented once, for all buffers, and then applies to these listings for free.

## The read-only-scratch mechanism: a real `modifiable` option

Today `modifiable()` is `!buffer().read_only() && !is_quickfix_buffer()` — purely
kind/registry-driven; there is **no** `modifiable` buffer option. The text listings need
to be read-only without being a `BufferKind`. Add the honest vim mechanism:

- `BufferOptions.modifiable: bool` (default `true`).
- `modifiable()` gains `&& self.buffer().options.modifiable`.
- `:setlocal [no]modifiable` / `:set [no]ma` and `nx.bo.modifiable` set it.

This generalizes beyond the listings (any plugin/buffer can be `nomodifiable`) and is a
real latent gap, not panel-specific scaffolding.

## Phases (each keeps the workspace green; default + `--no-default-features`)

### Phase 1 — `modifiable` option + the text listings
- Add the `modifiable` option (above) and an `Editor::open_scratch_listing(name, lines)`
  helper: empty buffer → set lines → `modifiable=false` → tag a filetype (so the window
  statusline names it) → `open_bottom_window`.
- Convert the pure-text sites: `:messages`, `:registers` (`buffers.rs`); `:LspInfo`,
  `:command`, `:TSInstall` (`excmd.rs`); plain "Diagnostics"/"LSP diagnostics" text
  (`diagnostics.rs`, `sync.rs`, `excmd.rs`) where they carried no targets.
- No grab, no `<CR>`; navigation is ordinary motion on a `nomodifiable` buffer.

### Phase 2 — location lists
- Route `:marks` / `:jumps` / `:changes` (`marks.rs`, `jumps.rs`, `changelist.rs`)
  through `qf_set_items` into a **location list**, then `:lopen`. Drop the
  `(path,line,col)` target plumbing — the qf entry already carries it.
- LSP locations (`request.rs` `apply_lsp_locations`) and diagnostics-with-targets
  (`request.rs`, `sync.rs`, `excmd.rs`) build a loclist instead of a target-panel.
- Delete `set_panel_targets`.

### Phase 3 — the two select-with-callback listings
- `:ls` (`ex_buffers`): scratch listing + a `FileType` autocmd installing a buffer-local
  `<CR>` → a prelude helper that parses the leading bufnr off the cursor line and
  `:b <n>`. Replaces the `vim.panel.on_select(nx._panel_select_buffer)` wiring.
- LSP code actions (`edit.rs`): present via `nx.ui.select`; apply the chosen action on
  confirm. Remove `CODE_ACTION_PANEL_TITLE` + its `effects.rs` routing.

### Phase 4 — delete the panel apparatus
- Core: `Panel` struct, `panel.rs` (whole file), `PanelView` (`view.rs`),
  `project_panel` (`redraw.rs`), `KeyContext::Panel` (`mode.rs`) + its `key_context`
  branch, `apply_panel_action`, `panel_selects`, `last_panel`, `:panelopen`
  (`ex.rs`), `panel_rows` + its removal from the layout/mouse chrome math
  (`windows.rs`, `mouse.rs`), the mouse panel-click path.
- Server: the `'L'` bucket in `widget_bucket`/`mode_buckets`/`mode_code` (`keymap.rs`),
  `nxvim_panel_*` dispatch (`dispatch.rs`), `take_panel_ops` drain (`input.rs`,
  `effects.rs`).
- Lua: `vim.panel.*` (`install.rs`), `PanelOp`/`panel_ops`/`take_panel_ops`
  (`ops.rs`, `runtime.rs`), `PANEL_ON_SELECT` / `store_panel_callback` /
  `nx._panel_select_buffer` / `nx._panel_action`.

### Phase 5 — tests + docs
- Re-home the panel tests: `:messages`/`:registers` assert a `nomodifiable` bottom
  buffer; `:marks`/`:jumps`/`:changes` assert a loclist (`:lopen` + `<CR>` jump);
  `:ls` asserts `<CR>` switches buffers; code actions assert the select menu.
- Mark this plan **done**; cross-link from the unify-special-buffer-kinds plan.

## As built (deviations from the sketch above)

- **`:marks` / `:jumps` / `:changes` → read-only scratch listings, not loclists.** The
  plan slotted them under "location lists", but the audit found their panels set **no**
  jump targets (`open_panel(..., false, 0)` with no `set_panel_targets`) — they were
  always *informational tables*, never navigable (vim prints them to the message area;
  you navigate with `` `a ``/`<C-o>`, not `<CR>`). So the faithful, behavior-preserving
  conversion is a scratch listing (Phase 1's `open_scratch_listing`), which also keeps
  their tabular format. Only the genuinely-navigable **LSP** lists (references,
  diagnostics-with-targets) became loclists, via the new
  [`Editor::open_location_list`](../../crates/nxvim-core/src/editor/quickfix.rs).
- **`:ls` rides its own `bufferlist_bufnr`** (filetype `nxbuffers`), separate from the
  shared `scratch_bufnr`, so its `<CR>`-switch buffer-local map can't bleed onto the
  plain text listings.
- **Code actions → `nx.ui.select`** via a `pending_code_action` flag routed in the
  `menu_results` drain (no existing test covers code actions — they need a mock LSP
  server — so this path is converted but, like before, untested).
- **Client panel-rendering code (tui/gui/web) was left in place**: it reads the now-absent
  `panel` redraw key and renders nothing. Harmless dead code; a follow-up cleanup.

## Out of scope
- Word-wrap (deliberately dropped; buffer-wide feature, separate effort).
- `nx.view` itself (unchanged; it's the scripting replacement for `vim.panel`).
- The terminal mode, floats, picker, select — they keep their mechanisms (see the
  [picker/floats analysis](2026-06-16-unify-special-buffer-kinds.md): a prompt or a
  non-window overlay genuinely justifies a widget; the panel had neither).
