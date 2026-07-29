local M = {}

function M.setup()
  _G.beta_ready = true

  -- Beta is a plugin, so `setup` runs from its `config` — several ticks into
  -- startup, well after the file named on the command line was read. Registering
  -- a plain `BufReadPost` here still works: every read that happened while the
  -- plugins were loading is replayed to this handler when they land, so it sees
  -- the startup file exactly as it sees every file you open afterwards.
  _G.beta_reads = {}
  _G.beta_filetypes = {}

  nx.on("BufReadPost", {}, function(a)
    table.insert(_G.beta_reads, a.file)
  end)

  nx.on("FileType", {}, function(a)
    table.insert(_G.beta_filetypes, a.match)
  end)
end

return M
