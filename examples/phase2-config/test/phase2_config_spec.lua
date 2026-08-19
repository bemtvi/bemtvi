-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/phase2-config
--
-- Every numbered section is typed exactly as its TYPE line says, and asserted on
-- what its SEE line promises — the message a mapping printed, or the edit a
-- literal key made.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The mappings announce themselves with `print`; record it at the source.
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

btv.test.describe("examples/phase2-config", function()
  -- 1. "<Space>h -> message 'hello from <leader> ...'"
  btv.test.it("§1 — <leader> resolves to the mapleader set before it", function(t)
    open(t)
    btv.test.expect(vim.g.mapleader).to_be(" ")
    t:feed("<Space>h")
    btv.test.expect(last_print()).to_contain("hello from <leader>")
  end)

  -- 2. "Y yanks to end-of-line (it's mapped to y$); p pastes it back."
  btv.test.it("§2 — a noremap string RHS runs the built-in it names", function(t)
    open(t)
    t:feed("1G0")
    local line = t:line(1)
    t:feed("wY")
    btv.test.expect(vim.fn.getreg('"')).to_be(line:sub(t:cursor()[2] + 1))
    t:feed("p")
    btv.test.expect(t:line(1)).never.to_be(line)
  end)

  -- 3. "Q -> SEE message 'X ACTION fired' / W -> a character deleted, NO message"
  btv.test.it("§3 — remap runs the target's ACTION", function(t)
    open(t)
    local before = t:line(1)
    t:feed("Q")
    btv.test.expect(last_print()).to_be("X ACTION fired")
    btv.test.expect(t:line(1)).to_be(before)
  end)

  btv.test.it("§3 — noremap feeds a LITERAL key to the editor", function(t)
    open(t)
    -- `X` deletes the character BEFORE the cursor, so start past column 0.
    t:feed("1G0ll")
    local before = t:line(1)
    printed[#printed + 1] = "marker"
    t:feed("W")
    -- A literal `X` reached core…
    btv.test.expect(t:line(1)).to_be(before:sub(1, 1) .. before:sub(3))
    -- …and the action never ran.
    btv.test.expect(last_print()).to_be("marker")
  end)

  -- 4. "<Space>a -> 'reached c via a -> b -> c' (two remap hops, then the fn)"
  btv.test.it("§4 — a remap chain resolves through every hop", function(t)
    open(t)
    t:feed("<Space>a")
    btv.test.expect(last_print()).to_be("reached c via a -> b -> c")
  end)

  -- 5. "Z -> the editor stays responsive; one literal Z reaches core."
  btv.test.it("§5 — a self-referential remap terminates instead of hanging", function(t)
    open(t)
    t:feed("1G0")
    t:feed("Z")
    -- Still answering, and the editor did not hang.
    btv.test.expect(t:mode()).to_be("n")
    t:feed("ix<Esc>")
    btv.test.expect(t:line(1):sub(1, 1)).to_be("x")
  end)

  -- 6. "<Space>p fires in normal AND visual"
  btv.test.it("§6 — a mode-list map fires in every listed mode", function(t)
    open(t)
    t:feed("<Space>p")
    btv.test.expect(last_print()).to_be("fires in normal AND visual")
    printed[#printed + 1] = "marker"
    t:feed("v<Space>p")
    btv.test.expect(last_print()).to_be("fires in normal AND visual")
    t:feed("<Esc>")
  end)

  -- 7. "vL -> in Visual, L is mapped to $, extending the selection to EOL.
  --     (In Normal, L is unmapped and keeps its normal meaning.)"
  btv.test.it("§7 — an x-mode map applies in Visual only", function(t)
    open(t)
    t:feed("1G0")
    t:feed("vL")
    btv.test.expect(t:cursor()[2]).to_be(#t:line(1) - 1)
    t:feed("<Esc>")
    -- In Normal, `L` is the built-in (the lowest line on screen), not `$`.
    t:feed("1G0")
    t:feed("L")
    -- `L` is the built-in here (the lowest line on screen), not `$`: the column
    -- did not run to end-of-line.
    btv.test.expect(t:cursor()[2]).to_be(0)
  end)

  -- 8. "gh -> SEE 'gh mapping' / gg -> cursor jumps to the first line"
  btv.test.it("§8 — an unmapped prefix replays to the built-in", function(t)
    open(t)
    t:feed("gh")
    btv.test.expect(last_print()).to_be("gh mapping")
    t:feed("G")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    t:feed("gg")
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)
end)
