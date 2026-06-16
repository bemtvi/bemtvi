-- A sample buffer with plenty of nesting for the rainbow provider to colour.
-- Open it under examples/rainbow/init.lua and scroll: each newly-revealed line's
-- brackets colour by depth as it comes into view.

local config = {
  editor = {
    options = { number = true, wrap = false, scrolloff = { top = 3, bot = 3 } },
    keymaps = {
      { mode = "n", lhs = "<leader>w", rhs = (function() return ":w<CR>" end)() },
      { mode = "n", lhs = "<leader>q", rhs = ((":q") .. ("<CR>")) },
    },
  },
  plugins = {
    rainbow = { enabled = true, colors = { 1, 2, 3, 4, 5, 6 } },
    statusline = { sections = { left = { "mode", "file" }, right = { "pos" } } },
  },
}

local function deep(a, b, c)
  return {
    sum = (((a + b) + c) + (a * (b + (c * 2)))),
    nested = { { { { "four levels deep" } } } },
    mixed = { tuple = ({ 1, 2, 3 })[2], call = math.max((a), (b), (c)) },
  }
end

local data = {
  rows = {
    { id = 1, tags = { "alpha", "beta" }, meta = { seen = (true and (1 or 2)) } },
    { id = 2, tags = { "gamma" }, meta = { seen = (false or (3 and 4)) } },
    { id = 3, tags = { "delta", "epsilon", "zeta" }, meta = { seen = nil } },
  },
  totals = (function(rows)
    local n = 0
    for _, r in ipairs(rows) do n = n + (#(r.tags)) end
    return n
  end)({}),
}

local handlers = {
  on_open = function(ev) return (ev and (ev.buf or 0)) end,
  on_close = function(ev) return ((ev or {}).win) end,
  on_change = function(ev) return (((ev or {}).lines or {})[1]) end,
}

local pipeline = compose(
  map(function(x) return (x * 2) end),
  filter(function(x) return (x > (10 - (2 * 2))) end),
  reduce(function(acc, x) return (acc + x) end, 0)
)

return {
  config = config,
  deep = deep,
  data = data,
  handlers = handlers,
  pipeline = pipeline,
}
