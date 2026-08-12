-- ~~~ bemtvi btv.view playground: a plugin-owned, dockable content surface ~~~
--
-- Run it (from the repo root):
--
--     BEMTVI_CONFIG=examples/btvview \
--       cargo run -p bemtvi -- examples/btvview/sample.txt
--
-- `btv.view` is the read-only, plugin-controlled content surface that generalizes
-- the bottom panel: an inert buffer whose lines a plugin sets, that mounts in a
-- dock or a split, decorates with extmarks, and whose `<CR>` dispatches to an
-- `on_select` callback. It is the surface a pure-Lua file tree / symbol list / any
-- line-oriented widget is built on. This tiny example is a *fixed* file list — a
-- full lazy filesystem tree would be a separate plugin built on exactly this.
--
-- TRY IT interactively:
--   <leader>e   toggle the view in the left dock
--   j / k       move within the list
--   <CR>        open the entry in the MAIN editor (not inside the sidebar)
--
-- The key trick: `<CR>` runs `btv.open(path, { where = "main" })`, which crosses
-- focus back to the main editor area before opening — so a file opened from the
-- sidebar lands in the editor, not in the sidebar itself.

--------------------------------------------------------------------------------
-- A fixed list of entries to show (label -> path opened on <CR>). A real tree
-- would build this from `btv.fs.readdir`, lazily, on expand.
local ENTRIES = {
  { label = "  sample.txt", path = "examples/btvview/sample.txt" },
  { label = "  init.lua", path = "examples/btvview/init.lua" },
  { label = "  README (this repo)", path = "README.md" },
}

-- A highlight + namespace for the per-line decoration (the leading glyph color).
btv.hl.define(0, "BtvViewIcon", { fg = "#89b4fa" })
local DECOR_NS = btv.ns.create("btvview-example")

-- Build (once) the view: set its lines + parallel userdata, decorate each line's
-- icon, and wire `<CR>` to open the entry's path in the main area.
local function build_view()
  local v = btv.view.create({ name = "btv-view", filetype = "btvview" })

  local lines, userdata, marks = {}, {}, {}
  for i, e in ipairs(ENTRIES) do
    lines[i] = e.label
    userdata[i] = e.path
    -- Color the two-cell leading glyph on each row.
    marks[i] = { line = i - 1, col = 0, end_row = i - 1, end_col = 3, hl_group = "BtvViewIcon" }
  end

  v:set_lines(lines)
  v:set_userdata(userdata)
  v:on_select(function(_, path)
    if path then
      btv.open(path, { where = "main" })
    end
  end)
  v:mount({ dock = "left", size = 32 })
  -- Decoration goes through the extmark layer, which needs the view's real buffer
  -- number — available only on the next tick (after the create/mount ops drain and
  -- the `btv._view_buf` mirror is pushed). A real tree decorates from its async
  -- render anyway; here we just defer one tick with btv.schedule.
  btv.schedule(function()
    v:set_decor(DECOR_NS, marks)
  end)
  -- Land focus back in the main editor after the initial mount, so the cursor
  -- doesn't start in the sidebar.
  btv.layer.main()
  return v
end

-- Lazily build + toggle. The first `<leader>e` builds and mounts the view; later
-- presses toggle the dock's visibility (its content is preserved).
local view = nil
btv.keymap.set("n", "<leader>e", function()
  if view == nil then
    view = build_view()
  else
    btv.dock.toggle("left")
  end
end, { desc = "Toggle the btv.view sidebar" })

-- Open it once at startup so the playground shows something immediately.
view = build_view()
