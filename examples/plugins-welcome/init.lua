-- Example: the FIRST-RUN WELCOME offer for bemtvi's native package manager.
--
-- This is the "distribution starter" shape: a config that REGISTERS a recommended
-- plugin set but declares NONE of its own. On a fresh setup, bemtvi opens a welcome
-- offer at startup (VimEnter) — bemtvi ships minimal, so it asks ONE question ("install
-- the recommended set?"), summarizing it by count and by the origins its code comes
-- from rather than listing every plugin. `c` opens the CUSTOMIZE checklist behind it
-- (every plugin with its exact source, pre-ticked and untickable), and `?` opens the
-- set's reference page in a browser (btv.plugins.RECOMMENDED_DOC_URL — override it if
-- you ship your own set). Whatever is chosen gets written to a managed
-- `lua/plugins.lua` (which it points `init.lua` at) and installed.
-- It asks AT MOST ONCE, ever (a marker under the data dir records that it asked).
--
-- The welcome only appears when ALL of these hold (see btv.plugins.bootstrap):
--   1. the ask-once marker doesn't exist yet  (a fresh data dir, or marker removed)
--   2. the user has declared no plugins of their own  (this config declares none)
--   3. a recommended set is registered  (gate 3)
-- For gate 3, the interactive binary ships a BUILT-IN default set, activated on a
-- fresh setup before init.lua runs — so even a totally empty config shows the welcome
-- (offering bemtvi's defaults). This example instead registers its OWN set with
-- btv.plugins.recommend{...} below, which OVERRIDES that built-in default. (Call
-- btv.plugins.recommend({}) to suppress the welcome entirely.)
--
-- TRY IT (use a FRESH data dir so the ask-once marker isn't already set):
--   BEMTVI_CONFIG=examples/plugins-welcome XDG_DATA_HOME=/tmp/bemtvi-welcome cargo run -p bemtvi
--
--   On the offer:      <CR> install all · c customize · ? reference page · <Esc> skip
--   In the checklist:  <Space> toggle · a all · <CR> install · <Esc>/q skip
--
-- RE-RUN: the marker lives at  $XDG_DATA_HOME/bemtvi/plugins/.recommended-prompted
--   * delete it (and use a config with no declared plugins) to replay at startup, OR
--   * just run  :PluginsWelcome  any time to reopen the offer on demand.
--
-- A recommended spec is DATA + STRING-form hooks only: because the set gets serialized
-- back into the user's plugins.lua, `config`/`init` must be a STRING of Lua (not a
-- function) — everything else is a normal spec (name, `desc` (the welcome blurb),
-- branch, tag, cmd/event/ft/keys, dependencies, …). See examples/plugins/init.lua for
-- the full spec vocabulary.

-- `desc` is a short human description shown next to each item in the customize
-- checklist — make it count, it's how a user decides whether to keep the plugin.
btv.plugins.recommend({
  -- A colorscheme (a pure-Lua module filling the highlight registry). Its `config`
  -- runs once it's installed + on the runtimepath, so `colorscheme` resolves.
  {
    "bemtvi/catppuccin-bemtvi",
    name = "catppuccin",
    desc = "Soothing pastel colorscheme",
    config = [[ vim.cmd("colorscheme catppuccin") ]],
  },

  -- An btv.*-native plugin, lazy by command: not loaded until the first `:Emoji`.
  {
    "bemtvi/btv-emoji",
    desc = "Insert emoji by name (:Emoji)",
    cmd = "Emoji",
  },

  -- An btv.*-native fuzzy finder, lazy by key, with a dependency (loaded first).
  {
    "bemtvi/btv-files",
    desc = "Fuzzy finder for files & buffers (<leader>ff)",
    keys = { "<leader>ff" },
    dependencies = { "bemtvi/btv-async-utils" },
  },
})

-- NOTE: declare nothing else here. Any `btv.plugins{...}` call would trip gate 2
-- above (the user "has plugins") and the first-run welcome would be skipped — at
-- which point you'd reach the checklist only via `:PluginsWelcome`.
