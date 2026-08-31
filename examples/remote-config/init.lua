-- ~~~ bemtvi remote config: config + plugins fetched FROM the daemon, run locally ~~~
--
-- In an edit-host (daemon) session, bemtvi does NOT load the *client* machine's
-- config. It fetches the **daemon's** config + plugins over the wire (one
-- `config_bundle` request), materializes them into a local cache, and runs them
-- locally — Lua's synchronous `require`/runtimepath can't await the network, so the
-- files must be local, but the source of truth is the remote.
--
-- This file IS that remote config. Everything below only takes effect because it was
-- served by the daemon and materialized on the client.
--
-- ── Try it on ONE machine (local two-process split) ──────────────────────────
--
-- The `--connect-daemon` role spawns `bemtvi --daemon` as a child and talks to it over
-- stdio. The child inherits BEMTVI_CONFIG, so it serves THIS directory as its config:
--
--     BEMTVI_CONFIG=examples/remote-config \
--       cargo run -p bemtvi -- --connect-daemon examples/remote-config/sample.txt
--
-- ── Try it across MACHINES (real remote over SSH) ────────────────────────────
--
-- Put this directory at ~/.config/bemtvi on the REMOTE host, then from the LOCAL one:
--
--     BEMTVI_DAEMON_CMD='ssh your-host bemtvi --daemon' \
--       cargo run -p bemtvi -- --connect-daemon
--
-- The keystroke path stays local (zero round trips); only fs/process/LSP — and this
-- config fetch — cross the wire. The editor now runs the remote host's config.
--
-- ── How to SEE that the remote config is live ────────────────────────────────
--
--   * On startup you get the notification below ("loaded … from the daemon").
--   * `:RemoteHello`  — a command defined HERE (init.lua).
--   * `:RemotePlugin` — a command from a PLUGIN the daemon served (see pack/ below).
--   * `:lua btv.notify(_G.REMOTE_GREETING)` — proves `require` resolved a module from
--     this config's `lua/` tree (also fetched).
--   * `:set scrolloff?` — shows 7, the distinctive option set here.

-- A distinctive option, so `:set scrolloff?` visibly reflects this config (and you can
-- see it: the cursor keeps a 7-line margin). Deliberately NOT an indent option —
-- `'indentdetect'` reads those off each file you open, and would outrank a marker set
-- here the moment the sample turned out to be indented. See examples/indent-detect.
btv.o.scrolloff = 7
vim.g.mapleader = " "

-- A user command that exists only if THIS init.lua loaded.
vim.api.nvim_create_user_command("RemoteHello", function()
  btv.notify("Hello from the init.lua the daemon served — the remote config is live.")
end, {})

-- `require` resolves against this config's `lua/` tree (fetched + materialized too).
_G.REMOTE_GREETING = require("remote_mod").greeting()

-- Announce, once the editor has finished starting, that the remote config loaded.
vim.api.nvim_create_autocmd("VimEnter", {
  callback = function()
    btv.notify(
      "bemtvi: loaded init.lua + plugins fetched from the daemon (" .. _G.REMOTE_GREETING .. ")"
    )
  end,
})
