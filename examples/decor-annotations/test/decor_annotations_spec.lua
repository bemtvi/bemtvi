-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/decor-annotations
--
-- All four payloads live in the decoration layers, not in the buffer and not in
-- the painted glyphs: the sign, the virtual text and the full-width row tint are
-- `t:decor()`, the span is `t:highlights()`. Rows are SCREEN rows, so a scrolled
-- viewport is read from the top of the window down.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  -- The provider runs off the viewport signal, off the frame.
  t:wait_for(function()
    return t:decor(4).sign ~= nil
  end, { message = "the provider never decorated the first error line" })
end

--- The AnnMarker spans on screen row `row`, as `{ first, last }` pairs.
local function markers(t, row)
  local out = {}
  for _, span in ipairs(t:highlights(row)) do
    if span[3] == "AnnMarker" then
      out[#out + 1] = { span[1], span[2] }
    end
  end
  return out
end

btv.test.describe("examples/decor-annotations", function()
  -- 1. "the sample's `!` / `?` / `>` lines are decorated the moment the file
  --     opens, with no keypress at all"
  btv.test.it("§1 — the decoration is there on the first paint", function(t)
    open(t)
    btv.test.expect(t:cursor()[1]).to_be(1)
    btv.test.expect(t:decor(4).sign).to_be("E>")
    btv.test.expect(t:decor(7).sign).to_be("W>")
  end)

  -- "Three payloads on ONE mark": sign + line background + end-of-line note.
  btv.test.it("§2 — a `!` line carries a sign, a note and a line background", function(t)
    open(t)
    local d = t:decor(4)
    btv.test.expect(d.sign).to_be("E>")
    btv.test.expect(d.virt_text).to_be("  ← needs attention")
    btv.test.expect(d.virt_pos).to_be("eol")
    -- The full-width tint rides its own layer, beside the row rather than over
    -- its cells — so it is `t:decor()`, not a highlight span.
    btv.test.expect(d.line_bg).to_be(true)
  end)

  -- "A sign-only mark is legal."
  btv.test.it("§2 — a `?` line carries a sign and nothing else", function(t)
    open(t)
    local d = t:decor(7)
    btv.test.expect(d.sign).to_be("W>")
    btv.test.expect(d.virt_text).to_be_nil()
    btv.test.expect(d.line_bg).to_be_nil()
  end)

  btv.test.it("§2 — a plain line carries no decoration at all", function(t)
    open(t)
    btv.test.expect(t:decor(12).sign).to_be_nil()
    btv.test.expect(t:decor(12).virt_text).to_be_nil()
    btv.test.expect(t:decor(12).line_bg).to_be_nil()
    btv.test.expect(#markers(t, 12)).to_be(0)
  end)

  -- "Every `>` in the line gets a highlight SPAN."
  btv.test.it("§2 — every > in a line is spanned, not just the first", function(t)
    open(t)
    -- "  drafted -> reviewed -> published" has two.
    local spans = markers(t, 5)
    btv.test.expect(#spans).to_be(2)
    for _, span in ipairs(spans) do
      btv.test.expect(span[2] - span[1]).to_be(1)
      btv.test.expect(t:screen()[5]:sub(span[1] + 1, span[2])).to_be(">")
    end
    -- …and a single one is spanned too.
    btv.test.expect(#markers(t, 13)).to_be(1)
  end)

  -- "the cost is bounded by the window height, never by the file size"
  btv.test.it("§2 — scrolling decorates the newly-revealed lines", function(t)
    open(t)
    t:feed("G")
    t:wait_for(function()
      -- Whatever is on screen now, some `>` marker or sign must have been drawn
      -- for it — the provider re-ran for the new range.
      for row = 1, #t:screen() do
        if t:decor(row).sign ~= nil or #markers(t, row) > 0 then
          return true
        end
      end
      return false
    end, { message = "the provider never re-ran for the scrolled viewport" })
    -- The decoration follows the screen: whatever row now shows a `!` line has
    -- the error sign.
    for row, text in ipairs(t:screen()) do
      if text:match("^%s*!") then
        btv.test.expect(t:decor(row).sign).to_be("E>")
      elseif text:match("^%s*%?") then
        btv.test.expect(t:decor(row).sign).to_be("W>")
      end
    end
  end)

  -- 3. ":AnnFlip -> the error tint changes colour immediately, without scrolling"
  btv.test.it("§3 — :AnnFlip re-dispatches the provider without a scroll", function(t)
    open(t)
    local before = btv.hl.get(0, { name = "AnnErrorLine" }).bg
    t:cmd("AnnFlip")
    local after = btv.hl.get(0, { name = "AnnErrorLine" }).bg
    btv.test.expect(after).never.to_be(before)
    -- The provider re-ran: the line is still tinted, now with the new colour.
    btv.test.expect(t:decor(4).line_bg).to_be(true)
    -- Flip back so the next test starts where this one found things.
    t:cmd("AnnFlip")
    btv.test.expect(btv.hl.get(0, { name = "AnnErrorLine" }).bg).to_be(before)
  end)

  -- "each republish replaces the provider's previous marks wholesale — so the
  --  provider never has to clear up after itself"
  btv.test.it("a republish replaces the previous marks rather than stacking", function(t)
    open(t)
    local before = #markers(t, 5)
    t:cmd("AnnFlip")
    t:cmd("AnnFlip")
    t:cmd("AnnFlip")
    t:cmd("AnnFlip")
    btv.test.expect(#markers(t, 5)).to_be(before)
    btv.test.expect(t:decor(4).virt_text).to_be("  ← needs attention")
  end)
end)
