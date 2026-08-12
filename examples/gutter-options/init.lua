-- ~~~ bemtvi gutter options: 'numberwidth' + 'signcolumn' ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/gutter-options \
--       cargo run -p bemtvi -- examples/gutter-options/sample.txt
--
-- Two neovim-compatible window-local options shape the left gutter:
--
--   'numberwidth' (nuw)  the MINIMUM width of the line-number column. The gutter
--                        is at least this wide, growing to fit the largest line
--                        number plus a trailing space. Default 4.
--
--   'signcolumn'  (scl)  the sign column (where LSP/diagnostic signs sit), left
--                        of the numbers. Each sign column is 2 cells:
--                            no            never show it
--                            auto          show 1 column when a sign is present,
--                                          collapse to 0 when none are
--                            auto:1-3      grow 1..3 columns to fit the signs
--                            yes / yes:2   ALWAYS reserve 1 (or N) columns
--                            yes:1-3       always >=1, grow to 3
--
-- Both are window-local, so two splits onto the same file can differ.

vim.g.mapleader = " "

-- Start wide so the difference is obvious: an 8-cell number gutter and a
-- permanent 2-column (4-cell) sign column. `yes:2` shows the column even with no
-- LSP attached, so you can see the reserved space immediately.
vim.o.numberwidth = 8
vim.o.signcolumn = "yes:2"

-- Cycle 'numberwidth' 4 -> 6 -> 8 -> 4 with <leader>n.
local NUW = { 4, 6, 8 }
local nuw_i = 3
vim.keymap.set("n", "<leader>n", function()
  nuw_i = (nuw_i % #NUW) + 1
  vim.o.numberwidth = NUW[nuw_i]
  vim.notify("numberwidth = " .. vim.o.numberwidth)
end, { desc = "cycle numberwidth" })

-- Cycle 'signcolumn' through the common policies with <leader>s.
local SCL = { "no", "auto", "yes", "yes:2", "auto:1-3" }
local scl_i = 4
vim.keymap.set("n", "<leader>s", function()
  scl_i = (scl_i % #SCL) + 1
  vim.o.signcolumn = SCL[scl_i]
  vim.notify("signcolumn = " .. vim.o.signcolumn)
end, { desc = "cycle signcolumn" })

-- `:vsplit` then set the new window narrower to prove the options are per-window:
--     <C-w>v  then  :setlocal nuw=4 scl=no
vim.notify("gutter: nuw=" .. vim.o.numberwidth .. " scl=" .. vim.o.signcolumn
  .. "  (<leader>n / <leader>s to cycle)")
