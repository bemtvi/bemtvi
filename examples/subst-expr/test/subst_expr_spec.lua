-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/subst-expr
--
-- It loads `init.lua` as a session would and drives the same `<leader>N`
-- shortcuts the notes tell you to press, so a demo cannot rot into an
-- instruction that no longer works. Each shortcut only *puts the command on the
-- command line* — the `<CR>` here is what a reader would type next.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- A fresh copy of the sample, so each demo starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:feed("gg")
end

--- Run demo `n` by pressing its shortcut and confirming the command line.
local function demo(t, n)
  t:feed("<Space>" .. n)
  t:feed("<CR>")
end

btv.test.describe("examples/subst-expr", function()
  btv.test.it("the config loads", function(t)
    open(t)
    btv.test.expect(btv.o.number).to_be(true)
  end)

  btv.test.it("a shortcut fills the command line without running it", function(t)
    open(t)
    t:feed("<Space>1")
    -- The command is typed but not confirmed: the buffer must be untouched.
    btv.test.expect(t:line(4)).to_be("alpha beta gamma")
    t:feed("<Esc>")
  end)

  btv.test.it("demo 1 — m[0] is the whole match", function(t)
    open(t)
    demo(t, 1)
    btv.test.expect(t:line(4)).to_be("ALPHA BETA GAMMA")
  end)

  btv.test.it("demo 2 — numbered groups reorder", function(t)
    open(t)
    demo(t, 2)
    btv.test.expect(t:line(7)).to_be("two_one")
    btv.test.expect(t:line(8)).to_be("four_three")
  end)

  btv.test.it("demo 3 — arithmetic on a captured number", function(t)
    open(t)
    demo(t, 3)
    btv.test.expect(t:line(11)).to_be("item 14, item 42")
  end)

  btv.test.it("demo 4 — lnum is the matched line", function(t)
    open(t)
    demo(t, 4)
    btv.test.expect(t:line(14)).to_be("14")
    btv.test.expect(t:line(15)).to_be("15")
    btv.test.expect(t:line(16)).to_be("16")
  end)

  btv.test.it("demo 5 — a group that did not participate is nil", function(t)
    open(t)
    demo(t, 5)
    btv.test.expect(t:line(19)).to_be("nilb")
  end)

  btv.test.it("demo 6 — the sandbox cannot reach the host or the editor", function(t)
    open(t)
    demo(t, 6)
    btv.test.expect(t:line(22)).to_be("nil nil nil")
  end)

  btv.test.it("demo 7 — a failing expression is loud and changes nothing", function(t)
    open(t)
    demo(t, 7)
    btv.test.expect(t:message()).to_contain("E1300")
    btv.test.expect(t:message()).to_contain("boom")
    btv.test.expect(t:line(25)).to_be("alpha")
  end)

  btv.test.it("demo 8 — the literal template form is unchanged", function(t)
    open(t)
    demo(t, 8)
    btv.test.expect(t:line(28)).to_be("right_left")
  end)

  btv.test.it(":Cheat opens its float", function(t)
    open(t)
    t:cmd("Cheat")
    t:wait_for(function()
      return t:float() ~= nil
    end, { message = ":Cheat opened no float" })
  end)
end)
