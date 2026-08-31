-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/virt-text
--
-- Virtual text is in neither the buffer nor the painted glyphs of the line it sits
-- on: `t:lines()` never shows it, and an `eol` / `right_align` / `win_col` note is
-- drawn past the text. `t:decor()` reads that layer; `t:screen()` is where the
-- whole virtual ROWS (`virt_lines`) show up, because those really are painted rows.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The screen row that paints buffer line `line` (1-based), or nil. Virtual rows
--- are `false` in `numbers`, which is exactly how they are told apart.
local function row_of(t, line)
  for row, number in ipairs(t:view().numbers) do
    if number == line then
      return row
    end
  end
end

btv.test.describe("examples/virt-text", function()
  -- "eol: a note after the line's last character."
  btv.test.it("the eol note is drawn after the line's text", function(t)
    open(t)
    local d = t:decor(3)
    btv.test.expect(d.virt_pos).to_be("eol")
    btv.test.expect(d.virt_text).to_contain("← end-of-line note")
    -- It is NOT in the buffer.
    btv.test.expect(t:line(3)).never.to_contain("end-of-line note")
  end)

  -- "virt_text_hide: this note disappears while the line is visually selected …
  --  the plain note above always stays."
  btv.test.it("virt_text_hide drops its note under a selection", function(t)
    open(t)
    btv.test.expect(t:decor(3).virt_text).to_contain("(hides under selection)")
    t:feed("3GV")
    local under = t:decor(3).virt_text
    btv.test.expect(under).never.to_contain("(hides under selection)")
    -- …while the sibling without the flag stays.
    btv.test.expect(under).to_contain("← end-of-line note")
    t:feed("<Esc>")
    btv.test.expect(t:decor(3).virt_text).to_contain("(hides under selection)")
  end)

  -- "inline: spliced into the line, pushing the real text (and the cursor) right."
  btv.test.it("the inline chunk is anchored inside the line", function(t)
    open(t)
    local d = t:decor(row_of(t, 4))
    btv.test.expect(d.virt_pos).to_be("inline")
    btv.test.expect(d.virt_text).to_be("[INLINE]")
    -- Byte column 15 — just before "spliced".
    btv.test.expect(d.virt_col).to_be(15)
    btv.test.expect(t:line(4):sub(16, 22)).to_be("spliced")
    -- The buffer text is untouched.
    btv.test.expect(t:line(4)).never.to_contain("[INLINE]")
  end)

  -- "overlay: painted over the cells starting at the anchor column (no shift)."
  btv.test.it("the overlay chunk covers the cells it is anchored at", function(t)
    open(t)
    local d = t:decor(row_of(t, 5))
    btv.test.expect(d.virt_pos).to_be("overlay")
    btv.test.expect(d.virt_text).to_be("≈≈covered≈≈")
    -- Byte column 9 — where "OVERLAY" starts, which it covers rather than shifts.
    btv.test.expect(d.virt_col).to_be(9)
    btv.test.expect(t:line(5):sub(10, 16)).to_be("OVERLAY")
  end)

  -- "right_align: flushed to the window's right edge. win_col: pinned to a fixed
  --  window column (here 50)."
  btv.test.it("the right-aligned and fixed-column notes are drawn", function(t)
    open(t)
    local d = t:decor(row_of(t, 6))
    btv.test.expect(d.virt_pos).to_be("right_align")
    btv.test.expect(d.virt_text).to_contain(" right-aligned ")
    btv.test.expect(d.virt_text).to_contain("│col50")
    -- The buffer line talks ABOUT the tag; the tag itself is virtual.
    btv.test.expect(t:line(6)).never.to_contain("│col50")
  end)

  -- "virt_lines: whole extra rows … One ABOVE the `def compute…` line … and two
  --  BELOW it."
  btv.test.it("virt_lines add whole rows above and below", function(t)
    open(t)
    -- The reserved row above line 8 carries the header, and is not a buffer line.
    local above = row_of(t, 8)
    btv.test
      .expect(t:decor(above - 1).virt_lines)
      .to_contain("┌─ compute(): doubles and offsets ─┐")
    btv.test.expect(t:view().numbers[above - 1]).to_be(false)
    -- …and the two annotation rows sit just below line 9.
    local below = row_of(t, 9)
    btv.test.expect(t:decor(below + 1).virt_lines).to_contain("└ note: pure, no side effects")
    btv.test.expect(t:decor(below + 2).virt_lines).to_contain("used by the demo harness")
    -- None of it is buffer text.
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("no side effects")
  end)

  -- "the cursor steps OVER the virtual rows, never onto them"
  btv.test.it("the cursor steps over the virtual rows", function(t)
    open(t)
    t:feed("8G")
    btv.test.expect(t:cursor()[1]).to_be(8)
    t:feed("j")
    btv.test.expect(t:cursor()[1]).to_be(9)
    t:feed("j")
    btv.test.expect(t:cursor()[1]).to_be(10)
    t:feed("j")
    btv.test.expect(t:cursor()[1]).to_be(11)
  end)

  -- ":set number — gutter numbers skip the virtual rows (they have no buffer line)"
  btv.test.it("the virtual rows carry no buffer line number", function(t)
    open(t)
    t:cmd("set number")
    local numbers = t:view().numbers
    local above = row_of(t, 8)
    -- The reserved rows own no buffer line, so they get no number.
    btv.test.expect(numbers[above]).to_be(8)
    btv.test.expect(numbers[above - 1]).to_be(false)
    t:cmd("set nonumber")
  end)

  -- "they ride edits and undo like any extmark"
  btv.test.it("the notes ride an edit above them", function(t)
    open(t)
    t:feed("ggO<Esc>")
    -- Everything moved down one line; the eol note went with its line.
    btv.test.expect(t:decor(4).virt_text).to_contain("← end-of-line note")
    btv.test.expect(t:decor(3).virt_text).to_be(nil)
    t:cmd("undo")
    btv.test.expect(t:decor(3).virt_text).to_contain("← end-of-line note")
  end)
end)
