-- The tabline-relevant subset of a real ~/.config/nvim/lua/myutils.lua, trimmed
-- to what a custom 'tabline' needs so it runs under bemtvi standalone (the full
-- file pulls in third-party helper libraries, none of which exist here). The
-- functions below are copied verbatim from that config — this is the actual
-- tabline code, not a bemtvi-specific rewrite.

local M = {}

-- The last `x` elements of a list, in order.
local function get_last_x(my_list, x)
  local len = #my_list
  local start_index = math.max(1, len - x + 1)
  local last_x = {}
  for i = start_index, len do
    table.insert(last_x, my_list[i])
  end
  return last_x
end
M.get_last_x = get_last_x

-- Join `arr` with `chr`, optionally mapping each element through `fn(part, i)`.
-- Uses vim.spairs so iteration order is stable.
local function str_join(chr, arr, fn)
  if #arr == 0 then
    return ""
  end
  local rest = ""
  for i, p in vim.spairs(arr) do
    rest = rest .. ((i > 1) and chr or "") .. (fn ~= nil and fn(p, i) or p)
  end
  return rest
end
M.str_join = str_join

-- Buffer names that shouldn't be chosen as a tab's label (side panels etc.).
local IGNORE_BUF_NAME = { "^NvimTree_[0-9]+", "^undotree_", "^diffpanel_" }

local function match_any(patterns, cmp)
  for i = 1, #patterns do
    if cmp:match(patterns[i]) then
      return true
    end
  end
  return false
end
M.match_any = match_any

-- The label for tab page `n`: skip ignored side-panel buffers, then show the
-- file tail (truncated past 20 cols), a `*` when modified, and a parenthesised
-- 3-char-per-segment hint of the parent directories.
local function my_tab_label(n)
  local buflist = vim.fn.tabpagebuflist(n)
  local i = 1
  while i <= #buflist and match_any(IGNORE_BUF_NAME, vim.fn.bufname(buflist[i])) do
    i = i + 1
  end

  if i > #buflist then
    i = #buflist
  end

  local bufnr = buflist[i]
  local bufname = vim.fn.bufname(bufnr)
  if #bufname == 0 then
    bufname = "[No Name]"
  end

  local parts = get_last_x(vim.split(bufname, "/"), 3)
  local fname = table.remove(parts)
  if #fname > 20 then
    fname = string.sub(fname, 0, 19) .. "…"
  end

  local rest = str_join("/", parts, function(part)
    return part:sub(1, 3)
  end)
  if #rest > 0 then
    rest = "(" .. rest .. ")"
  end
  return n .. ":" .. fname .. (vim.bo[bufnr].modified and "*" or "") .. rest
end
M.my_tab_label = my_tab_label

-- The whole 'tabline': one %nT-delimited, %#TabLine(Sel)#-coloured label per tab
-- (each label produced by a %{} expression calling my_tab_label), a %#TabLineFill#
-- spacer, and — with more than one tab — a right-aligned %999X "close" region.
local function my_tab_line()
  local s = ""
  local tabnr_last = vim.fn.tabpagenr("$")
  local tabnr_current = vim.fn.tabpagenr()

  for i = 1, tabnr_last do
    if i == tabnr_current then
      s = s .. "%#TabLineSel#"
    else
      s = s .. "%#TabLine#"
    end
    s = s .. "%" .. i .. "T"
    s = s .. " %{v:lua.require('myutils').my_tab_label(" .. i .. ")}"
  end

  s = s .. "%#TabLineFill#%T"

  if tabnr_last > 1 then
    s = s .. "%=%#TabLine#%999Xclose"
  end

  return s
end
M.my_tab_line = my_tab_line

return M
