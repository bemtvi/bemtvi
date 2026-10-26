-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/modeless
--
-- Each numbered note in init.lua, typed as written. `t:feed` is the keyboard.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Open the sample fresh, at the start of line 1.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("<C-Home>")
end

btv.test.describe("examples/modeless", function()
  btv.test.it("1 — the file opens in Insert, and <Esc> keeps you typing", function(t)
    open(t)
    btv.test.expect(t:mode()).to_be("i")
    t:feed("Hello <Esc>!")
    btv.test.expect(t:line(1)).to_be("Hello !The quick brown fox jumps over the lazy dog.")
  end)

  btv.test.it("2 — <C-z> undoes a word at a time, <C-S-z> redoes", function(t)
    open(t)
    t:feed("<End> and more words")
    t:feed("<C-z>")
    btv.test.expect(t:line(1)).to_be("The quick brown fox jumps over the lazy dog. and more")
    t:feed("<C-z><C-z>")
    btv.test.expect(t:line(1)).to_be("The quick brown fox jumps over the lazy dog.")
    t:feed("<C-S-z>")
    btv.test.expect(t:line(1)).to_be("The quick brown fox jumps over the lazy dog. and")
  end)

  btv.test.it("3 — Shift selects and typing replaces", function(t)
    open(t)
    t:feed("<C-S-Right>A")
    btv.test.expect(t:line(1)).to_be("A quick brown fox jumps over the lazy dog.")
    t:feed("<C-z>")
    btv.test.expect(t:line(1)).to_be("The quick brown fox jumps over the lazy dog.")
    btv.test.expect(t:mode()).to_be("s")
    t:feed("A")
    btv.test.expect(t:line(1)).to_be("A quick brown fox jumps over the lazy dog.")
    t:feed("<S-End>.")
    btv.test.expect(t:line(1)).to_be("A.")
  end)

  -- The runner's clipboard is an in-memory one (`btv.test.clipboard`), so note 4
  -- runs as written.
  btv.test.it("4 — <C-c> copies the line, <C-v> pastes it above, <C-x> cuts", function(t)
    open(t)
    t:feed("<C-c>")
    btv.test
      .expect(btv.test.clipboard.peek())
      .to_be("The quick brown fox jumps over the lazy dog.\n")
    -- Down to `first` (line 4) and paste: the whole-line copy goes in above it.
    t:feed("<Down><Down><Down><C-v>")
    btv.test.expect(t:line(4)).to_be("The quick brown fox jumps over the lazy dog.")
    btv.test.expect(t:line(5)).to_be("first")
    -- The caret rode down with its line, so it is on `first`: select the word, cut it.
    t:feed("<Home><C-S-Right><C-x>")
    btv.test.expect(t:line(5)).to_be("")
    btv.test.expect(btv.test.clipboard.peek()).to_be("first")
  end)

  btv.test.it("5 — <C-f> finds and selects, <C-g> / <C-S-g> move through matches", function(t)
    open(t)
    t:feed("<C-f>fox<CR>")
    btv.test.expect(t:mode()).to_be("s")
    t:feed("FOX")
    btv.test.expect(t:line(1)).to_be("The quick brown FOX jumps over the lazy dog.")
    t:feed("<C-g>1")
    btv.test.expect(t:line(2)).to_be("1 is the word to find, and fox again here.")
    t:feed("<C-g>2")
    btv.test.expect(t:line(2)).to_be("1 is the word to find, and 2 again here.")
  end)

  btv.test.it("6 — lines move, duplicate, and indent as a block", function(t)
    open(t)
    t:feed("<C-End><Up>")
    btv.test.expect(t:line(5)).to_be("second")
    t:feed("<A-Up>")
    btv.test.expect(t:line(4)).to_be("second")
    btv.test.expect(t:line(5)).to_be("first")
    t:feed("<A-Down><C-S-d>")
    btv.test.expect(t:line(5)).to_be("second")
    btv.test.expect(t:line(6)).to_be("second")
    t:cmd("set shiftwidth=2 expandtab")
    t:feed("<C-End><Up><Up><Up><Home><S-Down><S-Down><S-End><Tab>")
    btv.test.expect(t:line(4)).to_be("  first")
    btv.test.expect(t:line(5)).to_be("  second")
    btv.test.expect(t:line(6)).to_be("  second")
    t:feed("<S-Tab>")
    btv.test.expect(t:line(4)).to_be("first")
  end)

  btv.test.it("7 — the palette runs commands and pickers, <C-;> opens the : line", function(t)
    open(t)
    t:feed("<C-S-p>")
    t:wait_for(function()
      return t:menu() ~= nil
    end, { message = "the palette never opened" })
    t:feed("split")
    t:wait_for(function()
      local m = t:menu()
      return m and #m.items > 0 and m.items[1]:find("^split")
    end, { message = "split is not the top row" })
    t:feed("<CR>")
    t:wait_for(function()
      return #btv.win.list() == 2
    end, { message = "split never ran" })
    t:feed("<C-S-p>")
    t:wait_for(function()
      return t:menu() ~= nil
    end, { message = "the palette never reopened" })
    t:feed("Live")
    t:wait_for(function()
      local m = t:menu()
      return m and #m.items > 0 and m.items[1]:find("^Live Grep")
    end, { message = "Live Grep is not the top row" })
    t:feed("<CR>")
    t:wait_for(function()
      local m = t:menu()
      return m and #m.items == 0
    end, { message = "Live Grep never opened" })
    t:feed("<Esc>")
    t:feed("<C-;>")
    btv.test.expect(t:mode()).to_be("c")
    t:feed("<Esc>")
    btv.test.expect(t:mode()).to_be("i")
  end)

  btv.test.it("8 — a map in the modeless layer: <C-e> selects to the line's end", function(t)
    open(t)
    t:feed("<C-Right><C-e>!")
    btv.test.expect(t:line(1)).to_be("The!")
  end)

  btv.test.it("9 — the wheel scrolls the view, and Shift with it scrolls sideways", function(t)
    t:feed("<C-;>e " .. DIR .. "/wide.txt<CR>")
    t:feed("<C-;>set nowrap<CR>")
    btv.test.expect(t:mode()).to_be("i")
    for _ = 1, 5 do
      t:mouse("wheel", "down", 5, 5)
    end
    btv.test.expect(btv.win.saveview().topline).to_be(16)
    btv.test.expect(t:cursor()).to_equal({ 1, 0 })
    for _ = 1, 3 do
      t:mouse("wheel", "down", 5, 5, "s")
    end
    btv.test.expect(btv.win.saveview().leftcol).to_be(18)
    btv.test.expect(btv.win.saveview().topline).to_be(16)
    t:feed("Z")
    btv.test.expect(t:line(1):sub(1, 4)).to_be("Z001")
    btv.test.expect(btv.win.saveview().topline).to_be(1)
  end)

  -- A screen cell on buffer line `line` (1-based), `col` cells into its text: the
  -- window's gutter comes first, and the menu opens at whatever cell was clicked.
  local function cell(line, col)
    return line - 1, btv.win.textoff(btv.win.current()) + col
  end

  -- The row of the open menu labelled `label`, as a screen cell to click: the box's
  -- border sits on the right-clicked cell, so row `i` is `i` rows below it.
  local function row_of(t, at_row, at_col, label)
    local m = t:menu()
    for i, item in ipairs(m.items) do
      if item == label then
        return at_row + i, at_col + 2
      end
    end
    error("no menu row " .. label)
  end

  btv.test.it("10 — right-click opens a menu at the pointer; its rows do the work", function(t)
    open(t)
    -- Double-click `quick` (text columns 4-8 of line 1) and right-click inside it.
    local r, c = cell(1, 6)
    t:mouse("left", "press", r, c)
    t:mouse("left", "release", r, c)
    t:mouse("left", "press", r, c)
    t:mouse("left", "release", r, c)
    btv.test.expect(t:mode()).to_be("s")
    t:mouse("right", "press", r, c)
    local m = t:menu()
    btv.test.expect(m.items[4]).to_be("Cut")
    btv.test.expect(m.kinds[4]).to_contain("X")
    btv.test.expect(m.items[#m.items]).to_be("Duplicate Line")
    -- One click on Cut.
    local cr, cc = row_of(t, r, c, "Cut")
    t:mouse("left", "press", cr, cc)
    t:mouse("left", "release", cr, cc)
    btv.test.expect(t:line(1)).to_be("The  brown fox jumps over the lazy dog.")
    btv.test.expect(t:menu()).to_be_nil()

    -- Right-click line 2: the caret goes there, and the added row duplicates it.
    r, c = cell(2, 3)
    t:mouse("right", "press", r, c)
    btv.test.expect(t:cursor()[1]).to_be(2)
    local dr, dc = row_of(t, r, c, "Duplicate Line")
    t:mouse("left", "press", dr, dc)
    t:mouse("left", "release", dr, dc)
    btv.test.expect(t:line(2)).to_be("fox is the word to find, and fox again here.")
    btv.test.expect(t:line(3)).to_be("fox is the word to find, and fox again here.")

    -- `<Esc>` dismisses it, and typing goes back to the text.
    t:mouse("right", "press", r, c)
    btv.test.expect(t:menu()).never.to_be_nil()
    t:feed("<Esc>")
    btv.test.expect(t:menu()).to_be_nil()
  end)
end)
