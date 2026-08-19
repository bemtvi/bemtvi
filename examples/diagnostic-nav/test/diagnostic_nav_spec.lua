-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/diagnostic-nav
--
-- The three built-in motions, driven exactly as the notes say — including the
-- claim that matters most: these are CORE defaults, so they work with nothing
-- bound, and any of them can still be taken back by a config.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

-- The seeded diagnostics, as buffer lines (the config's `lnum` is 0-based).
local ERRORS = { 3, 4, 12, 16 }
local ALL = { 3, 4, 8, 10, 12, 16, 17 }

--- Open the sample, re-reading it so each test starts from the same text, and
--- wait for the BufEnter handler to seed the diagnostics.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  t:wait_for(function()
    return #btv.diagnostic.get(0) == #ALL
  end, { message = "the demo diagnostics were never seeded" })
end

--- The line the cursor is on.
local function line(t)
  return t:cursor()[1]
end

btv.test.describe("examples/diagnostic-nav", function()
  btv.test.it("the demo seeds its diagnostics with no language server", function(t)
    open(t)
    local got = {}
    for _, d in ipairs(btv.diagnostic.get(0)) do
      got[#got + 1] = d.lnum + 1
    end
    table.sort(got)
    btv.test.expect(got).to_equal(ALL)
  end)

  -- "]d  jump to the next diagnostic (any severity)"
  btv.test.it("]d walks every diagnostic in order", function(t)
    open(t)
    for _, want in ipairs(ALL) do
      t:feed("]d")
      btv.test.expect(line(t)).to_be(want)
    end
  end)

  -- "it wraps at the end"
  btv.test.it("]d wraps around at the end", function(t)
    open(t)
    t:feed("G")
    t:feed("]d")
    btv.test.expect(line(t)).to_be(ALL[1])
  end)

  -- "[d  jump to the previous"
  btv.test.it("[d walks backwards", function(t)
    open(t)
    -- The last line IS a diagnostic line, so the first `[d` steps off it to the
    -- one before — "previous", not "the one I am on".
    t:feed("G")
    for i = #ALL - 1, 1, -1 do
      t:feed("[d")
      btv.test.expect(line(t)).to_be(ALL[i])
    end
    -- …and it wraps at the top.
    t:feed("[d")
    btv.test.expect(line(t)).to_be(ALL[#ALL])
  end)

  -- "]e / [e  … (severity = ERROR only)"
  btv.test.it("]e stops only on errors", function(t)
    open(t)
    for _, want in ipairs(ERRORS) do
      t:feed("]e")
      btv.test.expect(line(t)).to_be(want)
    end
    -- …and skipped the warnings and the hint entirely.
    t:feed("gg")
    t:feed("]e")
    btv.test.expect(line(t)).never.to_be(8)
    btv.test.expect(line(t)).never.to_be(10)
  end)

  btv.test.it("[e walks the errors backwards", function(t)
    open(t)
    t:feed("G")
    for i = #ERRORS, 1, -1 do
      t:feed("[e")
      btv.test.expect(line(t)).to_be(ERRORS[i])
    end
    t:feed("[e")
    btv.test.expect(line(t)).to_be(ERRORS[#ERRORS])
  end)

  -- "<C-w>d  show the diagnostics under the cursor in a float". It is the same
  -- read-only listing surface hover uses, so its rows are `t:lines()`.
  btv.test.it("<C-w>d shows the line's diagnostics in full", function(t)
    open(t)
    t:feed("3G")
    t:feed("<C-w>d")
    t:wait_for(function()
      return btv.buf.name(0) == "[Diagnostics]"
    end, { message = "<C-w>d opened no diagnostics listing" })
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("undefined function `prnit`")
    t:feed("q")
  end)

  btv.test.it("<C-w><C-d> is the same key", function(t)
    open(t)
    t:feed("4G")
    t:feed("<C-w><C-d>")
    t:wait_for(function()
      return btv.buf.name(0) == "[Diagnostics]"
    end, { message = "<C-w><C-d> opened no diagnostics listing" })
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("undefined variable `naem`")
    t:feed("q")
  end)

  -- The scope is the LINE, not the cursor's column: sitting anywhere on a flagged
  -- line is enough, which is what the note tells a reader to do.
  btv.test.it("<C-w>d is line-scoped, not column-scoped", function(t)
    open(t)
    t:feed("3G$")
    t:feed("<C-w>d")
    t:wait_for(function()
      return btv.buf.name(0) == "[Diagnostics]"
    end, { message = "<C-w>d at end-of-line found nothing" })
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("undefined function `prnit`")
    t:feed("q")
  end)

  btv.test.it("<C-w>d on a clean line says so, loudly", function(t)
    open(t)
    t:feed("1G")
    t:feed("<C-w>d")
    t:sleep(60)
    btv.test.expect(btv.buf.name(0)).never.to_be("[Diagnostics]")
    btv.test.expect(t:message()).to_contain("No diagnostics on this line")
  end)

  -- The rendering the config turned on, so the seeded diagnostics are visible.
  btv.test.it("the signs and inline messages the config asked for are drawn", function(t)
    open(t)
    btv.test.expect(t:decor(3).sign).to_be("E")
    btv.test.expect(t:decor(8).sign).to_be("W")
    btv.test.expect(t:decor(17).sign).to_be("H")
    btv.test.expect(t:decor(3).diagnostic).to_contain("undefined function `prnit`")
    btv.test.expect(t:decor(3).severity).to_be(btv.diagnostic.severity.ERROR)
    btv.test.expect(t:decor(1).diagnostic).to_be_nil()
    btv.test.expect(t:decor(1).sign).to_be_nil()
  end)

  -- "Being defaults, any of them can be overridden … or disabled."
  btv.test.it("a config map on ]d wins over the default", function(t)
    open(t)
    local hit = 0
    btv.keymap.set("n", "]d", function()
      hit = hit + 1
    end)
    t:feed("]d")
    btv.test.expect(hit).to_be(1)
    btv.test.expect(line(t)).to_be(1)
  end)

  btv.test.it("an empty map disables one outright", function(t)
    open(t)
    btv.keymap.set("n", "]e", function() end)
    t:feed("]e")
    btv.test.expect(line(t)).to_be(1)
  end)

  -- A mapped `[`-prefix must not break the `` `[ `` mark motion beside it.
  btv.test.it("the [d default leaves the `[ mark motion alone", function(t)
    open(t)
    t:feed("5GyyP")
    -- `P` put the copy above, so the pasted line IS line 5 and `` `[ `` is on it.
    t:feed("G")
    t:feed("`[")
    btv.test.expect(line(t)).to_be(5)
  end)
end)
