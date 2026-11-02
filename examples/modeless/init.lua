-- ~~~ bemtvi modeless editing: type, select with Shift, Ctrl for the rest ~~~
--
-- Run it in the GUI (from the repo root):
--
--     BEMTVI_CONFIG=examples/modeless \
--       cargo run -p bemtvi-gui -- examples/modeless/sample.txt
--
-- The terminal client refuses modeless editing: there `btv.o.modeless = true`
-- is turned off when the terminal attaches, with a message saying why. On macOS,
-- read Command (Cmd) wherever a note says Ctrl.
btv.o.modeless = true

-- 1. Typing edits. The file opens in Insert: type `Hello ` and it goes in at the
--    start of line 1. Press `<Esc>`: you are still typing (`<Esc>` closes popups and
--    drops a selection, nothing else).

-- 2. Undo a word at a time. Type ` and more words` at the end of line 1 (`<End>`),
--    then press `<C-z>` three times: `words`, `more`, `and` come off one by one.
--    `<C-S-z>` (or `<C-y>`) puts them back.

-- 3. Select with Shift, type over it. On line 1, press `<Home>`, then
--    `<C-S-Right>`: `The` is selected. Type `A` and it replaces the word.
--    `<C-z>` brings `The` back, selected again. `<S-End>` selects to the end of
--    the line, `<S-Down>` into the next one.

-- 4. Copy, cut and paste. `<C-c>` with nothing selected copies the whole line;
--    move to `first` and press `<C-v>`: a copy of the line goes in above it.
--    Select a word and `<C-x>` cuts it.

-- 5. Find. Press `<C-f>`, type `fox`, `<Enter>`: the match comes up selected. `<C-g>`
--    selects the next one, `<C-S-g>` the one before. Type to replace a match. With
--    text selected, `<C-f>` starts from it: double-click `fox` on line 1, and `<C-f>`
--    shows `/fox`, so `<Enter>` finds the next one. `<C-h>` the same way opens
--    `:%s/fox/`, waiting for the replacement.

-- 6. Lines. On `second`, `<A-Up>` moves it above `first`, `<A-Down>` back, and
--    `<C-S-d>` duplicates it. Select from the start of `first` to the end of
--    `third` (`<S-Down>` twice, then `<S-End>`) and press `<Tab>`: all three indent.
--    `<S-Tab>` takes it back. (A selection that ends at the start of a line leaves
--    that line out, which is why `<S-End>` is needed here.)

-- 7. Commands without a Normal mode. `<C-S-p>` opens the command palette: type
--    `split`, `<Enter>`. Pickers are listed too: `<C-S-p>`, type `Live`, `<Enter>`
--    opens Live Grep. `<C-;>` opens the `:` line itself, and `<C-s>` saves.

-- 8. Your own keys. The layout is the `modeless` keymap layer, so a map there wins
--    over the default. `<C-e>` here selects the rest of the line, `<S-End>`'s twin.
btv.keymap.set("modeless", "<C-e>", "<S-End>", { desc = "Select to the end of the line" })

-- 9. Scrolling. Open the long, wide file beside this one: `<C-;>`, then
--    `e examples/modeless/wide.txt`, `<Enter>`, and `<C-;>` `set nowrap` `<Enter>`.
--    The wheel scrolls without moving the caret, and a slim thumb on the right edge
--    shows where you are. Shift with the wheel scrolls sideways (down goes right),
--    and a thumb along the bottom follows. Type a letter: the view comes back to
--    the caret.

-- 10. Right-click. Double-click `quick` on line 1 to select it, then right-click
--     it: a menu opens at the pointer with Undo, Cut, Copy, Paste and the rest, each
--     showing its key. Click Cut and `quick` is gone. Right-click on line 2 instead
--     and the caret moves there first; click `Duplicate Line` (the row added below)
--     and line 2 is doubled. A click anywhere else, or `<Esc>`, dismisses the menu.
--     The menu is made of groups (see docs/features/context-menu.md): this adds one,
--     `lines`, at the end of the list every editable buffer starts from.
btv.contextmenu.groups.lines = {
  items = { { label = "Duplicate Line", action = btv.modeless.actions.duplicate_line } },
}
table.insert(btv.contextmenu.default, "lines")

-- 11. Read-only buffers are modeless too. `<C-;>` `Outline` `<Enter>` opens a dock
--     listing this file's lines. It is a `btv.view`, read-only, yet there is no
--     Normal mode in it either: the arrows move the caret, `<S-Down>` selects and
--     `<C-c>` copies, and typing a letter says `E21` instead of editing. `<Enter>`
--     jumps the file to that line; `q` closes the outline. A read-only buffer takes
--     its keys only through the `modeless` layer in this session, so the outline
--     binds `q` with `{ "n", "modeless" }`, for both editing models (`btv.view`
--     binds its own `<Enter>` the same way).
btv.command("Outline", function()
  local main = btv.win.current()
  local outline = btv.view.create({ name = "[Outline]" })
  outline:set_lines(btv.buf.lines(0, 0, -1))
  outline:on_select(function(line)
    btv.cursor.set({ line, 0 }, main)
  end)
  outline:mount({ dock = "left", size = 30 })
  -- `buffer = outline`: the view's buffer is created after this chunk, and the map
  -- lands on it as soon as it exists.
  btv.keymap.set({ "n", "modeless" }, "q", function()
    outline:close()
  end, { buffer = outline, desc = "Close the outline" })
end, { desc = "List this file's lines in a dock" })
