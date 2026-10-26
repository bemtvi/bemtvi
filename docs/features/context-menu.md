# The context menu

A right-click opens a menu at the pointer, in every session: vim or
[modeless](modeless.md), the GUI, the browser or the terminal. Before the menu opens,
the click moves the cursor to where you clicked. A click inside the selection keeps
the selection instead, so the menu acts on it.

In an editable buffer the menu offers Undo and Redo; Cut, Copy, Paste and Select All;
the LSP verbs while a language server is attached; and the command palette. A
read-only buffer (help, the quickfix list) offers Select All, plus Copy when text is
selected there.

One click chooses a row, and the row under the pointer is highlighted. A click or
right-click anywhere else dismisses the menu, and a right-click opens a new one where
it landed. The keys work too: `<Down>` / `<Up>` move, `<CR>` chooses and `<Esc>`
dismisses.

A runnable tour of everything below, with its own spec, is
`examples/context-menu/`:

```sh
BEMTVI_CONFIG=examples/context-menu cargo run -p bemtvi -- examples/context-menu/sample.txt
```

## Groups

The menu is built from **groups**. Each buffer has its own ordered list of group
names, and each name refers to a registered group that gives rows. Groups that give
no rows are dropped, and a separator goes between the rest.

```lua
btv.contextmenu.list(0)   --> { "undo", "clipboard", "lsp", "commands" }
```

| Group | Rows | Modeless session | Vim session |
| --- | --- | --- | --- |
| `undo` | Undo, Redo | the modeless verbs | `:undo` / `:redo` |
| `clipboard` | Cut, Copy, Paste, Select All | the modeless verbs | `"+d`, `"+y`, `"+P`, `ggVG` |
| `lsp` | Go to Definition, Find References, Rename Symbol, Format Document, Code Actions | `btv.lsp.*` | `btv.lsp.*` |
| `commands` | Command Palette | the palette | the palette |

- `undo`, and Cut and Paste, show only in a buffer that is `'modifiable'`.
- Copy with nothing selected copies the line in an editable buffer. A read-only
  buffer needs a selection to copy. In a vim session Cut needs a selection too; a
  modeless Cut takes the line.
- `lsp` shows while a server is attached, and greys out a verb no attached server
  answers.
- In a modeless editable buffer each row shows the key that does the same thing,
  read from the live layout, so a rebound key shows its new chord. A vim session
  shows no keys: vim's keys are verbs plus motions, not one chord.

## Changing the menu

A buffer's list is live: edit it and that buffer's menu changes, and no other's.

```lua
local list = btv.contextmenu.list(0)
table.insert(list, 1, "commands")       -- the palette first
btv.contextmenu.set(0, { "clipboard" }) -- or replace the list
btv.contextmenu.set(0, {})              -- no menu in this buffer
```

A buffer gets its list the first time it's asked for: a copy of
`btv.contextmenu.default` for a `'modifiable'` buffer, or of
`btv.contextmenu.default_readonly` otherwise. Edit the defaults in your config to
change what every buffer starts with:

```lua
table.remove(btv.contextmenu.default, 1) -- no Undo / Redo in new buffers
```

A group is a table you can change. Add a row, remove one, or hide the whole group:

```lua
table.insert(btv.contextmenu.groups.commands.items, {
  label = "Save",
  action = function() btv.cmd("write") end,
})
btv.contextmenu.groups.undo.when = false
```

## Adding a group

A group has `items`, the rows, and an optional `when`. `items` is a list, or a
function that returns one, or returns a promise of one:

```lua
btv.contextmenu.groups.file = {
  when = function(ctx) return btv.buf.name(ctx.buf) ~= "" end,
  items = function(ctx)
    local path = btv.buf.name(ctx.buf)
    return btv.fs.stat(path):next(function(st)  -- answers later
      return {
        { label = "Copy Path", detail = st.size .. " bytes",
          action = function() return btv.clipboard.write(path) end },
      }
    end)
  end,
}
table.insert(btv.contextmenu.default, "file")
```

A row has:

| Field | |
| --- | --- |
| `label` | The row's text. Required. |
| `action` | `function(ctx)`, run when the row is chosen. It may return a promise. Required. |
| `detail` | The right-hand hint: a string, or `function(ctx)` returning one or `nil`. |
| `when` | Hide the row unless this is true: a boolean, or `function(ctx)`. |
| `enabled` | Grey the row out unless this is true: a boolean, or `function(ctx)`. |

A group's `when` hides the whole group the same way. A field a group or a row
doesn't know about is an error that names the group, so a typo doesn't silently drop
a row. So is a name in a list with no registered group. Lists hold names, so a group
registered after a buffer's list was made shows as soon as it exists.

The menu opens once every group has answered. Rows keep list order, whichever group
answers first.

`ctx` is built once per right-click and passed to every `items`, `when`, `enabled`,
`detail` and `action`:

```lua
{
  buf = 3, win = 1000,        -- the clicked buffer and window
  mouse = btv.getmousepos(),  -- the click
  mode = "n",                 -- the mode once the click has moved the cursor
  selection = false,          -- Visual or Select is active
  modifiable = true,          -- the buffer can be edited
  modeless = false,           -- a modeless session
}
```

For a modeless verb's key hint in your own row, use `btv.modeless.key_hint`:

```lua
{ label = "Save", action = btv.modeless.actions.save,
  detail = function(ctx) return ctx.modeless and btv.modeless.key_hint("save") or nil end }
```

## Opening it yourself

`btv.contextmenu.open()` opens the current buffer's menu at the pointer, which is
what a right-click does. From a key with no click behind it, the menu opens at the
text cursor. It returns a promise that settles once the chosen row's action has
run, or once the menu closed. `btv.contextmenu.entries(buf)` returns a promise of
the rows a buffer's menu would show right now, without showing it.

## A menu of your own

Groups are the one way to put rows in the right-click menu. For a buffer that should
show only your rows, set its list: `btv.contextmenu.set(buf, { "mine" })`. To take
the right-click over entirely, bind a buffer-local `<RightMouse>`: a map replaces a
key's default, so no menu opens.

Underneath, a right-click fires the `MenuPopup` event (see
[autocmd events](../autocmd-events.md)), as vim does just before its popup menu shows,
and this menu is what opens. Don't open a second menu from a `MenuPopup` handler: only
one menu shows at a time, so it would replace this one or be replaced by it.
