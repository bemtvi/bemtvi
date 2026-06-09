-- Open this with the init.lua next to it (see its header). Once lua_ls attaches,
-- inlay hints splice in INLINE between the code — none of it is in the file. Two
-- kinds show up here:
--
--   * `name:` PARAMETER hints, before each argument at a call site, and
--   * `: type` TYPE hints, after a `local` whose type isn't obvious from its value.
--
-- Toggle them off with `<leader>ih` and the text snaps back to exactly what you
-- typed; put the cursor on a hinted line and hit `<leader>ic` to read them back.
--
-- NOTE: lua_ls only adds a `: type` hint where the type isn't already obvious — it
-- deliberately omits them for plain literals (so `local TAU = 6.28` shows nothing,
-- its type is plainly a number). That's why not every `local` gets one; the
-- `local label = …` below does, because a function's return type is worth naming.

local TAU = 6.2831853 -- a literal: lua_ls shows NO type hint here (it's obviously a number)

local function arc_length(radius, turns)
  -- The `radius:` / `turns:` parameter hints show at the *call* sites below.
  local circumference = TAU * radius
  return circumference * turns
end

local function describe(name, radius, turns)
  return string.format("%s spans %.2f", name, arc_length(radius, turns))
end

-- `describe` returns a string, so THIS local picks up a `: string` TYPE hint:
--     local label: string = describe(name: "spiral", radius: 2.0, turns: 3)
local label = describe("spiral", 2.0, 3)

-- Every argument below gets a `name:`-style PARAMETER hint:
--     print(describe(name: "ring", radius: 1.0, turns: 1))
print(label)
print(describe("ring", 1.0, 1))
