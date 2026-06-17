-- nxtree.search — filter the flattened view by name.
--
-- `/` prompts for a needle (nx.ui.input — a single-shot prompt, robust on the
-- nomodifiable view buffer); a non-empty result sets `tree.filter`, which render.lua
-- narrows the visible nodes to (matches + their ancestors). An empty submit clears
-- the filter; cancelling (<Esc> at the prompt → nil) leaves it unchanged. `<Esc>` in
-- the tree clears an active filter. The match is a case-insensitive substring — see
-- render.lua's `apply_filter`.

local M = {}

-- prompt(tree, render, run) — ask for a filter and apply it. Pre-fills the current
-- filter so refining is cheap.
function M.prompt(tree, render, run)
  run(function()
    local q = nx.await(nx.ui.input({ prompt = "Filter: ", default = tree.filter or "" }))
    if q == nil then
      return -- cancelled: keep the current filter
    end
    tree.filter = (q ~= "") and q or nil
    render(tree)
  end)
end

-- clear(tree, render) — drop an active filter (a no-op when none is set).
function M.clear(tree, render)
  if tree.filter then
    tree.filter = nil
    render(tree)
  end
end

return M
