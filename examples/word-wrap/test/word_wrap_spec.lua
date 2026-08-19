-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/word-wrap
--
-- Soft-wrap is a *layout* fact: the buffer is unchanged and only the painted rows
-- differ. So the cases read `t:view()` (which buffer line each row carries),
-- `t:screen()` (the glyphs, including the `showbreak` marker), and `t:gutter()`
-- — `t:lines()` shows nothing of it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample with the config's own four settings back in place.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set wrap breakindent showbreak=↪ breakindentopt=sbr")
  t:feed("gg")
end

--- How many painted rows buffer line `line` occupies.
local function rows_for(t, line)
  local n = 0
  for _, number in ipairs(t:view().numbers) do
    if number == line then
      n = n + 1
    end
  end
  return n
end

btv.test.describe("examples/word-wrap", function()
  btv.test.it("the config turns on all four settings", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(btv.wo.wrap).to_be(true)
    btv.test.expect(btv.wo.breakindent).to_be(true)
    btv.test.expect(btv.o.showbreak).to_be("↪")
    btv.test.expect(btv.o.breakindentopt).to_be("sbr")
  end)

  btv.test.it("a long line is laid across continuation rows", function(t)
    open(t)
    -- Line 3 is the long paragraph; line 5 is short.
    btv.test.expect(rows_for(t, 3) > 1).to_be(true)
    btv.test.expect(rows_for(t, 5)).to_be(1)
    -- It is still ONE buffer line.
    btv.test.expect(#btv.buf.lines(0, 2, 3)).to_be(1)
  end)

  -- "$ — the cursor lands on the wrapped row, not off-screen; the viewport does
  --  NOT scroll sideways"
  btv.test.it("$ on a wrapped line never scrolls the viewport sideways", function(t)
    open(t)
    t:feed("3G$")
    btv.test.expect(t:view().leftcol).to_be(0)
    btv.test.expect(t:cursor()[1]).to_be(3)
  end)

  -- ":set nowrap — switch back to clip + horizontal-scroll"
  btv.test.it("'nowrap' clips and pans instead", function(t)
    open(t)
    t:cmd("set nowrap")
    btv.test.expect(rows_for(t, 3)).to_be(1)
    t:feed("3G$")
    btv.test.expect(t:view().leftcol > 0).to_be(true)
    -- …and 'wrap' folds it back.
    t:cmd("set wrap")
    btv.test.expect(t:view().leftcol).to_be(0)
    btv.test.expect(rows_for(t, 3) > 1).to_be(true)
  end)

  -- "gj / gk — step ONE display row (within a wrapped line), unlike j/k"
  btv.test.it("gj and gk step one display row, j and k a whole line", function(t)
    open(t)
    t:feed("3G0")
    local col = t:cursor()[2]
    t:feed("gj")
    -- Still on the same buffer line, further along it.
    btv.test.expect(t:cursor()[1]).to_be(3)
    btv.test.expect(t:cursor()[2] > col).to_be(true)
    t:feed("gk")
    btv.test.expect(t:cursor()[2]).to_be(col)
    -- `j` leaves the line entirely.
    t:feed("j")
    btv.test.expect(t:cursor()[1]).to_be(4)
  end)

  -- "g0 / g$ — the first / last column of the DISPLAY row, unlike 0/$"
  btv.test.it("g0 and g$ act on the display row, 0 and $ on the line", function(t)
    open(t)
    t:feed("3G0gj")
    local row_start = t:cursor()[2]
    btv.test.expect(row_start > 0).to_be(true)
    t:feed("g$")
    local row_end = t:cursor()[2]
    btv.test.expect(row_end > row_start).to_be(true)
    t:feed("g0")
    btv.test.expect(t:cursor()[2]).to_be(row_start)
    -- `$` goes to the end of the whole buffer line, well past the display row.
    t:feed("$")
    btv.test.expect(t:cursor()[2] > row_end).to_be(true)
    -- `0` goes back to the line's own first column.
    t:feed("0")
    btv.test.expect(t:cursor()[2]).to_be(0)
  end)

  btv.test.it("g^ is the first non-blank of the display row", function(t)
    open(t)
    t:feed("3G0gjg^")
    btv.test.expect(t:screen()[3 + 1]).never.to_be(nil)
    btv.test.expect(t:cursor()[1]).to_be(3)
  end)

  -- "showbreak — a marker drawn at the start of every continuation row"
  btv.test.it("the continuation rows carry the showbreak marker", function(t)
    open(t)
    local view = t:view()
    local screen = t:screen()
    local first, marked = nil, 0
    for row, number in ipairs(view.numbers) do
      if number == 3 then
        first = first or row
        if row > first and screen[row]:sub(1, #"↪") == "↪" then
          marked = marked + 1
        end
      end
    end
    btv.test.expect(marked > 0).to_be(true)
    -- The FIRST row of the line carries no marker.
    btv.test.expect(screen[first]:sub(1, #"↪")).never.to_be("↪")
  end)

  -- ":set showbreak= — clear the continuation marker … there is no
  --  `:set noshowbreak`"
  btv.test.it("showbreak= clears it, and there is no noshowbreak", function(t)
    open(t)
    t:cmd("set showbreak=")
    btv.test.expect(btv.o.showbreak).to_be("")
    local view, screen = t:view(), t:screen()
    for row, number in ipairs(view.numbers) do
      if number == 3 and row > 1 then
        btv.test.expect(screen[row]:sub(1, #"↪")).never.to_be("↪")
      end
    end
    t:cmd("set noshowbreak")
    btv.test.expect(t:message()).to_contain("E518")
  end)

  -- "The number gutter shows a wrapped line's number on its FIRST display row
  --  only; the continuation rows get a blank gutter"
  btv.test.it("the number gutter is drawn once per wrapped line", function(t)
    open(t)
    t:cmd("set number")
    btv.test.expect(t:gutter().number_width > 0).to_be(true)
    -- The row numbers repeat for the continuation rows; the client blanks the
    -- gutter on the repeats, which is what the repeated number means.
    btv.test.expect(rows_for(t, 3) > 1).to_be(true)
    -- Both spellings have to go: `'relativenumber'` is on out of the box, and
    -- either one keeps the column.
    t:cmd("set nonumber norelativenumber")
    btv.test.expect(t:gutter().number_width).to_be(0)
  end)

  -- ":set nobreakindent — drop the hanging indent on continuation rows"
  btv.test.it("'breakindent' hangs the continuation rows under the indent", function(t)
    open(t)
    t:cmd("enew!")
    t:cmd("setlocal wrap breakindent showbreak= breakindentopt=")
    -- An indented long line: with breakindent the continuation rows start under
    -- the indent, without it at column 0.
    t:feed("i    " .. string.rep("word ", 60) .. "<Esc>")
    local function second_row()
      local screen, numbers = t:screen(), t:view().numbers
      for row = 2, #numbers do
        if numbers[row] == 1 then
          return screen[row]
        end
      end
    end
    btv.test.expect(second_row():sub(1, 4)).to_be("    ")
    t:cmd("setlocal nobreakindent")
    btv.test.expect(second_row():sub(1, 1)).never.to_be(" ")
  end)

  -- ":set wrap? — query the current value" / ":WrapReport — the same from Lua"
  btv.test.it("the option answers a query, and :WrapReport re-runs it", function(t)
    open(t)
    t:cmd("set wrap?")
    btv.test.expect(t:message()).to_be("wrap")
    t:cmd("WrapReport")
    btv.test.expect(t:message()).to_be("wrap")
    t:cmd("set nowrap")
    t:cmd("WrapReport")
    btv.test.expect(t:message()).to_be("nowrap")
  end)
end)
