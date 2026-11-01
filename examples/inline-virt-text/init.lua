-- Inline virtual text under a sideways scroll.
--
--   BEMTVI_CONFIG=examples/inline-virt-text cargo run -p bemtvi-gui -- examples/inline-virt-text/sample.txt
--
-- 1. <F5>: line 1 draws `<<INLINE12>>abcdef...`. Press this first: it adds the mark.
-- 2. <F6>: the view pans to column 8, into the mark. See `NE12>>abcdef...`, the mark
--    sliding off a cell at a time.
-- 3. <F7>: pans to column 20, past the mark. See `ijklm...`, with no jump as it settles.
-- 4. <F8>: pans to column 50. See `mnopq...`.
-- 5. <F5> again: the mark slides back in.
--
-- 6. `$` then `0`: the view comes back far enough to show the whole mark again.
--
-- `:set scrollanimduration=` with a larger value makes a pan easier to watch.
local ns = btv.ns.create("pan-demo")
btv.o.wrap = false
btv.o.number = false

-- Keep the cursor in the view being panned to: the editor pulls a view back to a cursor
-- it would leave off screen. Column 40 paints at 52, in view from each of these columns.
local function pan_to(leftcol)
  btv.win.set_cursor(0, 1, 40)
  btv.win.set_leftcol(0, leftcol)
end

btv.keymap.set("n", "<F5>", function()
  vim.api.nvim_buf_set_extmark(0, ns, 0, 0, {
    id = 1,
    virt_text = { { "<<INLINE12>>", "Search" } },
    virt_text_pos = "inline",
  })
  pan_to(0)
end)
btv.keymap.set("n", "<F6>", function()
  pan_to(8)
end)
btv.keymap.set("n", "<F7>", function()
  pan_to(20)
end)
btv.keymap.set("n", "<F8>", function()
  pan_to(50)
end)
