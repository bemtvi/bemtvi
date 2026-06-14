-- ~~~ nxvim per-region tab pages: every region has its own tabline ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/per-region-tabs \
--       cargo run -p nxvim -- examples/per-region-tabs/sample.txt
--
-- nxvim's tab pages are PER REGION. The main editor area and *each* open dock
-- (the VSCode-style edge panels — see examples/dock/) carry their own independent
-- set of regular vim tab pages, each drawn as its OWN tabline:
--
--   * main's tabline is the top bar of the editor (below a top dock, if any);
--   * each dock's tabline is the first row of that dock's band.
--
-- Tab operations act on the FOCUSED region: `:tabnew` / `gt` / `gT` / `:tabclose`
-- add to / cycle / close the tabs of whichever region holds focus. Switch focus
-- between regions with the doubled window key (`<C-w><C-w>{h,j,k,l}`), and the
-- same tab keys now drive that region's own tab stack — main's tabs and a dock's
-- tabs never interfere.
--
-- TRY IT interactively:
--   <C-w><C-w>h            focus the LEFT dock   (j/k/l → bottom/top/right)
--   :tabnew               add a tab to the FOCUSED region only
--   gt / gT               cycle the FOCUSED region's tabs (next / previous)
--   <C-w><C-w>l           cross back to the main area; gt now cycles MAIN's tabs
--   <click a tabline>     a LEFT-CLICK on any region's tabline cell switches THAT
--                         region to the clicked tab and moves focus into it
--   :tabclose             close the focused tab (a dock's last tab closes the dock)
--
-- The closing-a-dock's-last-tab rule mirrors closing a window: the dock folds
-- away and the main area reclaims the space. Main always keeps at least one tab.

--------------------------------------------------------------------------------
-- Open a left side bar and a bottom tray, each with an always-on, titled strip so
-- the per-region tablines are visible from the very first frame (even before you
-- add a second tab). `showtabline = 2` = always show this region's tabline;
-- the global default (1) would only show it once a region has >1 of its own tabs.
--------------------------------------------------------------------------------
nx.dock.open({ side = "left", size = 28, title = "EXPLORER", showtabline = 2 })
nx.dock.open({ side = "bottom", size = 8, title = "TERMINAL", showtabline = 2 })

-- Main's tabline always on too, so all three regions show their strips at once.
vim.o.showtabline = 2
vim.o.number = true

--------------------------------------------------------------------------------
-- :T {n} — add `n` tabs (default 1) to the region that currently has focus. A
-- convenience for watching one region's tabline fill independently of the others:
--
--     <C-w><C-w>j     focus the bottom tray
--     :T 3            give it three extra tabs (the tray's strip, not main's)
--     <C-w><C-w>l     cross back to main; :T 1 now adds a MAIN tab
--
-- It just runs `:tabnew` against the focused region — `:tabnew` already targets
-- whichever region holds focus, which is the whole point of per-region tabs. (We
-- deliberately don't switch focus *inside* this command: a config callback queues
-- its `nx.dock.focus` and its `:tabnew` on separate effect queues that drain in a
-- fixed order, so focus + tab edits can't be interleaved from one callback — do
-- the focus switch with the interactive `<C-w><C-w>` keys instead.)
--------------------------------------------------------------------------------
nx.command("T", function(o)
  local n = tonumber(o.fargs[1]) or 1
  for _ = 1, n do
    vim.cmd("tabnew")
  end
end)
