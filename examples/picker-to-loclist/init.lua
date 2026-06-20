-- ~~~ nxvim: send search results to quickfix / location-list dock tabs ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/picker-to-loclist \
--       cargo run -p nxvim -- examples/picker-to-loclist/sample.txt
--
-- nxvim's port of telescope's "send results to a loclist" — made better: by
-- default each send opens as its own TAB in the BOTTOM DOCK, so you can save
-- several searches side by side, and pressing <CR> on an entry jumps into the
-- MAIN editing area (the dock stays put). `:set noqfdock` gives the classic vim /
-- telescope behavior instead (a bottom split, one list, replaced in place).
--
-- See docs/features/quickfix-dock-lists.md and docs/features/picker.md.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. The option that picks the behavior.
--    'qfdock' is ON by default (the nxvim way: dock tabs). Toggle it with \qd to
--    feel the difference — re-run a search and watch it open in a split instead.
--------------------------------------------------------------------------------
-- nx.o.qfdock = false        -- uncomment for the classic split behavior
nx.keymap.set("n", "<leader>qd", function()
  nx.o.qfdock = not nx.o.qfdock
  nx.notify("qfdock = " .. tostring(nx.o.qfdock))
end, { desc = "Toggle quickfix dock tabs vs split" })

--------------------------------------------------------------------------------
-- 2. From a picker: <C-q> sends results to a location list; <Tab> multi-selects.
--    The shipped sources work out of the box; here are the leader maps.
--      \ff  files      \fg  live_grep      \fb  buffers
--    In the open picker:
--      <Tab>   mark / unmark this row (and advance) — multi-select
--      <C-q>   send results to a loclist: the MARKED rows if any, else all
--              the rows currently matching your query (not every candidate)
--    With 'qfdock' on, each <C-q> opens the results as a new bottom-dock tab.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>ff", function()
  nx.picker.open("files")
end, { desc = "Find files" })
nx.keymap.set("n", "<leader>fg", function()
  nx.picker.open("live_grep")
end, { desc = "Live grep" })
nx.keymap.set("n", "<leader>fb", function()
  nx.picker.open("buffers")
end, { desc = "Buffers" })

-- A custom source over the example files, so <C-q> has something to send even
-- without `rg` installed. Each item carries a file `path` + 1-based `row` so the
-- loclist entries are jumpable.
nx.picker.source({
  name = "marks",
  preview = "location",
  items = function(ctx)
    local here = nx.fs and nx.fs.dirname and nx.fs.dirname(nx.buf.name()) or "."
    for _, e in ipairs({
      { text = "sample.txt:7  the dock model", path = "sample.txt", row = 7 },
      { text = "sample.txt:9  multi-select", path = "sample.txt", row = 9 },
      { text = "notes.txt:3   send_to_loclist", path = "notes.txt", row = 3 },
      { text = "notes.txt:7   add_to_loclist", path = "notes.txt", row = 7 },
    }) do
      ctx.push({ text = e.text, path = here .. "/" .. e.path, row = e.row, col = 1 })
    end
  end,
  confirm = function(item)
    nx.picker.edit(item)
  end,
})
nx.keymap.set("n", "<leader>fm", function()
  nx.picker.open("marks")
end, { desc = "Custom 'marks' picker (try <Tab> then <C-q>)" })

--------------------------------------------------------------------------------
-- 3. The nx.qf.* API directly — build a list yourself, no picker needed.
--      nx.qf.send_to_loclist(list, { title })   a NEW loclist  (dock: new tab)
--      nx.qf.add_to_loclist(list, { title })    APPEND to the focused loclist
--      nx.qf.send_to_qflist(list, { title })    the global quickfix list
--      nx.qf.add_to_qflist(list, { title })     APPEND to the quickfix list
--    \lt sends every TODO line in the current buffer to a saved loclist tab.
--------------------------------------------------------------------------------
nx.keymap.set("n", "<leader>lt", function()
  local items = {}
  for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    if line:find("TODO") then
      items[#items + 1] = { filename = nx.buf.name(), lnum = i, col = 1, text = line }
    end
  end
  if #items == 0 then
    nx.notify("no TODO lines in this buffer")
  else
    nx.qf.send_to_loclist(items, { title = "TODOs" })
  end
end, { desc = "Send TODO lines to a loclist tab" })

-- Step through whichever list is current with the usual commands:
--   :lnext / :lprev   (location list)     :cnext / :cprev   (quickfix)
--   <CR> in the list  jump to the entry (into the main area when docked)
--   :lclose / :cclose close the list (its dock tab, or the split)
nx.keymap.set("n", "]l", function()
  vim.cmd("lnext")
end, { desc = "Next loclist entry" })
nx.keymap.set("n", "[l", function()
  vim.cmd("lprev")
end, { desc = "Prev loclist entry" })
