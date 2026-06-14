-- ~~~ nxvim docks playground: permanent VSCode-style edge panels ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/dock \
--       cargo run -p nxvim -- examples/dock/sample.txt
--
-- A *dock* is a permanent, editable window region pinned to a screen edge — like
-- VSCode's side bars and bottom panel. Unlike a normal split, a dock is GLOBAL
-- (it shows on every tab) and is never disturbed by splits / window switches /
-- tab changes in the main editor area. The top dock sits ABOVE the tabline.
--
-- TRY IT interactively:
--   <C-w><C-w>h   cross focus INTO the left dock   (j/k/l → bottom/top/right dock)
--   <C-w><C-w>l   from a dock, cross back to the main area
--   <C-w>v / <C-w>s   while focused in a dock, split WITHIN it (single <C-w>!)
--   <C-w><C-w>v   from the main area, cross to the last dock and split it
--   :DockOpen left 30      open/resize a dock      (:DockClose / :DockFocus {side})
--
-- The double <C-w><C-w> is the LAYER switch (main <-> docks); a single <C-w>
-- always acts within the layer you are focused in. Each dock starts on an empty
-- scratch buffer — focus one with <C-w><C-w>{h,j,k,l} and just start typing.

--------------------------------------------------------------------------------
-- Open a left side bar and a bottom tray at startup. `nx.dock.open` takes `side`
-- (left/right/top/bottom), an optional `size` (columns for left/right, rows for
-- top/bottom), and an optional `buf` (an existing buffer to show; default: a fresh
-- scratch). The ops are queued and applied after this chunk runs — the editor's
-- "Lua queues, core mutates" flow — so the docks appear on the first frame.
--------------------------------------------------------------------------------
nx.dock.open({ side = "left", size = 28 })
nx.dock.open({ side = "bottom", size = 6 })

--------------------------------------------------------------------------------
-- :DockToggle {side} — open the dock (or resize/focus it if already open). Shows
-- that the whole dock surface is driveable from Lua, not just the keymaps.
--------------------------------------------------------------------------------
nx.command("DockToggle", function(o)
  local side = o.fargs[1] or "right"
  nx.dock.open({ side = side, size = 30 })
end)

vim.o.number = true
