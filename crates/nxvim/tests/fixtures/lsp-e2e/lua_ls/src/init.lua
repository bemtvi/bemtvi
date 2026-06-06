local function add(a, b)
  return a + b
end

-- Deliberate error: `undefined_global` is not a known global, so lua-language-server
-- must report an `undefined-global` diagnostic on this line.
print(undefined_global)

return { add = add }
