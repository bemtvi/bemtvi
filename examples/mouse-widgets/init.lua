-- ~~~ bemtvi overlay mouse: drive the floating widgets with the pointer ~~~
--
-- Run it (from the repo root) against the sample buffer:
--
--     BEMTVI_CONFIG=examples/mouse-widgets \
--       cargo run -p bemtvi -- examples/mouse-widgets/sample.txt
--
-- The four floating overlays — the insert-mode completion popup, the fuzzy
-- picker, the promptless `btv.ui.select`, and the command-line wildmenu — are all
-- mouse-driven, and (like every other gesture) the hit-test lives in the SERVER.
-- A front end forwards nothing but a raw screen cell
-- (`btv_input_mouse(button, action, modifier, 0, row, col)`); core maps that cell
-- back to the row of the box it painted, so the TUI, GUI, and web clients behave
-- identically with no client-side geometry. (See `examples/mouse` for the
-- text-area gestures — click / drag / wheel / dividers / tabs.)
--
-- `mouse = a` turns the pointer on in EVERY mode, including command-line mode —
-- the default `nvi` leaves cmdline mouse off, so the wildmenu wouldn't react.
vim.cmd("set mouse=a")

--------------------------------------------------------------------------------
-- 1. Insert-mode completion popup (`btv.complete`).
--    The `buffer` source scans words already in the buffer. TYPE in insert mode
--    until a popup floats under the caret (the sample seeds a few long words), then:
--      click a row        highlight it (no edit yet)
--      click it again     accept it — the typed prefix is replaced (like <C-y>)
--      wheel over the box scroll the highlight, one row per notch (non-wrapping)
--    The popup does NOT grab the mouse: a click off it falls through to the text.
--------------------------------------------------------------------------------
btv.complete.setup({ sources = { { "buffer", min_chars = 2 } } })

--------------------------------------------------------------------------------
-- 2. Command-line wildmenu (`btv.cmdline_complete`).
--    Open the command line with `:` and press <Tab> — a list of matching command
--    names floats just above the line. Then:
--      click a candidate  highlight it (it previews on the line)
--      click it again     accept it into the line (ready to run or edit)
--      wheel over the box cycle the highlight
--    Needs `mouse` to include `c` (set above via `mouse = a`).
--------------------------------------------------------------------------------
btv.cmdline_complete.setup({})

--------------------------------------------------------------------------------
-- 3. Fuzzy picker (`btv.picker`) — a centered box that GRABS the mouse modally.
--    TYPE  \o  to open it over a fixed list, then:
--      click a row        highlight it
--      click it again     confirm it (runs the source's `confirm`)
--      wheel over the list scroll the highlight
--      click OFF the box  cancel the picker (telescope-style)
--    A picker with a preview pane also scrolls the preview on a wheel over it.
--------------------------------------------------------------------------------
vim.g.mapleader = "\\"

btv.picker.source({
  name = "fruits",
  items = function(ctx)
    for _, t in ipairs({ "apple", "apricot", "banana", "blueberry", "cherry", "date" }) do
      ctx.push({ text = t, fruit = t })
    end
  end,
  confirm = function(item)
    btv.notify("picked " .. item.fruit)
  end,
})

-- `<leader>o`, not `<leader>f`: bemtvi ships a whole `<leader>f…` picker family
-- (`\ff` files, `\fg` grep, `\fb` buffers, …), so a bare `\f` is a live prefix of
-- them and would sit waiting for the timeout before this map could fire.
btv.keymap.set("n", "<leader>o", function()
  btv.picker.open("fruits")
end)

--------------------------------------------------------------------------------
-- 4. Promptless chooser (`btv.ui.select`) — a small list under the cursor that
--    also grabs the mouse. TYPE  \s , then:
--      click a row        highlight it
--      click it again     resolve the promise with it
--    A click off it dismisses it, resolving the promise with nothing — the same
--    as <Esc>. (The completion popup in section 1 is the odd one out: it lets an
--    outside click through to the text instead of taking it as a dismissal.)
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>s", function()
  btv.ui.select({ "north", "south", "east", "west" }, { prompt = "Heading:" }):next(function(item)
    btv.notify(item and ("heading " .. item) or "no heading (cancelled)")
  end)
end)
