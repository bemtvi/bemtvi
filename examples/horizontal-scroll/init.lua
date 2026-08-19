-- ~~~ bemtvi horizontal scrolling: long lines scroll, they don't clip ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/horizontal-scroll \
--       cargo run -p bemtvi -- examples/horizontal-scroll/sample.txt
--
-- This is the `nowrap` half of the pair (see `examples/word-wrap/` for `:set
-- wrap`): a line wider than the window is shown clipped, and the viewport scrolls
-- SIDEWAYS to keep the cursor on screen — vim's `nowrap`, tracked as `leftcol`.
-- Move the cursor right along a long line and watch the text slide left under a
-- FIXED number gutter; move back and it slides home. Two window-local options
-- tune it, exactly as in vim:
--
--   sidescroll     the scroll STEP. 0 recenters the cursor when it falls off an
--                  edge; >0 (default 1) scrolls just enough to bring it to the
--                  edge. (`:set ss=…`)
--   sidescrolloff  a MARGIN of columns kept between the cursor and the edge
--                  while scrolling, so you always see context ahead.
--                  (`:set siso=…`)
--
-- TRY IT interactively (on the long lines in the sample):
--   $                jump to end-of-line — the viewport scrolls right to follow
--   0                back to column 0 — it scrolls all the way home
--   :set sidescrolloff=10   keep 10 columns of lookahead, then move around
--   :set ss=0               switch to recenter-on-scroll, then run off an edge
--   :set ss?  then  :set siso?   query the current values (one per command — a
--                                `:set a? b?` leaves only the last on the line)
--   :SideReport                  report both at once, from Lua

-- These options live on the focused window and are set through the `:set` ex
-- path (the wired surface today). Give a generous lookahead margin out of the
-- box so the scrolling is obvious. (`vim.cmd` queues the ex command into the
-- core, the same route a user typing `:set` takes.)
vim.cmd("set sidescrolloff=8")

--------------------------------------------------------------------------------
-- :SideReport — report the focused window's horizontal-scroll options. Read
-- through `vim.wo` and announced in ONE notification: two `:set …?` queries echo
-- one after the other, so only the second would still be on the message line.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("SideReport", function()
  vim.notify(("sidescroll=%d  sidescrolloff=%d"):format(vim.wo.sidescroll, vim.wo.sidescrolloff))
end, {})

vim.notify("horizontal scroll demo: move along a long line with l / w / $ / 0")
