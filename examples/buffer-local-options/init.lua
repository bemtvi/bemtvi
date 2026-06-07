-- ~~~ nxvim buffer-local options: tabstop / shiftwidth / softtabstop / expandtab ~~~
--
-- Run it (from the repo root) against the sample Lua buffer:
--
--     NXVIM_CONFIG=examples/buffer-local-options \
--       cargo run -p nxvim -- examples/buffer-local-options/two.lua
--
-- Buffer-local options live on each buffer, so two files can indent differently.
-- A `FileType` autocmd is the idiomatic place to set them: it fires for whichever
-- buffer just loaded, and `vim.bo[buf]` targets *that* buffer.
--
-- nxvim's defaults already break with vim: tabstop=4, and shiftwidth/softtabstop
-- *follow* it via their sentinels (shiftwidth=0 -> follow tabstop, softtabstop=-1
-- -> follow shiftwidth). So the one `tabstop` knob sets the whole indent width;
-- setting `vim.bo.tabstop = 2` below makes Tab move by 2 as well, automatically.

-- Lua files indent with two spaces. Setting expandtab + tabstop=2 is enough:
-- shiftwidth and softtabstop follow tabstop, so <Tab> moves by 2 too.
vim.api.nvim_create_autocmd("FileType", {
  pattern = "lua",
  callback = function(args)
    -- Buffer-local writes: they change how THIS buffer renders tabs and what
    -- <Tab> inserts, without touching any other open buffer.
    vim.bo[args.buf].expandtab = true
    vim.bo[args.buf].tabstop = 2
  end,
})

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. In `two.lua` (filetype `lua` -> the autocmd ran), press `i` then:
--      TYPE:  <Tab>x      -> "  x"   (two SPACES, not a tab — expandtab + ts=2)
--
-- 2. Prove it is PER BUFFER. Open the plain-text file (no filetype, so the
--    autocmd did NOT touch it — it keeps the defaults: noexpandtab, tabstop 4):
--      TYPE:  :e notes.txt<CR>
--      TYPE:  i<Tab>x<Esc>   -> a real "\t", shown 4 cells wide.
--    Switch back:  :b two.lua   — it still indents by two spaces.
--
-- 3. Drive it by hand on the current buffer with :set / :setlocal:
--      :setlocal expandtab tabstop=8 softtabstop=4
--      TYPE:  i<Tab>y<Esc>   -> "    y"  (four spaces — softtabstop, not tabstop)
--      TYPE:  i<Tab><BS>z<Esc> -> "z"    (<BS> removes the whole 4-space soft tab)
--      :set tabstop?          -> echoes "tabstop=8"
--      :set noexpandtab       -> back to literal tabs in this buffer
--------------------------------------------------------------------------------
