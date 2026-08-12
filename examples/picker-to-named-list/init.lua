-- ~~~ bemtvi: send search results to named-list / quickfix dock tabs ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/picker-to-named-list \
--       cargo run -p bemtvi -- examples/picker-to-named-list/sample.txt
--
-- bemtvi's port of telescope's "send results to a loclist" — made better: by
-- default each send opens as its own TAB in the BOTTOM DOCK, so you can save
-- several searches side by side, and pressing <CR> on an entry jumps into the
-- MAIN editing area (the dock stays put). `:set noqfdock` gives the classic vim /
-- telescope behavior instead (a bottom split, one list, replaced in place).
--
-- See docs/features/quickfix-dock-lists.md and docs/features/picker.md.

vim.g.mapleader = "\\"

--------------------------------------------------------------------------------
-- 1. The option that picks the behavior.
--    'qfdock' is ON by default (the bemtvi way: dock tabs). Toggle it with \qd to
--    feel the difference — re-run a search and watch it open in a split instead.
--------------------------------------------------------------------------------
-- btv.o.qfdock = false        -- uncomment for the classic split behavior
btv.keymap.set("n", "<leader>qd", function()
  btv.o.qfdock = not btv.o.qfdock
  btv.notify("qfdock = " .. tostring(btv.o.qfdock))
end, { desc = "Toggle quickfix dock tabs vs split" })

--------------------------------------------------------------------------------
-- 2. From a picker: <C-q> sends results to a NAMED LIST; <Tab> multi-selects.
--    The shipped sources work out of the box; here are the leader maps.
--      \ff  files      \fg  live_grep      \fb  buffers
--    In the open picker:
--      <Tab>   mark / unmark this row (and advance) — multi-select
--      <C-q>   send results to the named list `<picker>:<query>`: the MARKED rows
--              if any, else all the rows currently matching your query (not every
--              candidate). Each distinct search is its own persistent dock tab
--              (re-running the same search updates it in place); it never collides
--              with the quickfix and survives closing the window you sent it from.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>ff", function()
  btv.picker.open("files")
end, { desc = "Find files" })
btv.keymap.set("n", "<leader>fg", function()
  btv.picker.open("live_grep")
end, { desc = "Live grep" })
btv.keymap.set("n", "<leader>fb", function()
  btv.picker.open("buffers")
end, { desc = "Buffers" })

-- A custom source over the example files, so <C-q> has something to send even
-- without `rg` installed. Each item carries a file `path` + 1-based `row` so the
-- named-list entries are jumpable.
btv.picker.source({
  name = "marks",
  preview = "location",
  items = function(ctx)
    local here = btv.fs and btv.fs.dirname and btv.fs.dirname(btv.buf.name()) or "."
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
    btv.picker.edit(item)
  end,
})
btv.keymap.set("n", "<leader>fm", function()
  btv.picker.open("marks")
end, { desc = "Custom 'marks' picker (try <Tab> then <C-q>)" })

--------------------------------------------------------------------------------
-- 3. The btv.qf.* API directly — build a list yourself, no picker needed.
--      btv.qf.send_to_loclist(list, { title })   the current window's loclist (split)
--      btv.qf.add_to_loclist(list, { title })    APPEND to the window's loclist
--      btv.qf.send_to_qflist(list, { title })    the global quickfix list (dock tab)
--      btv.qf.add_to_qflist(list, { title })     APPEND to the quickfix list
--    A loclist keeps vim behavior (a split, owned by the window). For a persistent,
--    named dock tab use btv.qf.list/show as in section 2. \lt sends this buffer's
--    TODO lines to the window's loclist.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>lt", function()
  local items = {}
  for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    if line:find("TODO") then
      items[#items + 1] = { filename = btv.buf.name(), lnum = i, col = 1, text = line }
    end
  end
  if #items == 0 then
    btv.notify("no TODO lines in this buffer")
  else
    btv.qf.send_to_loclist(items, { title = "TODOs" })
  end
end, { desc = "Send TODO lines to the window's loclist" })

-- Step through whichever list is current with the usual commands:
--   :lnext / :lprev   (location list)     :cnext / :cprev   (quickfix)
--   <CR> in the list  jump to the entry (into the main area)
--   :lclose / :cclose close the list (the loclist split, or the quickfix dock tab)
btv.keymap.set("n", "]l", function()
  vim.cmd("lnext")
end, { desc = "Next loclist entry" })
btv.keymap.set("n", "[l", function()
  vim.cmd("lprev")
end, { desc = "Prev loclist entry" })
