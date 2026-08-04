-- Example: nxvim's native package / plugin manager — `nx.plugins`.
--
-- There is no third-party plugin-manager layer in nxvim; the manager is built in
-- (ADR 0002 / docs/specs/2026-06-11-native-plugin-api.md). You DECLARE the set you
-- want, and the manager clones/updates them via `nx.git_local` (first-party gix, no
-- `git` binary, over the async runtime — never blocking the editor) and loads each
-- one — eagerly at startup or lazily on a trigger.
--
-- IMPORTANT: nxvim is its OWN editor, not a neovim build, and claims no neovim
-- compatibility. Plugins are written against the `nx.*` API (see the spec's worked
-- examples — picker/complete/statusline/snippet sources, decoration providers, tree
-- views). A neovim plugin like telescope or which-key would clone fine but error on
-- load against nxvim's API. So the specs below are nxvim-native plugins, plus a
-- **colorscheme** — colorschemes are nxvim's own pure-Lua modules that fill the
-- highlight registry through the highlight API (`vim.cmd.colorscheme` / the
-- `nvim_set_hl` alias), so a catppuccin-style colorscheme repo loads as-is.
--
-- Try it:
--   NXVIM_CONFIG=examples/plugins cargo run -p nxvim
--   :Plugins        -- the lazy-style dashboard: every plugin grouped by load state,
--                      LIVE clone/pull progress, and the action keys —
--                      UPPER-case acts on EVERYTHING:
--                      I install · U update · S sync · R restore · X clean ·
--                      <C-r> refresh · <CR> details · q quit
--                      lower-case acts on the plugin UNDER THE CURSOR:
--                      i install · u update · s sync · r restore · x remove
--   :PluginsWelcome -- re-open the first-run recommended-set checklist on demand
--   :PluginSync     -- clone what's missing, at the LOCKED commits (needs network)
--   :PluginList     -- show install / load state, incl. DRIFTED (text dump)
--   :PluginUpdate   -- fast-forward the unpinned ones, ADVANCING past the lockfile
--   :PluginRestore  -- check every plugin out at the commit the lockfile records
--   :PluginLock     -- (re)record the installed commits to the lockfile
--   :PluginClean    -- remove clones no spec declares
--
-- ONE PLUGIN AT A TIME. Every verb above takes an optional plugin list, `<Tab>`-
-- completed from the declared names — so you can take the fix you are waiting for
-- without dragging eleven other people's changes in with it:
--
--   :PluginUpdate catppuccin        -- fast-forward JUST that plugin; every other
--                                      checkout AND lockfile entry is untouched
--   :PluginSync nx-files            -- install/realize one plugin (its dependencies
--                                      come with it — a plugin whose dependency is
--                                      missing does not load)
--   :PluginRestore catppuccin       -- roll back only the plugin that broke
--   :PluginClean catppuccin         -- delete just that clone; :PluginInstall
--                                      catppuccin then gives you a fresh copy
--   :PluginLock catppuccin          -- pin the one plugin you have finished testing
--
-- Same thing from Lua: `nx.plugins.update({ plugins = "catppuccin" })` (a name or a
-- list of names). Same thing in the dashboard: put the cursor on a row and press the
-- lower-case key.
--
-- THE LOCKFILE. After the first `:PluginSync` you'll find `nxvim-lock.json` in this
-- example dir (the config dir) recording the exact commit each plugin resolved to.
-- Commit that next to your init.lua and another machine reproduces the same tree.
-- Try it: `:PluginUpdate` (advances + re-records), then `:PluginRestore` (back to
-- what the file says). `:PluginList` marks a plugin `DRIFTED` while its checkout
-- differs from the lockfile.
--
-- Clones land under stdpath("data")/plugins (overridable with
-- nx.plugins.setup_manager).

-- Optional: point the install root somewhere of your choosing.
-- nx.plugins.setup_manager({ root = vim.fn.stdpath("data") .. "/plugins" })

nx.plugins({
  -- A COLORSCHEME (a pure-Lua module that fills the highlight registry). Loaded
  -- eagerly; `config` runs once it is on the runtimepath, so `colorscheme`
  -- resolves the freshly-installed colors/.
  {
    "nxvim/catppuccin-nxvim",
    name = "catppuccin",
    config = function()
      vim.cmd("colorscheme catppuccin")
    end,
  },

  -- An nx.*-native plugin, lazy by command: the body is not loaded until the first
  -- `:Emoji`, which then re-dispatches against the real command the plugin defines.
  -- (An `nx.complete.source` / `nx.picker.source`-shaped plugin — see the spec.)
  {
    "nxvim/nx-emoji",
    cmd = "Emoji",
    config = function()
      require("nx-emoji").setup({})
    end,
    enabled = false,
  },

  -- An nx.*-native fuzzy-finder plugin (picker sources), lazy by key: pressing the
  -- lhs loads the plugin, then the keypress is fed back so its own mapping handles
  -- it. Dependencies load first.
  {
    "nxvim/nx-files",
    keys = { "<leader>ff" },
    dependencies = { "nxvim/nx-async-utils" },
    config = function()
      require("nx-files").setup({})
    end,
    enabled = false,
  },

  -- Pin to a tag (or `commit = "<sha>"`): a pinned plugin is cloned at that ref and
  -- never auto-updated by :PluginUpdate.
  {
    "davidrios/nx-statusline-extras",
    tag = "v0.1.0",
    enabled = false, -- illustrative; flip on to actually fetch it
  },

  -- Local development — point `dir` at a plugin checkout on disk: it is loaded
  -- directly, never cloned, and :PluginClean leaves it alone. The surest "runnable"
  -- setup, since it needs no network. Adjust the path and module to your plugin.
  -- { name = "myplugin", dir = "/path/to/your/plugin",
  --   config = function() require("myplugin").setup({}) end },
})

-- ----- First-run recommended set (for a distribution / starter config) --------
--
-- Register a curated set with nx.plugins.recommend{}. On a FRESH setup — the user
-- has declared no plugins of their own and hasn't been asked before — nxvim opens a
-- WELCOME checklist at startup (VimEnter): nxvim ships minimal, and the recommended
-- set is presented pre-ticked so the user can untick anything they don't want
-- (<Space> toggle · a all · <CR> install · <Esc> skip). The chosen subset is written
-- to the user's config (a managed lua/plugins.lua that init.lua requires) and
-- installed. It asks at most once, ever.
--
-- Because the set is serialized back to the user's config, a recommended spec's
-- `config`/`init` must be a STRING of Lua (not a function) — everything else is a
-- normal spec:
--
-- `desc` is a short blurb shown next to each item in the first-run welcome checklist.
--
-- nx.plugins.recommend({
--   { "catppuccin/nvim", name = "catppuccin", desc = "Soothing pastel colorscheme",
--     config = [[ vim.cmd("colorscheme catppuccin") ]] },
--   { "author/nx-files", desc = "Fuzzy file finder", keys = { "<leader>ff" } },
-- })
