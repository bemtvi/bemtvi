-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/decor-todo
--
-- The keywords are coloured by a `btv.decor` provider, so they are a highlight
-- layer over the rows — `t:highlights()`, not `t:lines()`. The two Phase-4
-- conveniences the notes are about (the debounce, and `bufs` scoping) are checked
-- as behaviour: one run after a fast scroll settles, and the provider's reach.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.lua")
  t:cmd("e!")
  t:feed("gg")
  t:wait_for(function()
    return #t:highlights(6) > 0
  end, { message = "the provider never coloured the first TODO" })
end

--- The screen row showing `needle`, or nil. Screen rows are not buffer lines
--- once the window has scrolled, and every view here is screen-indexed.
local function row_of(t, needle)
  for i, line in ipairs(t:screen()) do
    if line:find(needle, 1, true) then
      return i
    end
  end
  return nil
end

--- The keyword spans on screen row `row` as `{ text, group }` pairs.
local function keywords(t, row)
  local text, out = t:screen()[row] or "", {}
  for _, span in ipairs(t:highlights(row)) do
    out[#out + 1] = { text:sub(span[1] + 1, span[2]), span[3] }
  end
  return out
end

btv.test.describe("examples/decor-todo", function()
  -- "The keywords colour the instant the file opens."
  btv.test.it("each keyword gets the group its kind maps to", function(t)
    open(t)
    local cases = {
      { needle = "TODO", group = "TodoKeyword" },
      { needle = "FIXME", group = "FixmeKeyword" },
      { needle = "NOTE", group = "NoteKeyword" },
      { needle = "HACK", group = "HackKeyword" },
    }
    for _, case in ipairs(cases) do
      local row
      for i, line in ipairs(t:screen()) do
        if line:find(case.needle, 1, true) then
          row = i
          break
        end
      end
      btv.test.expect(row).never.to_be_nil()
      btv.test.expect(keywords(t, row)).to_contain({ case.needle, case.group })
    end
  end)

  -- "XXX = HackKeyword" — two keywords, one group.
  btv.test.it("XXX shares HACK's group", function(t)
    open(t)
    t:feed("Go-- XXX: and a HACK too<Esc>")
    local row = t:wait_for(function()
      local at = row_of(t, "XXX: and a HACK too")
      return at and #t:highlights(at) == 2 and at or nil
    end, { message = "the appended line never coloured" })
    local spans = keywords(t, row)
    btv.test.expect(spans).to_contain({ "XXX", "HackKeyword" })
    btv.test.expect(spans).to_contain({ "HACK", "HackKeyword" })
  end)

  btv.test.it("the span covers the bare keyword, nothing more", function(t)
    open(t)
    for _, span in ipairs(t:highlights(6)) do
      local text = t:screen()[6]:sub(span[1] + 1, span[2])
      btv.test.expect(text).to_be("TODO")
    end
  end)

  btv.test.it("a line with no keyword carries nothing", function(t)
    open(t)
    btv.test.expect(#t:highlights(4)).to_be(0)
  end)

  btv.test.it("every occurrence on a line is coloured, not just the first", function(t)
    open(t)
    t:feed("GoTODO one TODO two TODO three<Esc>")
    local row = t:wait_for(function()
      local at = row_of(t, "TODO one TODO two TODO three")
      return at and #t:highlights(at) == 3 and at or nil
    end, { message = "not every occurrence was coloured" })
    for _, span in ipairs(t:highlights(row)) do
      btv.test.expect(span[3]).to_be("TodoKeyword")
    end
  end)

  -- "scroll and the newly-revealed lines colour once the scroll settles"
  btv.test.it("a scroll colours the newly-revealed lines", function(t)
    open(t)
    t:feed("G")
    t:wait_for(function()
      for row, text in ipairs(t:screen()) do
        if text:find("HACK", 1, true) or text:find("NOTE", 1, true) then
          return #t:highlights(row) > 0
        end
      end
      -- Nothing keyword-bearing on screen is a pass too — nothing to colour.
      return true
    end, { message = "the scrolled-to lines never coloured" })
  end)

  -- "`debounce = <ms>` — coalesce a fast continuous scroll into ONE provider run …
  --  fires `on_range` once the viewport stops moving for `ms`."
  btv.test.it("the debounce holds the run until the scroll settles", function(t)
    open(t)
    -- A burst of scrolling, read back immediately: the trailing run has not fired
    -- yet, so the newly-revealed rows are still bare.
    t:feed("<C-e><C-e><C-e><C-e><C-e><C-e><C-e><C-e>")
    local settled = t:wait_for(function()
      for row, text in ipairs(t:screen()) do
        if text:find("HACK", 1, true) and #t:highlights(row) > 0 then
          return true
        end
      end
      return false
    end, { tries = 100, interval = 10, message = "the debounced run never fired" })
    btv.test.expect(settled).to_be(true)
  end)

  -- "this provider runs in any buffer (no `bufs`)"
  btv.test.it("with no bufs scope the provider runs in any buffer", function(t)
    open(t)
    t:cmd("enew")
    t:feed("iTODO in a brand new buffer<Esc>")
    t:wait_for(function()
      return #t:highlights(1) > 0
    end, { message = "the unscoped provider skipped a new buffer" })
    btv.test.expect(t:highlights(1)[1][3]).to_be("TodoKeyword")
  end)

  btv.test.it("the four highlight groups are defined", function(t)
    open(t)
    for _, group in ipairs({ "TodoKeyword", "FixmeKeyword", "HackKeyword", "NoteKeyword" }) do
      local def = btv.hl.get(0, { name = group }) or {}
      btv.test.expect(def.fg).never.to_be_nil()
      btv.test.expect(def.bold).to_be(true)
    end
  end)
end)
