-- Open this file with the diagnostics example config:
--
--     NXVIM_CONFIG=examples/diagnostics \
--       cargo run -p nxvim -- examples/diagnostics/sample.lua
--
-- lua-language-server flags the problems below. Each offending line should show
-- a squiggle AND an inline "■ <message>" after it, and moving the cursor onto one
-- echoes the full message on the command line. Try ]d / [d to jump between them.

local function greet(name)
  -- `nam` is undefined here — lua_ls reports an undefined global / wrong name.
  return "hello " .. nam
end

-- Unused local: lua_ls warns that `leftover` is never used.
local leftover = 42

-- Calling greet with no argument; `name` is then nil inside the concat above.
greet()

return greet
