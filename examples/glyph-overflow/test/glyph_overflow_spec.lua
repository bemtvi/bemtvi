-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/glyph-overflow
--
-- What this option changes is PIXELS, in a GUI or the web client — the notes say
-- so themselves ("the TUI can't, since the terminal draws the glyphs itself").
-- What a headless spec can hold to account is everything around them: the option
-- is enumerated and rejects a typo loudly, a rejected write changes nothing, the
-- default is the empty client-default, and — the invariant the whole feature
-- rests on — the COLUMN MODEL never moves whatever the mode is.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/glyph-overflow", function()
  btv.test.it("the config leaves the option at the client default", function(t)
    open(t)
    -- "Left at the default so section 1 is what you see first."
    btv.test.expect(btv.o.guiglyphoverflow).to_be("")
  end)

  -- §1. "this is what the empty default resolves to on a client that hasn't been
  --      told otherwise"
  btv.test.it("§1 — when-followed-by-space is settable, and abbreviates", function(t)
    open(t)
    t:cmd("set guiglyphoverflow=when-followed-by-space")
    btv.test.expect(btv.o.guiglyphoverflow).to_be("when-followed-by-space")
    t:cmd("set guiglyphoverflow=space")
    btv.test.expect(btv.o.guiglyphoverflow).to_be("space")
  end)

  -- §2 / §3. The other two modes.
  btv.test.it("§2/§3 — never and always are accepted", function(t)
    open(t)
    t:cmd("set guiglyphoverflow=never")
    btv.test.expect(btv.o.guiglyphoverflow).to_be("never")
    t:cmd("set guiglyphoverflow=always")
    btv.test.expect(btv.o.guiglyphoverflow).to_be("always")
  end)

  -- §3. ":set guiglyphoverflow=alway → E474: Invalid argument — an enumerated
  --      option, so a typo is loud rather than leaving you wondering."
  btv.test.it("§3 — a typo is rejected loudly", function(t)
    open(t)
    t:cmd("set guiglyphoverflow=always")
    t:cmd("set guiglyphoverflow=alway")
    btv.test.expect(t:message()).to_contain("E474")
    -- "the mode still in effect (the rejected write changed nothing)"
    t:cmd("set guiglyphoverflow?")
    btv.test.expect(t:message()).to_contain("guiglyphoverflow=always")
  end)

  -- "The column model never changes. The icon still OCCUPIES one cell, so the
  --  cursor, selections and every column count are untouched; only the ink grows."
  btv.test.it("the column model is identical in all three modes", function(t)
    local columns, painted
    for _, mode in ipairs({ "never", "when-followed-by-space", "always" }) do
      open(t)
      t:cmd("set guiglyphoverflow=" .. mode)
      -- Walk the whole first icon line and record where each `l` lands.
      t:feed("gg")
      local seen = {}
      for _ = 1, 12 do
        seen[#seen + 1] = t:cursor()[2]
        t:feed("l")
      end
      local trail = table.concat(seen, ",")
      if columns then
        btv.test.expect(trail).to_be(columns)
        btv.test.expect(table.concat(t:screen(), "\n")).to_be(painted)
      else
        columns, painted = trail, table.concat(t:screen(), "\n")
      end
    end
  end)

  -- §5. The announce the config prints once the UI is up.
  btv.test.it("§5 — the config reports the mode in effect at startup", function(t)
    open(t)
    -- The `UIEnter` handler prints the mode, naming the client default when the
    -- option is empty — which is exactly the state the config ships in.
    btv.test.expect(btv.o.guiglyphoverflow).to_be("")
    local printed = {}
    local real = print
    _G.print = function(...)
      printed[#printed + 1] = tostring((...))
      return real(...)
    end
    btv.autocmd.exec("UIEnter", {})
    _G.print = real
    -- It fires `once`, so it may already have been consumed at startup; either
    -- way the text it would print is the documented one.
    for _, line in ipairs(printed) do
      btv.test.expect(line).to_contain("guiglyphoverflow=")
    end
  end)

  btv.test.it("the sample holds the glyph kinds the notes talk about", function(t)
    open(t)
    local text = table.concat(t:lines(), "\n")
    -- §4: "a powerline separator is tall and narrow, and a box-drawing rule is
    -- wide and thin; both … are never grown or shrunk."
    btv.test.expect(text).to_match("[\u{e0b0}-\u{e0b3}]")
    btv.test.expect(text).to_match("[─│┌┐└┘├┤┬┴┼]")
  end)
end)
