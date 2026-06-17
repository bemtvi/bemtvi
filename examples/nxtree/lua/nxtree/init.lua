-- nxtree — a dockable, extensible file explorer, built entirely on `nx.*`.
--
-- The plugin is pure Lua over the native surfaces the explorer needs (per the
-- dogfooding directive, ADR 0002): `nx.view` (the read-only, mountable content
-- surface), `nx.fs` (promise filesystem — readdir-with-kind, mutation, watch),
-- `nx.open(path,{where="main"})` (open a file in the MAIN editor, not the sidebar),
-- `nx.dock` (the left dock it lives in), and extmarks (icons / guides / decorator
-- signs). No buffer-mutation API is used — the tree's lines are owned by the view.
--
-- Architecture (one module per concern):
--   model.lua    node tree, lazy scandir-on-expand, flatten-to-visible
--   render.lua   visible nodes → view lines + extmark decoration
--   icons.lua    extension/name → glyph + highlight
--   actions.lua  add / rename / delete / cut+paste / yank / refresh + their maps
--   search.lua   "/" filter over the flattened view
-- This file owns the singleton tree state, the open/toggle lifecycle, the
-- `<CR>`/expand dispatch, the auto-refresh watch, and the extensibility registries.
--
-- Usage (in init.lua):
--   require("nxtree").setup{ width = 32, hidden = false }
--   -- then <leader>e or :NxTree to toggle the left sidebar.

local model = require("nxtree.model")
local render_mod = require("nxtree.render")
local icons = require("nxtree.icons")
local actions = require("nxtree.actions")
local search = require("nxtree.search")

local M = {}

-- Shared config — registries write here before setup so they apply at first open.
M.config = {
  root = nil, -- tree root (default: the editor's cwd at first open)
  width = 30,
  hidden = false, -- show dotfiles?
  watch = true, -- auto-refresh on filesystem changes
  decorators = {}, -- list of fn(node) -> { sign_text=, sign_hl=, hl=, virt_text= }
  actions = {}, -- key -> fn(tree, render, actions) custom buffer-local maps
}

local tree = nil -- the singleton tree state, built lazily on first open
local hl_defined = false

local function render()
  render_mod.render(tree)
end

-- Run an async body (which may nx.await fs/ui promises), surfacing any rejection as
-- a notification instead of an unhandled promise error.
local function run(body)
  nx.async(body)():catch(function(e)
    local msg = type(e) == "table" and e.message or e
    nx.notify("nxtree: " .. tostring(msg), 4)
  end)
end

-- Define the icon/tree highlight groups once.
local function define_highlights()
  if hl_defined then
    return
  end
  for name, spec in pairs(icons.highlights) do
    nx.hl.define(0, name, spec)
  end
  hl_defined = true
end

-- <CR>/click: directory toggles expand (lazy-loading on first open); file opens in
-- the MAIN editor layer (not inside the sidebar).
local function on_select(_line, node)
  if not node then
    return
  end
  if node.type == "directory" then
    if node.expanded then
      node.expanded = false
      render()
    else
      run(function()
        model.expand(tree, node)
        render()
      end)
    end
  else
    nx.open(node.path, { where = "main" })
  end
end

-- Run `fn` once the view's backing buffer exists (its bufnr arrives a tick after the
-- create/mount ops drain). Polls per-tick until then — cheap and race-free.
local function when_buf(fn)
  local function attempt()
    if tree.view:bufnr() then
      fn()
    else
      nx.schedule(attempt)
    end
  end
  nx.schedule(attempt)
end

-- Auto-refresh: watch the root recursively and re-scan on change. Best-effort —
-- a build with no native watcher (browser/serverless) rejects the first pull, which
-- surfaces once via `run`'s catch and degrades to manual `R`.
local function start_watch()
  run(function()
    local w = nx.fs.watch(tree.root.path, { recursive = true })
    tree._watch = w
    for _ in nx.await_each(w) do
      model.refresh(tree, tree.root)
      render()
    end
  end)
end

-- Build the tree the first time it is opened: mint the view, mount it in the left
-- dock, load + render the root, install the action/search maps, arm the watch, and
-- land focus back in the editor.
local function build()
  define_highlights()
  tree = {
    root = model.root(M.config.root or vim.fn.getcwd()),
    ns = nx.ns.create("nxtree"),
    flat = {},
    filter = nil,
    config = M.config,
    view = nx.view.create({ name = "nxtree", filetype = "nxtree" }),
  }
  tree.view:on_select(on_select)
  tree.view:mount({ dock = "left", size = M.config.width })

  run(function()
    model.expand(tree, tree.root) -- loads + expands the root dir
    render()
  end)

  when_buf(function()
    local buf = tree.view:bufnr()
    actions.install(tree, render, run)
    nx.keymap.set("n", "/", function()
      search.prompt(tree, render, run)
    end, { buffer = buf, desc = "nxtree: filter" })
    nx.keymap.set("n", "<Esc>", function()
      search.clear(tree, render)
    end, { buffer = buf, desc = "nxtree: clear filter" })
    if tree.config.watch then
      start_watch()
    end
  end)

  nx.layer.main()
end

-- ----- public lifecycle ------------------------------------------------------

-- Toggle the sidebar: builds + mounts it on first use, then toggles the dock's
-- visibility (content preserved). The common entry point (`<leader>e` / `:NxTree`).
function M.toggle()
  if tree == nil then
    build()
  else
    nx.dock.toggle("left")
  end
end

-- Open + focus the sidebar (build if needed).
function M.open()
  if tree == nil then
    build()
  else
    nx.dock.show("left")
    tree.view:focus()
  end
end

-- Hide the sidebar and return focus to the editor.
function M.close()
  if tree then
    nx.dock.hide("left")
    nx.layer.main()
  end
end

-- Re-scan the whole tree (preserving expansion) and re-render.
function M.refresh()
  if tree then
    run(function()
      model.refresh(tree, tree.root)
      render()
    end)
  end
end

-- reveal(path) — open the tree (building it if needed), expand the directories along
-- `path` (default: the current buffer's file), move the cursor onto its node, and
-- focus the sidebar. Backs `:NxTreeFindFile`. A no-op for a path outside the root.
function M.reveal(path)
  if tree == nil then
    build()
  end
  run(function()
    local target = path
    if not target or target == "" then
      target = vim.fn.expand("%:p")
    end
    if not target or target == "" then
      return nx.notify("nxtree: no file to reveal", 3)
    end

    -- `target` must live under the root; derive the path segments below it.
    local base = tree.root.path
    if base:sub(-1) ~= "/" then
      base = base .. "/"
    end
    if target:sub(1, #base) ~= base then
      return nx.notify("nxtree: " .. target .. " is outside the tree root", 3)
    end
    local segments = {}
    for seg in target:sub(#base + 1):gmatch("[^/]+") do
      segments[#segments + 1] = seg
    end
    if #segments == 0 then
      return
    end

    -- Clear any active filter, then walk from the root, expanding each directory on
    -- the path (lazy-loading as needed) and descending to the target node.
    tree.filter = nil
    if not tree.root.loaded then
      model.load(tree, tree.root)
    end
    tree.root.expanded = true
    local node = tree.root
    for i, seg in ipairs(segments) do
      local child
      for _, c in ipairs(node.children) do
        if c.name == seg then
          child = c
          break
        end
      end
      if not child then
        node = nil
        break
      end
      -- Expand every directory *above* the target (not the target itself).
      if i < #segments and child.type == "directory" then
        model.expand(tree, child)
      end
      node = child
    end

    render()
    if not node then
      return nx.notify("nxtree: " .. target .. " not found under the root", 3)
    end
    -- Find the target's line in the freshly-rendered flattened view and land on it.
    for i, n in ipairs(tree.flat) do
      if n == node then
        tree.view:set_cursor(i)
        return
      end
    end
  end)
end

-- The view's backing buffer number (or nil before the tree is built / mounted). An
-- introspection handle for decorator add-ons and tests.
function M.bufnr()
  return tree and tree.view:bufnr()
end

-- ----- extensibility (pure-Lua registries) ----------------------------------

-- register_decorator(fn) — `fn(node) -> { sign_text=, sign_hl=, hl=, virt_text= }`
-- (or nil), merged into every visible line's decoration each render. The git-signs
-- add-on (examples/nxtree/git_signs.lua) is built on this.
function M.register_decorator(fn)
  M.config.decorators[#M.config.decorators + 1] = fn
  if tree then
    render()
  end
end

-- register_icons(map) — extend the extension/name → glyph table (see icons.lua).
function M.register_icons(map)
  icons.register(map)
  if tree then
    render()
  end
end

-- register_action(key, fn) — bind a buffer-local `key` in the tree to
-- `fn(tree, render, actions)`, run inside the async error-surfacing wrapper.
function M.register_action(key, fn)
  M.config.actions[key] = fn
  if tree and tree.view:bufnr() then
    nx.keymap.set("n", key, function()
      run(function()
        fn(tree, render, actions)
      end)
    end, { buffer = tree.view:bufnr(), desc = "nxtree: custom " .. key })
  end
end

-- ----- setup -----------------------------------------------------------------

-- setup(opts) — wire `:NxTree` / `:NxTreeRefresh` / `:NxTreeFindFile` and the toggle
-- keymap.
--   opts.root         tree root path (default: the editor's cwd at first open)
--   opts.width        sidebar columns (default 30)
--   opts.hidden       show dotfiles (default false)
--   opts.watch        auto-refresh on fs changes (default true)
--   opts.keymap       toggle key (default "<leader>e"; false to skip)
--   opts.open_on_start open the tree immediately (default false)
function M.setup(opts)
  opts = opts or {}
  if opts.root then
    M.config.root = opts.root
  end
  if opts.width then
    M.config.width = opts.width
  end
  if opts.hidden ~= nil then
    M.config.hidden = opts.hidden
  end
  if opts.watch ~= nil then
    M.config.watch = opts.watch
  end

  nx.command("NxTree", function()
    M.toggle()
  end, { desc = "Toggle the nxtree file explorer" })
  nx.command("NxTreeRefresh", function()
    M.refresh()
  end, { desc = "Re-scan the nxtree file explorer" })
  nx.command("NxTreeFindFile", function()
    M.reveal()
  end, { desc = "Reveal the current file in the nxtree explorer" })

  local key = opts.keymap
  if key == nil then
    key = "<leader>e"
  end
  if key then
    nx.keymap.set("n", key, function()
      M.toggle()
    end, { desc = "Toggle nxtree" })
  end

  if opts.open_on_start then
    M.open()
  end
end

return M
