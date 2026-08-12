-- Live `:substitute` diff preview (inccommand-style).
--
-- While you type a `:[range]s/pat/rep/` command line, bemtvi previews the change
-- right in the buffer: every match of `pat` is struck through as the REMOVED text
-- and `rep` is spliced in inline just after it as the ADDED text — so you see the
-- edit before pressing <CR>. This is built in and needs no config; it is on
-- whenever `'incsearch'` is (the default).
--
-- Open `sample.txt` and type (do NOT press <CR> yet):
--
--     :%s/teh/the/g        -- every "teh" struck red, "the" shown green after it
--     :2,4s/color/colour   -- confined to lines 2-4; first match per line (no /g)
--     :%s/foo//            -- an empty replacement previews a pure deletion
--
-- Press <Esc> to abandon (the preview vanishes, the buffer is untouched) or <CR>
-- to apply. Before the second `/` is typed the plain pattern preview (the yellow
-- match highlight) shows instead; opening the replacement hands off to the diff.
--
-- The `c` (confirm) flag carries the diff into the walk: `:%s/color/colour/gc`
-- prompts `replace with …?` on each match, and the match being decided shows the
-- same diff (struck old + inline new) while the pending matches keep the plain
-- yellow highlight. Answer y/n/a/l/q as usual.
--
-- Colours: with no colorscheme loaded the removed side is a built-in red (struck
-- through) and the added side a built-in green. Define the two groups yourself to
-- recolour them — a colorscheme that sets them wins too. The block below tints the
-- removed side with a red background and the added side with a green one, closer
-- to a diff view. Comment it out to see the plain built-in look.
vim.api.nvim_set_hl(0, "BtvSubstituteDelete", {
  fg = "#f38ba8",
  bg = "#45252a",
  strikethrough = true,
})
vim.api.nvim_set_hl(0, "BtvSubstituteAdd", {
  fg = "#a6e3a1",
  bg = "#24402b",
})

--   Run:  BEMTVI_CONFIG=examples/substitute-preview \
--           cargo run -p bemtvi -- examples/substitute-preview/sample.txt
