-- Example: the FIRST-RUN WELCOME checklist for nxvim's native package manager.
--
-- This is the "distribution starter" shape: a config that REGISTERS a recommended
-- plugin set but declares NONE of its own. On a fresh setup, nxvim opens a welcome
-- checklist at startup (VimEnter) — nxvim ships minimal, so it offers the recommended
-- set pre-ticked, lets the user untick anything, and on confirm writes the chosen
-- subset to a managed `lua/plugins.lua` (which it points `init.lua` at) and installs
-- it. It asks AT MOST ONCE, ever (a marker under the data dir records that it asked).
--
-- The welcome only appears when ALL of these hold (see nx.plugins.bootstrap):
--   1. the ask-once marker doesn't exist yet  (a fresh data dir, or marker removed)
--   2. the user has declared no plugins of their own  (this config declares none)
--   3. a recommended set is registered  (gate 3)
-- For gate 3, the interactive binary ships a BUILT-IN default set, activated on a
-- fresh setup before init.lua runs — so even a totally empty config shows the welcome
-- (offering nxvim's defaults). This example instead registers its OWN set with
-- nx.plugins.recommend{...} below, which OVERRIDES that built-in default. (Call
-- nx.plugins.recommend({}) to suppress the welcome entirely.)
--
-- TRY IT (use a FRESH data dir so the ask-once marker isn't already set):
--   NXVIM_CONFIG=examples/plugins-welcome XDG_DATA_HOME=/tmp/nxvim-welcome cargo run -p nxvim
--
--   In the checklist:  <Space> toggle · a all · <CR> install · <Esc>/q skip
--
-- RE-RUN: the marker lives at  $XDG_DATA_HOME/nxvim/plugins/.recommended-prompted
--   * delete it (and use a config with no declared plugins) to replay at startup, OR
--   * just run  :PluginsWelcome  any time to reopen the checklist on demand.
--
-- A recommended spec is DATA + STRING-form hooks only: because the set gets serialized
-- back into the user's plugins.lua, `config`/`init` must be a STRING of Lua (not a
-- function) — everything else is a normal spec (name, `desc` (the welcome blurb),
-- branch, tag, cmd/event/ft/keys, dependencies, …). See examples/plugins/init.lua for
-- the full spec vocabulary.

-- `desc` is a short human description shown next to each item in the welcome
-- checklist — make it count, it's how a user decides whether to keep the plugin.
nx.plugins.recommend({
  -- A colorscheme (a pure-Lua module filling the highlight registry). Its `config`
  -- runs once it's installed + on the runtimepath, so `colorscheme` resolves.
  {
    "nxvim/catppuccin-nxvim",
    name = "catppuccin",
    desc = "Soothing pastel colorscheme",
    config = [[ vim.cmd("colorscheme catppuccin") ]],
  },

  -- An nx.*-native plugin, lazy by command: not loaded until the first `:Emoji`.
  {
    "nxvim/nx-emoji",
    desc = "Insert emoji by name (:Emoji)",
    cmd = "Emoji",
  },

  -- An nx.*-native fuzzy finder, lazy by key, with a dependency (loaded first).
  {
    "nxvim/nx-files",
    desc = "Fuzzy finder for files & buffers (<leader>ff)",
    keys = { "<leader>ff" },
    dependencies = { "nxvim/nx-async-utils" },
  },
})

-- NOTE: declare nothing else here. Any `nx.plugins{...}` call would trip gate 2
-- above (the user "has plugins") and the first-run welcome would be skipped — at
-- which point you'd reach the checklist only via `:PluginsWelcome`.
