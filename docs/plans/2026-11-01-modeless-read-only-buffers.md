# Modeless everywhere: read-only buffers and plugin surfaces

*2026-11-01*

Status: **done**. All three phases landed on branch `modeless-everywhere`; see
[Outcome](#outcome). Follows [modeless editing](2026-10-22-modeless-editing.md)
and the [modeless context menu](2026-10-25-modeless-context-menu.md).

## The gap

A modeless session (`:set modeless`, GUI and web) is a GUI editor only while the
focused buffer takes edits. Focus a `btv.view` (a tree, a plugin sidebar), the
quickfix list, help, a panel, the explorer or any `nomodifiable` buffer and the
session drops into vim Normal mode. The layout chords are off there (`<C-c>`
does nothing, `<C-w>` waits for a window command, `<C-p>` moves up a line),
Shift-arrows are vim motions, and the mouse builds a Visual selection. A live
terminal rests in terminal-normal mode until something types `i`.

## Decision (settled with the user)

**No modal mode anywhere while `'modeless'` is on.** Every buffer rests the way an
editable one does. A read-only buffer gets the same caret, Shift-selection,
mouse selection and layout chords; what it refuses is the edit. Plugins and
bundled Lua surfaces implement modeless explicitly: a key a view needs in a
modeless session (its `<CR>`, `q`, `<Esc>`, a double-click) is bound in the
`modeless` keymap layer, buffer-local. A vim-mode Normal map does not reach a
modeless session, and there is no fallback that guesses one onto it.

## Design

### The resting mode

`Editor::rests_in_insert()` stops depending on `modifiable()`: with `'modeless'`
on, every window rests in Insert, except a live terminal, which rests in
Terminal mode (its keys go to the process). `settle_modeless` brings a clean
Normal over a live terminal back into terminal-job mode, as it brings any other
clean Normal into Insert.

### Edits refused in one place

Over a read-only buffer (`!modifiable()`), Insert and Select keep only the keys
that don't edit: the caret and selection keys `modeless_nav` handles (arrows,
Home/End, Page keys, with their Shift / Ctrl / Alt / Command forms) and `<Esc>`.
Every other key that reaches the core there is refused with
`E21: Cannot make changes, 'modifiable' is off` and changes nothing. One guard at
the key dispatch (`Editor::input`, ahead of `handle_insert` / `handle_select`)
covers typing, `<CR>`, `<BS>`, `<Tab>`, the Insert-mode Ctrl commands and Select's
type-over. Mapped keys never reach it: the server matches the `modeless` layer
and the buffer's maps first. A paste is already refused (`paste_literal`).

### The verbs

`apply_modeless_action` runs every verb from Insert or Select in any buffer. Over
a read-only one, the verbs that edit (`cut`, `paste`, `undo`, `redo`,
`delete_word_back` / `_forward`, `indent`, `outdent`, `duplicate_line`,
`move_line_up` / `_down`) refuse with `E21`. `copy`, `select_all`,
`search_next` / `search_prev` and `reveal_caret` work as in an editable buffer, on
the caret selection. The Visual-based read-only `copy` / `select_all` paths go:
no read-only buffer rests in Normal any more.

### What follows for free

The layout chords (`modeless_keys_active` follows `rests_in_insert`), the caret,
the I-beam pointer, mouse selection into Select, the wheel leaving the caret put,
`/` selecting its match, and the context menu's key hints.

### Bundled surfaces

Each first-party read-only surface whose keys are Normal maps or native Normal
keys gets explicit modeless bindings, buffer-local in the `modeless` layer:
`btv.view` (`<CR>` / double-click confirm), the quickfix and location lists
(`<CR>` jumps), panels and listings (`q` / `<Esc>` dismiss, `<CR>`), help (tag
jump), the explorer (`<CR>`, `-`). The exact list comes out of an audit of the
prelude's buffer-local Normal maps and the core's native Normal-only keys on
read-only kinds.

## Phases

Each lands as its own commit and pauses for review.

1. **Core.** Read-only buffers rest in Insert, live terminals in Terminal; the
   `E21` key guard; the verb policy; the context menu's hints in read-only
   buffers. Tests in `crates/bemtvi-server/tests/modeless.rs`, replacing the ones
   that asserted Normal.
2. **Bundled surfaces.** The audit and the modeless bindings for `btv.view`,
   quickfix / loclist, panels, help and the explorer, each tested through the
   real keys. *Done.* The audit found nine bundled surfaces, all bound in `"n"`:
   the explorer (`<CR>`, `-`, double-click), quickfix `<CR>`, `:ls` and
   `:lspanels` `<CR>`, `:messages` `C`, the panel dismiss keys (`q`, `<Esc>`),
   `btv.view` (`<CR>`, double-click) and the `:Plugins` screens
   (`plugins_ui.lua`). Each now binds `{ "n", "modeless" }`. A printable key in
   the modeless layer doesn't collide with typing, since a read-only buffer
   refuses typing anyway. Help's tag jump is the external bemtvi-help plugin's,
   so it moves to the external list.
3. **Docs and example.** The plugin-author contract (the bemtvi skills, the
   `btv.view` and keymap docs: bind your keys in the `modeless` layer), a
   read-only section in `examples/modeless/` with its spec, and an Outcome
   section here. The external plugins in `~/work/bemtvi-plugins` (bemtvi-tree
   first) are updated in their own repos after this lands. *Done*, except the
   skills, which wait for the merge (see Outcome).

## Outcome

Where the build departed from the sketch, and what is left.

- **The contract is `{ "n", "modeless" }`.** No new API: `btv.keymap.set` already
  took a mode list and the `modeless` layer already took buffer-local maps, which
  sort after the layout's global defaults, so a view's own chord wins. The docs
  (`docs/features/modeless.md` → Plugins, the `btv.modeless` and component
  docstrings, `docs/architecture.md`) teach the pair, and
  `examples/modeless/` note 11 builds an `:Outline` dock view that uses it.
- **The pointer is the I-beam over read-only text too.** It follows the resting
  mode, not modifiability: a read-only buffer is selected with the mouse the same
  way. Only a terminal keeps the arrow.
- **Every refused key echoes `E21`.** In a tree, an unbound letter says
  `E21: Cannot make changes, 'modifiable' is off`, the message the editor gives
  for every other refused edit on these buffers. It is loud by design (no silent
  no-ops); if it proves noisy in practice, the place to change it is
  `Editor::refuses_modeless_key`.
- **A view's buffer can't be nil any more.** The example first bound its `q` from
  `btv.schedule`, where `view:bufnr()` was still nil, and `{ buffer = nil }` is the
  same table as `{}`: the map went in globally without a word. Now `view:bufnr()`
  reads `false` until the buffer exists, the buffer-taking APIs refuse `false`
  loudly (`btv._refuse_pending_buffer`: keymaps, autocmds, buffer-local commands,
  the `btv._resolve_bufnr` readers, `btv.buf.name`, diagnostics, LSP), and
  `btv.keymap.set` takes the view itself (`{ buffer = view }`) and binds once the
  buffer exists. `buffer = false` no longer means "global" in `btv.keymap.set`;
  nothing in the tree or the plugins used it.

Still to do, outside this repo, once the branch is on `main`:

- **bemtvi-tree**: done in its repo as `0dae8e0` (committed, not pushed; it needs
  this branch). Its global `toggle_key` stays Normal-only, since a `modeless`
  map on `<leader>e` would hold every typed space.
- **bemtvi-help** (`1e2b693` + docs `1ae86a8`), **bemtvi-dap** (`3ff513c`),
  **bemtvi-diff** (`944338d`) and **bemtvi-workspaces** (`f1c0675`): done in their
  repos, committed, not pushed. A global key joins the `modeless` layer only when it
  types nothing (F-keys, chords); a `<leader>` or letter key stays Normal-only.
  The trust dialog in workspaces was the sharpest case: a grabbing float nothing
  could answer in a modeless session.
- **LSP verbs: VS Code's keys.** `gd` / `gr` / `K` / `]d` are Normal-mode letters a
  modeless session can't reach, so the layout gives the language verbs VS Code's keys
  (F12, Ctrl+F12, Shift+F12, Ctrl+K Ctrl+I, Ctrl+Shift+Space, F2, Ctrl+., Shift+Alt+F,
  Ctrl+Shift+O, Ctrl+T, F8 / Shift+F8) as named actions, and the context menu's LSP
  rows show them. bemtvi-dap moved its modeless keys to VS Code's debug set
  (`modeless_mappings`), so its F12 step-out no longer shadows go to definition. The
  web page now forwards the Mac Command forms and carries Shift on a modified Space.
- **bemtvi-plugin-skills**: `bemtvi-view-docks`, `bemtvi-ui` and
  `bemtvi-keymaps-commands-events` teach the pair, with a `verify/` spec, after
  `scripts/sync-reference.sh` picks up the new docstrings.
