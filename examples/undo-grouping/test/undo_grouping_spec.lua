-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/undo-grouping
--
-- Each numbered note in init.lua, typed exactly as written. `t:feed` is the
-- keyboard, so these undo the way a person's typing does.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample fresh and park on its last line, where every note types.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("G")
end

--- The sample's line count, before any note adds a line.
local function base(t)
  return #t:lines()
end

btv.test.describe("examples/undo-grouping", function()
  btv.test.it("1 — u takes back one word at a time, <C-r> brings them back", function(t)
    open(t)
    local n = base(t)
    t:feed("ofoo bar baz<Esc>")
    btv.test.expect(t:line(n + 1)).to_be("foo bar baz")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("foo bar")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("foo")
    t:feed("u")
    btv.test.expect(#t:lines()).to_be(n)
    t:feed("<C-r>")
    btv.test.expect(t:line(n + 1)).to_be("foo")
    t:feed("<C-r>")
    btv.test.expect(t:line(n + 1)).to_be("foo bar")
  end)

  btv.test.it("2 — typing and deleting are separate steps", function(t)
    open(t)
    local n = base(t)
    t:feed("oabcd<BS><BS>xy<Esc>")
    btv.test.expect(t:line(n + 1)).to_be("abxy")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("ab")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("abcd")
  end)

  btv.test.it("3 — a pause splits even a word", function(t)
    open(t)
    local n = base(t)
    t:feed("ofoo")
    t:sleep(1500)
    t:feed("bar<Esc>")
    btv.test.expect(t:line(n + 1)).to_be("foobar")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("foo")
  end)

  btv.test.it("4 — an edit after a cursor move is its own step", function(t)
    open(t)
    local n = base(t)
    t:feed("ofoobar<Left><Left><Left>X<Esc>")
    btv.test.expect(t:line(n + 1)).to_be("fooXbar")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("foobar")
  end)

  btv.test.it("5 — a mapping's output is one step", function(t)
    open(t)
    local n = base(t)
    t:feed("o<C-t>check this<Esc>")
    btv.test.expect(t:line(n + 1)).to_be("TODO: check this")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("TODO: check")
    t:feed("u")
    btv.test.expect(t:line(n + 1)).to_be("TODO: ")
    t:feed("u")
    btv.test.expect(#t:lines()).to_be(n)
  end)

  btv.test.it("6 — g- and g+ walk the word steps", function(t)
    open(t)
    local n = base(t)
    t:feed("ofoo bar<Esc>")
    t:feed("g-")
    btv.test.expect(t:line(n + 1)).to_be("foo")
    t:feed("g+")
    btv.test.expect(t:line(n + 1)).to_be("foo bar")
  end)
end)
