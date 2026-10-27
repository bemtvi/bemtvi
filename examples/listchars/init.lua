-- ~~~ bemtvi 'list' + 'listchars': draw tabs, trailing blanks and friends ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/listchars \
--       cargo run -p bemtvi -- examples/listchars/sample.txt
--
-- `'list'` (window-local, off by default) draws normally-invisible characters as
-- visible glyphs; `'listchars'` picks the glyphs, as `key:chars` pairs:
--
-- ```
--   tab:xy[z]          a tab: x, then y to fill its width (z on the last cell)
--   space:c            any space no other key claims
--   multispace:c…      a run of 2+ spaces, the glyphs repeating
--   lead:c             the spaces of a line's leading run
--   leadmultispace:c…  a leading run of 2+ spaces, the glyphs repeating
--   trail:c            the spaces of a line's trailing run
--   nbsp:c             a non-breaking space
--   eol:c              one glyph past the end of every line
-- ```
--
-- The default is neovim's `tab:> ,trail:-,nbsp:+`. Glyphs are colored with the
-- `Whitespace` highlight group (`NonText` for `eol`). Drawing never moves the cursor:
-- a tab's glyphs cover exactly the cells the tab did.

--------------------------------------------------------------------------------
-- 1. `list` on, with a set of glyphs that reads well.
--
-- Type-this:  nothing; open the sample.
-- See-that:   line 3's tab is `»` then blanks; line 6 ends in `···`; line 7's
--             non-breaking spaces show as `␣`.
--------------------------------------------------------------------------------
btv.wo.list = true
btv.wo.listchars = "tab:» ,trail:·,nbsp:␣"

--------------------------------------------------------------------------------
-- 2. Toggle it.
--
-- Type-this:  \l   (the default leader, then `l`)
-- See-that:   the glyphs vanish, and the message line says `list off`. `\l` again
--             brings them back. `:set list!` does the same from the command line.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>l", function()
  btv.wo.list = not btv.wo.list
  btv.notify(btv.wo.list and "list on" or "list off")
end, { desc = "toggle 'list'" })

--------------------------------------------------------------------------------
-- 3. Show everything.
--
-- Type-this:  :ListAll<CR>
-- See-that:   every space is a `·`, leading runs are `→` (line 4 starts `→→→→`),
--             the tabs are `>--` arrows, and each line ends in `¬`.
--------------------------------------------------------------------------------
btv.command("ListAll", function()
  btv.wo.list = true
  btv.wo.listchars = "tab:>-,space:·,lead:→,trail:·,nbsp:␣,eol:¬"
end, { desc = "draw every invisible character" })

--------------------------------------------------------------------------------
-- 4. Indent guides out of `leadmultispace`.
--
-- Type-this:  :set listchars=leadmultispace:│\ \ \ ,tab:│\ <CR>
-- See-that:   line 4's four-space indent shows `│   `, a guide every 4 columns,
--             and line 3's tab shows the same `│   `.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 5. Read it back, and bad values.
--
-- Type-this:  :set listchars?<CR>
-- See-that:   `listchars=` and the current value.
-- Type-this:  :set listchars=eol:ab<CR>
-- See-that:   `E474: Invalid argument: listchars=eol:ab` (one glyph per key), and
--             the old value is kept. `:set listchars&` restores the default.
--------------------------------------------------------------------------------
