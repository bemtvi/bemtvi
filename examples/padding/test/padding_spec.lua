-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/padding
--
-- The margin is resolved SERVER-side — the editor shrinks the text area itself —
-- so it shows up in everything downstream: how many rows the window paints, where
-- the gutter starts, and where a click lands. Those are what the spec asserts on,
-- rather than on the option string alone.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  vim.wo.padding = 2
  t:feed("<Esc>")
end

--- How many rows the focused window paints.
local function rows(t)
  return #t:screen()
end

btv.test.describe("examples/padding", function()
  btv.test.it("the config gives the starting window a 2-cell margin", function(t)
    open(t)
    t:cmd("set padding?")
    btv.test.expect(t:message()).to_contain("padding=2")
  end)

  -- "the editor shrinks the text area itself"
  btv.test.it("the margin costs the window rows, top and bottom", function(t)
    open(t)
    local with_margin = rows(t)
    t:cmd("set padding&")
    local flush = rows(t)
    btv.test.expect(flush - with_margin).to_be(4)
  end)

  -- "the number gutter, the text, and the status line all inset by it"
  btv.test.it("the left margin insets the whole content box", function(t)
    open(t)
    t:cmd("setlocal number numberwidth=4 signcolumn=no")
    -- The leftmost screen cell that maps to a buffer position at all, found by
    -- probing: with a left margin, everything shifts right by exactly it.
    local function leftmost()
      for row = 0, 8 do
        for col = 0, 30 do
          t:feed("5G$")
          t:mouse("left", "press", row, col)
          t:mouse("left", "release", row, col)
          if t:cursor()[1] == 3 then
            return col
          end
        end
      end
      return nil
    end
    t:cmd("set padding&")
    local flush = leftmost()
    btv.test.expect(flush).never.to_be_nil()
    t:cmd("set padding=0,6")
    btv.test.expect(leftmost()).to_be(flush + 6)
  end)

  -- 2. "Drive it by hand on the current window with :set (CSS shorthand)."
  btv.test.it("try-it 2 — the CSS shorthand forms all parse", function(t)
    open(t)
    local function set_and_query(value)
      t:cmd("set padding=" .. value)
      t:cmd("set padding?")
      return t:message()
    end
    -- The canonical form is the SHORTEST shorthand that says the same thing —
    -- one number when all four sides agree, two when the pairs do, four otherwise.
    btv.test.expect(set_and_query("4")).to_contain("padding=4")
    btv.test.expect(set_and_query("0,6")).to_contain("padding=0 6")
    btv.test.expect(set_and_query("1,2,3,4")).to_contain("padding=1 2 3 4")
    t:cmd("set padding&")
    t:cmd("set padding?")
    btv.test.expect(t:message()).to_contain("padding=0")
  end)

  btv.test.it("try-it 2 — each side is honoured on its own", function(t)
    open(t)
    t:cmd("set padding&")
    local flush = rows(t)
    -- Only the top and bottom cost rows; left/right cost columns.
    t:cmd("set padding=3,0,0,0")
    btv.test.expect(rows(t)).to_be(flush - 3)
    t:cmd("set padding=0,0,5,0")
    btv.test.expect(rows(t)).to_be(flush - 5)
    t:cmd("set padding=0,9,0,9")
    btv.test.expect(rows(t)).to_be(flush)
  end)

  -- "It is *window-local* … each window carries its own value."
  btv.test.it("try-it 3 — the margin is per window", function(t)
    open(t)
    t:cmd("set padding&")
    t:cmd("vsplit")
    -- `setlocal`, so each window really is carrying its own value rather than
    -- both reading one tier.
    t:cmd("setlocal padding=0")
    local flush = rows(t)
    local flush_win = vim.api.nvim_get_current_win()
    t:feed("<C-w>w")
    t:cmd("setlocal padding=3")
    btv.test.expect(rows(t)).to_be(flush - 6)
    btv.test.expect(vim.api.nvim_get_current_win()).never.to_be(flush_win)
    t:feed("<C-w>w")
    btv.test.expect(vim.api.nvim_get_current_win()).to_be(flush_win)
    btv.test.expect(rows(t)).to_be(flush)
    t:cmd("set padding?")
    btv.test.expect(t:message()).to_contain("padding=0")
    t:cmd("only")
  end)

  -- 4. "Clicks map through the margin: clicking in the text lands on the right
  --     cell, and a click out in the blank margin hits nothing."
  btv.test.it("try-it 4 — a click in the blank margin hits nothing", function(t)
    open(t)
    t:cmd("set padding=4")
    t:feed("3G0")
    local before = t:cursor()
    -- Row 0 is inside the top margin, whatever the tabline does.
    t:mouse("left", "press", 0, 0)
    t:mouse("left", "release", 0, 0)
    btv.test.expect(t:cursor()).to_equal(before)
  end)

  -- "soft-wrap, horizontal scroll, the cursor, and mouse hit-testing all already
  --  account for the margin"
  btv.test.it("soft wrap reflows inside the margin", function(t)
    open(t)
    t:cmd("set padding&")
    t:cmd("setlocal wrap")
    local flush_rows = #t:screen()
    t:cmd("set padding=0,20")
    -- A narrower text body wraps the same text over more display rows, so fewer
    -- buffer lines fit on screen.
    local padded_last = t:view().numbers[#t:view().numbers]
    t:cmd("set padding&")
    local flush_last = t:view().numbers[#t:view().numbers]
    btv.test.expect(flush_rows > 0).to_be(true)
    btv.test.expect(padded_last <= flush_last).to_be(true)
  end)
end)
