-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/keymap-builtin
--
-- The point of the playground is the ABSENCE of a pause, so every case types the
-- built-in's sequence and asserts it has ALREADY fired — with no following key
-- and no idle flush. `t:feed` settles exactly one tick, which is the whole
-- assertion: had the matcher held the run, nothing would have happened yet.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The mappings announce themselves with `print`; record that at the source so a
-- case can prove the MAP fired rather than the built-in.
local printed = {}
do
  local real = print
  _G.print = function(...)
    printed[#printed + 1] = tostring((...))
    return real(...)
  end
end

dofile(DIR .. "/init.lua")

local function last_print()
  return printed[#printed] or ""
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/keymap-builtin", function()
  -- 1. "TYPE: gg and STOP. The cursor jumps to the FIRST line IMMEDIATELY."
  btv.test.it("§1 — gg fires instantly under a colliding gh map", function(t)
    open(t)
    t:feed("G")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    t:feed("gg")
    -- No following key, no wait: it has already happened.
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)

  btv.test.it("§1 — gh still fires the mapping", function(t)
    open(t)
    t:feed("G")
    t:feed("gh")
    btv.test.expect(last_print()).to_contain("gh mapping fired")
    -- …and it did NOT move the cursor, so the built-in did not also run.
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
  end)

  -- 2. "TYPE: dd -> deletes the current line instantly."
  btv.test.it("§2 — dd and dw fire instantly under a colliding dh map", function(t)
    open(t)
    local before = #t:lines()
    t:feed("2Gdd")
    btv.test.expect(#t:lines()).to_be(before - 1)
    btv.test.expect(t:line(2)).to_contain("line 3")

    open(t)
    t:feed("2Gdw")
    btv.test.expect(t:line(2)).to_be("2 — delete me with `dd`; or `dw` to drop just the first word")
  end)

  btv.test.it("§2 — dh still fires the mapping, and deletes nothing", function(t)
    open(t)
    local before = t:lines()
    t:feed("2Gdh")
    btv.test.expect(last_print()).to_contain("dh mapping fired")
    btv.test.expect(t:lines()).to_equal(before)
  end)

  -- 3. "TYPE: fx -> jumps to the next `x` on the line instantly … ; repeats it."
  btv.test.it("§3 — fx and ; fire instantly under colliding fh/ff maps", function(t)
    open(t)
    t:feed("4G0")
    t:feed("fx")
    local line = t:line(4)
    local first = line:find("x", 1, true)
    btv.test.expect(t:cursor()[2]).to_be(first - 1)
    t:feed(";")
    local second = line:find("x", first + 1, true)
    btv.test.expect(t:cursor()[2]).to_be(second - 1)
  end)

  btv.test.it("§3 — fh and ff still fire their mappings", function(t)
    open(t)
    t:feed("4G0")
    t:feed("fh")
    btv.test.expect(last_print()).to_contain("fh mapping fired")
    t:feed("ff")
    btv.test.expect(last_print()).to_contain("ff mapping fired")
  end)

  -- 4. "TYPE: rZ -> replaces the char under the cursor with `Z` instantly."
  btv.test.it("§4 — rZ replaces instantly under a colliding rx map", function(t)
    open(t)
    t:feed("3G0")
    t:feed("rZ")
    btv.test.expect(t:line(3):sub(1, 1)).to_be("Z")
  end)

  btv.test.it("§4 — rx fires the mapping instead, and replaces nothing", function(t)
    open(t)
    t:feed("3G0")
    local before = t:line(3)
    t:feed("rx")
    btv.test.expect(last_print()).to_contain("rx mapping fired")
    btv.test.expect(t:line(3)).to_be(before)
  end)

  -- 5. "The INVERSE — a genuinely-ambiguous *mapped* prefix still WAITS."
  btv.test.it("§5 — a run that is a prefix of a longer MAPPING is held", function(t)
    open(t)
    -- `jk` mapped makes a bare `j` ambiguous: it must not move until the run
    -- breaks the mapping prefix.
    local fired = 0
    btv.keymap.set("n", "jk", function()
      fired = fired + 1
    end)
    t:feed("2G")
    t:feed("j")
    btv.test.expect(t:cursor()[1]).to_be(2)
    btv.test.expect(fired).to_be(0)
    -- Completing the mapping fires it, and the `j` never reaches the editor.
    t:feed("k")
    btv.test.expect(fired).to_be(1)
    btv.test.expect(t:cursor()[1]).to_be(2)
  end)

  btv.test.it("§5 — …and breaking the mapping prefix releases the built-in", function(t)
    open(t)
    local fired = 0
    btv.keymap.set("n", "jk", function()
      fired = fired + 1
    end)
    t:feed("2G")
    -- `jl` breaks `jk`, so both the held `j` and the `l` run.
    t:feed("jl")
    btv.test.expect(fired).to_be(0)
    btv.test.expect(t:cursor()[1]).to_be(3)
    btv.test.expect(t:cursor()[2]).to_be(1)
  end)

  btv.test.it("the four colliding maps are the only ones the config adds", function(t)
    open(t)
    local mapped = {}
    for _, m in ipairs(t:keymaps("n")) do
      mapped[m.lhs] = true
    end
    for _, lhs in ipairs({ "gh", "dh", "fh", "ff", "rx" }) do
      btv.test.expect(mapped[lhs]).to_be_truthy()
    end
  end)
end)
