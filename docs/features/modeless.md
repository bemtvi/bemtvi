# Modeless editing

`:set modeless` makes bemtvi edit the way VS Code, Sublime or a text field in a
browser do. You type and the text goes in. Shift with an arrow selects, and typing
over a selection replaces it. Ctrl-C, Ctrl-X and Ctrl-V use the system clipboard,
and Ctrl-Z undoes about a word at a time ([Undo steps](undo.md)). There is no Normal
mode to fall into, and `<Esc>` never takes you out of typing.

It is a setting for the GUI and the web client. The terminal client refuses it.

In the GUI and the browser the cursor is a blinking bar while you type or select, at
the moving end of a selection, and the mouse pointer is an I-beam over text you can
edit. It stays the arrow over the line numbers, status lines, popups and read-only
windows, which also keep vim's block cursor.

```lua
-- init.lua
btv.o.modeless = true
```

## How it works

A buffer you can edit rests in Insert. Whatever leaves it in Normal (closing the
command line, finishing an ex command, switching windows) lands back in Insert
before the next key. A read-only buffer, such as a file tree, the quickfix list or
help, keeps resting in Normal, because its keys (`j`, `<CR>`, `q`) are Normal-mode
maps. So the session moves between the two as focus moves.

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
| `<C-f>` | find (the `/` line; the match comes up selected) | `<C-g>` / `<C-S-g>` | next / previous match |
| `<C-h>` | replace (a `:%s/` line; `<A-D-f>` on macOS) | `<C-;>` | the `:` line |
| `<C-p>` | open a file | `<C-S-p>` | command palette |
| `<C-Tab>` / `<C-S-Tab>` | next / previous buffer | `<C-]>` | indent |
| `<C-BS>` / `<C-Del>` | delete a word back / forward (`<A-BS>` / `<A-Del>` on macOS) | `<C-S-d>` | duplicate the line |
| `<A-Up>` / `<A-Down>` | move the line up / down | `<Tab>` / `<S-Tab>` | indent / outdent a multi-line selection |

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
line only scrolls sideways with `'wrap'` off.

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
`prev_buffer` and `reveal_caret` (bring a scrolled-away view back to the caret).

## In the browser

On Windows and Linux, Chrome keeps `<C-w>`, `<C-t>`, `<C-n>` and `<C-Tab>` for
itself. The web client sends `<C-w>` when you press Alt-W instead; `<C-Tab>` has no
stand-in, so use the command palette for `:bnext`. On a Mac the client forwards the
layout's Command chords to the editor, except Command-V (the browser's own paste
does the same job) and Command-W (the browser keeps it).

## Plugins

A plugin written for the vim model mostly works unchanged. Three things differ in a
session that never leaves Insert:

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
