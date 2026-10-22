-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/intro
--
-- The intro is virtual lines, so `t:lines()` (the buffer) never shows it and
-- `t:screen()` reads its rows blank: `t:decor()` is the view that sees it. The test
-- runner isn't armed for the startup intro and has no truecolor client, so these
-- bring it up with `:intro` and see the text-only form (the logo, and the startup
-- rules, are covered by the server's own suite).

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- The intro's rows as drawn, each a virtual line's text.
local function intro(t)
  local rows = {}
  for _, row in ipairs(t:decor()) do
    if row.virt_lines then
      rows[#rows + 1] = row.virt_lines
    end
  end
  return rows
end

--- The first intro row containing `needle`, or nil.
local function row_with(t, needle)
  for _, row in ipairs(intro(t)) do
    if row:find(needle, 1, true) then
      return row
    end
  end
end

btv.test.describe("examples/intro", function()
  -- 1. "`bemtvi`, the version, and `:q<Enter>` / `:Plugins<Enter>` below it."
  btv.test.it("try-it 1 — :intro shows the name, the version and the commands", function(t)
    t:cmd("intro")
    btv.test.expect(row_with(t, "bemtvi")).never.to_be(nil)
    btv.test.expect(row_with(t, "version ")).never.to_be(nil)
    btv.test.expect(row_with(t, ":q<Enter>")).to_contain("to exit")
    btv.test.expect(row_with(t, ":Plugins<Enter>")).to_contain("to manage plugins")
    -- The buffer itself is untouched, and the cursor stays on its one line.
    btv.test.expect(t:lines()).to_equal({ "" })
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)

  -- 1. "the commands in yellow (section 1)": the config's group wins over the
  -- fallback link.
  btv.test.it("section 1 — the config's BtvIntroKey colour is kept", function(t)
    t:cmd("intro")
    local key = btv.hl.get(0, { name = "BtvIntroKey", link = false })
    btv.test.expect(key.fg).to_be(0xf9e931)
    btv.test.expect(key.bold).to_be(true)
    -- The scheme's own groups fill in the rest.
    btv.test.expect(btv.hl.get(0, { name = "BtvIntroTitle" }).link).to_be("Title")
  end)

  -- 2. "`:set colorcolumn=40` draws a ruler down the window": the ruler is set, and
  -- the intro still draws (its box hides the ruler behind it).
  btv.test.it("try-it 2 — a ruler and the intro together", function(t)
    t:cmd("intro")
    t:cmd("set colorcolumn=40")
    btv.test.expect(t:rulers()).to_equal({ 40 })
    btv.test.expect(row_with(t, "bemtvi")).never.to_be(nil)
  end)

  -- 3. "`ihello<Esc>`: the intro is gone. `u` empties the buffer again, and the
  -- intro stays gone."
  btv.test.it("try-it 3 — typing dismisses it, and undo doesn't bring it back", function(t)
    t:cmd("intro")
    t:feed("ihello<Esc>")
    btv.test.expect(t:lines()).to_equal({ "hello" })
    btv.test.expect(#intro(t)).to_be(0)
    t:feed("u")
    btv.test.expect(t:lines()).to_equal({ "" })
    btv.test.expect(#intro(t)).to_be(0)
  end)

  btv.test.it("try-it 3 — opening the sample dismisses it", function(t)
    t:cmd("intro")
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(t:lines()[1]).to_contain("A file to open")
    btv.test.expect(#intro(t)).to_be(0)
  end)

  -- 4. "In a buffer with text, `:intro` refuses and says to `:enew` first."
  btv.test.it("try-it 4 — :intro refuses a buffer with text", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    local ok, err = pcall(btv.intro.show)
    btv.test.expect(ok).to_be(false)
    btv.test.expect(tostring(err)).to_contain(":enew")
    btv.test.expect(#intro(t)).to_be(0)
  end)

  -- 4. "`:enew`, then `:intro`: the screen is back."
  btv.test.it("try-it 4 — :enew then :intro brings it back", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    t:cmd("enew")
    t:cmd("intro")
    btv.test.expect(row_with(t, "bemtvi")).never.to_be(nil)
  end)
end)
