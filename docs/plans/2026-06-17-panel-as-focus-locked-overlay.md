# Bring back the bottom panel — as a focus-locked overlay over an ordinary buffer

Status: **DONE** — 2026-06-17 (workspace green: default + `--no-default-features`, clippy
`-D warnings` + fmt clean). The follow-up that **reverses the scratch-listing half** of
[`2026-06-17-panel-as-loclist-and-scratch.md`](2026-06-17-panel-as-loclist-and-scratch.md):
that change dissolved the bottom panel two ways — real location lists → loclists (kept,
the right call), everything else → permanent read-only bottom-split windows (reverted
here). A permanent, leavable split lost what made the panel one of the editor's most
useful systems: the *transient, input-grabbing overlay* feel. This brings the panel back —
but **unified**, so none of the bespoke apparatus the retire commit deleted returns.

## What a panel is now

A panel is an ordinary `nomodifiable` buffer shown in a bottom split (vim's `botright`,
via the existing [`Editor::open_bottom_window`]), with exactly two properties layered on:

- **Displace** — opening shrinks the main window into the rows above; closing collapses
  the split and restores the layout (reuses [`Editor::remove_window`]).
- **Hard focus lock** — while a panel is open, focus is pinned to its window. Every focus
  change funnels through the private [`Editor::focus_window`], so a single guard there
  makes `<C-w>w`/`W` (cycle), `<C-w>hjkl` (directional), `nvim_set_current_win`, and mouse
  focus all inert. Only an explicit close ([`Editor::close_panel`]) dismisses it.

Everything *inside* the panel is plain buffer behavior — motions navigate, search works —
and every activation key is an ordinary **buffer-local keymap** installed by a `FileType`
autocmd (the `:ls`/`qf`/`nxdir` ftplugin model), never special-cased in the input loop:
`q`/`<Esc>` dismiss (the shared `nxlisting`/`nxbuffers`/`nxpanel` ftplugin), `<CR>` switches
buffers for `:ls`.

## Why it is NOT a widget

The picker/select widgets route keys through a widget keymap *bucket* that **bypasses the
per-buffer trie** — so buffer-local maps could never fire inside one. Buffer-local
shortcuts are the whole point, so the panel stays in `KeyContext::Editing` over a real
buffer; the "grab" is a *focus lock*, not key interception. Consequences: **no
`KeyContext::Panel`, no `'L'` keymap bucket, no `PanelView`/`project_panel`, no redraw
change** — the panel renders through the ordinary window path. None of the retired panel's
bespoke navigation/content/select API (`set_panel_targets`, `vim.panel.on_select`,
`apply_panel_action`, `PANEL_ON_SELECT`, `nx.panel.actions.*`) comes back.

## As built

- **Core** (`crates/nxvim-core/src/editor/`): new slim `panel.rs` —
  `PanelState { window, prev_window }`, `open_panel` (mount/reuse), `open_script_panel`
  (the `nx.panel.open` entry, over a third singleton `script_panel_bufnr`), `close_panel`,
  `panel_is_open`, `panel_window`. `Editor.panel: Option<PanelState>` in `mod.rs`. The
  focus-lock guard in `windows.rs::focus_window`; close routing in `windows.rs::close_window`
  so `:q`/`:close`/`<C-w>c`/`<C-w>q` on the panel dismiss it (restoring layout + focus)
  rather than leaving `Editor.panel` dangling. `buffers.rs::show_listing` now mounts via
  `open_panel` (so every `open_scratch_listing` / `open_buffer_listing` call site —
  `:messages`/`:registers`/`:marks`/`:jumps`/`:changes`/`:LspInfo`/`:command`/`:TSInstall`/
  diagnostics-text + `:ls`) becomes a panel; generic listings gained `filetype=nxlisting`.
- **Lua surface** (`nx.panel`): `open{ lines, filetype?, height? }` and `close()` — that is
  the whole API. `PanelOp::{Open,Close}` (ops.rs) drained in `effects.rs` into
  `open_script_panel` / `close_panel`; the `nxvim_panel_is_open` RPC (dispatch.rs) backs
  chrome + tests. Behavior attaches via a `FileType` autocmd on the chosen filetype
  (default `nxpanel`) — `nx.panel.open` deliberately does **not** return a bufnr (a fresh
  buffer's id is next-tick, and the FileType route is the unified mechanism).
- **ftplugins** (`prelude/keymap.lua`): one `FileType` autocmd over the pattern list
  `{ nxlisting, nxbuffers, nxpanel }` installs the buffer-local default `q`/`<Esc>` →
  `nx.panel.close`. `:ls`'s `<CR>` closes the panel then `nx.schedule`s the
  `:buffer <n>` switch (closing is a queued panel op; the switch must land in the restored
  main window, not the about-to-be-removed panel window — commands drain *before* panel ops).
- **Tests**: `tests/editing/listings.rs` rewritten to assert panel behavior via the
  `panel_is_open` harness oracle — open, nomodifiable, plain-motion navigation, the hard
  focus lock (`<C-w>w`/`<C-w>j` can't leave), `q`/`<Esc>` dismiss + focus restore, and a
  scripted `nx.panel.open` with a buffer-local `<CR>`. `:ls <CR>` coverage in `buffers.rs`
  still passes (now also closes the panel). Example: `examples/panel/`.

## Incidental fix

`crates/nxvim-server/src/lsp/edit.rs` `show_code_actions` set the
`#[cfg(feature = "native")]` field `pending_code_action` **without gating the assignment**,
breaking the `--no-default-features` (wasm edit-host) build. This was pre-existing — the
untested code-action path the retire-panel commit introduced. Gated the assignment to match
its `effects.rs` consumer; both build configs are green again.

## Follow-up: named panels + hidden from `:ls` (same day)

Two snags surfaced once it was in use, both fixed:

1. **Panels are not documents.** The display buffers were showing in `:ls` and were
   reachable by buffer navigation. Replaced the three singleton display buffers
   (`scratch_bufnr`/`bufferlist_bufnr`/`script_panel_bufnr`) with a **named-panel registry**
   `Editor.panel_buffers: Vec<(String, BufferId)>` — one reused buffer per distinct name.
   `buffers_in_layer` (the chokepoint for `:ls` / `:bnext` / `:bprev` / `:bfirst` / `:blast`
   / `nx.buf.list`) now filters out registry buffers via `is_panel_buffer`. `:lspanels` /
   `:panels` lists the registry as its own `[Panels]` panel.
2. **Panels always open as panels.** `switch_buffer` now reroutes any attempt to show a
   panel buffer from *outside* the panel window into `open_panel` (so `:b [Messages]` opens
   it as a panel, never as a main buffer); the in-panel-window swap is allowed through,
   which backs both `open_panel`'s own reuse and the `:lspanels` `<CR>` navigation.

**Named ⇒ unique.** Each listing now has its own buffer (`[Messages]`, `[Registers]`,
`[Marks]`, …) instead of sharing one scratch buffer, so they no longer clobber each other.
Re-running a command (`:messages`) replaces that named panel's content in place
(`open_named_panel` is the single home all listings + `nx.panel.open` flow through);
navigating to it via `:lspanels` `<CR>` (which is just `:b <n>`, swapping inside the panel
window) shows its **last content** with no regenerating command run. `nx.panel.open` gained
a `name?` field (default `[Panel]`). New ftplugin: `FileType nxpanels` maps `<CR>` →
`:b <n>`; `nxpanels` joined the `q`/`<Esc>` dismiss pattern list.

## Out of scope / notes

- **Word-wrap** stays dropped (a buffer-wide feature; the retire-panel plan's reasoning holds).
- **No selection highlight** (the old panel's reverse-video cursor row): the panel does
  not enable `cursorline`, so the cursor renders normally. (`cursorline` is a separate,
  window-local option — since landed — that a panel buffer could opt into via its
  `FileType` autocmd if a highlighted cursor row is ever wanted.)
- **`nx.view`** (the *persistent* dockable surface) is untouched and complementary — it is
  for docked plugin content; `nx.panel` is the *transient grabbing* surface.
- **Scripted-panel filetype churn**: `nx.panel.open` reuses one `script_panel_bufnr`
  singleton; opening with *different* filetypes across calls can leave a prior filetype's
  buffer-local maps on the buffer. Fine for the common one-filetype-per-plugin case; a
  caveat, not a blocker.
- **Client dead code**: tui/gui/web still carry the inert `panel` redraw-key readers from
  the retire commit. We render panels through the ordinary window path, so they stay dead;
  a separate cleanup.
