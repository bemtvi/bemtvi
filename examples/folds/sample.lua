-- A small nested Lua file to fold. Both the indent source (no grammar needed)
-- and the tree-sitter foldexpr (after `:TSInstall lua`) collapse these blocks.

local M = {}

local config = {
  window = {
    width = 80,
    height = 24,
    border = "rounded",
  },
  keymaps = {
    open = "<leader>o",
    close = "<leader>c",
    toggle = "<leader>t",
  },
}

function M.setup(opts)
  opts = opts or {}
  for key, value in pairs(opts) do
    if config[key] ~= nil then
      config[key] = value
    else
      error("unknown option: " .. tostring(key))
    end
  end
  return config
end

function M.open(name)
  if not name then
    return nil, "a name is required"
  end
  local win = {
    name = name,
    width = config.window.width,
    lines = {},
  }
  function win:append(line)
    table.insert(self.lines, line)
  end
  return win
end

return M
