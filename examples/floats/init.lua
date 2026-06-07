-- ~~~ nxvim floating-windows playground: open floats and watch them paint ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/floats \
--       cargo run -p nxvim -- examples/floats/sample.txt
--
-- A *floating* window is a window positioned by absolute coordinates ON TOP of
-- the tiled layout (the kind every plugin uses for hover, completion docs,
-- which-key, telescope, notifications). Unlike a split it steals no space from
-- its neighbours — it paints over them. This config opens a few floats so you
-- can see the border styles, titles, anchors, and zindex stacking in the TUI.
--
-- Each float here binds the CURRENT buffer (nxvim can't yet create a scratch
-- buffer from Lua), so a float shows the same text as the window beneath — the
-- border/title/position is what marks it as a float. The content equality is the
-- point: a float is a real window onto a buffer, just drawn on top.
--
-- TRY IT:
--   :FloatHello     a centered, rounded, titled float          ("hello from a float")
--   :FloatStack     two overlapping floats — higher zindex on top
--   :FloatCursor    a float anchored to the cursor cell (move first, then run it)
--   :FloatClose     close the most-recently opened demo float
--   <leader>x       (same as :FloatClose)  — <leader> is `\` by default
--
-- A small hint float opens on startup (top of the screen) so you see a float
-- immediately, without stealing focus.

--------------------------------------------------------------------------------
-- Keep the demo floats we open so a command / keymap can close them again.
--------------------------------------------------------------------------------
_G.float_wins = {}

local function open_float(opts)
  local enter = opts.enter
  opts.enter = nil
  local win = vim.api.nvim_open_win(0, enter ~= false, opts)
  _G.float_wins[#_G.float_wins + 1] = win
  return win
end

--------------------------------------------------------------------------------
-- :FloatHello — a centered, rounded, titled float. `enter = true` (the default)
-- focuses it, so the cursor lands inside the float's inner area; :FloatClose
-- (or <leader>x) dismisses it.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("FloatHello", function()
  local win = open_float({
    relative = "editor",
    row = 4, col = 18,
    width = 42, height = 8,
    border = "rounded",
    title = "hello from a float",
  })
  vim.notify("opened float " .. win .. "  (:FloatClose / <leader>x to dismiss)")
end, {})

--------------------------------------------------------------------------------
-- :FloatStack — two overlapping floats with different zindex. The lower one is
-- opened LAST, yet the higher-zindex float still sits on top: stacking is by
-- zindex, not creation order.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("FloatStack", function()
  open_float({
    relative = "editor", row = 6, col = 22,
    width = 32, height = 7, zindex = 200,
    border = "single", title = "over (z=200)", enter = false,
  })
  open_float({
    relative = "editor", row = 3, col = 8,
    width = 36, height = 9, zindex = 50,
    border = "double", title = "under (z=50)", enter = false,
  })
  vim.notify("two overlapping floats — the higher zindex paints over the lower")
end, {})

--------------------------------------------------------------------------------
-- :FloatCursor — a float pinned to the cursor cell (relative = "cursor"). Move
-- the cursor somewhere first, then run it: the float opens just below the cursor.
-- It is positioned once, at open time — it does not follow later motions.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("FloatCursor", function()
  open_float({
    relative = "cursor", row = 1, col = 0,
    width = 26, height = 4,
    border = "single", title = "at the cursor",
  })
end, {})

--------------------------------------------------------------------------------
-- :FloatClose / <leader>x — close the most-recently opened demo float.
--------------------------------------------------------------------------------
local function close_last()
  local win = table.remove(_G.float_wins)
  if win then
    vim.api.nvim_win_close(win, true)
    vim.notify("closed float " .. win)
  else
    vim.notify("no demo floats open")
  end
end
vim.api.nvim_create_user_command("FloatClose", close_last, {})
vim.keymap.set("n", "<leader>x", close_last)

--------------------------------------------------------------------------------
-- A hint float on startup: top of the screen, single border, NOT focused, so the
-- buffer keeps the cursor. Proves a float paints over the tiled window with no
-- input from the user.
--------------------------------------------------------------------------------
open_float({
  relative = "editor",
  row = 0, col = 20,
  width = 40, height = 3,
  border = "single",
  title = "nxvim floats",
  enter = false,
})

-- Hybrid line numbers, on in every window (floats included — they share the
-- global options), so you can see the float is a genuine window with its gutter.
vim.o.number = true
vim.o.relativenumber = true
