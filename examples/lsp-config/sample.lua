-- Sample buffer for the nxvim LSP playground (see init.lua for the keymaps).
--
-- Put the cursor on `greet` below and press the keys from init.lua:
--   gd on `greet` (line 13) jumps to its definition (line 2).
--   gr lists both call sites; K shows its hover docs.
--   <Space>rn renames it everywhere.

local function greet(name)
  return "hello, " .. name
end

local who = "world"
print(greet(who))
print(greet("nxvim"))

-- An UNDEFINED GLOBAL: lua_ls publishes a diagnostic here. Press `]d` to jump to
-- it, `K` for the message, or `<Space>e` to list it in the panel.
print(undefined_global)
