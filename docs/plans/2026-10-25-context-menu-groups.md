# Context menu groups

Follows [the modeless context menu](2026-10-25-modeless-context-menu.md). That
work gave a right-click a menu, but only in modeless sessions and from one flat
list of rows. This turns it into a general, composable surface: `btv.contextmenu`.
It works in every session, modeless or vim, and it is all Lua.

## What it does

A right-click calls `btv.contextmenu.open()`, which reads the clicked buffer's
**group list**, an ordered list of names such as `{ "undo", "clipboard", "lsp",
"commands" }`. Each name refers to a registered **group**. The function asks every
group for its rows, drops the groups with none, and draws them in list order with
a separator between groups. A right-click does nothing else in bemtvi, so this is
the right-click in every buffer whose list isn't empty.

You and plugins control what shows:

- **Per buffer:** each buffer has its own group list. Reorder it, add a group,
  remove one. Changing one buffer's list never changes another's.
- **Defaults:** a buffer starts from the default list for its kind, editable or
  read-only. Editing a default changes what later buffers start with.
- **Groups:** a group is a table you can change. Add or remove its items, or hide
  the whole group.
- **New groups:** a plugin registers a group and puts its name into the lists of
  the buffers it owns. bemtvi-tree does this, so its own rows sit above the
  read-only defaults.

Everything is async: a group's items may come back as a promise (a group that
asks a language server, or reads the filesystem). The menu opens once every group
has answered, in list order, whatever order they finish in.

## The API

```lua
-- The group registry. Each group is a table:
btv.contextmenu.groups.clipboard = {
  -- The rows: a list, or a function `(ctx) -> list` that may return a promise.
  items = {
    { label = "Copy", action = function(ctx) … end, detail = "Ctrl+C",
      when = function(ctx) … end,      -- hide the row unless true
      enabled = function(ctx) … end }, -- grey it out unless true
    …
  },
  when = function(ctx) … end,          -- hide the whole group unless true (or `false`)
}

-- The default group lists, per buffer kind.
btv.contextmenu.default = { "undo", "clipboard", "lsp", "commands" }
btv.contextmenu.default_readonly = { "clipboard" }

-- A buffer's own list, live: created from the default for its kind the first time
-- it's asked for, then the buffer keeps it. Edit it in place.
local list = btv.contextmenu.list(buf)
table.insert(list, 1, "tree")

-- Replace a buffer's list wholesale.
btv.contextmenu.set(buf, { "tree", "clipboard" })

-- Open the menu for the current buffer at the pointer (what a right-click does).
-- A promise that settles once the chosen row's action has run, or the menu closed.
btv.contextmenu.open()

-- The rows a buffer's menu would show right now, as `btv.ui.menu` entries: the
-- testing and introspection seam. A promise, like `open`.
btv.contextmenu.entries(buf)
```

`ctx` is built once per right-click and passed to every `when`, `enabled`,
`detail`, `items` and `action`:

```lua
{
  buf = 3, win = 1000,
  mouse = btv.getmousepos(),  -- the click
  mode = "n",                 -- the mode when it was clicked
  selection = false,          -- Visual / Select active
  modifiable = true,          -- the buffer can be edited
  modeless = false,           -- a modeless session
}
```

A row's `detail` may be a string or `(ctx) -> string|nil`. An `action` may return
a promise; `open` settles after it. Fields a row or group doesn't know about are
an error, with the group's name, so a typo doesn't silently drop a row.

## Default groups

Each one works in both editing models. Its rows choose the verb by session:

| Group | Rows | Modeless | Vim |
| --- | --- | --- | --- |
| `undo` | Undo, Redo | `btv.modeless.actions.*` | `:undo` / `:redo` |
| `clipboard` | Cut, Copy, Paste, Select All | the modeless verbs | `"+d`, `"+y`, `"+P`, `ggVG` |
| `lsp` | Go to Definition, Find References, Rename Symbol, Format Document, Code Actions | `btv.lsp.*` | `btv.lsp.*` |
| `commands` | Command Palette | the palette picker | the palette picker |

- `undo` and the editing rows of `clipboard` show only in a modifiable buffer.
- Copy needs a selection in a read-only buffer, and so does Cut in a vim session;
  modeless Copy and Cut take the whole line when nothing is selected.
- `lsp` shows while a server is attached, and greys out what no server answers.
- Key hints come from the `modeless` keymap in a modeless editable buffer, and are
  absent otherwise. Vim's keys are verbs plus motions, not one chord, so they
  aren't shown as hints.

The default lists are `{ "undo", "clipboard", "lsp", "commands" }` for an editable
buffer and `{ "clipboard" }` for a read-only one. That keeps what the previous
step settled on: a read-only buffer gets Select All, plus Copy over a selection.

## What changes from the previous step

- `btv.modeless.context_menu` and `btv.modeless.open_context_menu` go away. They
  were never released; their behaviour becomes the default groups.
- The right-click handler moves out of `prelude/modeless.lua` into a new
  `prelude/contextmenu.lua`, and it runs in every session.
- `MenuPopup`, `btv.ui.menu`, the pointer anchor, and the read-only `copy` /
  `select_all` verbs are all reused. (Two core gaps surfaced and were fixed on the
  way: `'modifiable'` now reads the effective value for read-only kinds, and
  `setreg('+')` writes the clipboard; see Status.)
- A buffer-local `<RightMouse>` map still replaces all of this for its buffer,
  which is how a plugin opts out.

## bemtvi-tree

The tree drops its `<RightMouse>` → `mouse_menu` mapping and its `btv.ui.select`
menu. Instead it registers a `tree` group whose `items` function builds the
node-specific rows (Open, Open in split, New file, Rename, Delete, Cut, Copy,
Paste, Copy path, and so on) for the node under the click. It also puts `"tree"`
at the front of its view buffer's list. The read-only defaults stay below it, so
Select All and Copy work in the tree too.

The right-click default already moves the cursor to the clicked row in a read-only
buffer, so the tree's actions, which act on the node under the cursor, hit the
clicked node without the `getmousepos` lookup the tree does today.

The plugin is its own repository (`bemtvi-tree`); its change lands there as a
separate commit, with its spec updated to drive the right-click through the real
gesture.

## Phases

1. **`btv.contextmenu`.** `prelude/contextmenu.lua`: the registry, per-buffer
   lists (dropped on `BufDelete`), `ctx`, async collection, `open`, `entries`,
   the default groups in both models, and the `MenuPopup` handler. Remove the
   modeless-only menu. Tests in a new `tests/contextmenu.rs`: ordering,
   separators, empty groups skipped, per-buffer independence, defaults by buffer
   kind, editing a default group's items, hiding a group, an async group, a vim
   session's verbs, and loud errors for unknown groups and fields. Docs:
   `docs/features/context-menu.md`, plus the modeless doc pointing at it.
2. **bemtvi-tree.** The `tree` group, the buffer list, removing the mapping, and
   the spec.
3. **Example.** `examples/context-menu/`: an `init.lua` that reorders a buffer's
   list, adds a group, edits and hides default groups, and adds an async group,
   with its own spec.

## Status

All three phases are in. Phase 2 landed in the bemtvi-tree repository (branch
`context-menu-group`); its `doc/bemtvi-tree.txt` still needs regenerating from the
`.md` with pandoc. Phase 3 is `examples/context-menu/`, whose spec drives a real
right-click and checks every section's rows. Notes from building phase 1:

- bemtvi has no `BufWipeout`; `BufDelete` fires once a buffer has left the store,
  so a buffer's list is dropped on that.
- The modeless menu's key hints moved into a public `btv.modeless.key_hint(action)`,
  which the default groups use and a user's own rows can too.
- Unknown group names fail loud, as proposed below: the menu's promise rejects with
  the name, and an unhandled rejection is reported.
- A row's `action` is a function. The previous step also took a
  `btv.modeless.actions` name there; the default groups pick the modeless verb or
  the vim keys themselves, so a name would only cover the modeless half.
- A vim-session verb runs as keys fed from wherever the click left the mode:
  through `<C-o>` from Insert and `<C-g>` from Select, so Paste in Insert stays in
  Insert.
- Groups are the one way to add rows. `MenuPopup` keeps vim's meaning (the context
  menu is about to open) and is documented as no place to open a second menu, since
  only one shows at a time; a buffer-local `<RightMouse>` is the one opt-out.
- `'modifiable'` now reads what the edit chokepoints enforce: a read-only kind (a
  `btv.view`, a live terminal, the quickfix list) reports `nomodifiable` through
  `btv.bo`. Without that, those buffers started from the editable default list.

- `btv.reg.set("+", …)` stored a register cell only Lua could see instead of
  writing the clipboard, so the example's (and the guide's) Copy Path copied
  nothing. `"+` / `"*` now write the provider, as vim's `setreg` does. Reading them
  back from Lua (`btv.reg.get("+")`) still sees only that chunk's own write, not the
  clipboard: a separate gap, left open.

## Open questions

- **Unknown group names.** A name in a list with no registered group: should it be
  skipped quietly (plugin not loaded yet) or fail loud? The plan is to fail loud
  when the menu opens, naming the group, in line with the no-silent-stubs rule.
- **Late registration.** Lists hold names, not tables, so a group registered after
  a buffer's list was created still shows as soon as it exists.
