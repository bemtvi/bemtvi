-- A module under the remote config's `lua/` tree. `init.lua` `require`s it, proving
-- `require` resolves against the *fetched-and-materialized* runtimepath, not the
-- client's local disk. Fetched over the daemon wire like every other source file.

local M = {}

function M.greeting()
  return "fetched from the daemon, running locally"
end

return M
