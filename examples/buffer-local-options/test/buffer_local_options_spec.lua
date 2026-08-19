-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/buffer-local-options
--
-- It sources `init.lua` as a session would and types the three numbered TRY-IT
-- blocks verbatim, including the "prove it is PER BUFFER" round trip.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open one of the example's files, re-read so each test starts from the same text.
local function open(t, name)
  t:cmd("e " .. DIR .. "/" .. name)
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/buffer-local-options", function()
  btv.test.it("the FileType autocmd sets the lua buffer's options", function(t)
    open(t, "two.lua")
    btv.test.expect(btv.bo.filetype).to_be("lua")
    btv.test.expect(btv.bo.expandtab).to_be(true)
    btv.test.expect(btv.bo.tabstop).to_be(2)
  end)

  -- "shiftwidth and softtabstop follow tabstop" — the sentinels are what make the
  -- single `tabstop` knob enough.
  btv.test.it("shiftwidth and softtabstop follow tabstop by sentinel", function(t)
    open(t, "two.lua")
    btv.test.expect(btv.bo.shiftwidth).to_be(0)
    btv.test.expect(btv.bo.softtabstop).to_be(-1)
    -- …and the effective width really is 2: `>>` indents by tabstop.
    t:feed("3G>>")
    btv.test.expect(t:line(3)).to_be("  return 1")
  end)

  -- 1. "TYPE: <Tab>x -> '  x' (two SPACES, not a tab)"
  btv.test.it("try-it 1 — <Tab> inserts two spaces in the lua buffer", function(t)
    open(t, "two.lua")
    t:feed("3GI<Tab>x<Esc>")
    btv.test.expect(t:line(3)).to_be("  xreturn 1")
    btv.test.expect(t:line(3)).never.to_contain("\t")
  end)

  -- 2. "Prove it is PER BUFFER."
  btv.test.it("try-it 2 — the plain-text buffer keeps the defaults", function(t)
    open(t, "notes.txt")
    -- No filetype, so the autocmd never touched it.
    btv.test.expect(btv.bo.filetype).to_be("")
    btv.test.expect(btv.bo.expandtab).to_be(false)
    btv.test.expect(btv.bo.tabstop).to_be(4)
    t:feed("Gi<Tab>x<Esc>")
    btv.test.expect(t:line(2)).to_contain("\t")
  end)

  btv.test.it("try-it 2 — switching back finds the lua buffer unchanged", function(t)
    open(t, "two.lua")
    open(t, "notes.txt")
    btv.test.expect(btv.bo.tabstop).to_be(4)
    t:cmd("b two.lua")
    btv.test.expect(btv.bo.expandtab).to_be(true)
    btv.test.expect(btv.bo.tabstop).to_be(2)
    t:feed("3GI<Tab>x<Esc>")
    btv.test.expect(t:line(3)).to_be("  xreturn 1")
  end)

  -- 3. ":setlocal expandtab tabstop=8 softtabstop=4"
  btv.test.it("try-it 3 — softtabstop, not tabstop, decides what <Tab> inserts", function(t)
    open(t, "two.lua")
    t:cmd("setlocal expandtab tabstop=8 softtabstop=4")
    t:feed("3GI<Tab>y<Esc>")
    btv.test.expect(t:line(3)).to_be("    yreturn 1")
  end)

  btv.test.it("try-it 3 — <BS> removes the whole soft tab", function(t)
    open(t, "two.lua")
    t:cmd("setlocal expandtab tabstop=8 softtabstop=4")
    t:feed("3GI<Tab><BS>z<Esc>")
    btv.test.expect(t:line(3)).to_be("zreturn 1")
  end)

  btv.test.it("try-it 3 — :set tabstop? echoes the buffer's value", function(t)
    open(t, "two.lua")
    t:cmd("setlocal expandtab tabstop=8 softtabstop=4")
    t:cmd("set tabstop?")
    btv.test.expect(t:message()).to_contain("tabstop=8")
  end)

  btv.test.it("try-it 3 — noexpandtab alone still spells the soft tab in spaces", function(t)
    open(t, "two.lua")
    t:cmd("setlocal expandtab tabstop=8 softtabstop=4")
    t:cmd("set noexpandtab")
    btv.test.expect(btv.bo.expandtab).to_be(false)
    -- A 4-cell soft tab cannot BE a tab when a tab is 8 cells wide, so it is
    -- still four spaces. This is why the note tells you to clear `softtabstop`.
    t:feed("3GI<Tab>q<Esc>")
    btv.test.expect(t:line(3)).to_be("    qreturn 1")
  end)

  btv.test.it("try-it 3 — clearing softtabstop too brings literal tabs back", function(t)
    open(t, "two.lua")
    t:cmd("setlocal expandtab tabstop=8 softtabstop=4")
    t:cmd("set noexpandtab softtabstop=0")
    t:feed("3GI<Tab>w<Esc>")
    btv.test.expect(t:line(3)).to_be("\twreturn 1")
  end)
end)
