-- ~~~ bemtvi docks playground: permanent VSCode-style edge panels ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/dock \
--       cargo run -p bemtvi -- examples/dock/sample.txt
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
--   :DockToggle left       collapse the dock from view / bring it back
--   <leader>e              toggle the left explorer (the same, by keymap)
--
-- TOGGLE vs CLOSE: :DockToggle (and btv.dock.hide/show) collapse a dock from view
-- while KEEPING its content — its splits, tabs, cursor and text all come back when
-- you toggle it open again. :DockClose instead drops the content. The bottom tray
-- below is set `autohide` — it collapses by itself the moment focus leaves it, and
-- pops back when you cross into it again.
--
-- A collapsed dock isn't gone: it shows a ▸LABEL chip on the command-line row
-- (bottom-left, when idle). Click the chip to bring that dock back.
--
-- The double <C-w><C-w> is the LAYER switch (main <-> docks); a single <C-w>
-- always acts within the layer you are focused in. Each dock starts on an empty
-- scratch buffer — focus one with <C-w><C-w>{h,j,k,l} and just start typing.

--------------------------------------------------------------------------------
-- Open a left side bar and a bottom tray at startup. `btv.dock.open` takes `side`
-- (left/right/top/bottom), an optional `size` (columns for left/right, rows for
-- top/bottom), and an optional `buf` (an existing buffer to show; default: a fresh
-- scratch). The ops are queued and applied after this chunk runs — the editor's
-- "Lua queues, core mutates" flow — so the docks appear on the first frame.
--------------------------------------------------------------------------------
btv.dock.open({ side = "left", size = 28 })
btv.dock.open({ side = "bottom", size = 6 })

--------------------------------------------------------------------------------
-- Per-dock OPTIONS (the dock scope, alongside btv.bo / btv.wo / btv.o). Set them
-- inline in `btv.dock.open{...}` or after the fact via `btv.dock.opt(side)`:
--
--   showtabline  per-dock override of the global option (0 never / 1 if >1 tab /
--                2 always) — e.g. always show the explorer's strip
--   title        a fixed strip label, shown ahead of the tab cells
--   size         the dock's width (left/right) or height (top/bottom), settable
--                live so you can grow/shrink a dock after opening
--   winhighlight  per-window highlight remap so a dock paints like a sidebar
--                 (e.g. "Normal:NormalSB") — see examples/dock-winhighlight
--------------------------------------------------------------------------------
-- A titled, always-on strip for the side bar.
btv.dock.opt("left").title = "EXPLORER"
btv.dock.opt("left").showtabline = 2
-- The bottom tray gets a title too; open a second tab in it (`:tabnew` while it
-- is focused) and its strip lights up on its own.
btv.dock.opt("bottom").title = "TERMINAL"
-- ...and `autohide`: it collapses the instant focus leaves it, and re-appears when
-- you cross back in (`<C-w><C-w>j`) or `:DockShow bottom`. Great for a panel you
-- want out of the way until you need it.
btv.dock.opt("bottom").autohide = true

-- `:DockGrow {side} {n}` — resize a dock live through the `size` option.
btv.command("DockGrow", function(o)
  local side = o.fargs[1] or "left"
  local n = tonumber(o.fargs[2]) or 40
  btv.dock.opt(side).size = n
end)

--------------------------------------------------------------------------------
-- Toggle the left explorer from a keymap, using the built-in `btv.dock.toggle`
-- (the same path as the `:DockToggle` ex-command). A hidden dock keeps its
-- content; toggling it back restores exactly what was there.
--------------------------------------------------------------------------------
vim.g.mapleader = " "
vim.keymap.set("n", "<leader>e", function()
  btv.dock.toggle("left")
end, { desc = "toggle the left explorer dock" })

vim.o.number = true
