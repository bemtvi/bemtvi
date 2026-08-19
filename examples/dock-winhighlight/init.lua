-- ~~~ bemtvi dock winhighlight: paint a dock like a VSCode sidebar ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/dock-winhighlight \
--       cargo run -p bemtvi -- examples/dock-winhighlight/sample.txt
--
-- `'winhighlight'` is vim's per-window highlight-group REMAP: while a window is
-- rendered, every group on the left of a `from:to` pair resolves to the group on
-- its right — in THAT window only, leaving the rest of the editor untouched. It is
-- the mechanism behind the "dimmer sidebar" look: a file-tree panel painted on a
-- slightly different background from the code you are editing.
--
-- bemtvi exposes it on the dock scope: `btv.dock.opt(side).winhighlight = "..."`
-- applies the remap to every window in that dock. The value is a comma-separated
-- list of `FromGroup:ToGroup` pairs.
--
-- TRY IT interactively (once it is open):
--   <C-w><C-w>h   cross focus INTO the left sidebar dock
--   <C-w><C-w>l   cross back to the main area — note the background flips
--   :DockToggle left   collapse / restore the styled sidebar
--
-- Compare the two regions: the main area uses the global `Normal`; the left dock
-- uses `NormalSB` instead, with a dimmer end-of-buffer marker and gutter. Only the
-- dock is recolored — the same buffer shown in the main area would use the global
-- theme.

--------------------------------------------------------------------------------
-- 1. Define the sidebar highlight groups the remap points AT. `winhighlight` only
--    renames groups; the target groups must exist (here we define them outright so
--    the example works with no colorscheme loaded). A real config would let its
--    theme define `NormalSB` etc., or link them to theme groups.
--------------------------------------------------------------------------------
btv.hl.define(0, "NormalSB", { bg = "#181825", fg = "#cdd6f4" }) -- the sidebar body
btv.hl.define(0, "SidebarEob", { fg = "#313244" }) -- dim the trailing ~ fillers
btv.hl.define(0, "SidebarLineNr", { fg = "#45475a" }) -- a quieter number gutter

--------------------------------------------------------------------------------
-- 2. Open a left dock and a titled, always-on strip — the file-tree panel stand-in.
--------------------------------------------------------------------------------
btv.dock.open({ side = "left", size = 30 })
btv.dock.opt("left").title = "EXPLORER"
btv.dock.opt("left").showtabline = 2

--------------------------------------------------------------------------------
-- 3. The star of the example: remap the dock's chrome so it reads as a sidebar.
--    `Normal:NormalSB`        → the whole dock paints on the sidebar background
--    `EndOfBuffer:SidebarEob` → the ~ fillers below the content fade out
--    `LineNr:SidebarLineNr`   → a quieter line-number gutter inside the dock
--
--    Equivalent inline form: `btv.dock.open{ side = "left", size = 30,
--      winhighlight = "Normal:NormalSB,EndOfBuffer:SidebarEob,LineNr:SidebarLineNr" }`.
--------------------------------------------------------------------------------
btv.dock.opt("left").winhighlight = "Normal:NormalSB,EndOfBuffer:SidebarEob,LineNr:SidebarLineNr"

-- Give the sidebar some content so the background is obvious.
--
-- `btv.dock.open` QUEUES the dock — the ops are applied after this chunk runs (the
-- editor's "Lua queues, core mutates" flow), so the dock's scratch buffer does not
-- exist yet and the current buffer is still the one you launched with. Reading it
-- here would write this listing into the file you are editing. `btv.on_next_tick`
-- runs on the next loop turn, once the dock is up and focused — and it is
-- `on_next_tick`, not `btv.schedule`, because the dock's buffer only appears
-- BETWEEN ticks.
btv.on_next_tick(function()
  local sidebar = vim.api.nvim_get_current_buf()
  vim.api.nvim_buf_set_lines(sidebar, 0, -1, false, {
    "  src/",
    "    main.rs",
    "    lib.rs",
    "  Cargo.toml",
    "  README.md",
  })
  -- …and land back in the main area, so the cursor starts in the file you opened
  -- rather than in the sidebar.
  btv.layer.main()
end)

-- winhighlight is per-WINDOW, not per-buffer: it is also exposed on `btv.wo`, so a
-- single window (not in any dock) can remap its own groups the same way. Docks are
-- just the most common use.
