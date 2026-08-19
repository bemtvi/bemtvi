-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/fillchars
--
-- The filler rows are drawn INSTEAD of buffer lines, so they exist only on the
-- painted screen — `t:screen()`, never `t:lines()`. Each numbered TRY-IT is typed
-- as written.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The painted rows past the end of the buffer.
local function filler_rows(t)
  local out = {}
  for row = #t:lines() + 1, #t:screen() do
    out[#out + 1] = t:screen()[row]
  end
  return out
end

btv.test.describe("examples/fillchars", function()
  -- 1. "On open the rows below the three sample lines are BLANK (no `~`)."
  btv.test.it("try-it 1 — the config blanks the end-of-buffer markers", function(t)
    open(t)
    btv.test.expect(btv.wo.fillchars).to_be("eob: ")
    local fillers = filler_rows(t)
    btv.test.expect(#fillers > 0).to_be(true)
    for _, row in ipairs(fillers) do
      btv.test.expect(row:gsub("%s", "")).to_be("")
    end
  end)

  -- 2. ":TildeBack -> the `~` markers come back."
  btv.test.it("try-it 2 — :TildeBack restores vim's ~", function(t)
    open(t)
    t:cmd("TildeBack")
    btv.test.expect(btv.wo.fillchars).to_be("eob:~")
    btv.test.expect(t:message()).to_contain("vim default")
    for _, row in ipairs(filler_rows(t)) do
      btv.test.expect(row:sub(1, 1)).to_be("~")
    end
  end)

  btv.test.it("try-it 2 — :TildeHide blanks them again", function(t)
    open(t)
    t:cmd("TildeBack")
    t:cmd("TildeHide")
    btv.test.expect(btv.wo.fillchars).to_be("eob: ")
    btv.test.expect(t:message()).to_contain("hidden")
    for _, row in ipairs(filler_rows(t)) do
      btv.test.expect(row:gsub("%s", "")).to_be("")
    end
  end)

  -- ":set fillchars=eob:· use a mid-dot instead"
  btv.test.it("any character works, not just ~ and blank", function(t)
    open(t)
    t:cmd("set fillchars=eob:·")
    for _, row in ipairs(filler_rows(t)) do
      btv.test.expect(row:sub(1, #"·")).to_be("·")
    end
  end)

  btv.test.it(":set fillchars? echoes the current value", function(t)
    open(t)
    t:cmd("TildeBack")
    t:cmd("set fillchars?")
    btv.test.expect(t:message()).to_contain("fillchars=eob:~")
  end)

  -- 3. "<C-w>v then :TildeHide -> blank this split only … (window-local)"
  btv.test.it("try-it 3 — 'fillchars' is per window", function(t)
    open(t)
    t:cmd("TildeBack")
    t:feed("<C-w>v")
    t:cmd("TildeHide")
    btv.test.expect(btv.wo.fillchars).to_be("eob: ")
    for _, row in ipairs(filler_rows(t)) do
      btv.test.expect(row:gsub("%s", "")).to_be("")
    end
    t:feed("<C-w>w")
    btv.test.expect(btv.wo.fillchars).to_be("eob:~")
    for _, row in ipairs(filler_rows(t)) do
      btv.test.expect(row:sub(1, 1)).to_be("~")
    end
    t:cmd("only")
  end)

  -- 4. ":FillReport -> 'win N: fillchars=… | win M: fillchars=…'"
  btv.test.it("try-it 4 — :FillReport reads every window's value back", function(t)
    open(t)
    t:cmd("TildeBack")
    t:feed("<C-w>v")
    t:cmd("TildeHide")
    t:cmd("FillReport")
    local report = t:message()
    btv.test.expect(report).to_contain([[fillchars="eob: "]])
    btv.test.expect(report).to_contain([[fillchars="eob:~"]])
    btv.test.expect(report).to_contain("|")
    t:cmd("only")
  end)
end)
