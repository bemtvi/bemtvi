-- ~~~ bemtvi docker-daemon: the LOCAL (client) config ~~~
--
-- This is the config bemtvi runs when you launch it normally — an embedded,
-- in-process server, everything on this one machine:
--
--     BEMTVI_CONFIG=examples/docker-daemon/local \
--       cargo run -p bemtvi -- examples/docker-daemon/workspace/sample.txt
--
-- When you connect to the containerized daemon (see ../README.md) WITH
-- `--remote-config`, bemtvi runs the *daemon's* config instead — fetched over the wire,
-- and this file is skipped. Without the flag (the native default), this file still
-- loads: you keep your local config over the daemon's filesystem. The two configs are
-- deliberately different so you can tell, at a glance, which one is live: run `:WhoAmI`
-- and `:set tabstop?` in each mode.

-- A distinctive option so `:set tabstop?` shows which config is active.
btv.o.tabstop = 2
vim.g.mapleader = " "

-- A command that only exists if THIS (local) init.lua loaded.
vim.api.nvim_create_user_command("WhoAmI", function()
  btv.notify("LOCAL config — embedded server, running on this machine (tabstop=2).")
end, {})

vim.api.nvim_create_autocmd("VimEnter", {
  callback = function()
    btv.notify("bemtvi: loaded the LOCAL config (examples/docker-daemon/local).")
  end,
})
