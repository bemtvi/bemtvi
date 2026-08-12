-- ~~~ bemtvi: named lists (window-independent quickfix-flavored lists) ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/named-lists \
--       cargo run -p bemtvi -- examples/named-lists/sample.txt
--
-- A *named list* is like the global quickfix list — structured entries, its own
-- bottom-dock tab, `<CR>` jumps to the entry in the main editing layer — but there
-- can be MANY, each addressed by a stable NAME. Storage lives on the editor, not a
-- window, so a named list survives closing any window and never collides with the
-- single quickfix list (or with `:grep` / `:make`). That makes it the right home for
-- a persistent plugin panel.
--
--   btv.qf.list(name, items[, opts])   create / replace the list `name` (opts.title,
--                                      opts.action); repaints its tab if open
--   btv.qf.show(name)                   open or focus the list's dock tab
--   btv.qf.drop(name)                   close its tab and forget the list
--
-- You push items whenever your data changes, then show the tab on command — no
-- datasource/refresh indirection. See docs/features/quickfix-dock-lists.md.

vim.g.mapleader = "\\"

-- Collect every TODO/FIXME line in the current buffer into an items array (the same
-- entry-dict shape `setqflist` takes: filename / lnum / col / text / type).
local function todo_items()
  local items = {}
  for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    if line:find("TODO") or line:find("FIXME") then
      local typ = line:find("FIXME") and "E" or "W"
      items[#items + 1] = { filename = btv.buf.name(), lnum = i, col = 1, text = line, type = typ }
    end
  end
  return items
end

-- Lines wider than 40 columns — a SECOND, independent named list. Two named lists sit
-- side by side as separate dock tabs; neither is the quickfix, so a `:grep` later
-- won't clobber either.
local function long_line_items()
  local items = {}
  for i, line in ipairs(vim.api.nvim_buf_get_lines(0, 0, -1, false)) do
    if #line > 40 then
      items[#items + 1] =
        { filename = btv.buf.name(), lnum = i, col = 41, text = (#line .. " cols: " .. line) }
    end
  end
  return items
end

-- \tl — rebuild the "todos" named list from the live buffer and show its tab. Press
-- it again after editing the file: btv.qf.list replaces the contents in place, so the
-- open tab repaints (no stale snapshot, no duplicate tab).
btv.keymap.set("n", "<leader>tl", function()
  btv.qf.list("todos", todo_items(), { title = "TODO / FIXME" })
  btv.qf.show("todos")
end, { desc = "Collect TODO/FIXME into a named list and show it" })

-- \ll — the long-lines named list, shown as its own tab beside "todos".
btv.keymap.set("n", "<leader>ll", function()
  btv.qf.list("long-lines", long_line_items(), { title = "Lines over 40 cols" })
  btv.qf.show("long-lines")
end, { desc = "Collect long lines into a named list and show it" })

-- \td — drop the "todos" list: its dock tab closes and the list is forgotten.
btv.keymap.set("n", "<leader>td", function()
  btv.qf.drop("todos")
end, { desc = "Drop the todos named list" })

-- <CR> on a row jumps to the entry in the main editing layer; the dock tab stays put.
-- Closing the tab (or the window) never destroys a named list — re-show it by name.
