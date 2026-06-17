-- ~~~ nxvim nxtree: a full file explorer, built as a pure-Lua nx.* plugin ~~~
--
-- Run it (from the repo root):
--
--     NXVIM_CONFIG=examples/nxtree \
--       cargo run -p nxvim -- examples/nxtree/sample/hello.txt
--
-- nxtree is a real, lazy, dockable file tree written entirely against the `nx.*`
-- API — no buffer-mutation API, no native widget. It is the proof that the editor's
-- plugin surface (nx.view + nx.fs + nx.open + nx.dock + extmarks) is enough to build
-- the kind of explorer people expect. The plugin lives in `lua/nxtree/` (require-able
-- because NXVIM_CONFIG is on the runtimepath); this file just configures it.
--
-- TRY IT interactively:
--   <leader>e / :NxTree   toggle the sidebar (left dock)
--   j / k                 move; <CR> on a dir expands/collapses, on a file OPENS it
--                         in the MAIN editor (not inside the sidebar)
--   a                     add a file (or a dir if you end the name with "/")
--   r                     rename            d  delete (confirms)
--   x then p              cut an entry, then paste (move) it under the cursor's dir
--   y                     yank the absolute path to the " and + registers
--   /                     filter by name (Esc clears the filter)
--   H                     toggle hidden (dot)files     R  refresh     q  close
--   :NxTreeFindFile        reveal the file in the current window (expands to it)
--
-- The leader is space here; set it before anything maps <leader>.
vim.g.mapleader = " "

local nxtree = require("nxtree")

-- A custom icon (extends the seeded extension table) + a custom action: `<CR>`-less
-- "o" that opens a file in a vertical split instead of the main window — shows the
-- register_action extensibility seam.
nxtree.register_icons({ conf = { glyph = "", hl = "NxTreeIconDefault" } })

nxtree.setup({
  width = 32,
  hidden = false,
  open_on_start = true, -- show the tree immediately so the playground isn't empty
})

-- Optional add-on: colour entries by git status (signs in the gutter). Zero coupling
-- with the core plugin — it only calls register_decorator. Safe to delete.
require("git_signs").setup(nxtree)
