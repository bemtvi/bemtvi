-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/listchars
--
-- The glyphs are drawn INSTEAD of the buffer's characters, so they exist only on
-- the painted screen: `t:screen()`, never `t:lines()`. Each numbered note is typed
-- as written.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Painted row `n` (1-based buffer line, the sample is short enough to fit).
local function row(t, n)
  return t:screen()[n]
end

btv.test.describe("examples/listchars", function()
  -- 1. "line 3's tab is `»` then blanks; line 6 ends in `···`; line 7's
  --    non-breaking spaces show as `␣`."
  btv.test.it("1 — the config's glyphs are drawn on open", function(t)
    open(t)
    btv.test.expect(btv.wo.list).to_be(true)
    btv.test.expect(row(t, 3)).to_contain("»   indented with a tab")
    btv.test.expect(row(t, 6)).to_contain("three spaces···")
    btv.test.expect(row(t, 7)).to_contain("this␣word␣gap")
    -- The buffer itself is untouched.
    btv.test.expect(t:lines()[3]).to_be("\tindented with a tab")
  end)

  -- 2. "\l … the glyphs vanish, and the message line says `list off`."
  btv.test.it("2 — \\l toggles 'list'", function(t)
    open(t)
    t:feed("\\l")
    btv.test.expect(btv.wo.list).to_be(false)
    btv.test.expect(t:message()).to_contain("list off")
    btv.test.expect(row(t, 6)).never.to_contain("·")
    t:feed("\\l")
    btv.test.expect(btv.wo.list).to_be(true)
    btv.test.expect(t:message()).to_contain("list on")
    btv.test.expect(row(t, 6)).to_contain("···")
  end)

  btv.test.it("2 — :set list! toggles it too", function(t)
    open(t)
    t:cmd("set list!")
    btv.test.expect(btv.wo.list).to_be(false)
    t:cmd("set list!")
    btv.test.expect(btv.wo.list).to_be(true)
  end)

  -- 3. "every space is a `·`, leading runs are `→` (line 4 starts `→→→→`), the
  --    tabs are `>--` arrows, and each line ends in `¬`."
  btv.test.it("3 — :ListAll draws every invisible character", function(t)
    open(t)
    t:cmd("ListAll")
    btv.test.expect(row(t, 1)).to_contain("Tabs,·blanks·and")
    btv.test.expect(row(t, 1)).to_contain(":set·list¬")
    btv.test.expect(row(t, 4)).to_contain("→→→→indented·with·four·spaces¬")
    btv.test.expect(row(t, 3)).to_contain(">---indented")
    btv.test.expect(row(t, 8)).to_contain("col>umns>---line>---up¬")
  end)

  -- 4. "line 4's four-space indent shows `│   `, a guide every 4 columns, and
  --    line 3's tab shows the same `│   `."
  btv.test.it("4 — indent guides from leadmultispace", function(t)
    open(t)
    t:feed(":set listchars=leadmultispace:│\\ \\ \\ ,tab:│\\ <CR>")
    btv.test.expect(btv.wo.listchars).to_be("leadmultispace:│   ,tab:│ ")
    btv.test.expect(row(t, 4)).to_contain("│   indented with four spaces")
    btv.test.expect(row(t, 3)).to_contain("│   indented with a tab")
  end)

  -- 5. ":set listchars? … `E474: Invalid argument: listchars=eol:ab` … the old
  --    value is kept. `:set listchars&` restores the default."
  btv.test.it("5 — read back, reject a bad value, restore the default", function(t)
    open(t)
    t:cmd("set listchars?")
    btv.test.expect(t:message()).to_contain("listchars=tab:» ,trail:·,nbsp:␣")
    t:feed(":set listchars=eol:ab<CR>")
    btv.test.expect(t:message()).to_contain("E474: Invalid argument: listchars=eol:ab")
    btv.test.expect(btv.wo.listchars).to_be("tab:» ,trail:·,nbsp:␣")
    t:cmd("set listchars&")
    btv.test.expect(btv.wo.listchars).to_be("tab:> ,trail:-,nbsp:+")
  end)
end)
