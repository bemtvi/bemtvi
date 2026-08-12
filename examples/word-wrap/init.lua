-- ~~~ bemtvi word wrap: long lines fold onto the next row, they don't clip ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/word-wrap \
--       cargo run -p bemtvi -- examples/word-wrap/sample.txt
--
-- With `:set wrap` a line wider than the text area is laid out across several
-- screen rows instead of scrolling sideways (vim's default, the counterpart to
-- the `nowrap` `examples/horizontal-scroll/`). Wrapping is a window-local option
-- and rides the same screen-row layout the smooth scroll animates, so scrolling
-- through wrapped text slides exactly as it does through plain text.
--
--   wrap         soft-wrap long lines onto continuation rows (`:set wrap` /
--                `:set nowrap`). On by default here; off restores horizontal
--                scrolling.
--   breakindent  indent each continuation row to match the wrapped line's own
--                indent, so the folded text reads as a hanging block.
--   showbreak    a marker drawn at the start of every continuation row.
--   breakindentopt  `sbr` draws the marker WITHIN the indent so the wrapped text
--                still aligns under the line's indent (vim's default adds the
--                marker on top, shifting the text right by its width).
--
-- The number gutter shows a wrapped line's number on its FIRST display row only;
-- the continuation rows get a blank gutter (vim's look).
--
-- TRY IT interactively (on the long paragraphs in the sample):
--   $                jump to end-of-line — the cursor lands on the wrapped row,
--                    not off-screen; the viewport does NOT scroll sideways
--   :set nowrap      switch back to clip + horizontal-scroll, then move around
--   :set wrap        fold the long lines back onto continuation rows
--   gj / gk          step ONE display row (within a wrapped line), unlike j/k
--                    which move a whole buffer line
--   g0 / g$          jump to the first / last column of the DISPLAY row (the
--                    within-row siblings of gj/gk), unlike 0/$ which act on the
--                    whole buffer line
--   g^               first non-blank of the display row
--   :set nobreakindent   drop the hanging indent on continuation rows
--   :set showbreak=     clear the continuation marker (set it with e.g.
--                    `:set showbreak=↪`). `showbreak` is a STRING option, so
--                    there is no `:set noshowbreak` — `:set showbreak=` clears it.
--   :set briopt=sbr  align the wrapped text under the indent (marker absorbed);
--                    `:set briopt=` restores the default additive marker
--   :set wrap?       query the current value (echoed to :messages)
--   <C-d> / <C-u>    half-page scroll — wrapped lines slide smoothly with the rest
--   :WrapReport      re-run the query from Lua

-- `wrap` and its polish (`breakindent` / `showbreak`) live on the focused window
-- and are set through the `:set` ex path. Turn them on out of the box so the
-- sample's long paragraphs fold — indented, with a marker — immediately.
-- (`vim.cmd` queues the ex command into the core, the same route `:set` takes.)
vim.cmd("set wrap")
vim.cmd("set breakindent")
vim.cmd("set showbreak=↪")
-- `sbr`: keep the wrapped text aligned under the line's indent (the marker is drawn
-- within the breakindent rather than added on top). Drop it for vim's default look.
vim.cmd("set breakindentopt=sbr")

--------------------------------------------------------------------------------
-- :WrapReport — echo the focused window's wrap option via the `:set wrap?` query
-- (it lands on the message line / in `:messages`).
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("WrapReport", function()
  vim.cmd("set wrap?")
end, {})
