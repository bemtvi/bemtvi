-- ~~~ bemtvi 'scrolloff' (and 'wrap'): keep context around the cursor ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/scrolloff \
--       cargo run -p bemtvi -- examples/scrolloff/sample.txt
--
-- 'scrolloff' is the VERTICAL scroll margin — the number of screen lines the
-- editor keeps above AND below the cursor. With it set, the viewport scrolls
-- EARLY: as you move down, the window starts sliding before the cursor reaches
-- the bottom row, so you always see that many lines of context ahead. It is the
-- vertical twin of 'sidescrolloff' (the horizontal margin, see
-- `examples/horizontal-scroll/`). Off (0) by default — the cursor may sit on the
-- very top/bottom row — so you opt in.
--
-- It is window-local (each split carries its own value) and clamped to half the
-- window height, so a top AND a bottom margin can always both fit. Against the
-- buffer's own first/last line there is no context to show, so the cursor is
-- allowed into the margin there rather than opening blank `~` rows.
--
-- Set it the neovim way — `vim.opt` (the rich Option surface), `vim.o` (a plain
-- scalar), or `:set scrolloff=…` / the `so` abbreviation all reach the same
-- window option.
vim.opt.scrolloff = 8

-- 'wrap' is the other viewport option this demo shows. bemtvi is `nowrap` by
-- default (a long line is clipped and the viewport pans sideways — see
-- `examples/horizontal-scroll/`). Turning 'wrap' ON lays a long line across
-- several screen rows instead, so nothing scrolls off to the right. It, too, is a
-- window-local option you set through any of the neovim-style surfaces.
--
-- Left OFF here so the vertical scrolloff behavior is the star; flip it on with
-- the `:Wrap` command below (or `:set wrap`) to see soft-wrap kick in.
vim.opt.wrap = false

--------------------------------------------------------------------------------
-- :Wrap / :NoWrap — toggle soft-wrap on the focused window from Lua, proving the
-- `vim.wo` window-local surface reaches the core (not just `:set`).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("Wrap", function()
  vim.wo.wrap = true
  vim.notify("wrap ON — long lines now lay across several rows")
end, {})

vim.api.nvim_create_user_command("NoWrap", function()
  vim.wo.wrap = false
  vim.notify("wrap OFF — long lines clip and the viewport pans sideways")
end, {})

--------------------------------------------------------------------------------
-- :SoReport — echo the focused window's scrolloff / wrap values by issuing the
-- `:set …?` queries (they land on the message line / in `:messages`).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("SoReport", function()
  vim.cmd("set scrolloff? wrap?")
end, {})

--------------------------------------------------------------------------------
-- Try it:
--
-- 1. Put the cursor at the top and hold `j` (or press G to leap to the end). The
--    window starts scrolling once the cursor is 8 lines from the bottom — you
--    always see 8 lines of what's coming.  See-that: the cursor never touches the
--    bottom text row while there are more lines below.
--
-- 2. Now hold `k` back up. Symmetrically, the window scrolls once the cursor is 8
--    lines from the TOP.  See-that: 8 lines of context stay visible above.
--
-- 3. Press G (last line) — the cursor DOES reach the bottom row and no blank rows
--    open below it: there is nothing past end-of-file to keep in view.
--
-- 4. `:set scrolloff=0` and repeat step 1 — the cursor now rides the bottom row
--    with no lookahead. `:set scrolloff=8` (or `:set so=8`) to restore it.
--
-- 5. `:Wrap` then move onto one of the long paragraph lines — it lays across
--    several rows instead of scrolling sideways. `:NoWrap` to switch back.
--
-- 6. `:SoReport` to echo the current values.
--------------------------------------------------------------------------------

vim.notify("scrolloff demo: hold j to fall down the file — the view scrolls 8 lines early (:SoReport, :Wrap)")
