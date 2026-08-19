-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/smooth-scroll
--
-- The slide itself is the CLIENT's animation; what the editor does is hand it a
-- self-contained descriptor — where the viewport is coming from, where it is
-- going, and how long to take. That descriptor is the feature, and `t:scroll()` is
-- the only view of it: the scroll has already settled by the time any other view
-- looks, animated or not.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample at the top with the config's own duration back in place.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set scrollanim scrollanimduration=220")
  t:feed("gg")
end

btv.test.describe("examples/smooth-scroll", function()
  -- "here we use `vim.o` … Start with a slightly longer-than-default slide."
  btv.test.it("the config lengthens the slide and leaves it on", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(btv.o.scrollanim).to_be(true)
    btv.test.expect(btv.o.scrollanimduration).to_be(220)
  end)

  -- "<C-d> / <C-u> half-page slide down / up"
  btv.test.it("<C-d> and <C-u> hand the client a half-page slide", function(t)
    open(t)
    t:feed("<C-d>")
    local down = t:scroll()
    btv.test.expect(down).never.to_be(nil)
    btv.test.expect(down.to_row > down.from_row).to_be(true)
    -- The viewport really moved there, too.
    btv.test.expect(t:view().topline).to_be(down.to_row + 1)
    t:feed("<C-u>")
    local up = t:scroll()
    btv.test.expect(up.to_row < up.from_row).to_be(true)
    btv.test.expect(t:view().topline).to_be(1)
  end)

  -- "<C-f> / <C-b> full-page slide down / up — the longest slide"
  btv.test.it("<C-f> travels further than <C-d>, so it takes longer", function(t)
    open(t)
    t:feed("<C-d>")
    local half = t:scroll()
    t:feed("gg")
    t:feed("<C-f>")
    local full = t:scroll()
    btv.test.expect(full.to_row > half.to_row).to_be(true)
    btv.test.expect(full.duration_ms > half.duration_ms).to_be(true)
  end)

  -- "the per-scroll duration scales with the travel distance (8ms/line) and is
  --  clamped to this ceiling"
  btv.test.it("the duration scales with travel and stops at the ceiling", function(t)
    open(t)
    t:cmd("set scrollanimduration=400")
    t:feed("<C-d>")
    local half = t:scroll()
    btv.test.expect(half.duration_ms).to_be((half.to_row - half.from_row) * 8)
    -- A long jump is clamped rather than taking forever.
    t:cmd("set scrollanimduration=60")
    t:feed("gg")
    t:feed("G")
    btv.test.expect(t:scroll().duration_ms).to_be(60)
  end)

  -- "G then gg — jump to the bottom, then the top — both animate"
  btv.test.it("an off-screen jump animates too", function(t)
    open(t)
    t:feed("G")
    btv.test.expect(t:scroll()).never.to_be(nil)
    btv.test.expect(t:cursor()[1]).to_be(#btv.buf.lines(0, 0, -1))
    t:feed("gg")
    local back = t:scroll()
    btv.test.expect(back).never.to_be(nil)
    btv.test.expect(back.to_row).to_be(0)
  end)

  btv.test.it("an on-screen move animates nothing", function(t)
    open(t)
    t:feed("j")
    btv.test.expect(t:scroll()).to_be(nil)
  end)

  -- ":set noscrollanim — turn it off — scrolls teleport now"
  btv.test.it("'noscrollanim' moves the viewport without a slide", function(t)
    open(t)
    t:cmd("set noscrollanim")
    t:feed("<C-d>")
    btv.test.expect(t:scroll()).to_be(nil)
    btv.test.expect(t:view().topline > 1).to_be(true)
    t:cmd("set scrollanim")
    t:feed("<C-d>")
    btv.test.expect(t:scroll()).never.to_be(nil)
  end)

  -- "`0` disables animation, like `noscrollanim`."
  btv.test.it("a zero duration disables it the same way", function(t)
    open(t)
    t:cmd("set scrollanimduration=0")
    t:feed("<C-d>")
    btv.test.expect(t:scroll()).to_be(nil)
    btv.test.expect(t:view().topline > 1).to_be(true)
  end)

  -- ":set scrollanim? scrollanimduration? — query the values"
  btv.test.it("the options answer a query, under both spellings", function(t)
    open(t)
    t:cmd("set scrollanim? scrollanimduration?")
    btv.test.expect(t:message()).to_contain("scrollanim")
    btv.test.expect(t:message()).to_contain("scrollanimduration=220")
    -- `scad` is the abbreviation the notes give.
    t:cmd("set scad=90")
    btv.test.expect(btv.o.scrollanimduration).to_be(90)
  end)

  -- ":ScrollReport — re-run those queries from Lua"
  btv.test.it(":ScrollReport reports what the core holds", function(t)
    open(t)
    local got
    local prev_vim, prev_btv = vim.notify, btv.notify
    vim.notify = function(msg)
      got = tostring(msg)
    end
    btv.notify = vim.notify
    t:cmd("ScrollReport")
    vim.notify, btv.notify = prev_vim, prev_btv
    btv.test.expect(got).to_be("scrollanim=true  scrollanimduration=220")
  end)
end)
