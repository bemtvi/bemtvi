-- ~~~ bemtvi windows playground: splits, the layout tree, and the nvim_win_* API ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/windows \
--       cargo run -p bemtvi -- examples/windows/sample.txt
--
-- A "window" is a viewport onto a buffer; splitting tiles more of them. This
-- config wires up the window autocmds and a few helper commands so you can watch
-- the window lifecycle and drive the layout from Lua.
--
-- TRY IT interactively:
--   <C-w>s / <C-w>v   split the focused window (horizontal / vertical)
--   <C-w>h/j/k/l      move focus by direction;  <C-w>w cycles
--   <C-w>+ <C-w>-     grow / shrink height;  <C-w>< <C-w>>  width  (take a count!)
--   <C-w>=            equalize;   <C-w>_ / <C-w>|  maximize height / width
--   <C-w>c            close the focused window;  <C-w>o  keep only it
--   :split / :vsplit / :new / :vnew / :only / :resize 12 / :vertical resize 30
--   :q                closes a window when several are open; quits on the last
--
-- Watch the MESSAGE LINE (and `:messages` for the history): every split, focus
-- move, resize, and close announces itself through the window autocmds below.

--------------------------------------------------------------------------------
-- 1. Window lifecycle autocmds. `args.match` is the window id that fired; the
--    server emits these from the same diff as the buffer events, ordered
--    WinLeave -> (buffer events) -> WinEnter around a focus change.
--------------------------------------------------------------------------------
_G.win_log = {}
local function rec(tag)
  return function(a)
    _G.win_log[#_G.win_log + 1] = tag .. ":" .. tostring(a.match)
    vim.notify("[" .. tag .. "] window " .. tostring(a.match))
  end
end
vim.api.nvim_create_autocmd("WinNew", { callback = rec("WinNew") })
vim.api.nvim_create_autocmd("WinEnter", { callback = rec("WinEnter") })
vim.api.nvim_create_autocmd("WinLeave", { callback = rec("WinLeave") })
vim.api.nvim_create_autocmd("WinClosed", { callback = rec("WinClosed") })
vim.api.nvim_create_autocmd("WinResized", { callback = rec("WinResized") })

--------------------------------------------------------------------------------
-- 2. :WinList — the programmatic read surface. Lists every window with its
--    buffer, cursor, and size, resolved from the live snapshot.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("WinList", function()
  local cur = vim.api.nvim_get_current_win()
  local lines = {}
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    local pos = vim.api.nvim_win_get_cursor(w)
    lines[#lines + 1] = string.format(
      "win %d%s  buf=%d  cursor=%d,%d  %dx%d",
      w, w == cur and " (current)" or "",
      vim.api.nvim_win_get_buf(w),
      pos[1], pos[2],
      vim.api.nvim_win_get_width(w), vim.api.nvim_win_get_height(w)
    )
  end
  vim.notify(table.concat(lines, "  |  "))
end, {})

--------------------------------------------------------------------------------
-- 3. :WinDemo — the programmatic write surface. Opens a vertical split on the
--    current buffer, parks its cursor a few lines down, then reports the layout.
--    (Mutation from Lua goes through `vim.cmd` — the "Lua queues, core mutates"
--    flow — so the new window only exists on the NEXT tick; read its id there
--    with btv.on_next_tick.)
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("WinDemo", function()
  vim.cmd("vsplit")
  btv.on_next_tick(function()
    local win = vim.api.nvim_get_current_win()
    btv.win.set_cursor(win, 3, 0)
    vim.notify("[WinDemo] opened window " .. tostring(win)
      .. "; now " .. #vim.api.nvim_list_wins() .. " windows. Try :WinList")
  end)
end, {})

-- Hybrid line numbers make it easy to see each window keeps its own cursor/view.
vim.o.number = true
vim.o.relativenumber = true
