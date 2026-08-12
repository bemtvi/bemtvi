-- A sample buffer for the btv.decor todo-keywords provider. Open it under the
-- example config and the keywords below colour by kind.

local M = {}

-- TODO: wire this up to the real config loader once the schema settles.
function M.setup(opts)
  opts = opts or {}
  -- FIXME: this silently drops unknown keys; validate against the schema instead.
  M.opts = opts
  return M
end

-- NOTE: the cache is intentionally unbounded for now — small inputs only.
local cache = {}

function M.get(key)
  -- HACK: stringify the key so table keys and numbers collide deterministically.
  local k = tostring(key)
  if cache[k] == nil then
    -- XXX: recomputing on every miss is O(n); memoise the expensive branch.
    cache[k] = #k * 2
  end
  return cache[k]
end

return M
