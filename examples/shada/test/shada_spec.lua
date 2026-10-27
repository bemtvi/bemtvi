-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/shada
--
-- The tour's payoff is a *second* session, which one test run cannot stage. What
-- it can drive is everything the second session depends on: the state `:SeedShada`
-- stores, the explicit `:wshada` / `:rshada` the two maps run, and the report
-- `:ShadaShow` gives — the halves that, between them, are the whole feature. (The
-- cross-session restore itself is covered natively, in the server's shada suite.)

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Open the sample at the top, with the slots `:SeedShada` writes cleared so each
--- case sees its own effect. (The per-test baseline restores the named registers;
--- the global marks and the search register it does not.)
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:exec(function()
    vim.fn.setreg("a", "")
  end)
  t:feed("gg")
end

btv.test.describe("examples/shada", function()
  -- ":SeedShada — stash a register, set global mark A, push a search"
  btv.test.it(":SeedShada fills the three slots it names", function(t)
    open(t)
    t:feed("9G")
    t:cmd("SeedShada")
    btv.test.expect(vim.fn.getreg("a")).to_be("hello from the previous session")
    btv.test.expect(vim.fn.getreg("/")).to_be("needle")
    btv.test.expect(t:message()).to_contain("seeded:")
    -- The global mark landed on the line it ran from: jumping to it comes back.
    t:feed("gg")
    t:feed("`A")
    btv.test.expect(t:cursor()[1]).to_be(9)
  end)

  -- '"ap — paste register "a from last session'
  btv.test.it('the seeded "a pastes with "ap', function(t)
    open(t)
    t:cmd("SeedShada")
    t:feed("gg$")
    t:feed('"ap')
    btv.test.expect(t:line(1)).to_contain("hello from the previous session")
  end)

  -- "n — repeat last session's search pattern"
  btv.test.it("the armed pattern is the one n repeats", function(t)
    open(t)
    t:cmd("SeedShada")
    t:feed("gg")
    -- No `/` was ever typed: `setreg("/")` armed the pattern, `n` repeats it.
    t:feed("n")
    btv.test.expect(t:line(t:cursor()[1])).to_contain("needle")
  end)

  -- ":ShadaShow — show whether register \"a survived from a prior session"
  btv.test.it(':ShadaShow reports both states of "a', function(t)
    open(t)
    t:cmd("ShadaShow")
    btv.test.expect(t:message()).to_contain('"a is empty')
    t:cmd("SeedShada")
    t:cmd("ShadaShow")
    btv.test.expect(t:message()).to_contain('"a (restored): hello from the previous session')
  end)

  -- "<space>w — :wshada, the explicit flush."
  btv.test.it("<space>w runs :wshada and disturbs nothing", function(t)
    open(t)
    t:cmd("SeedShada")
    t:cmd("echo ''")
    t:feed("<Space>w")
    -- The test runner keeps no shada store, so the flush has nowhere to go and says
    -- so, naming the command: what this holds is that the key runs `:wshada` and
    -- leaves the state as it was. (In your editor, with a store, it's quiet.)
    btv.test.expect(t:message()).to_contain(":wshada")
    btv.test.expect(vim.fn.getreg("a")).to_be("hello from the previous session")
  end)

  -- "<space>r — :rshada, the explicit re-read … plain :rshada keeps your live
  --  value and only fills empty slots."
  btv.test.it("<space>r runs :rshada without overwriting a live register", function(t)
    open(t)
    t:cmd("SeedShada")
    t:exec(function()
      vim.fn.setreg("a", "set in THIS session")
    end)
    t:cmd("echo ''")
    t:feed("<Space>r")
    btv.test.expect(vim.fn.getreg("a")).to_be("set in THIS session")
    -- (No store in the test runner: the re-read says so, naming the command.)
    btv.test.expect(t:message()).to_contain(":rshada")
  end)

  btv.test.it("the two maps are the ex-commands the notes name", function(t)
    open(t)
    local rhs = {}
    for _, m in ipairs(t:keymaps("n")) do
      rhs[m.lhs] = m.rhs
    end
    btv.test.expect(rhs["<space>w"]).to_be(":wshada<CR>")
    btv.test.expect(rhs["<space>r"]).to_be(":rshada<CR>")
  end)
end)
