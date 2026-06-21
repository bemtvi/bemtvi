-- ~~~ nxvim nx.view playground: a plugin-owned, dockable content surface ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/nxview \
--       cargo run -p nxvim -- examples/nxview/sample.txt
--
-- `nx.view` is the read-only, plugin-controlled content surface that generalizes
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
-- The key trick: `<CR>` runs `nx.open(path, { where = "main" })`, which crosses
-- focus back to the main editor area before opening — so a file opened from the
-- sidebar lands in the editor, not in the sidebar itself.

--------------------------------------------------------------------------------
-- A fixed list of entries to show (label -> path opened on <CR>). A real tree
-- would build this from `nx.fs.readdir`, lazily, on expand.
local ENTRIES = {
  { label = "  sample.txt", path = "examples/nxview/sample.txt" },
  { label = "  init.lua", path = "examples/nxview/init.lua" },
  { label = "  README (this repo)", path = "README.md" },
}

-- A highlight + namespace for the per-line decoration (the leading glyph color).
nx.hl.define(0, "NxViewIcon", { fg = "#89b4fa" })
local DECOR_NS = nx.ns.create("nxview-example")

-- Build (once) the view: set its lines + parallel userdata, decorate each line's
-- icon, and wire `<CR>` to open the entry's path in the main area.
local function build_view()
  local v = nx.view.create({ name = "nx-view", filetype = "nxview" })

  local lines, userdata, marks = {}, {}, {}
  for i, e in ipairs(ENTRIES) do
    lines[i] = e.label
    userdata[i] = e.path
    -- Color the two-cell leading glyph on each row.
    marks[i] = { line = i - 1, col = 0, end_row = i - 1, end_col = 3, hl_group = "NxViewIcon" }
  end

  v:set_lines(lines)
  v:set_userdata(userdata)
  v:on_select(function(_, path)
    if path then
      nx.open(path, { where = "main" })
    end
  end)
  v:mount({ dock = "left", size = 32 })
  -- Decoration goes through the extmark layer, which needs the view's real buffer
  -- number — available only on the next tick (after the create/mount ops drain and
  -- the `nx._view_buf` mirror is pushed). A real tree decorates from its async
  -- render anyway; here we just defer one tick with nx.schedule.
  nx.schedule(function()
    v:set_decor(DECOR_NS, marks)
  end)
  -- Land focus back in the main editor after the initial mount, so the cursor
  -- doesn't start in the sidebar.
  nx.layer.main()
  return v
end

-- Lazily build + toggle. The first `<leader>e` builds and mounts the view; later
-- presses toggle the dock's visibility (its content is preserved).
local view = nil
nx.keymap.set("n", "<leader>e", function()
  if view == nil then
    view = build_view()
  else
    nx.dock.toggle("left")
  end
end, { desc = "Toggle the nx.view sidebar" })

-- Open it once at startup so the playground shows something immediately.
view = build_view()
