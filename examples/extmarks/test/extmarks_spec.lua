-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/extmarks
--
-- A mark is a highlight over the buffer's own cells, so what it paints is
-- `t:highlights()`. The claim the demo is really about — that a mark SHIFTS with
-- edits rather than staying at a byte offset — is checked by editing around one
-- and watching the span move.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The commands report through `vim.notify`; record them at the source.
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

--- Open the sample and paint the startup marks on it.
---
--- The config paints them against whatever buffer is current when it is sourced —
--- which, in the session the notes describe (`bemtvi -- sample.txt`), is the
--- sample. A headless runner has no file argument, so the spec re-sources the
--- config once the sample IS current, which is the same thing a user re-sourcing
--- their init does. `:ExtClear` first, so a re-source cannot stack marks.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  t:cmd("ExtClear")
  dofile(DIR .. "/init.lua")
  t:feed("gg")
end

--- The span of the first `group` highlight on screen row `row`, or nil.
local function span_of(t, row, group)
  for _, span in ipairs(t:highlights(row)) do
    if span[3] == group then
      return { span[1], span[2] }
    end
  end
  return nil
end

--- The text a span covers on its row.
local function span_text(t, row, span)
  return t:screen()[row]:sub(span[1] + 1, span[2])
end

btv.test.describe("examples/extmarks", function()
  btv.test.it("the startup marks paint the moment the file opens", function(t)
    open(t)
    -- Line 2: the two keywords in blue.
    btv.test.expect(span_text(t, 2, span_of(t, 2, "ExtNote"))).to_be("extmark")
    -- Line 3 / 4: the leading tags.
    btv.test.expect(span_text(t, 3, span_of(t, 3, "ExtTodo"))).to_be("TODO:")
    btv.test.expect(span_text(t, 4, span_of(t, 4, "ExtWarn"))).to_be("NOTE:")
  end)

  btv.test.it("one line can carry marks of two different groups", function(t)
    open(t)
    -- Line 4 has both the `NOTE:` tag and the `namespace` keyword.
    btv.test.expect(span_text(t, 4, span_of(t, 4, "ExtWarn"))).to_be("NOTE:")
    btv.test.expect(span_text(t, 4, span_of(t, 4, "ExtNote"))).to_be("namespace")
  end)

  -- "Edit any line (e.g. type at its start) and the highlighted ranges slide to
  --  stay on the same text — that's the anchor-shifting."
  btv.test.it("a mark slides with an edit before it", function(t)
    open(t)
    local before = span_of(t, 3, "ExtTodo")
    t:feed("3GI>> <Esc>")
    local after = span_of(t, 3, "ExtTodo")
    btv.test.expect(after[1]).to_be(before[1] + 3)
    -- It is still on the same text, which is the whole point.
    btv.test.expect(span_text(t, 3, after)).to_be("TODO:")
  end)

  btv.test.it("a mark on a later line slides with a line inserted above", function(t)
    open(t)
    local before = span_of(t, 4, "ExtWarn")
    t:feed("ggOa new first line<Esc>")
    -- The row moved down by one; the span within the row did not move.
    btv.test.expect(span_of(t, 4, "ExtWarn")).to_be_nil()
    local after = span_of(t, 5, "ExtWarn")
    btv.test.expect(after).to_equal(before)
    btv.test.expect(span_text(t, 5, after)).to_be("NOTE:")
  end)

  -- ":ExtMark — highlight the word under the cursor in ExtTodo."
  btv.test.it(":ExtMark marks the word under the cursor", function(t)
    open(t)
    t:feed("5G0w")
    local word = t:current_line():match("^%s*%S+%s+(%S+)")
    t:cmd("ExtMark")
    btv.test.expect(last_notify()).to_contain("marked")
    local span = span_of(t, 5, "ExtTodo")
    btv.test.expect(span).never.to_be_nil()
    btv.test.expect(span_text(t, 5, span)).to_be(word)
  end)

  btv.test.it(":ExtMark on no word says so rather than marking nothing", function(t)
    open(t)
    t:feed("Go   <Esc>0")
    t:cmd("ExtMark")
    btv.test.expect(last_notify()).to_contain("no word under the cursor")
  end)

  -- ":ExtList — count the marks currently in our namespace."
  btv.test.it(":ExtList counts the namespace's marks", function(t)
    open(t)
    t:cmd("ExtList")
    local before = tonumber(last_notify():match("(%d+) extmark"))
    btv.test.expect(before).never.to_be_nil()
    t:feed("5G0w")
    t:cmd("ExtMark")
    t:cmd("ExtList")
    btv.test.expect(tonumber(last_notify():match("(%d+) extmark"))).to_be(before + 1)
  end)

  -- ":ExtClear / <leader>x — clear every mark in the namespace at once."
  btv.test.it(":ExtClear wipes the whole namespace", function(t)
    open(t)
    btv.test.expect(span_of(t, 3, "ExtTodo")).never.to_be_nil()
    t:cmd("ExtClear")
    btv.test.expect(last_notify()).to_contain("cleared")
    btv.test.expect(span_of(t, 2, "ExtNote")).to_be_nil()
    btv.test.expect(span_of(t, 3, "ExtTodo")).to_be_nil()
    btv.test.expect(span_of(t, 4, "ExtWarn")).to_be_nil()
    t:cmd("ExtList")
    btv.test.expect(last_notify()).to_contain("0 extmark")
  end)

  btv.test.it("<leader>x is the same clear", function(t)
    open(t)
    t:feed("<Bslash>x")
    btv.test.expect(last_notify()).to_contain("cleared")
    btv.test.expect(span_of(t, 3, "ExtTodo")).to_be_nil()
  end)

  btv.test.it("the three highlight groups the marks reference are defined", function(t)
    open(t)
    for _, group in ipairs({ "ExtNote", "ExtTodo", "ExtWarn" }) do
      local def = btv.hl.get(0, { name = group }) or {}
      btv.test.expect(def.fg or def.bg).never.to_be_nil()
    end
  end)
end)
