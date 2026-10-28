-- ~~~ bemtvi 'list' + 'listchars': draw tabs, trailing blanks and friends ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/listchars \
--       cargo run -p bemtvi -- examples/listchars/sample.txt
--
-- `'list'` (window-local, off by default) draws normally-invisible characters as
-- visible glyphs; `'listchars'` picks the glyphs, as `key:chars` pairs. bemtvi also
-- spells them `showwhitespace` and `whitespacechars`, since vim's names only make
-- sense if you know ed's `l` (list) command.
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
--   extends:c          nowrap: the last column, when the line runs past it
--   precedes:c         nowrap: the first column, when text is scrolled off the left
-- ```
--
-- The default is `tab:→ ,space:·,trail:•,nbsp:◦,eol:¬`: glyphs nobody types, so a drawn
-- blank can't be mistaken for text (neovim's `>`/`-`/`+` can). Glyphs are colored
-- with the `Whitespace` highlight group (`NonText` for `eol`). Drawing never moves
-- the cursor: a tab's glyphs cover exactly the cells the tab did.

--------------------------------------------------------------------------------
-- 1. `list` on, with the default glyphs.
--
-- Type-this:  nothing; open the sample.
-- See-that:   every space is a `·` (line 4 starts `····`); line 3's tab is `→` then
--             blanks; line 6 ends in `•••`; line 7's non-breaking spaces are `◦`;
--             every line, the empty line 2 too, ends in `¬`.
--------------------------------------------------------------------------------
btv.wo.list = true

--------------------------------------------------------------------------------
-- 2. Toggle it.
--
-- Type-this:  \l   (the default leader, then `l`)
-- See-that:   the glyphs vanish, and the message line says `list off`. `\l` again
--             brings them back. `:set list!` does the same from the command line,
--             and so does `:set showwhitespace!`.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>l", function()
  btv.wo.list = not btv.wo.list
  btv.notify(btv.wo.list and "list on" or "list off")
end, { desc = "toggle 'list'" })

--------------------------------------------------------------------------------
-- 3. Show everything.
--
-- Type-this:  :ListAll<CR>
-- See-that:   leading runs are `›` (line 4 starts `››››`) and the tabs are `→───`
--             arrows.
--------------------------------------------------------------------------------
btv.command("ListAll", function()
  btv.wo.list = true
  btv.wo.listchars = "tab:→─,space:·,lead:›,trail:•,nbsp:◦,eol:¬"
end, { desc = "draw every invisible character" })

--------------------------------------------------------------------------------
-- 4. Indent guides out of `leadmultispace`.
--
-- Type-this:  :set listchars=leadmultispace:│\ \ \ ,tab:│\ <CR>
-- See-that:   line 4's four-space indent shows `│   `, a guide every 4 columns,
--             and line 3's tab shows the same `│   `.
--------------------------------------------------------------------------------

--------------------------------------------------------------------------------
-- 5. Mark the edges of a line that runs off the window.
--
-- Type-this:  :Edges<CR>   then   9G   then   $
-- See-that:   before `$`, line 9 ends in `»` at the right edge (it continues);
--             after `$` the window has scrolled right, and lines 1 and 9 start with
--             `«` at the left edge (text is hidden there). Line 2 is empty and the
--             short lines are scrolled out of view, so they get no marker.
--------------------------------------------------------------------------------
btv.command("Edges", function()
  btv.wo.wrap = false
  btv.wo.list = true
  btv.wo.listchars = "tab:→ ,space:·,trail:•,nbsp:◦,eol:¬,extends:»,precedes:«"
end, { desc = "mark where a nowrap line runs off an edge" })

--------------------------------------------------------------------------------
-- 6. Read it back, and bad values.
--
-- Type-this:  :set whitespacechars?<CR>
-- See-that:   `listchars=` and the current value (the alias reports the vim name).
-- Type-this:  :set listchars=eol:ab<CR>
-- See-that:   `E474: Invalid argument: listchars=eol:ab` (one glyph per key), and
--             the old value is kept. `:set listchars&` restores the default.
--------------------------------------------------------------------------------
