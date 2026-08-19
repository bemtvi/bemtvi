-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/colorcolumn
--
-- A ruler is neither buffer text nor a painted glyph, and it is not a highlight
-- span either: the server sends the resolved column list and the CLIENT paints it.
-- `t:rulers()` is the view that can see it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/colorcolumn", function()
  btv.test.it("the config sets the two rulers", function(t)
    open(t)
    btv.test.expect(btv.wo.colorcolumn).to_be("80,120")
  end)

  -- 1. "Two faint vertical guides run down the text at columns 80 and 120."
  btv.test.it("try-it 1 — both rulers are drawn, at 80 and 120", function(t)
    open(t)
    btv.test.expect(t:rulers()).to_equal({ 80, 120 })
  end)

  -- A ruler is a property of the WINDOW, not of any line: it runs down the whole
  -- text body, over short lines and past the end of the buffer alike.
  btv.test.it("the rulers do not depend on what any line contains", function(t)
    open(t)
    local everywhere = t:rulers()
    t:feed("G")
    btv.test.expect(t:rulers()).to_equal(everywhere)
    t:cmd("enew")
    t:cmd("setlocal colorcolumn=80,120")
    btv.test.expect(t:rulers()).to_equal({ 80, 120 })
  end)

  -- 3. ":CC80 / :CC80120 / :NoCC / :CCReport"
  btv.test.it("try-it 3 — :CC80 keeps one guide", function(t)
    open(t)
    t:cmd("CC80")
    btv.test.expect(btv.wo.colorcolumn).to_be("80")
    btv.test.expect(t:message()).to_contain("colorcolumn = 80")
    btv.test.expect(t:rulers()).to_equal({ 80 })
  end)

  btv.test.it("try-it 3 — :CC80120 brings both back", function(t)
    open(t)
    t:cmd("CC80")
    t:cmd("CC80120")
    btv.test.expect(btv.wo.colorcolumn).to_be("80,120")
    btv.test.expect(t:rulers()).to_equal({ 80, 120 })
  end)

  btv.test.it("try-it 3 — :NoCC clears them", function(t)
    open(t)
    t:cmd("NoCC")
    btv.test.expect(btv.wo.colorcolumn).to_be("")
    btv.test.expect(t:message()).to_contain("colorcolumn cleared")
    btv.test.expect(t:rulers()).to_equal({})
  end)

  btv.test.it("try-it 3 — :CCReport echoes the current value", function(t)
    open(t)
    t:cmd("CC80")
    t:cmd("CCReport")
    btv.test.expect(t:message()).to_contain("colorcolumn=80")
  end)

  -- 4. "`:set cc=+1` — a 'textwidth'-relative entry, which bemtvi skips."
  btv.test.it("try-it 4 — a textwidth-relative entry draws no ruler", function(t)
    open(t)
    t:cmd("set cc=+1")
    -- Accepted — the option really holds it — but it resolves to no ruler.
    btv.test.expect(btv.wo.colorcolumn).to_be("+1")
    btv.test.expect(t:rulers()).to_equal({})
    -- …while an absolute one does draw.
    t:cmd("set cc=100")
    btv.test.expect(t:rulers()).to_equal({ 100 })
  end)

  -- "It is window-local (each split carries its own rulers)."
  btv.test.it("the ruler set is window-local", function(t)
    open(t)
    t:cmd("split")
    t:cmd("CC80")
    btv.test.expect(btv.wo.colorcolumn).to_be("80")
    t:feed("<C-w>j")
    btv.test.expect(btv.wo.colorcolumn).to_be("80,120")
    t:cmd("only")
  end)

  -- 5. The double-width lines. Where the ruler lands inside a CJK glyph is the
  -- client's problem (a terminal cannot paint half a glyph, so it tints the whole
  -- one); what the server owes it is the same column list whatever the text holds.
  btv.test.it("try-it 5 — the ruler set is unchanged over double-width text", function(t)
    open(t)
    for _, needle in ipairs({ "wide: ", "wide:  " }) do
      for i, line in ipairs(t:lines()) do
        if line:find(needle, 1, true) then
          t:feed(i .. "G")
          break
        end
      end
      btv.test.expect(t:rulers()).to_equal({ 80, 120 })
    end
  end)

  -- "Empty (no ruler) by default, so you opt in." The window keeps its own value
  -- across a buffer swap (it is window-local, and `:enew` only changes the
  -- buffer), so reset the option itself to see the default.
  btv.test.it("the option's default is empty — you opt in", function(t)
    open(t)
    t:cmd("set colorcolumn&")
    btv.test.expect(btv.wo.colorcolumn).to_be("")
    btv.test.expect(t:rulers()).to_equal({})
  end)
end)
