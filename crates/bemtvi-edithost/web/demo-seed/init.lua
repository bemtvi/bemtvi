-- bemtvi python-demo config (docs/plans/2026-06-23-web-python-demo.md, Phases 6–7).
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
require("bemtvi-keys-helper").setup({})

-- File-explorer sidebar — `<leader>e` / `:Tree` toggle (setup installs both).
require("bemtvi-tree").setup({
  width = 32,
  open_on_start = true,
})

-- Statusline; `theme = "auto"` derives the palette from the active colorscheme (catppuccin).
require("bemtvi-line").setup({ options = { theme = "auto" } })

-- An empty bottom tray (a permanent edge dock on a scratch buffer). `autohide = true`
-- collapses it the instant focus leaves, and pops it back when you cross in
-- (`<C-w><C-w>j`) or `:DockShow bottom` — out of the way until you want it.
btv.dock.open({ side = "bottom", size = 8, autohide = true, title = "PANEL" })

-- LSP keymaps (gd / K / grn / gra / grr / gO / <leader>l…); servers configured below.
require("bemtvi-lspconfig").setup({})

-- Diff / merge-conflict visualizer — `:DiffGit` / `:DiffConflict`. `signs = true`
-- shows the per-hunk gutter signs (`+`/`~`/`-`) on changed rows in the diff panes.
require("bemtvi-diff").setup({ signs = true })

-- Python language server: basedpyright, running fully in-browser (Phase 4). The local
-- process host routes any LSP spawn to the bundled basedpyright worker; `typeshedPaths`
-- points it at the stubs mounted in that worker's virtual FS.
btv.lsp.config("basedpyright", {
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
btv.lsp.enable("basedpyright")

-- Signature help that pops up as you type a call: after `print(` (refreshed at each
-- `,`), the parameter hints float under the cursor and stay while you fill the args.
-- Driven by basedpyright's own `signatureHelpProvider.triggerCharacters`; <C-k> still
-- summons it manually anywhere.
btv.lsp.signature_help_autotrigger(true)

-- Snippets. The engine is pure Lua over five core primitives; a snippet is offered as a
-- completion row that EXPANDS instead of inserting its text, into a live tabstop session.
-- The jump keys are moved off the defaults (`<C-j>`/`<C-k>`) because this config keeps
-- `<C-k>` for signature help: the jump map covers insert + select mode and does not fall
-- through, so the default would shadow it while you type a call's arguments.
--   <C-j> next tabstop · <C-h> previous · the mirrors of a repeated tabstop update live
local snippets = require("bemtvi-snippets")
snippets.setup({ jump_next = "<C-j>", jump_prev = "<C-h>" })

-- No snippet collection ships with the demo (friendly-snippets is a runtimepath install,
-- and the browser has no runtimepath), so register a few python ones here — otherwise the
-- source would load and offer nothing. Between them these cover the whole snippet grammar,
-- one feature at a time, so there is something to actually try:
--
--   ${1:name}          a TABSTOP with a default    (`def`, `try`)
--   $0                 where the caret finishes    (all of them)
--   a repeated $1      a MIRROR - retype it and every copy follows (`class`, `for`)
--   ${1|a,b,c|}        a CHOICE - the jump keys cycle the alternatives (`log`)
--   ${1/re/fmt/}       a TRANSFORM - derived text, updated live (`test`)
--   $TM_FILENAME_BASE  a VARIABLE - resolved from context at expand (`head`)
snippets.add("python", {
  { trigger = "def", description = "function", body = "def ${1:name}(${2:args}) -> ${3:None}:\n    $0" },
  { trigger = "class", description = "dataclass", body = "@dataclass\nclass ${1:Name}:\n    ${2:field}: ${3:int}\n\n    def __repr__(self) -> str:\n        return f\"${1:Name}({self.${2:field}})\"\n$0" },
  { trigger = "main", description = "entry point", body = 'if __name__ == "__main__":\n    ${1:main()}\n$0' },
  { trigger = "try", description = "try/except", body = "try:\n    ${1:pass}\nexcept ${2:Exception} as e:\n    ${3:raise}\n$0" },
  -- MIRROR: `$1` inside the body follows the loop variable as you rename it.
  { trigger = "for", description = "for loop", body = "for ${1:item} in ${2:items}:\n    ${3:print($1)}\n$0" },
  -- CHOICE list. Landing on tabstop 1 offers the alternatives instead of free text; the
  -- first is the default.
  { trigger = "log", description = "log a message", body = 'logger.${1|debug,info,warning,error,critical|}("${2:message}")\n$0' },
  -- TRANSFORM: the docstring is DERIVED from the test name as you type it. `(.)(.*)` splits
  -- off the first character and `${1:/upcase}` capitalises it, so typing `parses_utf8`
  -- writes "Parses_utf8." on the line below without you touching it.
  {
    trigger = "test",
    description = "test case",
    body = 'def test_${1:name}() -> None:\n    """${1/(.)(.*)/${1:/upcase}$2/}."""\n    ${2:assert False}\n$0',
  },
  -- VARIABLES, resolved from context at expand time - this file's name and the clock.
  -- Nothing is typed for either; they arrive already filled in.
  {
    trigger = "head",
    description = "module docstring",
    body = '"""${TM_FILENAME_BASE} - ${1:what this module does}.\n\nCopyright (c) ${CURRENT_YEAR}.\n"""\n$0',
  },
})

-- Autocompletion: the native btv.complete engine, popping up as you type. The `lsp`
-- source (basedpyright, above) leads, then the snippets registered above; the `buffer`
-- word-scan is a fallback for prose and comments. `min_chars = 1` opens the popup after a
-- single character; the docs sidebar (on by default) shows the highlighted item's
-- signature/doc, and a snippet row previews the body it will expand to.
--   <C-n>/<Tab> next · <C-p>/<S-Tab> prev · <C-y>/<CR> accept · <C-e> dismiss
btv.complete.setup({
  sources = { { "lsp" }, { "bemtvi-snippets" }, { "buffer", min_chars = 2 } },
  min_chars = 1,
})

-- Live markdown preview — `:MarkdownPreview` on a markdown buffer (TOUR.md is one) opens
-- a rendered view in a second browser tab, following your edits without a `:w`. It is
-- served over a `btv.http.mount` (a subroute of this page's own origin) rather than a
-- bound port, which is exactly why it works here: on the web build a Service Worker
-- answers the same routes the native HTTP server would. Nothing is mounted until the
-- command runs.
require("bemtvi-markdown-preview").setup()

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
