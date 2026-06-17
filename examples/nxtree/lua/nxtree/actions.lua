-- nxtree.actions — file operations + their buffer-local key maps.
--
-- Every action reads the node under the cursor (`tree.flat[view:line()]`), performs
-- an `nx.fs` mutation (awaited, so call inside the async wrapper init passes as
-- `run`), then re-loads the affected directory and re-renders. The maps are
-- buffer-local on the view buffer, so they only fire while the tree is focused.
--
--   a  add (trailing "/" → directory)      r  rename
--   d  delete (confirm)                     x  cut    p  paste (move here)
--   y  yank absolute path to a register     R  refresh whole tree
--   H  toggle hidden files                  q  close the tree
--
-- Custom actions registered via `nxtree.register_action(key, fn)` are installed too;
-- `fn(tree, render, actions)` runs inside the same error-surfacing async wrapper.

local model = require("nxtree.model")

local M = {}

-- The node under the cursor, or nil if the view has no cursor line yet.
local function current(tree)
  local line = tree.view:line()
  return line and tree.flat[line]
end
M.current = current

-- The directory a "create" should target: the node itself if a directory, else its
-- parent (so `a` on a file creates a sibling).
local function dir_of(node)
  return node.type == "directory" and node or node.parent
end

-- Reload `dir` from disk and ensure it is open, then re-render.
local function reload(tree, render, dir)
  dir.expanded = true
  model.load(tree, dir)
  render(tree)
end

function M.add(tree, render)
  local node = current(tree)
  if not node then
    return
  end
  local dir = dir_of(node)
  local name = nx.await(nx.ui.input({ prompt = "Create (end / for dir): " }))
  if not name or name == "" then
    return
  end
  local is_dir = name:sub(-1) == "/"
  local target = model.join(dir.path, (name:gsub("/+$", "")))
  if is_dir then
    nx.await(nx.fs.mkdir(target, { recursive = true }))
  else
    nx.await(nx.fs.write(target, ""))
  end
  reload(tree, render, dir)
end

function M.rename(tree, render)
  local node = current(tree)
  if not node or node.depth == 0 then
    return
  end
  local new = nx.await(nx.ui.input({ prompt = "Rename: ", default = node.name }))
  if not new or new == "" or new == node.name then
    return
  end
  nx.await(nx.fs.rename(node.path, model.join(node.parent.path, new)))
  reload(tree, render, node.parent)
end

function M.delete(tree, render)
  local node = current(tree)
  if not node or node.depth == 0 then
    return
  end
  local ok = nx.await(nx.ui.confirm("Delete " .. node.name .. "?", { default = false }))
  if not ok then
    return
  end
  nx.await(nx.fs.remove(node.path, { recursive = node.type == "directory" }))
  reload(tree, render, node.parent)
end

function M.cut(tree, _)
  local node = current(tree)
  if not node or node.depth == 0 then
    return
  end
  tree._cut = node
  nx.notify("nxtree: cut " .. node.name .. " (press p to move it here)")
end

function M.paste(tree, render)
  local src = tree._cut
  if not src then
    return nx.notify("nxtree: nothing cut (press x on an entry first)", 3)
  end
  local node = current(tree)
  if not node then
    return
  end
  local dir = dir_of(node)
  local old_parent = src.parent
  nx.await(nx.fs.rename(src.path, model.join(dir.path, src.name)))
  tree._cut = nil
  if old_parent and old_parent ~= dir and old_parent.loaded then
    model.load(tree, old_parent)
  end
  reload(tree, render, dir)
end

function M.yank(tree, _)
  local node = current(tree)
  if not node then
    return
  end
  nx.reg.set('"', node.path)
  nx.reg.set("+", node.path)
  nx.notify("nxtree: yanked " .. node.path)
end

function M.refresh(tree, render)
  model.refresh(tree, tree.root)
  render(tree)
end

function M.toggle_hidden(tree, render)
  tree.config.hidden = not tree.config.hidden
  model.refresh(tree, tree.root)
  render(tree)
  nx.notify("nxtree: hidden files " .. (tree.config.hidden and "shown" or "hidden"))
end

-- install(tree, render, run) — bind the buffer-local action maps on the view buffer.
-- `run(body)` wraps `body` in nx.async with error surfacing (init owns it). Returns
-- false (warned) if the view buffer doesn't exist yet.
function M.install(tree, render, run)
  local buf = tree.view:bufnr()
  if not buf then
    nx.notify("nxtree: cannot install action maps before the view buffer exists", 4)
    return false
  end

  local function map(key, fn, desc)
    nx.keymap.set("n", key, fn, { buffer = buf, desc = desc })
  end
  local function act(fn)
    return function()
      run(function()
        fn(tree, render)
      end)
    end
  end

  map("a", act(M.add), "nxtree: add file/dir")
  map("r", act(M.rename), "nxtree: rename")
  map("d", act(M.delete), "nxtree: delete")
  map("x", act(M.cut), "nxtree: cut")
  map("p", act(M.paste), "nxtree: paste (move)")
  map("y", act(M.yank), "nxtree: yank path")
  map("R", act(M.refresh), "nxtree: refresh")
  map("H", act(M.toggle_hidden), "nxtree: toggle hidden")
  map("q", function()
    nx.dock.toggle("left")
    nx.layer.main()
  end, "nxtree: close")

  for key, fn in pairs(tree.config.actions) do
    map(key, function()
      run(function()
        fn(tree, render, M)
      end)
    end, "nxtree: custom " .. key)
  end

  return true
end

return M
