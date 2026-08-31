-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/mouse
--
-- Every gesture is a hit-test the SERVER does, so the spec sends raw screen cells
-- (`btv._input_mouse`, the same call the TUI makes) and asserts on where the
-- cursor, the selection or the viewport ended up.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Break the multi-click chain: `'mousetime'` is 400ms here, and a spec's clicks
--- land far quicker than a human's, so an earlier click would otherwise count
--- toward the next gesture's run.
local function settle_clicks(t)
  t:sleep(450)
end

--- One mouse event at a global screen cell.
local function mouse(t, button, action, modifier, row, col)
  t:mouse(button, action, row, col, modifier)
end

--- One wheel notch. The wheel's direction is the ACTION, not the button.
local function wheel(t, direction, row, col)
  t:mouse("wheel", direction, row, col)
end

--- Where a buffer position is on the GLOBAL screen. `t:screen()` is the window's
--- own rows and `t:gutter()` its reserved width, but a mouse event names a cell on
--- the whole screen — the tabline sits above, and the gutter's own cells are to the
--- left — so both offsets are calibrated once by clicking and reading back where
--- the cursor landed. (Clicking the tabline is inert, which is why the probe starts
--- from a cursor that is deliberately NOT on line 1.)
local row_offset, col_offset

local function calibrate(t)
  if row_offset then
    return
  end
  for row = 0, 8 do
    t:feed("5G0")
    t:mouse("left", "press", row, 0)
    t:mouse("left", "release", row, 0)
    if t:cursor()[1] == 1 then
      row_offset = row
      break
    end
  end
  btv.test.expect(row_offset).never.to_be_nil()
  for col = 0, 16 do
    t:mouse("left", "press", row_offset, col)
    t:mouse("left", "release", row_offset, col)
    if t:cursor()[2] == 1 then
      col_offset = col - 1
      break
    end
  end
  btv.test.expect(col_offset).never.to_be_nil()
end

--- The global screen row showing buffer line `line`.
local function text_row(t, line)
  calibrate(t)
  return (line - t:view().topline) + row_offset
end

--- The global screen column of 0-based buffer column `col`.
local function text_col(t, col)
  calibrate(t)
  return col + col_offset
end

btv.test.describe("examples/mouse", function()
  btv.test.it("the config sets the four options it names", function(t)
    open(t)
    -- Queried through `:set x?`, the surface the config itself writes through.
    local function query(name)
      t:cmd("set " .. name .. "?")
      return t:message()
    end
    btv.test.expect(query("mousescroll")).to_contain("mousescroll=ver:5,hor:6")
    btv.test.expect(query("mousetime")).to_contain("mousetime=400")
    btv.test.expect(btv.o.showtabline).to_be(2)
    -- vim's defaults for the two it leaves alone.
    btv.test.expect(query("mouse")).to_contain("mouse=nvi")
    btv.test.expect(query("mousemodel")).to_contain("mousemodel=popup_setpos")
  end)

  -- ":MouseReport — echo the four mouse options"
  btv.test.it(":MouseReport reports the options", function(t)
    open(t)
    t:cmd("MouseReport")
    -- Each `:set x?` echoes in turn, so the line holds the last of them.
    btv.test.expect(t:message()).to_contain("mousetime=400")
  end)

  -- "click  place the cursor on the clicked character"
  btv.test.it("try-it — a click places the cursor on that character", function(t)
    open(t)
    local row = text_row(t, 3)
    btv.test.expect(row).never.to_be_nil()
    mouse(t, "left", "press", "", row, t:gutter().total + 4)
    mouse(t, "left", "release", "", row, t:gutter().total + 4)
    btv.test.expect(t:cursor()[1]).to_be(3)
    btv.test.expect(t:cursor()[2]).to_be(4)
  end)

  -- "The number gutter is click-through to column 0."
  btv.test.it("try-it — the gutter is click-through to column 0", function(t)
    open(t)
    local row = text_row(t, 3)
    mouse(t, "left", "press", "", row, 0)
    mouse(t, "left", "release", "", row, 0)
    btv.test.expect(t:cursor()[1]).to_be(3)
    btv.test.expect(t:cursor()[2]).to_be(0)
  end)

  -- "click + drag  charwise Visual selection … let go and the selection stays"
  btv.test.it("try-it — click and drag makes a charwise selection that stays", function(t)
    open(t)
    local row = text_row(t, 3)
    mouse(t, "left", "press", "", row, text_col(t, 2))
    mouse(t, "left", "drag", "", row, text_col(t, 8))
    btv.test.expect(t:mode()).to_be("v")
    mouse(t, "left", "release", "", row, text_col(t, 8))
    btv.test.expect(t:mode()).to_be("v")
    -- `y` yanks it, as the notes say.
    t:feed("y")
    btv.test.expect(vim.fn.getreg('"')).to_be(t:line(3):sub(3, 9))
  end)

  -- "double-click  select the word under the pointer (the `iw` run)"
  btv.test.it("try-it — a double-click selects the word", function(t)
    open(t)
    local row = text_row(t, 3)
    settle_clicks(t)
    mouse(t, "left", "press", "", row, text_col(t, 2))
    mouse(t, "left", "release", "", row, text_col(t, 2))
    mouse(t, "left", "press", "", row, text_col(t, 2))
    mouse(t, "left", "release", "", row, text_col(t, 2))
    btv.test.expect(t:mode()).to_be("v")
    t:feed("y")
    btv.test.expect(vim.fn.getreg('"')).to_match("^%w+$")
  end)

  -- "triple-click  select the whole line (linewise Visual)"
  btv.test.it("try-it — a triple-click selects the line", function(t)
    open(t)
    local row = text_row(t, 3)
    settle_clicks(t)
    for _ = 1, 3 do
      mouse(t, "left", "press", "", row, text_col(t, 2))
      mouse(t, "left", "release", "", row, text_col(t, 2))
    end
    btv.test.expect(t:mode()).to_be("V")
    t:feed("y")
    btv.test.expect(vim.fn.getreg('"')).to_be(t:line(3) .. "\n")
  end)

  -- "Shift+click  extend the current selection to the click, keeping the anchor"
  btv.test.it("try-it — Shift+click extends the selection", function(t)
    open(t)
    local row = text_row(t, 3)
    mouse(t, "left", "press", "", row, text_col(t, 2))
    mouse(t, "left", "release", "", row, text_col(t, 2))
    t:feed("v")
    local anchor = t:cursor()[2]
    settle_clicks(t)
    mouse(t, "left", "press", "s", row, text_col(t, 10))
    mouse(t, "left", "release", "s", row, text_col(t, 10))
    btv.test.expect(t:mode()).to_be("v")
    -- The anchor was KEPT: the selection runs from where the first click put it
    -- to where the shift-click did.
    local head = t:cursor()[2]
    btv.test.expect(head > anchor).to_be(true)
    t:feed("y")
    btv.test.expect(vim.fn.getreg('"')).to_be(t:line(3):sub(anchor + 1, head + 1))
  end)

  -- "wheel up/down  scroll the window UNDER THE POINTER by 'mousescroll' lines
  --  WITHOUT moving focus or (while it stays visible) the cursor"
  btv.test.it("try-it — the wheel scrolls by the configured step", function(t)
    open(t)
    -- Park the cursor deep enough that it stays visible across a five-line notch:
    -- the notes' "without moving the cursor" holds only while it does.
    t:feed("12G")
    local before_top = t:view().topline
    local before_cursor = t:cursor()
    local row = text_row(t, before_cursor[1])
    wheel(t, "down", row, 10)
    btv.test.expect(t:view().topline).to_be(before_top + 5)
    btv.test.expect(t:cursor()).to_equal(before_cursor)
    wheel(t, "up", row, 10)
    btv.test.expect(t:view().topline).to_be(before_top)
  end)

  btv.test.it("try-it — the wheel scrolls a split you are not focused in", function(t)
    open(t)
    t:cmd("split")
    -- Focus is in the top split; scroll the BOTTOM one by pointing at it.
    local focused = vim.api.nvim_get_current_win()
    local rows = #t:screen()
    wheel(t, "down", rows + 3, 10)
    btv.test.expect(vim.api.nvim_get_current_win()).to_be(focused)
    t:cmd("only")
  end)

  -- "insert + click  click while in Insert mode moves the caret without leaving
  --  Insert ('mouse' includes `i` by default)"
  btv.test.it("try-it — a click in insert mode moves the caret, staying in insert", function(t)
    open(t)
    local row = text_row(t, 3)
    t:feed("i")
    mouse(t, "left", "press", "", row, text_col(t, 5))
    mouse(t, "left", "release", "", row, text_col(t, 5))
    btv.test.expect(t:mode()).to_be("i")
    btv.test.expect(t:cursor()[1]).to_be(3)
    t:feed("<Esc>")
  end)

  -- "click another split focuses it (focus-follows-click)"
  btv.test.it("try-it — clicking another split focuses it", function(t)
    open(t)
    t:cmd("split")
    local top = vim.api.nvim_get_current_win()
    local rows = #t:screen()
    mouse(t, "left", "press", "", rows + 3, 10)
    mouse(t, "left", "release", "", rows + 3, 10)
    btv.test.expect(vim.api.nvim_get_current_win()).never.to_be(top)
    t:cmd("only")
  end)

  -- "'mousemodel' … with `:set mousemodel=extend`, right-click EXTENDS the
  --  selection to the click"
  btv.test.it("try-it — mousemodel=extend makes right-click extend", function(t)
    open(t)
    local row = text_row(t, 3)
    t:cmd("set mousemodel=extend")
    mouse(t, "left", "press", "", row, text_col(t, 2))
    mouse(t, "left", "release", "", row, text_col(t, 2))
    t:feed("v")
    local anchor = t:cursor()[2]
    mouse(t, "right", "press", "", row, text_col(t, 10))
    mouse(t, "right", "release", "", row, text_col(t, 10))
    btv.test.expect(t:mode()).to_be("v")
    local head = t:cursor()[2]
    btv.test.expect(head > anchor).to_be(true)
    t:feed("y")
    btv.test.expect(vim.fn.getreg('"')).to_be(t:line(3):sub(anchor + 1, head + 1))
    t:cmd("set mousemodel=popup_setpos")
  end)

  -- "middle-click  paste the `\"*` clipboard register at the click"
  btv.test.it("try-it — middle-click pastes the clipboard at the click", function(t)
    open(t)
    btv.test.clipboard.seed("PASTED", false)
    local row = text_row(t, 3)
    mouse(t, "middle", "press", "", row, text_col(t, 2))
    mouse(t, "middle", "release", "", row, text_col(t, 2))
    btv.test.expect(t:line(3)).to_contain("PASTED")
  end)
end)
