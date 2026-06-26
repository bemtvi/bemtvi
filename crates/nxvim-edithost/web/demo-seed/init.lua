-- nxvim python-demo config (docs/plans/2026-06-23-web-python-demo.md, Phases 6–7).
--
-- Seeded into OPFS as /init.lua on first boot (worker.mjs demo build, alongside the project
-- + TOUR.md), then sourced AFTER the amalgamated first-party plugin bundle (build-plugins.sh
-- → web/vendor/plugins/plugins-bundle.lua), so the require()s below resolve each plugin from
-- package.preload — the browser VM has no filesystem/runtimepath. It is a normal editable
-- config: a user's edits persist (the seeder never re-writes once the sentinel is set).

vim.g.mapleader = " "

-- Colorscheme: catppuccin mocha. `load()` applies it directly — the runtimepath
-- `colors/catppuccin.lua` a browser can't source is itself just `require("catppuccin").load()`.
require("catppuccin").setup({ flavour = "mocha" })
require("catppuccin").load("mocha")

-- which-key-style popup over the pending-key oracle.
require("nxvim-keys-helper").setup({})

-- File-explorer sidebar — `<leader>e` / `:NxvimTree` toggle (setup installs both).
require("nxvim-tree").setup({
  width = 32,
  open_on_start = true,
})

-- Statusline; `theme = "auto"` derives the palette from the active colorscheme (catppuccin).
require("nxvim-line").setup({ options = { theme = "auto" } })

-- An empty bottom tray (a permanent edge dock on a scratch buffer). `autohide = true`
-- collapses it the instant focus leaves, and pops it back when you cross in
-- (`<C-w><C-w>j`) or `:DockShow bottom` — out of the way until you want it.
nx.dock.open({ side = "bottom", size = 8, autohide = true, title = "PANEL" })

-- LSP keymaps (gd / K / grn / gra / grr / gO / <leader>l…); servers configured below.
require("nxvim-lspconfig").setup({})

-- Diff / merge-conflict visualizer — `:NxDiffGit` / `:NxDiffConflict`.
require("nxvim-diff").setup({})

-- Python language server: basedpyright, running fully in-browser (Phase 4). The local
-- process host routes any LSP spawn to the bundled basedpyright worker; `typeshedPaths`
-- points it at the stubs mounted in that worker's virtual FS.
nx.lsp.config("basedpyright", {
  cmd = { "basedpyright-langserver", "--stdio" },
  filetypes = { "python" },
  settings = {
    basedpyright = {
      analysis = {
        typeshedPaths = { "/typeshed" },
        typeCheckingMode = "basic",
      },
    },
  },
})
nx.lsp.enable("basedpyright")

-- Autocompletion: the native nx.complete engine, popping up as you type. The `lsp`
-- source (basedpyright, above) leads; the `buffer` word-scan is a fallback for
-- prose and comments. `min_chars = 1` opens the popup after a single character;
-- the docs sidebar (on by default) shows the highlighted item's signature/doc.
--   <C-n>/<Tab> next · <C-p>/<S-Tab> prev · <C-y>/<CR> accept · <C-e> dismiss
nx.complete.setup({
  sources = { { "lsp" }, { "buffer", min_chars = 2 } },
  min_chars = 1,
})

-- Python indentation: 4 spaces, no hard tabs (PEP 8). Buffer-local options, so a
-- `FileType` autocmd pins them as each python buffer loads.
vim.api.nvim_create_autocmd("FileType", {
  pattern = "python",
  callback = function(args)
    local bo = vim.bo[args.buf]
    bo.expandtab = true
    bo.shiftwidth = 4
    bo.softtabstop = 4
    bo.tabstop = 4
  end,
})

-- A couple of demo keymaps (the tree, LSP, and which-key sets cover the rest).
vim.keymap.set("n", "<leader>w", "<cmd>write<cr>", { desc = "Write file" })
vim.keymap.set("n", "<leader>q", "<cmd>quit<cr>", { desc = "Quit window" })

-- Open the guided tour as the startup buffer (seeded next to this file). Harmless if
-- absent (opens an empty buffer) — so a config-only boot without the project still works.
vim.cmd("edit /TOUR.md")
