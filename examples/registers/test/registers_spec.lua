-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/registers
--
-- The registers the config seeds at load time are part of the per-test baseline
-- (the snapshot is taken after the spec files are sourced), so every case starts
-- from the same `"h` / `"t` a reader gets on launch.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample list at the top, off disk.
local function open(t)
  t:cmd("e " .. DIR .. "/shopping.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/registers", function()
  -- "Seed two registers at startup, before you touch the keyboard."
  btv.test.it('the config seeds "h charwise and "t linewise', function(t)
    open(t)
    btv.test.expect(vim.fn.getreg("h")).to_be("hello from setreg")
    btv.test.expect(vim.fn.getregtype("h")).to_be("v")
    btv.test.expect(vim.fn.getreg("t")).to_be("- buy milk\n- water plants\n")
    btv.test.expect(vim.fn.getregtype("t")).to_be("V")
  end)

  -- '"hp — paste the seeded charwise greeting after the cursor'
  btv.test.it('"hp pastes the greeting inline', function(t)
    open(t)
    -- Charwise, so it lands *after* the cursor character rather than on its own
    -- line — the whole point of the `"h` / `"t` pair.
    t:feed('gg"hp')
    btv.test.expect(t:line(1)).to_be("Shello from setreghopping & chores")
    btv.test.expect(#t:lines()).to_be(#btv.buf.lines(0, 0, -1))
  end)

  -- ":put t — drop the seeded two-line todo block below this line"
  btv.test.it(":put t drops the seeded block below the cursor", function(t)
    open(t)
    t:feed("4G") -- "eggs"
    t:cmd("put t")
    btv.test.expect(t:line(4)).to_be("eggs")
    btv.test.expect(t:line(5)).to_be("- buy milk")
    btv.test.expect(t:line(6)).to_be("- water plants")
    btv.test.expect(t:line(7)).to_be("bread")
  end)

  -- "<space>p — same, via the mapped key"
  btv.test.it("<space>p is the same put, from a keymap", function(t)
    open(t)
    t:feed("4G")
    t:feed("<Space>p")
    btv.test.expect(t:line(5)).to_be("- buy milk")
    btv.test.expect(t:line(6)).to_be("- water plants")
  end)

  -- ":Stash — copy the current line into register \"s"
  btv.test.it(':Stash copies the cursor line into "s, linewise', function(t)
    open(t)
    t:feed("5G") -- "bread"
    t:cmd("Stash")
    btv.test.expect(vim.fn.getreg("s")).to_be("bread\n")
    btv.test.expect(vim.fn.getregtype("s")).to_be("V")
    btv.test.expect(t:message()).to_contain('stashed "bread" into "s [V]')
  end)

  -- ":Stashed — paste \"s back below the cursor"
  btv.test.it(":Stashed puts it back below the cursor", function(t)
    open(t)
    t:feed("5G")
    t:cmd("Stash")
    t:feed("gg")
    t:cmd("Stashed")
    btv.test.expect(t:line(2)).to_be("bread")
  end)

  btv.test.it(':Stashed says so while "s is empty', function(t)
    open(t)
    local before = #t:lines()
    t:cmd("Stashed")
    btv.test.expect(t:message()).to_contain('"s is empty')
    btv.test.expect(#t:lines()).to_be(before)
  end)

  -- ":Shout — upper-case + append register \"h, then \"hp to see it"
  btv.test.it(':Shout appends the upper-cased text to "h', function(t)
    open(t)
    t:cmd("Shout")
    btv.test.expect(vim.fn.getreg("h")).to_be("hello from setregHELLO FROM SETREG")
    btv.test.expect(t:message()).to_contain('"h is now: hello from setregHELLO FROM SETREG')
    -- …and the paste the notes promise shows the whole thing.
    t:feed("gg$")
    t:feed('"hp')
    btv.test.expect(t:line(1)).to_contain("hello from setregHELLO FROM SETREG")
  end)

  -- ":registers — list every populated register"
  btv.test.it(":registers lists the seeded ones", function(t)
    open(t)
    t:cmd("registers")
    local listing = table.concat(t:lines(), "\n")
    btv.test.expect(listing).to_contain('"h')
    btv.test.expect(listing).to_contain("hello from setreg")
    btv.test.expect(listing).to_contain('"t')
    btv.test.expect(listing).to_contain("buy milk")
  end)

  -- "<space>y / <space>P mirror common vim muscle memory" — the maps exist and
  -- carry the clipboard prefix (the provider itself is the harness's seam).
  btv.test.it("<space>y and <space>P are the clipboard maps", function(t)
    open(t)
    local rhs = {}
    for _, m in ipairs(t:keymaps("n")) do
      rhs[m.lhs] = m.rhs
    end
    btv.test.expect(rhs["<space>y"]).to_be('"+y')
    btv.test.expect(rhs["<space>P"]).to_be('"+p')
  end)

  btv.test.it("<space>y yanks the line to the clipboard register", function(t)
    open(t)
    t:feed("4G")
    t:feed("<Space>yy")
    btv.test.expect(btv.test.clipboard.peek()).to_contain("eggs")
  end)
end)
