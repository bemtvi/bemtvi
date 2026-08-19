-- ~~~ bemtvi smart indenting & auto-pairs: autoindent / smartindent / autopairs ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/smart-indent \
--       cargo run -p bemtvi -- examples/smart-indent/sample.txt
--
-- bemtvi auto-indents from tree-sitter when a grammar's `indents.scm` is loaded.
-- For everything else — plain text, config snippets, a language with no indent
-- query — three buffer-local options (off by default, just like vim/neovim) fill
-- the gap:
--
--   * autoindent  (ai) — a new line copies the previous line's indent.
--   * smartindent (si) — bracket-aware: a line opened after one ending in
--                        `{`, `(`, or `[` gains a shiftwidth, and a closing
--                        bracket typed on its own line snaps back to its opener.
--   * autopairs        — type `(` `[` `{` `'` `"` and the closer is inserted for
--                        you, with the cursor parked between; `<CR>` between a
--                        bracket pair opens an indented block; `<BS>` between an
--                        empty pair deletes both.
--
-- They are buffer-local, so a `FileType` autocmd (or a blanket default) decides
-- per buffer. Here we turn them on everywhere via a `*` autocmd, and use two
-- spaces so the result is easy to see.

vim.api.nvim_create_autocmd("FileType", {
  pattern = "*",
  callback = function(args)
    local bo = vim.bo[args.buf]
    bo.expandtab = true
    bo.tabstop = 2
    bo.smartindent = true -- implies the autoindent copy-previous base
    bo.autopairs = true
  end,
})

-- A buffer with no filetype (a `.txt` file) never fires `FileType`, so set the
-- defaults globally too via `vim.o` (it routes the buffer-local options to the
-- current buffer as each one loads).
vim.o.expandtab = true
vim.o.tabstop = 2
vim.o.smartindent = true
vim.o.autopairs = true

--------------------------------------------------------------------------------
-- Try it (in `sample.txt`, press `i` to start typing):
--
-- 1. AUTO-PAIRS — type an opener, the closer appears and the cursor sits between:
--      TYPE:  foo(          -> "foo(|)"        ( | is the cursor )
--      TYPE:  bar           -> "foo(bar|)"
--      TYPE:  )             -> "foo(bar)|"     (typed through the auto-closer)
--
-- 2. BLOCK EXPANSION — with auto-pairs on, an opener already carries its closer,
--    so <CR> between the two lays the whole body out for you:
--      TYPE:  if cond {     -> "if cond {|}"     (the } came for free)
--      TYPE:  <CR>          -> "if cond {"
--                              "  |"             (one level deeper)
--                              "}"               (snapped back to column 0)
--      TYPE:  work          -> "if cond {"
--                              "  work"
--                              "}"               — done; do NOT type the }, it is
--                                                already there
--      TYPE:  fn(<CR>       -> the same three lines for a paren: "fn(", "  |", ")"
--
-- 3. SMARTINDENT ON ITS OWN — turn auto-pairs off and the closer is yours to type;
--    what smartindent still does is indent after the opener and snap the closer
--    back to it:
--      TYPE:  :setlocal noautopairs<CR>
--      TYPE:  if cond {<CR> -> "if cond {"
--                              "  |"             (one level deeper)
--      TYPE:  work<CR>}     -> "if cond {"
--                              "  work"
--                              "}"               (the } snapped back to column 0)
--
-- 4. BACKSPACE over an empty pair removes both halves:
--      TYPE:  [   then <BS> -> "[|]" then ""    (both brackets gone)
--
-- 5. Quotes are smart about words — an apostrophe inside a word is NOT paired:
--      TYPE:  don't         -> "don't"          (not "don''t")
--      TYPE:  "             -> "\"|\""           (a fresh string IS paired)
--
-- Toggle any of them live to feel the difference:
--      :setlocal noautopairs    -- type `(` -> just "("
--      :setlocal nosmartindent  -- <CR> no longer indents after `{`
--      :set smartindent?        -- echoes "smartindent"
--------------------------------------------------------------------------------
