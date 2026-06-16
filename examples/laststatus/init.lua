-- ~~~ nxvim 'laststatus' playground: per-window vs. one global status line ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/laststatus \
--       cargo run -p nxvim -- examples/laststatus/sample.txt
--
-- 'laststatus' decides WHERE status lines are drawn:
--
--     0   never  (the freed bottom row becomes text)
--     1   only when two or more windows are open
--     2   every window gets its own status line   (vim's default)
--     3   a single GLOBAL status line at the bottom, shared by all windows
--
-- This config keeps the rich %-format 'statusline' below (so you can see what
-- each mode does to a real, styled line) and binds <leader>0..3 to switch modes
-- live. Open a split (<C-w>s) to feel the difference between modes 1, 2 and 3.

vim.g.mapleader = " "

--------------------------------------------------------------------------------
-- A small styled statusline so the modes are visually obvious. (Same engine the
-- per-window AND the global (mode 3) bar run through — one drives both.)
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "StlMode", { fg = "#1a1b26", bg = "#7aa2f7", bold = true })
vim.api.nvim_set_hl(0, "StlFile", { fg = "#c0caf5", bg = "#3b4261" })

local MODES = { n = "NORMAL", i = "INSERT", v = "VISUAL", V = "V-LINE", R = "REPLACE", c = "COMMAND" }

function _G.statusline()
  local mode = MODES[vim.fn.mode()] or vim.fn.mode():upper()
  local tail = vim.fn.expand("%:t")
  if tail == "" then tail = "[No Name]" end
  local line, col = vim.fn.line("."), vim.fn.col(".")
  return table.concat({
    -- The mode block is a click region (`%@v:lua.fn@…%X`): clicking it cycles
    -- 'laststatus'. Click regions work on the per-window status lines (modes 1/2)
    -- AND on this single global bar (mode 3) — the same engine drives both.
    "%@v:lua.on_mode_click@%#StlMode# ", mode, " %X",
    "%#StlFile# ", tail, "%m ",
    "%#StatusLine#%=",
    "%#StlMode# ", tostring(line), ":", tostring(col), " ",
    "  ls=", tostring(vim.o.laststatus), " ",
  })
end

-- Cycle 'laststatus' 3 → 2 → 1 → 0 → 3 when the mode block is clicked. `on click`
-- handlers get (minwid, clicks, button, modifiers); here we ignore them all.
function _G.on_mode_click()
  vim.o.laststatus = (vim.o.laststatus + 1) % 4
  vim.notify("laststatus = " .. vim.o.laststatus)
end

vim.o.statusline = "%!v:lua.statusline()"

--------------------------------------------------------------------------------
-- Start in mode 3 (the new global bar) so it is the first thing you see, then
-- bind <leader>0..3 to switch between every mode without leaving the editor.
--------------------------------------------------------------------------------
vim.o.laststatus = 3

for _, n in ipairs({ 0, 1, 2, 3 }) do
  vim.keymap.set("n", "<leader>" .. n, function()
    vim.o.laststatus = n
    vim.notify("laststatus = " .. n)
  end, { desc = "set laststatus=" .. n })
end
