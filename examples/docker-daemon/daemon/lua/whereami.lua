-- A require-able module that ships with the daemon config. If `:WhoAmI` can call
-- this, the client resolved a module from the container's lua/ tree — the whole
-- runtimepath was fetched and materialized, not just init.lua.
local M = {}

function M.describe()
  return "DAEMON config — served from the container over QUIC, run locally (tabstop=8)."
end

return M
