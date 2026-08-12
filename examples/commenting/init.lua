-- ~~~ bemtvi commenting: the gc / gcc operator and 'commentstring' ~~~
--
-- Run it (from the repo root) against the sample Rust buffer:
--
--     BEMTVI_CONFIG=examples/commenting \
--       cargo run -p bemtvi -- examples/commenting/sample.rs
--
-- `gc` is the comment operator and `gcc` toggles the current line — both are
-- built in, no plugin needed. The comment template comes from 'commentstring',
-- which bemtvi sets per buffer from the filetype: `// %s` for rust/c/js/go/…,
-- `# %s` for python/shell/yaml/…, `-- %s` for lua/sql, `/* %s */` for css, and so
-- on for the ~20 most common languages. So `gcc` Just Works on a known file.
--
-- This config doesn't need to set anything for the defaults to work; it only
-- shows the two ways to customize.

-- 1. Override 'commentstring' for a filetype. bemtvi already defaults shell to
--    "# %s"; suppose you prefer two spaces after the marker for shell scripts —
--    a FileType autocmd is the idiomatic place (it targets the buffer that
--    loaded). bemtvi's filetype for a `.sh` file is `bash`.
vim.api.nvim_create_autocmd("FileType", {
  pattern = "bash",
  callback = function(args)
    -- The <left>%s<right> form; the part after %s is the (optional) suffix.
    vim.bo[args.buf].commentstring = "#  %s"
  end,
})

-- 2. A muscle-memory alias: many people map <leader>/ to "toggle comment". `gcc`
--    is the line toggle; `gc` waits for a motion. Map both.
vim.keymap.set("n", "<leader>/", "gcc", { desc = "Toggle comment line" })
vim.keymap.set("x", "<leader>/", "gc", { desc = "Toggle comment selection" })

--------------------------------------------------------------------------------
-- Try it (in sample.rs, filetype `rust` -> commentstring "// %s"):
--
-- 1. Line toggle:
--      gcc            -> "// let greeting = ..."   (comment this line)
--      gcc            -> back to uncommented        (toggle off)
--
-- 2. Operator + motion (always linewise):
--      gc2j           -> comment this line and the next two
--      gcip           -> comment the whole paragraph (inner-paragraph text object)
--      gcG            -> comment from here to the end of the file
--
-- 3. Indent-aware: inside the `if` block, comment the two println! lines —
--    the markers align to the block's indent, each line keeps its own:
--      <select them with V + j, then>  gc
--
-- 4. Counted line toggle:
--      3gcc           -> toggle three lines from the cursor down
--
-- 5. Visual:
--      Vjj  then  gc  -> comment the selected lines
--
-- 6. Query / set the template by hand:
--      :set commentstring?            -> echoes "commentstring=// %s"
--      :set commentstring=//\ %s      -> set it explicitly (escape the space)
--      :e notes.sh<CR>                -> a bash buffer; gcc -> "#  echo ..."
--                                        (the "#  %s" override from the autocmd)
--------------------------------------------------------------------------------
