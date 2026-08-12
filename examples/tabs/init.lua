-- ~~~ bemtvi tabs playground: tab pages, the tabline, and the nvim_tabpage_* API ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/tabs \
--       cargo run -p bemtvi -- examples/tabs/sample.txt
--
-- A "tab page" is a named collection of windows: each tab owns its own split
-- layout, and only one tab is visible at a time. bemtvi draws a tabline across the
-- top when more than one is open. This config wires the tab autocmds and a couple
-- of helper commands so you can watch the tab lifecycle and drive it from Lua.
--
-- TRY IT interactively:
--   :tabnew / :tabedit FILE   open a new tab (empty / on a file)
--   gt / gT                   next / previous tab;  {count}gt jumps to tab N
--   :tabnext / :tabprevious / :tablast / :tabfirst
--   <C-w>T                    move the focused window to its own new tab
--   :tab split                clone the current buffer + cursor into a new tab
--   :tab edit FILE            open FILE in a new tab (the `:tab {cmd}` modifier)
--   :drop FILE                jump to a window already showing FILE (in any tab),
--                             else :edit it here
--   :tab drop FILE            same, but open FILE in a NEW tab when not shown
--   :tabclose                 close the current tab (refuses the last one)
--   :tabonly                  close every tab but this one
--   :q                        on a tab's last window, closes the TAB (other tabs
--                             remain); only the very last window quits the editor
--   :set showtabline=0/1/2    never / only-with-2+ (default) / always show the bar
--
-- Watch the MESSAGE LINE (and `:messages`): every tab create, switch, and close
-- announces itself through the tab autocmds below, ordered with the window events
-- as TabLeave -> WinLeave -> ... -> WinEnter -> TabEnter.

--------------------------------------------------------------------------------
-- 1. Tab lifecycle autocmds. `args.match` is the tab id that fired. The server
--    emits these from the same diff as the window/buffer events, bracketing the
--    window events around a tab switch.
--------------------------------------------------------------------------------
_G.tab_log = {}
local function rec(tag)
  return function(a)
    _G.tab_log[#_G.tab_log + 1] = tag .. ":" .. tostring(a.match)
    vim.notify("[" .. tag .. "] tab " .. tostring(a.match))
  end
end
vim.api.nvim_create_autocmd("TabNew", { callback = rec("TabNew") })
vim.api.nvim_create_autocmd("TabEnter", { callback = rec("TabEnter") })
vim.api.nvim_create_autocmd("TabLeave", { callback = rec("TabLeave") })
vim.api.nvim_create_autocmd("TabClosed", { callback = rec("TabClosed") })

--------------------------------------------------------------------------------
-- 2. :TabList — the programmatic read surface. Lists every tab page with its
--    1-based number, window count, and focused window, resolved from the live
--    mirror (no RPC round-trip).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TabList", function()
  local cur = vim.api.nvim_get_current_tabpage()
  local lines = {}
  for _, t in ipairs(vim.api.nvim_list_tabpages()) do
    local wins = vim.api.nvim_tabpage_list_wins(t)
    lines[#lines + 1] = string.format(
      "tab %d (id %d)%s  wins=%d  focus=win %d",
      vim.api.nvim_tabpage_get_number(t), t,
      t == cur and " (current)" or "",
      #wins, vim.api.nvim_tabpage_get_win(t)
    )
  end
  vim.notify(table.concat(lines, "  |  "))
end, {})

--------------------------------------------------------------------------------
-- 3. :TabFirst — the programmatic write surface. Jumps to the first tab via
--    nvim_set_current_tabpage (the lone tab mutation; the server queues it and
--    the core switches, the "Lua queues, core mutates" flow).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("TabFirst", function()
  local first = vim.api.nvim_list_tabpages()[1]
  vim.api.nvim_set_current_tabpage(first)
  vim.notify("[TabFirst] now on tab " .. vim.api.nvim_tabpage_get_number(first))
end, {})

-- Always show the tabline, even with a single tab, so the bar is visible the
-- moment you open this config (the default would hide it until the 2nd tab).
vim.o.showtabline = 2
