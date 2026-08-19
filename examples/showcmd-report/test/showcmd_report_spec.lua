-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/showcmd-report
--
-- `'showcmd'` paints a corner the client draws from its own redraw key, and a
-- *withheld* mapped prefix never reaches the editor at all — so `t:showcmd()` is
-- the only view that can see either. `'report'` speaks through the message line.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample at the top with both indicators as the config leaves them.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set showcmd report=2")
  t:feed("gg")
end

btv.test.describe("examples/showcmd-report", function()
  btv.test.it("the config turns both indicators on", function(t)
    open(t)
    btv.test.expect(btv.o.showcmd).to_be(true)
    btv.test.expect(btv.o.report).to_be(2)
  end)

  -- "1. Type-this: 2 … See-that: `2` in the corner. Add `d` -> `2d`; add `3` -> `2d3`."
  btv.test.it("1 — the corner grows with the partly-typed command", function(t)
    open(t)
    t:feed("2")
    btv.test.expect(t:showcmd()).to_be("2")
    t:feed("d")
    btv.test.expect(t:showcmd()).to_be("2d")
    t:feed("3")
    btv.test.expect(t:showcmd()).to_be("2d3")
    t:feed("<Esc>")
    btv.test.expect(t:showcmd()).to_be("")
  end)

  -- "an armed register (`\"a`), a key waiting for its argument (`f`, `z`, `<C-w>`)"
  btv.test.it("1 — every stage of the grammar shows", function(t)
    open(t)
    t:feed('"a')
    btv.test.expect(t:showcmd()).to_be('"a')
    t:feed("<Esc>")
    t:feed("f")
    btv.test.expect(t:showcmd()).to_be("f")
    t:feed("<Esc>")
    t:feed("z")
    btv.test.expect(t:showcmd()).to_be("z")
    t:feed("<Esc>")
  end)

  -- "2. `'showcmd'` in Visual mode — the SIZE of the selection."
  btv.test.it("2 — a selection reports its size", function(t)
    open(t)
    t:feed("Vjj")
    btv.test.expect(t:showcmd()).to_be("3")
    t:feed("<Esc>")
    t:feed("vll")
    btv.test.expect(t:showcmd()).to_be("3")
    t:feed("<Esc>")
  end)

  -- "3. A half-typed MAPPING shows too. Type-this: <Space> f … See-that:
  --  `<Space>f` in the corner. Press `s` to complete the mapping."
  btv.test.it("3 — a withheld mapped prefix shows in the corner", function(t)
    open(t)
    local line, col = t:cursor()[1], t:cursor()[2]
    t:feed("<Space>f")
    btv.test.expect(t:showcmd()).to_be("<Space>f")
    -- Nothing else moved: the keys never reached the editor.
    btv.test.expect(t:cursor()[1]).to_be(line)
    btv.test.expect(t:cursor()[2]).to_be(col)
    t:feed("s")
    btv.test.expect(t:message()).to_contain("the mapping fired")
    btv.test.expect(t:showcmd()).to_be("")
  end)

  btv.test.it("'noshowcmd' empties the corner", function(t)
    open(t)
    t:cmd("set noshowcmd")
    t:feed("2d")
    btv.test.expect(t:showcmd()).to_be("")
    t:feed("<Esc>")
  end)

  -- "4. Type-this: 5dd — See-that: `5 fewer lines`."
  btv.test.it("4 — 'report' names what the last command changed", function(t)
    open(t)
    t:feed("5dd")
    btv.test.expect(t:message()).to_contain("5 fewer lines")
    t:feed("p")
    btv.test.expect(t:message()).to_contain("5 more lines")
    t:feed("6yy")
    btv.test.expect(t:message()).to_contain("6 lines yanked")
    t:feed('"a6yy')
    btv.test.expect(t:message()).to_contain('6 lines yanked into "a')
    t:feed("5>>")
    btv.test.expect(t:message()).to_contain("5 lines >ed 1 time")
  end)

  -- "The default is 2: a command has to change MORE than two lines to say so."
  btv.test.it("4 — an everyday dd stays quiet under the threshold", function(t)
    open(t)
    t:cmd("echo ''")
    t:feed("dd")
    btv.test.expect(t:message()).never.to_contain("fewer lines")
    t:feed("2dd")
    btv.test.expect(t:message()).never.to_contain("fewer lines")
    t:feed("3dd")
    btv.test.expect(t:message()).to_contain("3 fewer lines")
  end)

  -- "5. report = 0 reports EVERYTHING … a single `dd` says `1 line less` —
  --  vim's wording, singular and all. A big 'report' (99) silences the lot."
  btv.test.it("5 — report=0 reports a single line, report=99 nothing", function(t)
    open(t)
    t:cmd("set report=0")
    t:feed("dd")
    btv.test.expect(t:message()).to_contain("1 line less")
    t:cmd("set report=99")
    t:cmd("echo ''")
    t:feed("10dd")
    btv.test.expect(t:message()).to_be("")
  end)
end)
