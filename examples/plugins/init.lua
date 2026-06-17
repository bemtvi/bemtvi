-- Example: nxvim's native package / plugin manager — `nx.plugins`.
--
-- There is no third-party plugin-manager layer in nxvim; the manager is built in
-- (ADR 0002 / docs/specs/2026-06-11-native-plugin-api.md). You DECLARE the set you
-- want, and the manager clones/updates them with real `git` (over the async
-- runtime, never blocking the editor) and loads each one — eagerly at startup or
-- lazily on a trigger.
--
-- IMPORTANT: nxvim is its OWN editor, not a neovim build. Plugins are written
-- against the `nx.*` API (see the spec's worked examples — picker/complete/
-- statusline/snippet sources, decoration providers, tree views). nxvim does NOT
-- run neovim plugins: the *only* neovim surface it supports is **colorschemes**
-- (neovim `nvim_set_hl` colorschemes run unmodified). So the specs below are
-- nxvim-native plugins (or a neovim colorscheme) — a neovim plugin like telescope
-- or which-key would clone fine but error on load against nxvim's API.
--
-- Try it:
--   NXVIM_CONFIG=examples/plugins cargo run -p nxvim
--   :PluginSync     -- clone everything declared below (needs network + git)
--   :PluginList     -- show install / load state
--   :PluginUpdate   -- fast-forward the unpinned ones
--   :PluginClean    -- remove clones no spec declares
--
-- Clones land under stdpath("data")/plugins (overridable with nx.plugins.setup).

-- Optional: point the install root somewhere of your choosing.
-- nx.plugins.setup({ root = vim.fn.stdpath("data") .. "/plugins" })

nx.plugins({
  -- A neovim COLORSCHEME — the one neovim surface nxvim runs unmodified. Loaded
  -- eagerly; `config` runs once it is on the runtimepath, so `colorscheme`
  -- resolves the freshly-installed colors/.
  {
    "catppuccin/nvim",
    name = "catppuccin",
    config = function()
      vim.cmd("colorscheme catppuccin")
    end,
  },

  -- An nx.*-native plugin, lazy by command: the body is not loaded until the first
  -- `:Emoji`, which then re-dispatches against the real command the plugin defines.
  -- (An `nx.complete.source` / `nx.picker.source`-shaped plugin — see the spec.)
  {
    "davidrios/nx-emoji",
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
    "davidrios/nx-files",
    keys = { "<leader>ff" },
    dependencies = { "davidrios/nx-async-utils" },
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

  -- Local development — the surest "runnable" example, since it loads a plugin that
  -- ships in THIS repo with no network. Point `dir` at the in-tree nxtree plugin
  -- (a pure-Lua nx.* file explorer); it is loaded directly, never cloned, and
  -- :PluginClean leaves it alone. Adjust the path to your checkout.
  -- { name = "nxtree", dir = "/path/to/nxvim/examples/nxtree",
  --   config = function() require("nxtree").setup({}) end },
})
