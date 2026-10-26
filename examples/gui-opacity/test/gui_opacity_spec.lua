-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/gui-opacity
--
-- What this option changes is PIXELS, in the native GUI on Windows — the notes
-- say so. What a headless spec can hold to account is everything around them:
-- the config's values land, both options take the whole 0–100 range on their
-- own, the float key opens a float, and a value past either end is rejected
-- loudly and changes nothing.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/gui-opacity", function()
  -- §1. "this config sets it below"
  btv.test.it("§1 — the config sets 85", function(t)
    open(t)
    btv.test.expect(btv.o.guiopacity).to_be(85)
    btv.test.expect(vim.o.guiopacity).to_be(85)
  end)

  -- §2. ":set guiopacity=0" — and the selection is still just a selection.
  btv.test.it("§2 — all the way to 0, and a selection still selects", function(t)
    open(t)
    t:cmd("set guiopacity=0")
    btv.test.expect(btv.o.guiopacity).to_be(0)
    t:feed("Vj")
    btv.test.expect(t:mode()).to_be("V")
    t:feed("<Esc>")
  end)

  -- §3. ":set guiopacity=100" — "100 is the default".
  btv.test.it("§3 — back to solid", function(t)
    open(t)
    t:cmd("set guiopacity=0")
    t:cmd("set guiopacity=100")
    btv.test.expect(btv.o.guiopacity).to_be(100)
  end)

  -- §4. ":set guiopacity=0 guihlopacity=50", then "<Space>f" opens a float.
  btv.test.it("§4 — guihlopacity is its own knob, and <Space>f opens a float", function(t)
    open(t)
    btv.test.expect(btv.o.cursorline).to_be(true)
    t:cmd("set guiopacity=0 guihlopacity=50")
    btv.test.expect(btv.o.guiopacity).to_be(0)
    btv.test.expect(btv.o.guihlopacity).to_be(50)
    t:feed("<Space>f")
    t:wait_for(function()
      return t:float() ~= nil
    end, { message = "<Space>f opened no float" })
    btv.test.expect(t:float().text).to_contain("A float clears the editor under it")
    -- "set guihlopacity=0 then <Space>f again"
    t:cmd("set guihlopacity=0")
    btv.test.expect(btv.o.guihlopacity).to_be(0)
  end)

  -- §5. ":set guiopacity=150 → E474: Invalid argument — not quietly clamped".
  btv.test.it("§5 — out of range is rejected loudly", function(t)
    open(t)
    t:cmd("set guiopacity=150")
    btv.test.expect(t:message()).to_contain("E474")
    -- "the value still in effect (the rejected write changed nothing)"
    t:cmd("set guiopacity?")
    btv.test.expect(t:message()).to_contain("guiopacity=85")
    t:cmd("set guiopacity=-5")
    btv.test.expect(t:message()).to_contain("E487")
    btv.test.expect(btv.o.guiopacity).to_be(85)
  end)
end)
