# Modeless editing

`:set modeless` makes bemtvi edit the way VS Code, Sublime or a text field in a
browser do. You type and the text goes in. Shift with an arrow selects, and typing
over a selection replaces it. Ctrl-C, Ctrl-X and Ctrl-V use the system clipboard,
and Ctrl-Z undoes about a word at a time ([Undo steps](undo.md)). There is no Normal
mode to fall into, and `<Esc>` never takes you out of typing.

It is a setting for the GUI and the web client. The terminal client refuses it.

In the GUI and the browser the cursor is a blinking bar while you type or select, at
the moving end of a selection, and the mouse pointer is an I-beam over text,
read-only text included. It stays the arrow over the line numbers, status lines and
popups, and over a terminal.

```lua
-- init.lua
btv.o.modeless = true
```

## How it works

Every buffer rests in Insert. Whatever leaves it in Normal (closing the command
line, finishing an ex command, switching windows) lands back in Insert before the
next key. There is no Normal mode anywhere in the session.

A read-only buffer (a file tree, the quickfix list, help, a panel) rests in Insert
too, with the same caret, arrows, Shift-selection, mouse selection and keys. What
it refuses is the edit: typing, `<CR>`, `<BS>`, paste, cut and undo there say
`E21: Cannot make changes, 'modifiable' is off` and change nothing. Copy, Select
All and find work as anywhere else. The keys a read-only buffer answers to (the
quickfix list's `<CR>`, a panel's `q`) are its own maps in the `modeless` layer; see
[Plugins](#plugins).

A terminal rests in the running program: its keys go to the process, and
`<C-\><C-n>` lands straight back there. Once the program exits, the buffer is
ordinary text and rests in Insert.

`<Esc>` closes what is open over the text (the completion popup, signature help, a
snippet session) and drops a selection, and that is all.

A selection is Select mode, the mode vim's snippets use: typing replaces it, and
replacing it writes no register, so typing over text never clobbers the clipboard.
A mouse drag, a double click (word) and a triple click (line) select the same way.

Undo and redo bring the selection back. Undoing a type-over, a cut or an indent
leaves the text it changed selected again, facing the way you selected it, and redo
returns to whatever was selected when you undid.

## Keys

On macOS the layout uses Command (`<D-…>`) where this table says Ctrl.

| Key | Does | Key | Does |
| --- | --- | --- | --- |
| `<C-z>` | undo | `<C-S-z>`, `<C-y>` | redo |
| `<C-c>` | copy (the line, with nothing selected) | `<C-x>` | cut (likewise) |
| `<C-v>` | paste | `<C-a>` | select all |
| `<C-s>` | save | `<C-w>` | close the buffer |
| `<C-f>` | find (the `/` line, holding the selected text; the match comes up selected) | `<C-g>` / `<C-S-g>` | next / previous match |
| `<C-h>` | replace (a `:%s/` line, holding the selected text as its pattern; `<A-D-f>` on macOS) | `<C-;>` | the `:` line |
| `<C-p>` | open a file | `<C-S-p>` | command palette |
| `<C-Tab>` / `<C-S-Tab>` | next / previous buffer | `<C-]>` | indent |
| `<C-BS>` / `<C-Del>` | delete a word back / forward (`<A-BS>` / `<A-Del>` on macOS) | `<C-S-d>` | duplicate the line |
| `<A-Up>` / `<A-Down>` | move the line up / down | `<Tab>` / `<S-Tab>` | indent / outdent a multi-line selection |

The language keys are VS Code's. They run the `btv.lsp` verbs, so they need a language
server attached to the buffer, and they stand in for vim's `gd`, `gr`, `K` and `]d`, which
are Normal-mode letters a modeless session can't reach:

| Key | Does | Key | Does |
| --- | --- | --- | --- |
| `<F12>` | go to definition | `<C-F12>` | go to implementation |
| `<S-F12>` | find references | `<F2>` | rename the symbol |
| `<C-k><C-i>` | show the hover | `<C-S-Space>` | signature help |
| `<C-.>` | quick fix / code actions | `<S-A-f>` | format the document |
| `<C-S-o>` | go to a symbol in the file | `<C-t>` | go to a symbol in the workspace |
| `<F8>` / `<S-F8>` | next / previous problem (diagnostic) | | |

On macOS the Ctrl ones use Command, as above; the F-keys and `<S-A-f>` (Shift+Option+F) are
the same. `<C-k>` starts the hover chord, so an Insert-mode `<C-k>` (the signature help
bemtvi binds when a language server attaches, or one of your own) now waits
`'timeoutlen'` for a second key; signature help has `<C-S-Space>` here.

The arrows move the caret the way a GUI editor does: `<Right>` at the end of a line
goes to the next one, `<C-Left>` and `<C-Right>` move by word, `<Home>` goes to the
first non-blank and then to column 0, `<C-Home>` / `<C-End>` go to the buffer's
ends, and `<PageUp>` / `<PageDown>` slide the view a page with the caret keeping its
row on screen.

On macOS, Command with an arrow goes to the line's edges or the buffer's ends, and
Option with Left or Right moves by word. Shift with any of them selects.

The mouse wheel scrolls the view and nothing else. The caret and any selection stay
where they are, even off screen, and the first key that types or moves the caret
brings the view back to it, as does an input method starting to compose. A page key
pages from the caret's place. Each window keeps its own scroll while focus is
elsewhere, including one you scrolled without focusing it. Shift with the wheel
scrolls sideways (down goes right), as does a tilting wheel or a touchpad; a long
line only scrolls sideways with `'wrap'` off. Sideways scrolls slide like vertical
ones.

Dragging a selection to the top or bottom of a window scrolls it while the pointer
stays there, and with `'wrap'` off so does dragging it to the window's left or right
edge, so a selection can grow past what is on screen in any direction.

Each window shows where its view is: a slim thumb along the right edge for the lines
in view, and with `'wrap'` off one along the bottom of the text for the columns,
measured against the widest line on screen (as far as you can scroll). A thumb
shows only when there is something to scroll to. It marks the place and does not
take clicks or drags yet. Floating windows have none.

The command palette lists every picker (Live Grep, Buffers, Find Files, and any a
plugin registers with a title) and every ex command. A picker row opens that picker.
A command that needs an argument opens the `:` line with its name typed, ready for
the rest.

Every key is a default you can replace. The layout lives in its own keymap layer,
`modeless`, which is consulted ahead of the Insert maps while a modeless session is
typing or selecting, so your Insert maps (completion, snippets, your own) still
work. The verbs are named, so you can bind them anywhere:

```lua
btv.keymap.set("modeless", "<C-d>", btv.modeless.actions.duplicate_line)
btv.keymap.set("modeless", "<C-s>", function() end) -- free the key
```

The named actions are `copy`, `cut`, `paste`, `undo`, `redo`, `select_all`,
`delete_word_back`, `delete_word_forward`, `indent`, `outdent`, `duplicate_line`,
`move_line_up`, `move_line_down`, `search_next`, `search_prev`, `save`, `find`,
`replace`, `command_line`, `palette`, `files`, `close`, `next_buffer`,
`prev_buffer`, `reveal_caret` (bring a scrolled-away view back to the caret), the language
verbs `definition`, `implementation`, `references`, `hover`, `signature_help`, `rename`,
`code_action`, `format`, `document_symbol` and `workspace_symbol`, and `next_diagnostic` /
`prev_diagnostic`.
In a read-only buffer the verbs that don't edit (`copy`, `select_all`,
`search_next`, `search_prev`, `reveal_caret` and the commands) run as anywhere
else, and the editing ones refuse with `E21`.

## The context menu

A right-click opens a menu at the pointer. A click inside the selection keeps it,
so the menu acts on it; anywhere else the caret moves to the click first. The menu
offers Undo and Redo, Cut, Copy, Paste and Select All, the LSP verbs when a language
server is attached, and the command palette, and each row shows the key that does
the same thing. A read-only window (help, the quickfix list) gets Copy and Select
All.

The same menu works in a vim session, and it is built from groups you can reorder,
edit and add to: see [the context menu](context-menu.md). For a key hint in a row of
your own, `btv.modeless.key_hint(action)` gives the chord that runs a named action.

## In the browser

On Windows and Linux, Chrome keeps `<C-w>`, `<C-t>`, `<C-n>` and `<C-Tab>` for
itself. The web client sends `<C-w>` when you press Alt-W instead; `<C-Tab>` has no
stand-in, so use the command palette for `:bnext`. On a Mac the client forwards the
layout's Command chords to the editor, except Command-V (the browser's own paste
does the same job) and Command-W (the browser keeps it).

Of the language keys, the browser keeps a bare F12 (its developer tools), so go to
definition is in the right-click menu and the command palette there; `<C-F12>` and
`<S-F12>` reach the editor. `<C-t>` (symbols
in the workspace) is Alt-T on Windows and Linux, like `<C-w>`, and has no key on a Mac,
where the browser keeps Command-T.

## Plugins

A plugin with a read-only buffer (a tree, a sidebar, a results list, a dialog)
binds its keys for both models. A modeless session never reaches Normal mode, so an
`"n"` map never fires there, and nothing guesses one into the `modeless` layer for
you. Bind the same keys in both:

```lua
local view = btv.view.create({ name = "[Tree]" })
-- …
btv.keymap.set({ "n", "modeless" }, "<CR>", open_entry, { buffer = view })
btv.keymap.set({ "n", "modeless" }, "q", close_tree, { buffer = view })
```

`buffer = view` binds to the view's buffer, which is created after the chunk that
creates the view, as soon as it exists. Until then `view:bufnr()` reads `false`,
and a `buffer` option given `false` is an error rather than a global map.

A printable key such as `q` is safe in the `modeless` layer on a read-only buffer:
typing is refused there anyway, so the map takes nothing away. A buffer-local map
in the layer beats the layout's own key on the same chord, so a view can bind
`<C-v>` to "open in a split" without losing it to paste. `btv.view` installs its
`<CR>`, and with an `on_select` its double-click, this way, and so do the quickfix list, the panels, the
explorer and the `:Plugins` screens. On an editable buffer, keep to keys that don't
type (chords, function keys), since the layer comes ahead of typing.

The rest of a plugin written for the vim model mostly works unchanged. Three things
differ in a session that never leaves Insert:

- `TextChanged` and `InsertLeave` don't fire while you type; `TextChangedI` does. A
  `[+]` indicator should listen to `BufModifiedSet`, which fires the same way in
  either model.
- `btv.diagnostic.config({ update_in_insert = false })` ("hold until `InsertLeave`")
  holds updates until typing goes quiet for 3 seconds instead.
- `btv.cmd("normal! …")` and keys fed with `btv._feedkeys` still run as Normal-mode
  keys: an `<Esc>` in them leaves Insert, and the session comes back once they end.

`btv.ui.caps()` reports `client` (`"gui"`, `"web"`, `"tui"`) and `platform`
(`"macos"`, `"linux"`, `"windows"`), which is how the layout picks Command.

## Limits

- The terminal client: `:set modeless` fails there, and a terminal attaching to a
  session that has it on turns it off.
- Helix mode and modeless editing can't both be on.
- Several cursors at once (`<A-c>` placement) keep their own motions; the GUI
  gestures for them (Ctrl-D, Alt-click) aren't bound yet.
