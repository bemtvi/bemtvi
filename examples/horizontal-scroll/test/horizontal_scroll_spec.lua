-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/horizontal-scroll
--
-- Sideways scrolling is a WINDOW offset: the painted rows still carry each line's
-- full text and the client slides them left by `leftcol`. So `t:view().leftcol` is
-- what proves it — the buffer line never changes, and neither does the row text.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The first line in the sample longer than the window is wide.
local function long_line(t)
  for i, line in ipairs(t:lines()) do
    if #line > 100 then
      return i, line
    end
  end
  error("the sample has no long line", 0)
end

btv.test.describe("examples/horizontal-scroll", function()
  btv.test.it("the config sets a generous lookahead margin", function(t)
    open(t)
    btv.test.expect(btv.wo.sidescrolloff).to_be(8)
    -- The `nowrap` half of the pair: long lines clip rather than wrap.
    btv.test.expect(btv.wo.wrap).to_be(false)
  end)

  -- "$ jump to end-of-line — the viewport scrolls right to follow"
  btv.test.it("try-it — $ scrolls the viewport right to follow the cursor", function(t)
    open(t)
    local row, line = long_line(t)
    t:feed(row .. "G0")
    btv.test.expect(t:view().leftcol).to_be(0)
    t:feed("$")
    local leftcol = t:view().leftcol
    btv.test.expect(leftcol > 0).to_be(true)
    -- Scrolled far enough to bring the end of the line into view.
    btv.test.expect(leftcol < #line).to_be(true)
  end)

  -- "0 back to column 0 — it scrolls all the way home"
  btv.test.it("try-it — 0 scrolls all the way home again", function(t)
    open(t)
    local row = long_line(t)
    t:feed(row .. "G$")
    btv.test.expect(t:view().leftcol > 0).to_be(true)
    t:feed("0")
    btv.test.expect(t:view().leftcol).to_be(0)
  end)

  -- The buffer never changes — only the window's view of it.
  btv.test.it("scrolling sideways changes nothing in the buffer", function(t)
    open(t)
    local before = t:lines()
    local row = long_line(t)
    t:feed(row .. "G$")
    btv.test.expect(t:lines()).to_equal(before)
    btv.test.expect(btv.bo.modified).to_be(false)
  end)

  -- "sidescrolloff — a MARGIN of columns kept between the cursor and the edge
  --  while scrolling, so you always see context ahead."
  btv.test.it("try-it — sidescrolloff keeps context ahead of the cursor", function(t)
    open(t)
    local row, line = long_line(t)
    btv.test.expect(#line > 120).to_be(true)
    -- With no margin the cursor can sit on the very last painted cell.
    t:cmd("setlocal sidescrolloff=0")
    t:feed(row .. "G0")
    t:feed("120l")
    local tight = t:view().leftcol
    -- The same walk with a margin has scrolled FURTHER, keeping context ahead.
    t:cmd("setlocal sidescrolloff=20")
    t:feed(row .. "G0")
    t:feed("120l")
    btv.test.expect(t:view().leftcol > tight).to_be(true)
  end)

  -- "sidescroll — the scroll STEP. 0 recenters the cursor when it falls off an
  --  edge; >0 (default 1) scrolls just enough to bring it to the edge."
  btv.test.it("try-it — :set ss=0 recenters instead of stepping", function(t)
    open(t)
    local row = long_line(t)
    t:cmd("setlocal sidescrolloff=0 sidescroll=1")
    t:feed(row .. "G0")
    t:feed("100l")
    -- Stepping brings the cursor just to the edge, so the offset is the smallest
    -- that keeps it visible.
    local stepped = t:view().leftcol
    btv.test.expect(stepped > 0).to_be(true)

    t:cmd("setlocal sidescroll=0")
    t:feed(row .. "G0")
    t:feed("100l")
    -- Recentering puts the cursor in the middle instead, so it scrolls further.
    btv.test.expect(t:view().leftcol).never.to_be(stepped)
  end)

  -- ":set ss? siso?  query the current values" / ":SideReport"
  btv.test.it("try-it — :SideReport echoes both options", function(t)
    open(t)
    t:cmd("setlocal sidescroll=3 sidescrolloff=8")
    t:cmd("SideReport")
    btv.test.expect(t:message()).to_contain("sidescroll=3")
    btv.test.expect(t:message()).to_contain("sidescrolloff=8")
    -- The single-option query form the notes now point at.
    t:cmd("set sidescrolloff?")
    btv.test.expect(t:message()).to_contain("sidescrolloff=8")
  end)

  -- "Two window-local options tune it, exactly as in vim."
  btv.test.it("both options are per window", function(t)
    open(t)
    t:feed("<C-w>v")
    t:cmd("setlocal sidescrolloff=0")
    btv.test.expect(btv.wo.sidescrolloff).to_be(0)
    t:feed("<C-w>w")
    btv.test.expect(btv.wo.sidescrolloff).to_be(8)
    t:cmd("only")
  end)
end)
