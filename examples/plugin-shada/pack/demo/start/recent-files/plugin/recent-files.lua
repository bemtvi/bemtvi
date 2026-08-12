-- A tiny bundled "plugin" that remembers the files you open, ACROSS sessions, in
-- its own isolated shada namespace.
--
-- It lives under  pack/demo/start/recent-files/ , so bemtvi auto-sources this script
-- at startup (the package `plugin/` convention). Because the script lives there,
-- `btv.shada.plugin()` — called with NO argument — attributes it to its directory:
-- the namespace is "recent-files". The plugin neither names that namespace nor can
-- reach any other: it cannot see the user config's "user" store, and the config
-- cannot see this one. The namespace is assigned from *where the code lives*.

-- Opt in. The handle is captured once; it stays bound to "recent-files" no matter
-- where its methods are later called from (e.g. inside the autocmd below).
local store = btv.shada.plugin()

local MAX = 10

-- The remembered list (most-recent first), seeded from the previous session.
local function recent()
  return store:get("files") or {}
end

-- Push `path` to the front, de-duplicated, capped at MAX, then persist. The write
-- rides the ordinary shada cadence (the debounced checkpoint + the clean-exit
-- flush) — no explicit save needed.
local function remember(path)
  if not path or path == "" then
    return
  end
  local out = { path }
  for _, p in ipairs(recent()) do
    if p ~= path and #out < MAX then
      out[#out + 1] = p
    end
  end
  store:set("files", out)
end

-- Record every file as it is opened.
vim.api.nvim_create_autocmd("BufReadPost", {
  callback = function(args)
    remember(args.file)
  end,
})

-- Also sweep the file bemtvi started on, once startup is complete. (At startup the
-- initial buffer's BufReadPost can fire before this freshly-sourced autocmd is live,
-- so VimEnter — which runs after everything is loaded — catches that first file.)
vim.api.nvim_create_autocmd("VimEnter", {
  callback = function()
    remember(vim.api.nvim_buf_get_name(0))
  end,
})

-- :RecentFiles — show what survived from prior sessions.
vim.api.nvim_create_user_command("RecentFiles", function()
  local files = recent()
  if #files == 0 then
    print("recent-files: nothing remembered yet — open a file, then relaunch")
    return
  end
  print("recent-files (most recent first):")
  for i, p in ipairs(files) do
    print(("  %d. %s"):format(i, p))
  end
end, {})

-- A one-line note at startup so you can see persistence working between launches.
local remembered = #recent()
if remembered > 0 then
  vim.schedule(function()
    print(("recent-files: %d file(s) remembered from a previous session — :RecentFiles"):format(remembered))
  end)
end
