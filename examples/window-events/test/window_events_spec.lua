-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/window-events
--
-- Each section of the notes names the exact event sequence a command fires, and
-- each case below types that command and asserts that sequence — read back from
-- the config's own `:Events` log, which prints and clears.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- One window on the sample, with the log cleared.
local function open(t)
  t:cmd("only")
  while #vim.api.nvim_list_tabpages() > 1 do
    t:cmd("tabclose")
  end
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  t:cmd("Events")
end

--- Run `:Events` and return what it printed (and cleared).
local function events(t)
  t:cmd("Events")
  return t:message()
end

btv.test.describe("examples/window-events", function()
  btv.test.it(":Events says so when nothing has fired", function(t)
    open(t)
    btv.test.expect(events(t)).to_be("no events")
  end)

  -- "1. BufWinEnter is PER WINDOW, not per buffer."
  -- "See-that: WinNew BufEnter WinEnter BufWinEnter WinResized … And note what is
  --  NOT there: BufReadPost."
  btv.test.it("1 — a split onto the SAME file fires BufWinEnter again", function(t)
    open(t)
    t:cmd("vsplit " .. DIR .. "/sample.txt")
    local fired = events(t)
    btv.test.expect(fired).to_contain("WinNew")
    btv.test.expect(fired).to_contain("WinEnter")
    btv.test.expect(fired).to_contain("BufWinEnter(sample.txt)")
    btv.test.expect(fired).to_contain("WinResized")
    -- The buffer was already here, so nothing was read.
    btv.test.expect(fired).never.to_contain("BufReadPost")
    t:cmd("only")
  end)

  -- "2. A bare :split displays NOTHING new. See-that: WinNew WinEnter WinResized
  --  — and no BufWinEnter at all."
  btv.test.it("2 — a bare :split fires no BufWinEnter", function(t)
    open(t)
    t:cmd("split")
    local fired = events(t)
    btv.test.expect(fired).to_contain("WinNew")
    btv.test.expect(fired).to_contain("WinEnter")
    btv.test.expect(fired).to_contain("WinResized")
    btv.test.expect(fired).never.to_contain("BufWinEnter")
    t:cmd("only")
  end)

  -- "Then contrast: :split other.txt — … then the arrival of a file that really
  --  was read: BufLeave(sample.txt) BufReadPost(other.txt) BufEnter(other.txt)
  --  BufWinEnter(other.txt)."
  btv.test.it("2 — :split FILE is a split and then a load", function(t)
    open(t)
    t:cmd("split " .. DIR .. "/other.txt")
    local fired = events(t)
    btv.test.expect(fired).to_contain("WinNew")
    btv.test.expect(fired).to_contain("BufLeave(sample.txt)")
    btv.test.expect(fired).to_contain("BufReadPost(other.txt)")
    btv.test.expect(fired).to_contain("BufEnter(other.txt)")
    btv.test.expect(fired).to_contain("BufWinEnter(other.txt)")
    -- …in that order: the leave precedes the read, which precedes the display.
    btv.test.expect(fired:find("BufLeave") < fired:find("BufReadPost")).to_be(true)
    btv.test.expect(fired:find("BufReadPost") < fired:find("BufWinEnter")).to_be(true)
    t:cmd("only")
  end)

  -- "3. Navigation fires nothing about displays. See-that: WinEnter and TabEnter,
  --  and nothing else — no WinNew, no WinClosed, no WinResized, no BufWinEnter."
  btv.test.it("3 — switching tabs displays nothing", function(t)
    open(t)
    t:cmd("tabnew " .. DIR .. "/other.txt")
    t:cmd("Events") -- the new tab's own events; clear them
    t:cmd("tabnext")
    local fired = events(t)
    btv.test.expect(fired).to_contain("WinEnter")
    btv.test.expect(fired).to_contain("TabEnter")
    btv.test.expect(fired).never.to_contain("WinNew")
    btv.test.expect(fired).never.to_contain("WinClosed")
    btv.test.expect(fired).never.to_contain("WinResized")
    btv.test.expect(fired).never.to_contain("BufWinEnter")
    t:cmd("tabonly")
  end)

  btv.test.it("3 — <C-w>w between windows displays nothing either", function(t)
    open(t)
    t:cmd("split")
    t:cmd("Events")
    t:feed("<C-w>w")
    local fired = events(t)
    btv.test.expect(fired).to_contain("WinEnter")
    btv.test.expect(fired).never.to_contain("BufWinEnter")
    btv.test.expect(fired).never.to_contain("WinNew")
    btv.test.expect(fired).never.to_contain("WinResized")
    t:cmd("only")
  end)

  -- "4. A reload re-runs the whole enter sequence. See-that: BufReadPost
  --  BufEnter BufWinEnter, and no BufLeave — nothing was left."
  btv.test.it("4 — :e! re-runs the enter sequence", function(t)
    open(t)
    t:cmd("e!")
    local fired = events(t)
    btv.test.expect(fired).to_contain("BufReadPost(sample.txt)")
    btv.test.expect(fired).to_contain("BufEnter(sample.txt)")
    btv.test.expect(fired).to_contain("BufWinEnter(sample.txt)")
    btv.test.expect(fired).never.to_contain("BufLeave")
  end)

  -- "5. … each of the three windows shows its column rule in a different place."
  btv.test.it("5 — each window that displays the file gets its own rule", function(t)
    open(t)
    local seen = {}
    seen[#seen + 1] = btv.wo.colorcolumn
    t:cmd("vsplit " .. DIR .. "/sample.txt")
    seen[#seen + 1] = btv.wo.colorcolumn
    t:cmd("vsplit " .. DIR .. "/sample.txt")
    seen[#seen + 1] = btv.wo.colorcolumn
    -- Three windows, three different columns — the handler ran once per WINDOW.
    btv.test.expect(seen[1]).never.to_be(seen[2])
    btv.test.expect(seen[2]).never.to_be(seen[3])
    for _, col in ipairs(seen) do
      btv.test.expect(col == "20" or col == "40" or col == "60").to_be(true)
    end
    -- And the rule is really painted where the option says.
    btv.test.expect(t:rulers()[1]).to_be(tonumber(btv.wo.colorcolumn))
    t:cmd("only")
  end)

  -- "the handler runs with the window that displayed as the current one"
  btv.test.it("5 — the sibling window keeps its own rule", function(t)
    open(t)
    local first = vim.api.nvim_get_current_win()
    local first_col = btv.wo.colorcolumn
    t:cmd("vsplit " .. DIR .. "/sample.txt")
    local second_col = btv.wo.colorcolumn
    btv.test.expect(vim.wo[first].colorcolumn).to_be(first_col)
    btv.test.expect(second_col).never.to_be(first_col)
    t:cmd("only")
  end)
end)
