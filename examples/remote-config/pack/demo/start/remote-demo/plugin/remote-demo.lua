-- A packaged plugin (neovim's `pack/*/start/*` layout) shipped INSIDE the remote
-- config. Plugins are fetched from the daemon and sourced from the materialized
-- runtimepath exactly like local ones — so `:RemotePlugin` below exists only because
-- the daemon served this plugin and the client ran it.

vim.api.nvim_create_user_command("RemotePlugin", function()
  nx.notify("This :RemotePlugin command came from a plugin the daemon served.")
end, {})
