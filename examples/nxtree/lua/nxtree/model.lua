-- nxtree.model — the node tree, lazy directory loading, and flatten-to-visible.
--
-- A node is `{ path, name, type, depth, parent, expanded, loaded, children, _last }`:
--   path      absolute filesystem path
--   name      basename (the root node's name is its full path, shown as a header)
--   type      "file" | "directory" | "link"   (lstat-flavoured, from nx.fs.readdir)
--   depth     0 for the root, +1 per level
--   expanded  directory is open (children shown)
--   loaded    children were scandir'd at least once (lazy: only on first expand)
--   children  ordered child nodes (dirs first, then alpha; hidden filtered)
--   _last      true when this node is the last among its siblings (for tree guides)
--
-- Everything here is pure data + async fs reads — no editor calls. `load` and the
-- callers run inside an `nx.async` coroutine (nx.fs is promise-only); `flatten` is
-- synchronous.

local M = {}

-- Join a directory path and a child name without doubling the separator.
local function join(dir, name)
  if dir:sub(-1) == "/" then
    return dir .. name
  end
  return dir .. "/" .. name
end
M.join = join

-- node(path, name, type, depth, parent) -> a fresh unexpanded, unloaded node.
function M.node(path, name, typ, depth, parent)
  return {
    path = path,
    name = name,
    type = typ,
    depth = depth,
    parent = parent,
    expanded = false,
    loaded = false,
    children = {},
    _last = false,
  }
end

-- root(path) -> a root node (depth 0, pre-marked expanded so its children show once
-- loaded). The basename-less full path is the display name.
function M.root(path)
  local n = M.node(path, path, "directory", 0, nil)
  n.expanded = true
  return n
end

-- load(tree, node) — scandir `node` (ONE nx.fs.readdir round-trip, kind included),
-- build its child nodes sorted dirs-first then case-insensitive alpha, applying the
-- hidden-file filter from `tree.config.hidden`, and mark `loaded`. Awaits; call
-- inside nx.async. Re-loading an already-loaded node refreshes it in place while
-- preserving the expand/load state of children that still exist (by path), so a
-- refresh doesn't collapse the tree.
function M.load(tree, node)
  local entries = nx.await(nx.fs.readdir(node.path))

  table.sort(entries, function(a, b)
    local ad, bd = a.type == "directory", b.type == "directory"
    if ad ~= bd then
      return ad
    end
    return a.name:lower() < b.name:lower()
  end)

  -- Preserve existing child state across a reload, keyed by name.
  local prev = {}
  for _, c in ipairs(node.children) do
    prev[c.name] = c
  end

  local children = {}
  for _, e in ipairs(entries) do
    if tree.config.hidden or e.name:sub(1, 1) ~= "." then
      local existing = prev[e.name]
      if existing and existing.type == e.type then
        -- Keep the same node object (and its expanded/loaded subtree).
        children[#children + 1] = existing
      else
        children[#children + 1] =
          M.node(join(node.path, e.name), e.name, e.type, node.depth + 1, node)
      end
    end
  end

  for i, c in ipairs(children) do
    c._last = (i == #children)
  end
  node.children = children
  node.loaded = true
end

-- expand(tree, node) — ensure `node` is loaded then mark it expanded. Awaits.
function M.expand(tree, node)
  if not node.loaded then
    M.load(tree, node)
  end
  node.expanded = true
end

-- refresh(tree, node) — re-scandir `node` and every still-loaded descendant,
-- preserving expansion. Awaits; call inside nx.async.
function M.refresh(tree, node)
  if not node.loaded then
    return
  end
  M.load(tree, node)
  for _, c in ipairs(node.children) do
    if c.type == "directory" and c.loaded then
      M.refresh(tree, c)
    end
  end
end

-- flatten(root) -> ordered list of the visible nodes (depth-first, descending only
-- into expanded+loaded directories). The root is the first entry; the returned list
-- is parallel to the view's lines, so `list[i]` is the node on view line `i`.
function M.flatten(root)
  local out = {}
  local function walk(node)
    out[#out + 1] = node
    if node.type == "directory" and node.expanded and node.loaded then
      for _, c in ipairs(node.children) do
        walk(c)
      end
    end
  end
  walk(root)
  return out
end

return M
