-- ~~~ bemtvi docker-daemon: the DAEMON (container) config ~~~
--
-- This file lives INSIDE the container image (copied to /etc/bemtvi, which the
-- container sets as $BEMTVI_CONFIG). The daemon process — `bemtvi --daemon --listen`
-- — never runs an editor itself; it just serves fs/process/watch/LSP and, on
-- connect, ships THIS config to the client over one `config_bundle` request. The
-- client materializes it locally and runs it, so everything below takes effect on
-- the client even though the source of truth is the container.
--
-- It is intentionally different from examples/docker-daemon/local/init.lua so the
-- swap is visible: `:WhoAmI` and `:set tabstop?` report the daemon, not the client.

-- A distinctive option so `:set tabstop?` shows which config is active.
btv.o.tabstop = 8
vim.g.mapleader = ","

-- A command that only exists if THIS (daemon) init.lua loaded.
vim.api.nvim_create_user_command("WhoAmI", function()
  btv.notify(require("whereami").describe())
end, {})

-- `require` resolves against this config's lua/ tree — also fetched + materialized,
-- proving the whole runtimepath crossed the wire, not just init.lua.
_G.WHERE = require("whereami").describe()

vim.api.nvim_create_autocmd("VimEnter", {
  callback = function()
    btv.notify("bemtvi: loaded the DAEMON config — fetched from the container (" .. _G.WHERE .. ")")
  end,
})
