-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/mouse-widgets
--
-- All four overlays are the one float-list widget, so what is offered and which
-- row leads is `t:menu()`, and the gestures go through `t:mouse` — the same screen
-- cell a client forwards, hit-tested server-side.
--
-- Most gestures need no row arithmetic: a WHEEL anywhere over the box moves the
-- highlight, and an off-box click is off-box wherever the box is. Clicking a
-- specific ROW does need to know where that row was painted, and the box's rect is
-- not it (a picker paints a prompt and a border inside its rect) — so that one case
-- scans for the row instead of assuming, re-opening the widget after each miss
-- because a miss cancels a picker.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The picker and the chooser report through `btv.notify`; `vim.notify` is a
-- separate binding captured when the prelude loaded, so both are wrapped.
local notified = {}
do
  local real_btv, real_vim = btv.notify, vim.notify
  btv.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_btv(msg, ...)
  end
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_vim(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

--- Open the sample, back in normal mode with no overlay up.
local function open(t)
  for _ = 1, 6 do
    if t:menu() == nil and t:mode() == "n" then
      break
    end
    t:feed("<Esc>")
    t:sleep(40)
  end
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Wait for the widget to be up WITH content: a source may stream its items a tick
--- after the box appears.
local function menu(t)
  return t:wait_for(function()
    local m = t:menu()
    return m and #m.items > 0 and m or nil
  end, { message = "no overlay opened" })
end

--- A screen cell that lands ON one of the widget's rows, found by probing.
---
--- The box reports where it was placed, but in its own coordinate space — a
--- cursor-anchored menu in window cells, an editor-level picker in windows-area
--- cells — and a mouse event names a GLOBAL screen cell, so the two cannot simply
--- be equated. Probing asks the widget where it really is. Each miss re-opens it,
--- because a miss cancels a picker.
---
--- Returns `row, col, selected` — the row the click highlighted.
local hit = {}

--- One click that cannot be read as a multi-click: `'mousetime'` is dropped to
--- 1ms around it, so a spec's back-to-back clicks stay separate gestures without
--- sleeping out the real 400ms window each time.
local function single_click(t, row, col)
  t:cmd("set mousetime=1")
  t:mouse("left", "press", row, col)
  t:mouse("left", "release", row, col)
  t:cmd("set mousetime=400")
end

local function probe_cell(t, widget, open_widget)
  local cached = hit[widget]
  if cached then
    open_widget(t)
    menu(t)
    single_click(t, cached[1], cached[2])
    local m = t:menu()
    if m and m.selected and m.selected >= 2 then
      return cached[1], cached[2], m.selected
    end
  end
  for row = 0, 22 do
    for _, col in ipairs({ 2, 6, 8, 12, 20 }) do
      open(t)
      open_widget(t)
      menu(t)
      single_click(t, row, col)
      local m = t:menu()
      -- `selected >= 2` on purpose: the chrome above the list (a picker's prompt
      -- and border) clamps onto the first row, so a click there reports 1 without
      -- being on a row at all — and clicking it again does nothing.
      if m and m.selected and m.selected >= 2 then
        hit[widget] = { row, col }
        return row, col, m.selected
      end
    end
  end
  error("no screen cell hit a row of the " .. widget, 0)
end

btv.test.describe("examples/mouse-widgets", function()
  btv.test.it("the config turns the pointer on in every mode", function(t)
    open(t)
    t:cmd("set mouse?")
    -- "the default `nvi` leaves cmdline mouse off, so the wildmenu wouldn't react"
    btv.test.expect(t:message()).to_contain("mouse=a")
  end)

  -- 1. The insert-mode completion popup.
  btv.test.it("§1 — the popup floats under the caret as you type", function(t)
    open(t)
    t:feed("Go")
    t:feed("co", { insert = true })
    local m = menu(t)
    btv.test.expect(#m.items > 0).to_be(true)
    t:feed("<Esc>")
  end)

  -- "wheel over the box  scroll the highlight, one row per notch (non-wrapping)"
  btv.test.it("§1 — the wheel scrolls the popup's highlight, one row per notch", function(t)
    open(t)
    local row, col, first = probe_cell(t, "complete", function(tt)
      tt:feed("Go")
      tt:feed("co", { insert = true })
    end)
    t:mouse("wheel", "down", row, col)
    btv.test.expect(t:menu().selected).to_be(first + 1)
    t:mouse("wheel", "up", row, col)
    btv.test.expect(t:menu().selected).to_be(first)
    t:feed("<Esc>")
  end)

  -- "The popup does NOT grab the mouse: a click off it falls through to the text."
  btv.test.it("§1 — a click off the completion popup falls through", function(t)
    open(t)
    t:feed("Go")
    t:feed("co", { insert = true })
    menu(t)
    t:mouse("left", "press", 0, 0)
    t:mouse("left", "release", 0, 0)
    t:sleep(40)
    btv.test.expect(t:menu()).to_be_nil()
    t:feed("<Esc>")
  end)

  -- 2. The command-line wildmenu — which only reacts because `mouse` includes `c`.
  btv.test.it("§2 — the wildmenu takes the wheel in command-line mode", function(t)
    open(t)
    t:feed(":ene<Tab>")
    local m = menu(t)
    btv.test.expect(m.items[1]).to_be("enew")
    btv.test.expect(m.selected).to_be_nil()
    t:feed("<Esc><Esc>")
    local row, col, first = probe_cell(t, "wildmenu", function(tt)
      tt:feed(":ene<Tab>")
    end)
    -- The wildmenu paints BOTTOM-UP, so its highlight walks the other way: a
    -- notch up is what moves toward the rows above.
    t:mouse("wheel", "up", row, col)
    btv.test.expect(t:menu().selected).to_be(first - 1)
    t:mouse("wheel", "down", row, col)
    btv.test.expect(t:menu().selected).to_be(first)
    t:feed("<Esc><Esc>")
  end)

  -- 3. The fuzzy picker — a centered box that GRABS the mouse modally.
  btv.test.it("§3 — the picker opens over its source's items", function(t)
    open(t)
    t:feed("<Bslash>o")
    local m = menu(t)
    btv.test.expect(m.items).to_contain("apple")
    btv.test.expect(m.items).to_contain("date")
    t:feed("<Esc>")
  end)

  btv.test.it("§3 — the wheel scrolls the picker's highlight", function(t)
    open(t)
    local row, col, first = probe_cell(t, "picker", function(tt)
      tt:feed("<Bslash>o")
    end)
    t:mouse("wheel", "down", row, col)
    btv.test.expect(t:menu().selected).to_be(first + 1)
    t:mouse("wheel", "up", row, col)
    btv.test.expect(t:menu().selected).to_be(first)
    t:feed("<Esc>")
  end)

  -- "click a row  highlight it / click it again  confirm it (runs the source's
  --  `confirm`)"
  btv.test.it("§3 — a click highlights a row, a second confirms it", function(t)
    open(t)
    local row, col, selected = probe_cell(t, "picker", function(tt)
      tt:feed("<Bslash>o")
    end)
    -- Highlighted, and nothing confirmed by that click.
    btv.test.expect(t:menu().selected).to_be(selected)
    local want = t:menu().items[selected]
    local before = #notified
    -- The same row again confirms it. A real second click, a full `'mousetime'`
    -- later, so it is an ordinary click rather than a double.
    t:sleep(450)
    t:mouse("left", "press", row, col)
    t:mouse("left", "release", row, col)
    t:wait_for(function()
      return #notified > before
    end, { message = "the picker never confirmed" })
    btv.test.expect(last_notify()).to_be("picked " .. want)
  end)

  -- The chooser's own half of the same gesture.
  btv.test.it("§4 — a click highlights a chooser row, a second resolves it", function(t)
    open(t)
    local row, col, selected = probe_cell(t, "select", function(tt)
      tt:feed("<Bslash>s")
    end)
    local want = t:menu().items[selected]
    local before = #notified
    t:sleep(450)
    t:mouse("left", "press", row, col)
    t:mouse("left", "release", row, col)
    t:wait_for(function()
      return #notified > before
    end, { message = "the chooser never resolved" })
    btv.test.expect(last_notify()).to_be("heading " .. want)
  end)

  -- "click OFF the box  cancel the picker (telescope-style)"
  btv.test.it("§3 — a click off the picker cancels it", function(t)
    open(t)
    t:feed("<Bslash>o")
    menu(t)
    t:mouse("left", "press", 0, 0)
    t:mouse("left", "release", 0, 0)
    t:wait_for(function()
      return t:menu() == nil
    end, { message = "the picker did not cancel" })
  end)

  -- 4. The promptless chooser, which also grabs the mouse.
  btv.test.it("§4 — the chooser opens over its list", function(t)
    open(t)
    t:feed("<Bslash>s")
    local m = menu(t)
    btv.test.expect(m.items).to_equal({ "north", "south", "east", "west" })
    t:feed("<Esc>")
  end)

  btv.test.it("§4 — the wheel scrolls the chooser's highlight", function(t)
    open(t)
    local row, col, first = probe_cell(t, "select", function(tt)
      tt:feed("<Bslash>s")
    end)
    t:mouse("wheel", "down", row, col)
    btv.test.expect(t:menu().selected).to_be(first + 1)
    t:feed("<Esc>")
  end)

  -- A click off the chooser dismisses it, resolving with nothing — the same as
  -- `<Esc>`. (The picker's outside-click is a cancel too; the difference the notes
  -- draw is that a completion popup lets the click through to the text instead.)
  btv.test.it("§4 — a click off the chooser dismisses it", function(t)
    open(t)
    t:feed("<Bslash>s")
    menu(t)
    local before = #notified
    t:mouse("left", "press", 0, 0)
    t:mouse("left", "release", 0, 0)
    t:wait_for(function()
      return #notified > before
    end, { message = "the chooser never resolved" })
    btv.test.expect(last_notify()).to_contain("no heading")
  end)

  -- "<Esc> cancels it, resolving with nil" — the promise the notes describe.
  btv.test.it("§4 — cancelling resolves the promise with nothing", function(t)
    open(t)
    t:feed("<Bslash>s")
    menu(t)
    t:feed("<Esc>")
    t:wait_for(function()
      return last_notify():find("no heading", 1, true) ~= nil
    end, { message = "the chooser never resolved on cancel" })
  end)
end)
