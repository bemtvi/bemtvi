-- ~~~ bemtvi 'colorcolumn': a vertical ruler down the text ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/colorcolumn \
--       cargo run -p bemtvi -- examples/colorcolumn/sample.txt
--
-- 'colorcolumn' (abbrev 'cc') highlights one or more text columns with the
-- `ColorColumn` highlight group — a vertical guide line drawn down the whole text
-- body, so you can see at a glance when a line runs past a width budget (the
-- classic use: a marker at column 80 and/or 120). It is the COLUMN analogue of
-- 'cursorline' (which tints the cursor's whole ROW).
--
-- It is window-local (each split carries its own rulers) and takes a
-- comma-separated list of columns. bemtvi honors ABSOLUTE column numbers; vim's
-- 'textwidth'-relative "+N"/"-N" forms are accepted but skipped (bemtvi models no
-- 'textwidth' to anchor them). Empty (no ruler) by default, so you opt in.
--
-- Set it the neovim way — `vim.opt`/`vim.o` (a string or a list) or `:set cc=…`:
vim.opt.colorcolumn = "80,120"

-- Give the rulers a clear colour out of the box. `ColorColumn` is a normal
-- highlight group — a colorscheme usually defines it, but here (no theme loaded)
-- we set it ourselves so the guides are obviously visible. Without any definition
-- bemtvi still falls back to a subtle grey so the ruler never vanishes silently.
vim.api.nvim_set_hl(0, "ColorColumn", { bg = "#3a2a2a" })

--------------------------------------------------------------------------------
-- :CC80 / :CC80120 / :NoCC — swap the ruler set from Lua, proving the `vim.wo`
-- window-local surface reaches the core (not just `:set`).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("CC80", function()
  vim.wo.colorcolumn = "80"
  vim.notify("colorcolumn = 80")
end, {})

vim.api.nvim_create_user_command("CC80120", function()
  vim.wo.colorcolumn = "80,120"
  vim.notify("colorcolumn = 80,120")
end, {})

vim.api.nvim_create_user_command("NoCC", function()
  vim.wo.colorcolumn = ""
  vim.notify("colorcolumn cleared")
end, {})

--------------------------------------------------------------------------------
-- :CCReport — echo the focused window's colorcolumn value (message line / :messages).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("CCReport", function()
  vim.cmd("set colorcolumn?")
end, {})

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. Two faint vertical guides run down the text at columns 80 and 120. The lines
--    in the sample cross both, so you can see exactly where each budget falls.
--
-- 2. Move the cursor along a long line with `l` / `w` / `$`. Because these are
--    `nowrap` buffers, a very long line pans the viewport sideways — watch the
--    ruler stay pinned to its TEXT column (it scrolls with the text, not the
--    screen), and disappear off the left once column 80 scrolls out of view.
--
-- 3. `:CC80` to keep only the 80-column guide, `:CC80120` for both, `:NoCC` to
--    clear them. `:CCReport` to echo the current value.
--
-- 4. `:set cc=+1` — a 'textwidth'-relative entry, which bemtvi skips (no ruler),
--    since there is no 'textwidth'. `:set cc=100` for an absolute one instead.
--
-- 5. The two `wide: …` lines are double-width (CJK) text: one is laid out so the
--    80-column guide falls on a glyph's RIGHT half, the other so it falls on a
--    glyph's own cell. A terminal cannot paint half a glyph, so in the TUI the
--    ruler tints the whole glyph it lands on — two columns wide, but never
--    missing. (A GUI cell grid has no such limit and keeps the ruler one column
--    wide.) Type and delete characters in front of the CJK run with `I`: every
--    glyph slides a column and back, and the background must stay unbroken to the
--    right of the text.
--------------------------------------------------------------------------------

vim.notify("colorcolumn demo: guides at 80 and 120 (:CC80, :CC80120, :NoCC, :CCReport)")
