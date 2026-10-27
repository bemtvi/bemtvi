--------------------------------------------------------------------------------
-- A single-file editor — the editor an app embeds to edit one given file.
--
-- Run it (a mitsuami window editing sample.txt, or the file you name):
--   cargo run -p bemtvi-mitsuami --example single_file [file]
--
-- The app starts the editor with `SessionOptions::single_file(path)`: locked to the
-- file, the commands that run code or processes taken away, and user commands only
-- from this config and the plugins loaded with it. This directory is its config
-- (`Config::Dir`). Plain bemtvi runs the config too, without any of that:
--   BEMTVI_CONFIG=examples/single-file cargo run -p bemtvi -- examples/single-file/sample.txt
--------------------------------------------------------------------------------

btv.g.mapleader = " "

--------------------------------------------------------------------------------
-- 1. The file, and no other.
--
-- Type-this:  :e examples/single-file/other.txt<CR>
-- See-that:   E5801: this editor only edits …/sample.txt
--
-- Type-this:  :tabnew<CR>   or   :enew<CR>
-- See-that:   the same E5801: no new file buffer either.
--
-- Type-this:  :e <Tab>
-- See-that:   only sample.txt is offered: no file picker over the rest of the disk.
--
-- Type-this:  ihello<Esc>:e!<CR>
-- See-that:   the edit is reverted: `:e!` on the file itself still works, as do
--             `:w`, `:sp` (a second view of it), `:copen` and `:messages`.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 2. What's taken away.
--
-- Type-this:  :lua print(1)<CR>   or   :terminal<CR>   or   :cd /<CR>
-- See-that:   E5800: :lua is disabled in this editor (and :terminal, :cd).
--
-- Type-this:  :autocmd<CR>   or   :Plugins<CR>   or   :LspInfo<CR>
-- See-that:   E5800 too: no autocmds from the command line, and the plugin manager
--             and LSP are off (`btv.plugins.disabled()`, `btv.lsp.disabled()`).
--
-- Type-this:  <Space>f   (normal mode: this config maps it to the files picker)
-- See-that:   E5803: the files picker is disabled in this editor. So are buffers,
--             diagnostics and live_grep; the other pickers still open. (Plain bemtvi
--             opens it.)
--
-- Type-this:  :<Tab>
-- See-that:   the disabled commands aren't offered.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>f", function()
  btv.picker.open("files")
end, { desc = "Find files (disabled in the single-file editor)" })

--------------------------------------------------------------------------------
-- 3. What the config gives stays.
--
-- Type-this:  :Stamp<CR>   or   <Space>s   (normal mode)
-- See-that:   a line with today's date below the cursor: a user command defined
--             here, by the config, is one the editor still takes.
--------------------------------------------------------------------------------
local function stamp()
  btv.cmd("normal! o" .. os.date("%Y-%m-%d"))
end

btv.command("Stamp", stamp, { desc = "Insert today's date below the cursor" })
btv.keymap.set("n", "<leader>s", stamp, { desc = "Insert today's date below the cursor" })
