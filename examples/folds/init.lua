-- ~~~ nxvim code folding: indent, tree-sitter, LSP, and manual folds ~~~
--
-- Run it (from the repo root) against the sample file:
--
--     NXVIM_CONFIG=examples/folds \
--       cargo run -p nxvim -- examples/folds/sample.lua
--
-- A *fold* hides a range of lines behind a single placeholder row so you can
-- collapse the parts of a file you aren't reading. nxvim folds the same way vim
-- does, from any of four sources — and the model (the `z` commands, fold-aware
-- motion, the `foldcolumn` gutter, operators acting on a whole closed fold) is
-- shared by all of them:
--
--   * manual   — folds you create by hand with `zf{motion}` (no config needed)
--   * indent   — `foldmethod=indent`: fold level follows leading indentation
--   * expr     — `foldmethod=expr` + a `'foldexpr'`; the headline is the native
--                tree-sitter foldexpr, which folds by real syntax (functions,
--                tables, blocks) rather than indentation
--   * lsp      — `foldmethod=expr` with `v:lua.vim.lsp.foldexpr()`, folding from
--                the language server's `textDocument/foldingRange`
--
-- This config turns on the fold-column gutter and uses the **indent** source,
-- which needs no grammar and folds the sample the moment it opens. The two richer
-- sources are one line away — see "UPGRADE" at the bottom.

--------------------------------------------------------------------------------
-- Display: show the fold-column gutter (the `-`/`+`/`│` markers on the left) so
-- you can see fold boundaries and click them. `foldenable` keeps folding on (it
-- is the default; `zi` toggles it). Computed folds open at `foldlevel` (default 0,
-- so nesting shows collapsed on open — `zR` opens everything, `zM` re-closes).
--------------------------------------------------------------------------------
vim.o.foldcolumn = 1 -- a 1-cell fold gutter (0 hides it)
vim.o.foldenable = true -- folding is on (zi toggles it off/on globally)

--------------------------------------------------------------------------------
-- Fold source: indent. A `FileType` autocmd sets it per buffer as files load.
-- (Set it on every buffer, including the no-filetype case, via `BufReadPost` too.)
--------------------------------------------------------------------------------
local function use_indent_folds()
  vim.bo.foldmethod = "indent"
end

vim.api.nvim_create_autocmd("FileType", { pattern = "*", callback = use_indent_folds })
vim.api.nvim_create_autocmd("BufReadPost", { pattern = "*", callback = use_indent_folds })

--------------------------------------------------------------------------------
-- TRY IT — the fold commands (all standard vim):
--
--   za / zo / zc   toggle / open / close the fold under the cursor
--   zR / zM        open / close every fold in the buffer
--   zj / zk        jump to the next / previous fold
--   zf{motion}     create a MANUAL fold (e.g. `zfap` folds a paragraph; manual
--                  folds coexist with the computed ones)
--   :set foldlevel=1   show only the top level of nesting (then `=2`, …)
--   zi             toggle folding off/on entirely
--
-- With the cursor on a CLOSED fold, linewise operators act on the whole fold:
--   dd  deletes every line in the fold · yy yanks it · Vd takes it
--
-- Manual folds you create are saved to shada and restored when you reopen the file.
--
--------------------------------------------------------------------------------
-- UPGRADE to syntax-aware folds. Install a grammar once with `:TSInstall lua`
-- (or python, rust, …), then swap the source in the autocmd above for:
--
--     vim.bo.foldmethod = "expr"
--     vim.bo.foldexpr   = "v:lua.vim.treesitter.foldexpr()"   -- tree-sitter
--
-- or, with a language server attached (see examples/ for LSP configs):
--
--     vim.bo.foldmethod = "expr"
--     vim.bo.foldexpr   = "v:lua.vim.lsp.foldexpr()"          -- LSP foldingRange
--
-- The commands, the gutter, motion, persistence, and operator behavior above are
-- identical — only where the fold ranges come from changes.
--------------------------------------------------------------------------------
