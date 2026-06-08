-- Open this with the init.lua next to it (see its header). Once lua_ls attaches,
-- the names below pick up the SERVER's classification on top of the treesitter
-- colors: functions vs. parameters vs. locals vs. the read-only constant, which
-- treesitter alone can't tell apart.

local TAU = 6.2831853 -- a read-only local: @lsp.typemod.variable.readonly

-- `radius` and `turns` are parameters (@lsp.type.parameter); `circumference` is
-- a local variable (@lsp.type.variable); `arc` is a method call on a table.
local function arc_length(radius, turns)
  local circumference = TAU * radius
  return circumference * turns
end

local geometry = {}

function geometry.describe(shape)
  local label = shape.name -- `name` is a property (@lsp.type.property)
  return string.format("%s spans %.2f", label, arc_length(shape.radius, shape.turns))
end

print(geometry.describe({ name = "spiral", radius = 2.0, turns = 3 }))
print("one full turn is " .. arc_length(1.0, 1) .. " units")
