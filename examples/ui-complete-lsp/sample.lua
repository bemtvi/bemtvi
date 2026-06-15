-- A scratch Lua buffer to exercise nx.complete's `lsp` source + docs sidebar.
--
-- Try, in insert mode (the docs sidebar floats beside the popup):
--   * type `str` and trigger  → `string` and friends, with their docs
--   * type `string.`          → the stdlib table's members (`format`, `gsub`, …)
--   * type `tab` → `table.`   → `insert`, `remove`, `concat`, … with signatures
--
-- lua-language-server sends documentation lazily, so the sidebar fills in a moment
-- after you land on a row (nxvim issues `completionItem/resolve` for it).

local function greet(name)
  -- put the cursor at the end of the next line, enter insert, type `string.`:
  return string.format("hello, %s", name)
end

local people = { "ada", "alan", "grace" }
for _, who in ipairs(people) do
  -- type `tab` here to complete `table`, then `.insert(...)`:
  print(greet(who))
end

return greet
