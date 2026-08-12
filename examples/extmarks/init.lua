-- ~~~ bemtvi extmarks playground: paint highlights from Lua, watch them track ~~~
--
-- This drives the decoration layer — neovim's extmark API on bemtvi. An *extmark*
-- anchors a highlight group to a byte range in a buffer; it shifts with edits and
-- is grouped under a *namespace* you can clear all at once. LSP semantic tokens,
-- git-status-gutter plugins, and diagnostics-as-marks are all built on this surface.
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/extmarks \
--       cargo run -p bemtvi -- examples/extmarks/sample.txt
--
-- On startup a few marks are painted (see below). Then try the commands.

--------------------------------------------------------------------------------
-- Define the highlight groups our marks reference, so they paint in real colors.
-- (Without a colorscheme the client would fall back to a built-in style.)
--------------------------------------------------------------------------------
vim.api.nvim_set_hl(0, "ExtNote", { fg = "#89b4fa" }) -- blue
vim.api.nvim_set_hl(0, "ExtTodo", { fg = "#1e1e2e", bg = "#f9e2af" }) -- on yellow
vim.api.nvim_set_hl(0, "ExtWarn", { fg = "#f38ba8", bold = true }) -- red, bold

-- One namespace owns every mark this config sets, so :ExtClear wipes them all.
local ns = vim.api.nvim_create_namespace("playground")

-- Helper: highlight `text`'s first occurrence on line `row` (0-based) in `group`.
local function mark_word(row, text, group)
  local line = vim.api.nvim_buf_get_lines(0, row, row + 1, false)[1]
  if not line then return end
  local s = line:find(text, 1, true)
  if not s then return end
  return vim.api.nvim_buf_set_extmark(0, ns, row, s - 1, {
    end_row = row,
    end_col = s - 1 + #text,
    hl_group = group,
  })
end

--------------------------------------------------------------------------------
-- Startup: paint a handful of marks so highlights show the moment the file opens.
--   * the keywords "extmark" / "namespace" in blue,
--   * the leading "TODO:" / "NOTE:" tags as a tag and a warning.
-- Edit any line (e.g. type at its start) and the highlighted ranges slide to stay
-- on the same text — that's the anchor-shifting, exercised through real edits.
--------------------------------------------------------------------------------
mark_word(1, "extmark", "ExtNote")
mark_word(1, "namespace", "ExtNote")
mark_word(2, "TODO:", "ExtTodo")
mark_word(3, "NOTE:", "ExtWarn")

--------------------------------------------------------------------------------
-- :ExtMark — highlight the word under the cursor in ExtTodo. Move the cursor onto
--   a word and run it; the mark tracks that text as you edit around it.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("ExtMark", function()
  local row = vim.api.nvim_win_get_cursor(0)[1] - 1
  local line = vim.api.nvim_buf_get_lines(0, row, row + 1, false)[1] or ""
  local col = vim.api.nvim_win_get_cursor(0)[2]
  -- Expand to the word around the cursor (simple [%w_] run).
  local s, e = col + 1, col + 1
  while s > 1 and line:sub(s - 1, s - 1):match("[%w_]") do s = s - 1 end
  while e <= #line and line:sub(e, e):match("[%w_]") do e = e + 1 end
  if e <= s then vim.notify("no word under the cursor") return end
  vim.api.nvim_buf_set_extmark(0, ns, row, s - 1, {
    end_row = row, end_col = e - 1, hl_group = "ExtTodo",
  })
  vim.notify(("marked %q"):format(line:sub(s, e - 1)))
end, {})

--------------------------------------------------------------------------------
-- :ExtList — count the marks currently in our namespace (reads them back via
--   nvim_buf_get_extmarks, the same call a plugin uses to inspect its own marks).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("ExtList", function()
  local marks = vim.api.nvim_buf_get_extmarks(0, ns, 0, -1, { details = true })
  vim.notify(("%d extmark(s) in the playground namespace"):format(#marks))
end, {})

--------------------------------------------------------------------------------
-- :ExtClear / <leader>x — clear every mark in the namespace at once.
--------------------------------------------------------------------------------
local function clear_all()
  vim.api.nvim_buf_clear_namespace(0, ns, 0, -1)
  vim.notify("cleared the playground namespace")
end
vim.api.nvim_create_user_command("ExtClear", clear_all, {})
vim.keymap.set("n", "<leader>x", clear_all)

vim.o.number = true

print("extmarks playground: try :ExtMark (on a word), :ExtList, :ExtClear / <leader>x")
