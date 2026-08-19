-- btv.ui.float — the list-less content float
-- ==========================================
--
-- Maps this config wires (leader = "\"):
--
--   \o   open a cursor-anchored float (a multi-line string, titled)
--   \O   open a centered, double-bordered float (a list of lines)
--   K    LSP hover for the symbol under the cursor (needs lua-language-server)
--   \s   LSP signature help
--
-- A content float is transient: press any key to dismiss it. It never grabs
-- input — unlike btv.ui.select, the key you press is still handled normally (so
-- K then j moves down after the hover closes).
--
-- For K: put the cursor on a stdlib symbol below and press it. lua-language-
-- server takes ~20s to index on first attach, so hover is empty until it warms.

local function greet(name)
  -- cursor on `string` or `format` here, press K for its hover:
  return string.format("hello, %s", name)
end

local people = { "ada", "alan", "grace" }
for _, who in ipairs(people) do
  -- cursor on `print` / `ipairs` and press K:
  print(greet(who))
end

return greet
