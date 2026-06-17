-- git_signs — an OPTIONAL nxtree add-on: colour entries by git status.
--
-- Zero coupling with the core plugin: it only calls `nxtree.register_decorator`
-- (the per-node decoration seam) and `nxtree.refresh` (re-render). It shells out to
-- `git status --porcelain` via `nx.run` (promise), builds a path → status map, and a
-- decorator turns that into a gutter sign per file (and a "dirty" dot on directories
-- that contain changes). It re-fetches on `BufWritePost`. Delete this file and the
-- tree still works — that's the point of the decorator registry.

local M = {}

-- Highlight palette for the signs (defined on first setup).
local HL = {
  NxTreeGitNew = { fg = "#a6e3a1" },
  NxTreeGitModified = { fg = "#f9e2af" },
  NxTreeGitDeleted = { fg = "#f38ba8" },
  NxTreeGitDirty = { fg = "#fab387" },
}

local function join(dir, name)
  if dir:sub(-1) == "/" then
    return dir .. name
  end
  return dir .. "/" .. name
end

-- Classify a 2-char porcelain status into { sign, hl }.
local function classify(code)
  if code == "??" then
    return { sign = "+", hl = "NxTreeGitNew" }
  end
  local x, y = code:sub(1, 1), code:sub(2, 2)
  if x == "D" or y == "D" then
    return { sign = "-", hl = "NxTreeGitDeleted" }
  elseif x == "A" or y == "A" then
    return { sign = "+", hl = "NxTreeGitNew" }
  end
  return { sign = "~", hl = "NxTreeGitModified" }
end

function M.setup(nxtree)
  for name, spec in pairs(HL) do
    nx.hl.define(0, name, spec)
  end

  local root = vim.fn.getcwd()
  local file_status = {} -- abspath -> { sign, hl }
  local dir_dirty = {} -- abspath -> true (ancestor of a change)

  -- The decorator: read the live maps, no work per call.
  nxtree.register_decorator(function(node)
    if node.type == "directory" then
      if dir_dirty[node.path] then
        return { sign_text = "•", sign_hl = "NxTreeGitDirty" }
      end
    else
      local s = file_status[node.path]
      if s then
        return { sign_text = s.sign, sign_hl = s.hl }
      end
    end
  end)

  -- Re-run `git status` and rebuild the maps, then re-render the tree.
  local function fetch()
    nx.async(function()
      local res = nx.await(nx.run({ cmd = "git", args = { "status", "--porcelain" }, cwd = root }))
      if res.code ~= 0 then
        return -- not a git repo (or git missing): leave the tree unmarked
      end
      file_status = {}
      dir_dirty = {}
      for line in (res.stdout .. "\n"):gmatch("([^\n]*)\n") do
        if #line > 3 then
          local code = line:sub(1, 2)
          local rel = line:sub(4)
          -- a rename is "old -> new"; mark the new path
          local arrow = rel:find(" %-> ")
          if arrow then
            rel = rel:sub(arrow + 4)
          end
          local abs = join(root, rel)
          file_status[abs] = classify(code)
          -- propagate a dirty flag up to the ancestors so changed dirs show
          local p = abs:match("(.*)/[^/]+$")
          while p and #p >= #root do
            dir_dirty[p] = true
            p = p:match("(.*)/[^/]+$")
          end
        end
      end
      nxtree.refresh()
    end)():catch(function(e)
      nx.notify("git_signs: " .. tostring(type(e) == "table" and e.message or e), 4)
    end)
  end

  nx.on("BufWritePost", {}, fetch)
  fetch() -- initial paint
end

return M
