-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/gutter-text
--
-- The gutter text column is drawn beside the text rather than in it, so it is
-- `t:decor(row).gutter` and `t:gutter().gutter_text_width`, never `t:lines()` or
-- `t:screen()`.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Open the sample afresh and paint the diff numbers over it: the config paints
--- once, against whatever buffer was current when it ran.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  t:cmd("GutterDiff")
end

local expect = btv.test.expect

btv.test.describe("examples/gutter-text", function()
  -- 1. "each line of the hunk shows its old number, its new one, or both"
  btv.test.it("each line shows its old number, its new one, or both", function(t)
    open(t)
    expect(t:decor(2).gutter).to_be("  10   10 ")
    expect(t:decor(3).gutter).to_be("  11      ")
    expect(t:decor(4).gutter).to_be("       11 ")
    expect(t:decor(5).gutter).to_be("       12 ")
    expect(t:decor(6).gutter).to_be("  12   13 ")
    expect(t:decor(9).gutter).to_be("  14   15 ")
    expect(t:gutter().gutter_text_width).to_be(10)
  end)

  -- "The `@@` header line has none, and its cell is blank."
  btv.test.it("the header line has none", function(t)
    open(t)
    expect(t:decor(1).gutter).to_be_nil()
  end)

  -- "removed lines show only the old number in red, added lines only the new one"
  btv.test.it("removed and added lines are tinted", function(t)
    open(t)
    expect(t:decor(3).line_bg).to_be(true)
    expect(t:decor(4).line_bg).to_be(true)
    expect(t:decor(2).line_bg).to_be_nil()
  end)

  -- 2. "`:set nowrap`, then `$` on line 6 … the numbers stay where they are"
  btv.test.it("the numbers stay put as a long line scrolls sideways", function(t)
    open(t)
    t:cmd("set nowrap")
    t:feed("6G$")
    expect(t:view().leftcol > 0).to_be(true)
    expect(t:decor(6).gutter).to_be("  12   13 ")
  end)

  -- "`0`, then click a character of the text … the cursor lands on the character"
  btv.test.it("a click lands on the character clicked", function(t)
    open(t)
    t:feed("0")
    -- Screen row 1 (0-based) shows line 2, `     let args = …`; byte 9 is the `a` of
    -- `args`, painted past every gutter column.
    local col = t:gutter().total + 9
    t:mouse("left", "press", 1, col)
    t:mouse("left", "release", 1, col)
    expect(t:cursor()).to_equal({ 2, 9 })
  end)

  -- 3. "`ggo` then `<Esc>` … every number moves down with its line"
  btv.test.it("the numbers move with their lines, and back on undo", function(t)
    open(t)
    t:feed("ggo<Esc>")
    expect(t:decor(2).gutter).to_be_nil()
    expect(t:decor(3).gutter).to_be("  10   10 ")
    t:feed("u")
    expect(t:decor(2).gutter).to_be("  10   10 ")
  end)

  -- 4. "`:GutterBlame` … a commit hash and an author per line … still lines up"
  btv.test.it("the blame column is as wide as its widest entry, in cells", function(t)
    open(t)
    t:cmd("GutterBlame")
    expect(t:decor(1).gutter).to_be("3f9a1c2 ana ")
    expect(t:decor(3).gutter).to_be("77be0d4 李雷 ")
    expect(t:decor(7).gutter).to_be("c01dbee joão ")
    -- `joão` is 4 cells and `李雷` is 4 too (two wide characters), so the widest is
    -- 7 + 6 = 13 cells, though `李雷`'s row is 15 bytes.
    expect(t:gutter().gutter_text_width).to_be(13)
  end)

  -- "`:GutterDiff` to go back to the numbers, `:GutterOff` to clear the column"
  btv.test.it("the column goes back to numbers, and clears", function(t)
    open(t)
    t:cmd("GutterBlame")
    t:cmd("GutterDiff")
    expect(t:decor(2).gutter).to_be("  10   10 ")
    t:cmd("GutterOff")
    expect(t:decor(2).gutter).to_be_nil()
    expect(t:gutter().gutter_text_width).to_be(0)
  end)
end)
