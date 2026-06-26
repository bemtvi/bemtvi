-- ~~~ nxvim nx.shada.plugin: opt-in, ISOLATED per-plugin persistence ~~~
--
-- A plugin can opt into shada and keep its own cross-session key/value data with
--
--     local store = nx.shada.plugin()
--     store:set(key, value)   -- value: any JSON-able Lua value
--     local v = store:get(key)
--     store:delete(key); store:keys(); store:clear()
--
-- The namespace is **assigned, not chosen**: it is derived from WHERE the calling
-- code lives (its runtimepath / plugin directory). So a plugin persists its own
-- data and can never name — and so never read or clobber — another plugin's slice.
-- Plugin data is also walled off from the core editor shada (registers / marks /
-- history). It lives in the *current* shada store, whichever this session uses
-- (global, a `--shada-namespace` workspace, or a remote daemon).
--
-- This example has two independent stores that can't see each other:
--   * THIS config file -> the reserved "user" namespace (the launch counter below);
--   * the bundled plugin under pack/demo/start/recent-files/ -> "recent-files"
--     (it remembers the files you open). See that file for the plugin side.
--
-- Run it TWICE against a *scratch* state dir so your real ~/.local/state is never
-- touched. From the repo root:
--
--     # First session — bumps the launch counter, remembers sample.txt, then :qa
--     XDG_STATE_HOME=/tmp/nxvim-plugin-shada NXVIM_CONFIG=examples/plugin-shada \
--       cargo run -p nxvim -- examples/plugin-shada/sample.txt
--
--     # Second session — the counter and the recent-files list are restored
--     XDG_STATE_HOME=/tmp/nxvim-plugin-shada NXVIM_CONFIG=examples/plugin-shada \
--       cargo run -p nxvim -- examples/plugin-shada/sample.txt
--
-- (Delete /tmp/nxvim-plugin-shada to start fresh.)
--
-- Try in the SECOND session:
--   :Launches      how many times this config has been launched
--   :RecentFiles   the files the bundled plugin remembers (its own namespace)

-- Opt in. From init.lua, `nx.shada.plugin()` attributes to the config root, which
-- maps to the reserved "user" namespace.
local store = nx.shada.plugin()

-- A trivial cross-session counter: read last session's value, bump, persist.
local launches = (store:get("launches") or 0) + 1
store:set("launches", launches)

vim.schedule(function()
  print(("config: launch #%d (stored in the 'user' shada namespace)"):format(launches))
end)

-- :Launches — show the counter. Note it reads only the "user" namespace; it has no
-- way to reach the plugin's "recent-files" store (a sourced file can't name another
-- namespace — that is the whole point of assigned namespaces).
vim.api.nvim_create_user_command("Launches", function()
  print("config launches so far: " .. tostring(store:get("launches")))
end, {})
