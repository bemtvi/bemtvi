-- nxtree.render — project the model into view lines + extmark decoration.
--
-- `render(tree)` flattens the (optionally filtered) tree, builds one display line
-- per visible node — `<indent guides><icon> <name>` — pushes them through
-- `view:set_lines` / `view:set_userdata` (userdata[i] is the node, so on_select gets
-- it directly), and computes a parallel batch of extmarks: indent-guide colour, icon
-- colour, name colour (dir/file/link/root), plus whatever the registered decorators
-- contribute (git signs, diagnostics, …). Decoration needs the view's real buffer
-- number, which only exists once the create/mount ops have drained, so the
-- `set_decor` is deferred a tick with nx.schedule (the buffer is stable by then).

local model = require("nxtree.model")
local icons = require("nxtree.icons")

local M = {}

-- The tree-guide prefix for a node: vertical bars for each ancestor that has more
-- siblings below it, then a "├ "/"└ " connector for the node itself. Root (depth 0)
-- has no guide. ASCII-measured by the caller via `#prefix` (each box-drawing segment
-- is 4 bytes, each blank segment 2) so column math stays byte-exact.
local function guide(node)
  if node.depth == 0 then
    return ""
  end
  local connector = node._last and "└ " or "├ "
  local bars = ""
  local p = node.parent
  while p and p.depth >= 1 do
    bars = (p._last and "  " or "│ ") .. bars
    p = p.parent
  end
  return bars .. connector
end

-- Keep only nodes whose name matches the (lowercased substring) filter, plus the
-- ancestors of every match so the path to a hit stays visible.
local function apply_filter(entries, filter)
  local needle = filter:lower()
  local keep = {}
  for _, n in ipairs(entries) do
    if n.depth == 0 or n.name:lower():find(needle, 1, true) then
      keep[n] = true
      local p = n.parent
      while p do
        keep[p] = true
        p = p.parent
      end
    end
  end
  local out = {}
  for _, n in ipairs(entries) do
    if keep[n] then
      out[#out + 1] = n
    end
  end
  return out
end

-- render(tree) — rebuild the view's content and decoration from the current model.
function M.render(tree)
  local entries = model.flatten(tree.root)
  if tree.filter and tree.filter ~= "" then
    entries = apply_filter(entries, tree.filter)
  end

  local lines, userdata, marks = {}, {}, {}
  for i, node in ipairs(entries) do
    local prefix = guide(node)
    local glyph, ghl = icons.get(node)
    local name = (node.depth == 0) and node.path or node.name
    if node.type == "directory" then
      name = name .. "/"
    end
    local text = prefix .. glyph .. " " .. name
    lines[i] = text
    userdata[i] = node

    local line = i - 1
    local pbytes = #prefix
    local gbytes = #glyph
    local name_col = pbytes + gbytes + 1 -- after "<prefix><glyph> "
    local eol = #text

    if pbytes > 0 then
      marks[#marks + 1] =
        { line = line, col = 0, end_row = line, end_col = pbytes, hl_group = "NxTreeIndent" }
    end
    marks[#marks + 1] =
      { line = line, col = pbytes, end_row = line, end_col = pbytes + gbytes, hl_group = ghl }

    local namehl = "NxTreeFile"
    if node.depth == 0 then
      namehl = "NxTreeRootName"
    elseif node.type == "directory" then
      namehl = "NxTreeDir"
    elseif node.type == "link" then
      namehl = "NxTreeLink"
    end
    marks[#marks + 1] =
      { line = line, col = name_col, end_row = line, end_col = eol, hl_group = namehl }

    -- Decorators: each returns nil or { sign_text=, sign_hl=, hl=, virt_text= }.
    for _, dec in ipairs(tree.config.decorators) do
      local d = dec(node)
      if d then
        if d.sign_text then
          marks[#marks + 1] = {
            line = line,
            col = 0,
            sign_text = d.sign_text,
            sign_hl_group = d.sign_hl,
          }
        end
        if d.hl then
          marks[#marks + 1] =
            { line = line, col = name_col, end_row = line, end_col = eol, hl_group = d.hl }
        end
        if d.virt_text then
          marks[#marks + 1] = { line = line, col = eol, virt_text = d.virt_text }
        end
      end
    end
  end

  tree.flat = entries
  tree.view:set_lines(lines)
  tree.view:set_userdata(userdata)
  -- set_decor needs the backing buffer; it exists by the next tick at the latest.
  nx.schedule(function()
    if tree.view:bufnr() then
      tree.view:set_decor(tree.ns, marks)
    end
  end)
end

return M
