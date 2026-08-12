-- ~~~ bemtvi mouse support: the server owns every hit-test ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/mouse \
--       cargo run -p bemtvi -- examples/mouse/sample.txt
--
-- The mouse is wired straight into the text area. The TUI forwards nothing but a
-- raw screen cell (`nvim_input_mouse(button, action, modifier, 0, row, col)`);
-- the SERVER does all the hit-testing — cell -> window -> buffer position — so
-- every front end behaves identically and the geometry knowledge (gutter width,
-- tab expansion, horizontal scroll, split layout) stays in one place. This is
-- exactly how real neovim works (single global grid, `grid = 0`).
--
-- TRY IT interactively in the sample buffer:
--
--   click            place the cursor on the clicked character; clicking another
--                    split focuses it (focus-follows-click). The number gutter is
--                    click-through to column 0.
--   click + drag     charwise Visual selection from the press to the release;
--                    let go and the selection stays (vim keeps it). `y` yanks it.
--   double-click     select the word under the pointer (the `iw` run); drag to
--                    extend the selection by whole words.
--   triple-click     select the whole line (linewise Visual); drag for more lines.
--   Shift+click      extend the current selection to the click, keeping the
--                    anchor (`<S-LeftMouse>`); a following drag keeps extending.
--   wheel up/down    scroll the window UNDER THE POINTER by `'mousescroll'` lines
--                    WITHOUT moving focus or (while it stays visible) the cursor —
--                    so you can scroll an inactive split. Shift+wheel = one page.
--   wheel left/right scroll sideways by `'mousescroll'` columns (under `nowrap`).
--   drag a divider  press a window separator (or a status line with a window
--                    below it) and drag to resize the adjacent splits — width for
--                    a vertical divider, height for a horizontal one. Focus and
--                    the selection are untouched.
--   click a tab      with the tabline shown, click a tab's cell to switch to it.
--                    The click is consumed by the tabline — it never places a
--                    cursor in the text below. (A custom `'tabline'` carries no
--                    built-in click regions, so it's inert, as in vim.)
--   right-click      depends on `'mousemodel'`. Default `popup_setpos`: move the
--                    cursor to the click (a click inside a selection keeps it; a
--                    context menu is a separate, not-yet-built feature). With
--                    `:set mousemodel=extend`, right-click EXTENDS the selection
--                    to the click (like Shift+click).
--   middle-click     paste the `"*` clipboard register at the click (`gP` — the
--                    cursor lands just past the pasted text). Try it after a
--                    `"+y`/`"+yy`, or with text put on the clipboard externally.
--   insert + click   click while in Insert mode moves the caret without leaving
--                    Insert (`'mouse'` includes `i` by default).
--
-- Open a split first (`:vsplit` or `<C-w>v`) to feel focus-follows-click, the
-- wheel scrolling a window you are not focused in, and dragging the divider
-- between them to resize. Open a tab (`:tabnew`) to click between tabs — this
-- config sets `showtabline=2` so the tabline is visible from the start.
--
-- Four global options tune it, with vim's exact defaults:
--
--   mouse        per-mode enable (`n`/`v`/`i`/`c`/`a`). Default `nvi`: normal,
--                visual, and insert — cmdline mouse is off out of the box.
--   mousescroll  wheel step, `ver:{lines},hor:{cols}`. Default `ver:3,hor:6`;
--                a `0` count disables that direction.
--   mousemodel   right-click semantics: `popup_setpos` (default) moves the
--                cursor / would pop a menu, `extend` makes right-click extend
--                the selection.
--   mousetime    max ms between presses to count as a multi-click (default 500).

-- A slightly larger wheel step so a notch is obvious on the tall sample, and a
-- snappier multi-click window. These go through the same `:set` ex path a user
-- typing `:set` would take.
vim.cmd("set mousescroll=ver:5,hor:6")
vim.cmd("set mousetime=400")
-- Always show the tabline so a tab click is demoable from the first frame
-- (default `showtabline=1` only shows it once a second tab exists).
vim.cmd("set showtabline=2")

--------------------------------------------------------------------------------
-- :MouseReport — echo the four mouse options to the message line / :messages.
--------------------------------------------------------------------------------
vim.api.nvim_create_user_command("MouseReport", function()
  vim.cmd("set mouse? mousescroll? mousemodel? mousetime?")
end, {})

vim.notify("mouse demo: click to place, drag to select, wheel to scroll a split")
