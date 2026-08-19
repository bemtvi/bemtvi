-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/laststatus
--
-- 'laststatus' decides WHERE the bar is drawn, so the assertions are on whether
-- there is a bar at all (`t:statusline()`) and on how many text rows the window
-- has left — mode 0 gives the freed bottom row back to the text.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The click handler and the <leader>N maps report through `vim.notify`.
local notified = {}
do
  local real = vim.notify
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  vim.o.laststatus = 3
  t:feed("<Esc>")
end

--- How many rows the focused window paints (text + fillers).
local function painted_rows(t)
  return #t:screen()
end

btv.test.describe("examples/laststatus", function()
  btv.test.it("the config starts in mode 3, with its own %-format bar", function(t)
    open(t)
    btv.test.expect(btv.o.laststatus).to_be(3)
    btv.test.expect(btv.o.statusline).to_be("%!v:lua.statusline()")
    local bar = t:statusline()
    btv.test.expect(bar).to_contain("NORMAL")
    btv.test.expect(bar).to_contain("sample.txt")
    btv.test.expect(bar).to_contain("ls=3")
  end)

  -- "3  a single GLOBAL status line at the bottom, shared by all windows"
  btv.test.it("mode 3 — one bar, whatever the split count", function(t)
    open(t)
    local one = painted_rows(t)
    t:feed("<C-w>s")
    btv.test.expect(t:statusline()).to_contain("ls=3")
    -- Two windows now share the rows, so each paints fewer.
    btv.test.expect(painted_rows(t) < one).to_be(true)
    t:cmd("only")
  end)

  -- "2  every window gets its own status line (vim's default)"
  btv.test.it("mode 2 — every window has its own bar", function(t)
    open(t)
    t:feed("<Space>2")
    btv.test.expect(btv.o.laststatus).to_be(2)
    btv.test.expect(last_notify()).to_contain("laststatus = 2")
    btv.test.expect(t:statusline()).to_contain("ls=2")
    t:feed("<C-w>s")
    btv.test.expect(t:statusline()).to_contain("ls=2")
    t:feed("<C-w>w")
    btv.test.expect(t:statusline()).to_contain("ls=2")
    t:cmd("only")
  end)

  -- "1  only when two or more windows are open"
  btv.test.it("mode 1 — no bar with one window, a bar with two", function(t)
    open(t)
    t:feed("<Space>1")
    btv.test.expect(btv.o.laststatus).to_be(1)
    local alone = painted_rows(t)
    btv.test.expect(t:statusline()).to_be("")
    t:feed("<C-w>s")
    btv.test.expect(t:statusline()).to_contain("ls=1")
    t:cmd("only")
    btv.test.expect(t:statusline()).to_be("")
    btv.test.expect(painted_rows(t)).to_be(alone)
  end)

  -- "0  never  (the freed bottom row becomes text)"
  btv.test.it("mode 0 — no bar, and the freed row becomes text", function(t)
    open(t)
    t:feed("<Space>2")
    local with_bar = painted_rows(t)
    t:feed("<Space>0")
    btv.test.expect(btv.o.laststatus).to_be(0)
    btv.test.expect(t:statusline()).to_be("")
    btv.test.expect(painted_rows(t)).to_be(with_bar + 1)
  end)

  btv.test.it("<leader>3 puts the global bar back", function(t)
    open(t)
    t:feed("<Space>0")
    btv.test.expect(t:statusline()).to_be("")
    t:feed("<Space>3")
    btv.test.expect(btv.o.laststatus).to_be(3)
    btv.test.expect(t:statusline()).to_contain("ls=3")
  end)

  -- "The mode block is a click region (`%@v:lua.fn@…%X`): clicking it cycles
  --  'laststatus'."
  btv.test.it("the click handler cycles the mode", function(t)
    open(t)
    btv.test.expect(btv.o.laststatus).to_be(3)
    _G.on_mode_click(0, 1, "l", "")
    t:feed("<Esc>")
    btv.test.expect(btv.o.laststatus).to_be(0)
    btv.test.expect(last_notify()).to_contain("laststatus = 0")
    _G.on_mode_click(0, 1, "l", "")
    t:feed("<Esc>")
    btv.test.expect(btv.o.laststatus).to_be(1)
  end)

  -- "Same engine the per-window AND the global (mode 3) bar run through."
  btv.test.it("the %-format bar tracks the mode and the cursor", function(t)
    open(t)
    t:feed("3G")
    -- `G` lands on the first non-blank, so the column is whatever that is.
    btv.test.expect(t:statusline()).to_contain("3:" .. t:cursor()[2] + 1)
    t:feed("i")
    btv.test.expect(t:statusline()).to_contain("INSERT")
    t:feed("<Esc>")
    btv.test.expect(t:statusline()).to_contain("NORMAL")
    t:feed("V")
    btv.test.expect(t:statusline()).to_contain("V-LINE")
    t:feed("<Esc>")
  end)

  btv.test.it("the bar marks a modified buffer", function(t)
    open(t)
    btv.test.expect(t:statusline()).never.to_contain("[+]")
    t:feed("x")
    btv.test.expect(t:statusline()).to_contain("[+]")
  end)
end)
