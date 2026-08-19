-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ts-install
--
-- `:TSInstall` downloads a grammar from the network and compiles it, so no case
-- here runs it — a test suite must not depend on the network, and installing into
-- the real data dir is not this spec's business. What IS checked is everything
-- around it: the commands exist, `:TSInstallInfo` reports the real layout, the
-- config's indentation options apply, and the treesitter-driven editing the notes
-- promise works once a grammar is present (skipped when it is not).

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.rs")
  t:cmd("e!")
  t:cmd("set expandtab shiftwidth=4 tabstop=4")
  t:feed("gg")
end

--- Whether a rust grammar is loaded for this buffer (the notes' prerequisite).
local function has_grammar(t)
  for row = 9, 14 do
    if #t:highlights(row) > 0 then
      return true
    end
  end
  return false
end

btv.test.describe("examples/ts-install", function()
  -- "Spaces, not tabs, so the indentation below is visible as columns."
  btv.test.it("the config sets spaces four wide", function(t)
    t:cmd("e " .. DIR .. "/sample.rs")
    btv.test.expect(btv.bo.expandtab).to_be(true)
    btv.test.expect(btv.bo.shiftwidth).to_be(4)
    btv.test.expect(btv.bo.tabstop).to_be(4)
  end)

  btv.test.it("the sample really is a rust buffer", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("rust")
  end)

  -- ":TSInstallInfo — list installed parsers + their queries / root"
  btv.test.it(":TSInstallInfo reports the data-dir layout", function(t)
    open(t)
    t:cmd("TSInstallInfo")
    local report = t:message()
    if report == "" then
      -- The listing may open as a panel instead of an echo; read it there.
      report = table.concat(t:lines(), "\n")
    end
    btv.test.expect(report).never.to_be("")
    -- Whatever the shape, it names where things live.
    btv.test.expect(report:lower()).to_match("parser")
  end)

  btv.test.it(":TSInstall refuses a language it does not know", function(t)
    open(t)
    t:cmd("TSInstall definitelynotalanguage")
    t:wait_for(function()
      return (t:message() or "") ~= ""
    end, { tries = 100, interval = 20, message = ":TSInstall said nothing at all" })
    btv.test.expect(t:message():lower()).never.to_contain("installed definitelynotalanguage")
  end)

  -- "Put the cursor on the `fn main() {` line and press `o` — the new line lands
  --  one level in (4 spaces)."
  btv.test.it("o after an opening brace indents one level", function(t)
    open(t)
    t:sleep(120)
    if not has_grammar(t) then
      print("skip: no rust treesitter grammar installed")
      return
    end
    -- Line 9 is `fn main() {`.
    t:feed("9G")
    btv.test.expect(t:line(9)).to_be("fn main() {")
    t:feed("ox<Esc>")
    btv.test.expect(t:line(10)).to_be("    x")
    t:cmd("undo")
  end)

  -- "Jam a statement to column 0 inside a block and press `==` — it reindents."
  btv.test.it("== reindents a line the grammar knows the depth of", function(t)
    open(t)
    t:sleep(120)
    if not has_grammar(t) then
      print("skip: no rust treesitter grammar installed")
      return
    end
    t:feed("10G")
    local original = t:line(10)
    t:feed("0d^")
    btv.test.expect(t:line(10)).never.to_be(original)
    t:feed("==")
    btv.test.expect(t:line(10)).to_be(original)
    t:cmd("undo")
    t:cmd("undo")
  end)
end)
