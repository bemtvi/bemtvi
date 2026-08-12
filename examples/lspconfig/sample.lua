-- sample.lua — a Lua buffer for lua_ls to chew on.
--
-- Everything the steps in init.lua ask you to try has a target here. The line
-- numbers in the comments below are the ones the steps quote.

local M = {}

-- Step 3: put the cursor on `btv.buf` and press K for a hover float.
local function current_line_count()
  return #btv.buf.lines(0, 0, -1, false)
end

-- Step 4: `undefined_global` is not defined anywhere and is not a known global,
-- so lua_ls reports it. `btv` on the line above does NOT report, because section 2
-- of init.lua declared it — that contrast is the diagnostic worth looking at.
local function broken()
  return undefined_global.field
end

-- Step 3 (`gd`) and step 5 (`grn`, `grr`): `helper` is defined here…
local function helper(prefix, n)
  return prefix .. ": " .. tostring(n)
end

-- …and used here, twice. `gd` from either use jumps up; `grr` lists both.
function M.report()
  local count = helper("lines", current_line_count())
  local again = helper("lines", current_line_count())
  return count, again
end

-- Step 6: with inlay hints on, the parameter names show inline at this call and
-- the local's inferred type shows after the `=`.
function M.describe()
  return helper("buffer", 42)
end

-- Step 5 (`gO`): the document-symbol list is every function on this page.
function M.unused()
  return broken
end

return M
