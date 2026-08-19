-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/scrolloff
--
-- Scrolling is a *viewport* fact, not a buffer one, so almost everything here
-- reads `t:view()` — where the focused window is scrolled to, and which buffer
-- line each painted row carries. `t:lines()` cannot see any of it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample at the top with the config's own settings back in place.
--- `'scrolloff'` and `'wrap'` are window-local and the demo's commands change
--- them, so each case restores both rather than trusting the last one.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("setlocal scrolloff=8 nowrap")
  t:feed("gg")
end

--- How many rows the focused window paints.
local function rows(t)
  return #t:view().numbers
end

btv.test.describe("examples/scrolloff", function()
  btv.test.it("the config sets scrolloff and leaves wrap off", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(btv.wo.scrolloff).to_be(8)
    btv.test.expect(btv.wo.wrap).to_be(false)
  end)

  -- "1. Put the cursor at the top and hold j … the window starts scrolling once
  --  the cursor is 8 lines from the bottom."
  btv.test.it("1 — the view scrolls 8 lines before the cursor reaches the bottom", function(t)
    open(t)
    local h = rows(t)
    btv.test.expect(t:view().topline).to_be(1)
    -- The last row the cursor may occupy while the top of the file is still shown.
    t:feed((h - 8) .. "G")
    btv.test.expect(t:cursor()[1]).to_be(h - 8)
    btv.test.expect(t:view().topline).to_be(1)
    -- One more line down and the window has already started sliding.
    t:feed("j")
    btv.test.expect(t:view().topline).to_be(2)
    -- …and there are still 8 painted rows of what is coming.
    local numbers = t:view().numbers
    btv.test.expect(numbers[#numbers] - t:cursor()[1]).to_be(8)
  end)

  -- "2. Now hold k back up … the window scrolls once the cursor is 8 lines from
  --  the TOP."
  btv.test.it("2 — the margin is symmetric on the way back up", function(t)
    open(t)
    t:feed("40G")
    local top = t:view().topline
    -- Step up to exactly 8 rows below the top row: still no scroll.
    t:feed((top + 8) .. "G")
    btv.test.expect(t:view().topline).to_be(top)
    t:feed("k")
    btv.test.expect(t:view().topline).to_be(top - 1)
  end)

  -- "3. Press G (last line) — the cursor DOES reach the bottom row and no blank
  --  rows open below it."
  btv.test.it("3 — end-of-file lets the cursor into the margin", function(t)
    open(t)
    t:feed("G")
    local view = t:view()
    local last = #btv.buf.lines(0, 0, -1)
    btv.test.expect(t:cursor()[1]).to_be(last)
    -- The last painted row IS the last line: no `~` filler opened below it.
    btv.test.expect(view.numbers[#view.numbers]).to_be(last)
  end)

  -- "4. :set scrolloff=0 and repeat step 1 — the cursor now rides the bottom row."
  btv.test.it("4 — scrolloff=0 lets the cursor ride the bottom row", function(t)
    open(t)
    local h = rows(t)
    t:cmd("set scrolloff=0")
    t:feed("gg")
    t:feed(h .. "G")
    btv.test.expect(t:cursor()[1]).to_be(h)
    btv.test.expect(t:view().topline).to_be(1)
    -- `:set so=8` is the same option under its abbreviation.
    t:cmd("set so=8")
    btv.test.expect(btv.wo.scrolloff).to_be(8)
  end)

  -- "5. :Wrap then move onto one of the long paragraph lines — it lays across
  --  several rows instead of scrolling sideways."
  btv.test.it("5 — :Wrap lays the long line across several rows", function(t)
    open(t)
    -- With nowrap the long paragraph line (line 5) is one row.
    local function rows_for(line)
      local n = 0
      for _, number in ipairs(t:view().numbers) do
        if number == line then
          n = n + 1
        end
      end
      return n
    end
    t:feed("5G")
    btv.test.expect(rows_for(5)).to_be(1)
    t:cmd("Wrap")
    btv.test.expect(btv.wo.wrap).to_be(true)
    btv.test.expect(rows_for(5) > 1).to_be(true)
    t:cmd("NoWrap")
    btv.test.expect(btv.wo.wrap).to_be(false)
    btv.test.expect(rows_for(5)).to_be(1)
  end)

  btv.test.it("5 — the wrap commands report which way they went", function(t)
    open(t)
    t:cmd("Wrap")
    btv.test.expect(t:message()).to_contain("wrap ON")
    t:cmd("NoWrap")
    btv.test.expect(t:message()).to_contain("wrap OFF")
  end)

  -- "6. :SoReport to echo the current values."
  btv.test.it("6 — :SoReport echoes scrolloff and wrap", function(t)
    open(t)
    t:cmd("SoReport")
    btv.test.expect(t:message()).to_contain("scrolloff=8")
    btv.test.expect(t:message()).to_contain("nowrap")
  end)

  -- "It is window-local (each split carries its own value)."
  btv.test.it("the margin is window-local, not editor-wide", function(t)
    open(t)
    t:cmd("split")
    t:cmd("setlocal scrolloff=0")
    btv.test.expect(btv.wo.scrolloff).to_be(0)
    t:feed("<C-w>w")
    btv.test.expect(btv.wo.scrolloff).to_be(8)
    t:cmd("only")
  end)
end)
