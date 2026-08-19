-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/gutter-signs
--
-- A gutter sign and a line fill are both drawn beside the text rather than in it,
-- so they are `t:decor()` / the fill's own painted row — never `t:lines()`.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample and (re)place the demo signs on it — the config paints once,
--- against whatever buffer was current when it ran.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  t:cmd("GutterSigns")
end

--- The screen row showing the line whose text contains `needle`.
local function row_of(t, needle)
  for i, line in ipairs(t:screen()) do
    if line:find(needle, 1, true) then
      return i
    end
  end
  return nil
end

btv.test.describe("examples/gutter-signs", function()
  btv.test.it("the config reserves the gutter so it cannot jump", function(t)
    open(t)
    btv.test.expect(btv.wo.signcolumn).to_be("yes")
    btv.test.expect(t:gutter().sign_width).to_be(2)
  end)

  -- "a `┃` bar on added lines, `~` on a changed line, a `_` under a deletion"
  btv.test.it("each planned line carries its own sign", function(t)
    open(t)
    btv.test.expect(t:decor(1).sign).to_be("┃")
    btv.test.expect(t:decor(2).sign).to_be("┃")
    btv.test.expect(t:decor(5).sign).to_be("~")
    btv.test.expect(t:decor(8).sign).to_be("_")
  end)

  btv.test.it("an unplanned line carries none", function(t)
    open(t)
    btv.test.expect(t:decor(3).sign).to_be_nil()
    btv.test.expect(t:decor(4).sign).to_be_nil()
  end)

  -- "a `─` rule filling the blank separator line"
  btv.test.it("the blank separator row is filled with a rule", function(t)
    open(t)
    -- The first blank line in the sample is line 5.
    local blank
    for i, line in ipairs(t:lines()) do
      if line == "" then
        blank = i
        break
      end
    end
    btv.test.expect(blank).never.to_be_nil()
    -- The fill rides the virtual-text layer as a full-width OVERLAY placement, so
    -- it is `t:decor()` — the painted row is the (empty) buffer line underneath.
    local d = t:decor(blank)
    btv.test.expect(d.virt_pos).to_be("overlay")
    btv.test.expect(d.virt_text:sub(1, #"─")).to_be("─")
    btv.test.expect(#d.virt_text > 10).to_be(true)
  end)

  btv.test.it("only the FIRST blank line is filled", function(t)
    open(t)
    local filled = 0
    for row = 1, #t:screen() do
      local d = t:decor(row)
      if d.virt_text and d.virt_text:sub(1, #"─") == "─" then
        filled = filled + 1
      end
    end
    btv.test.expect(filled).to_be(1)
  end)

  -- "The signs anchor to their lines, so they track edits — press `o` to open a
  --  line and watch the ones below slide down."
  btv.test.it("the signs track an edit above them", function(t)
    open(t)
    t:feed("1GO<Esc>")
    -- Everything moved down one row.
    btv.test.expect(t:decor(1).sign).to_be_nil()
    btv.test.expect(t:decor(2).sign).to_be("┃")
    btv.test.expect(t:decor(3).sign).to_be("┃")
    btv.test.expect(t:decor(6).sign).to_be("~")
    btv.test.expect(t:decor(9).sign).to_be("_")
  end)

  -- ":GutterClear"
  btv.test.it(":GutterClear wipes the signs and the fill in one call", function(t)
    open(t)
    t:cmd("GutterClear")
    for row = 1, #t:screen() do
      btv.test.expect(t:decor(row).sign).to_be_nil()
    end
    for row = 1, #t:screen() do
      btv.test.expect(t:decor(row).virt_text).to_be_nil()
    end
    -- …and the gutter stays reserved, so the layout does not jump.
    btv.test.expect(t:gutter().sign_width).to_be(2)
  end)

  -- ":GutterSigns (re)place"
  btv.test.it(":GutterSigns puts them back, without stacking", function(t)
    open(t)
    t:cmd("GutterClear")
    t:cmd("GutterSigns")
    btv.test.expect(t:decor(1).sign).to_be("┃")
    t:cmd("GutterSigns")
    t:cmd("GutterSigns")
    btv.test.expect(t:decor(1).sign).to_be("┃")
    btv.test.expect(t:decor(5).sign).to_be("~")
  end)

  -- ":SignClash — a higher-priority sign wins the shared column"
  btv.test.it(":SignClash — priority decides which sign the column shows", function(t)
    open(t)
    btv.test.expect(t:decor(1).sign).to_be("┃")
    t:cmd("SignClash")
    btv.test.expect(t:decor(1).sign).to_be("★")
    -- Only the contested line changed.
    btv.test.expect(t:decor(2).sign).to_be("┃")
    -- …and clearing the namespace takes both.
    t:cmd("GutterClear")
    btv.test.expect(t:decor(1).sign).to_be_nil()
  end)

  btv.test.it("the four gutter highlight groups are defined", function(t)
    open(t)
    for _, group in ipairs({ "GutterAdd", "GutterChange", "GutterDelete", "FillRule" }) do
      local def = btv.hl.get(0, { name = group }) or {}
      btv.test.expect(def.fg).never.to_be_nil()
    end
  end)
end)
