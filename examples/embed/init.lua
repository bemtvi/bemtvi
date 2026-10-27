--------------------------------------------------------------------------------
-- btv.host — the editor talking to the app that embeds it.
--
-- Run the embedding app (a mitsuami window: notes down the side, this editor
-- beside them, and a line under it showing what the editor tells the app):
--   cargo run -p bemtvi-mitsuami --example embed
--
-- It runs the editor with this directory as its whole config (`Config::Dir`), so
-- this file is what the app's editor does. Plain bemtvi runs it too, with no app
-- around it:
--   BEMTVI_CONFIG=examples/embed cargo run -p bemtvi -- examples/embed/sample.txt
--------------------------------------------------------------------------------

btv.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. Who embeds the editor.
--
-- Type-this:  :lua print(btv.host.name())<CR>
-- See-that:   embed-example in the app (the id it started the editor with);
--             nil in plain bemtvi.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2. Edits reach the app.
--
-- Type-this:  ihello<Esc>
-- See-that:   the app's line: "ideas.md changed (unsaved)", once typing pauses.
--
-- Type-this:  :w<CR>
-- See-that:   "ideas.md written".
--
-- Only the buffer's name and state go to the app, never its text: the app asks
-- for what it needs.
--------------------------------------------------------------------------------
if btv.host.name() then
  btv.host.forward({ "changed", "wrote" })
end

--------------------------------------------------------------------------------
-- 3. A keymap that asks the app for something.
--
-- Type-this:  <Space>p   (normal mode)
-- See-that:   the app's line: "preview asked for ideas.md".
--             In plain bemtvi: "no app to preview in".
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>p", function()
  if not btv.host.name() then
    return btv.notify("no app to preview in", btv.log.levels.WARN)
  end
  btv.host.send("preview", { file = btv.buf.name(0) })
end, { desc = "Ask the app embedding the editor to preview this file" })

--------------------------------------------------------------------------------
-- 4. The app's own shortcuts win.
--
-- Type-this:  <C-s>   (⌘S on macOS): the app's File › Save
-- See-that:   the app writes the note through its handle (`:w`), and its line
--             says "saved from the app's menu": the editor never sees the chord.
--------------------------------------------------------------------------------
