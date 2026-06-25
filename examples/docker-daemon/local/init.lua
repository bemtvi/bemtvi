-- ~~~ nxvim docker-daemon: the LOCAL (client) config ~~~
--
-- This is the config nxvim runs when you launch it normally — an embedded,
-- in-process server, everything on this one machine:
--
--     NXVIM_CONFIG=examples/docker-daemon/local \
--       cargo run -p nxvim -- examples/docker-daemon/workspace/sample.txt
--
-- When you instead connect to the containerized daemon (see ../README.md), nxvim
-- does NOT load this file. The editor runs the *daemon's* config, fetched over the
-- wire. The two configs below are deliberately different so you can tell, at a
-- glance, which one is live: run `:WhoAmI` and `:set tabstop?` in each mode.

-- A distinctive option so `:set tabstop?` shows which config is active.
nx.o.tabstop = 2
vim.g.mapleader = " "

-- A command that only exists if THIS (local) init.lua loaded.
vim.api.nvim_create_user_command("WhoAmI", function()
  nx.notify("LOCAL config — embedded server, running on this machine (tabstop=2).")
end, {})

vim.api.nvim_create_autocmd("VimEnter", {
  callback = function()
    nx.notify("nxvim: loaded the LOCAL config (examples/docker-daemon/local).")
  end,
})
