-- ~~~ nxvim word wrap: long lines fold onto the next row, they don't clip ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     NXVIM_CONFIG=examples/word-wrap \
--       cargo run -p nxvim -- examples/word-wrap/sample.txt
--
-- With `:set wrap` a line wider than the text area is laid out across several
-- screen rows instead of scrolling sideways (vim's default, the counterpart to
-- the `nowrap` `examples/horizontal-scroll/`). Wrapping is a window-local option
-- and rides the same screen-row layout the smooth scroll animates, so scrolling
-- through wrapped text slides exactly as it does through plain text.
--
--   wrap   soft-wrap long lines onto continuation rows (`:set wrap` / `:set
--          nowrap`). On by default here; off restores horizontal scrolling.
--
-- TRY IT interactively (on the long paragraphs in the sample):
--   $                jump to end-of-line — the cursor lands on the wrapped row,
--                    not off-screen; the viewport does NOT scroll sideways
--   :set nowrap      switch back to clip + horizontal-scroll, then move around
--   :set wrap        fold the long lines back onto continuation rows
--   gj / gk          step ONE display row (within a wrapped line), unlike j/k
--                    which move a whole buffer line
--   :set wrap?       query the current value (echoed to :messages)
--   <C-d> / <C-u>    half-page scroll — wrapped lines slide smoothly with the rest
--   :WrapReport      re-run the query from Lua

-- `wrap` lives on the focused window and is set through the `:set` ex path. Turn
-- it on out of the box so the sample's long paragraphs fold immediately.
-- (`vim.cmd` queues the ex command into the core, the same route `:set` takes.)
vim.cmd("set wrap")

--------------------------------------------------------------------------------
-- :WrapReport — echo the focused window's wrap option via the `:set wrap?` query
-- (it lands on the message line / in `:messages`).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("WrapReport", function()
  vim.cmd("set wrap?")
end, {})
