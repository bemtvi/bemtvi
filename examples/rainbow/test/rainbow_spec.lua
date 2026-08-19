-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/rainbow
--
-- The colours are a highlight layer over the buffer's own cells, so they are
-- `t:highlights()`. The provider's contract — one run per visible-range change,
-- off the frame, over the visible slice only — shows up as: the brackets colour
-- without a keypress, and newly-revealed lines colour as they come into view.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text, and wait
--- for the first paint.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.lua")
  t:cmd("e!")
  t:feed("gg")
  t:wait_for(function()
    for row = 1, #t:screen() do
      if #t:highlights(row) > 0 then
        return true
      end
    end
    return false
  end, { message = "the rainbow provider never painted" })
end

--- The rainbow spans on screen row `row`, as `{ char, group }` pairs.
local function brackets(t, row)
  local text, out = t:screen()[row] or "", {}
  for _, span in ipairs(t:highlights(row)) do
    if span[3]:find("^Rainbow") then
      out[#out + 1] = { text:sub(span[1] + 1, span[2]), span[3] }
    end
  end
  return out
end

btv.test.describe("examples/rainbow", function()
  btv.test.it("the six depth colours are defined", function(t)
    open(t)
    for i = 1, 6 do
      local def = btv.hl.get(0, { name = "Rainbow" .. i }) or {}
      btv.test.expect(def.fg).never.to_be_nil()
      btv.test.expect(def.bold).to_be(true)
    end
  end)

  -- "The brackets colour by nesting depth the instant the file opens."
  btv.test.it("what is painted is brackets, and only brackets", function(t)
    open(t)
    local painted = 0
    for row = 1, #t:screen() do
      for _, b in ipairs(brackets(t, row)) do
        -- Adjacent brackets of one depth merge into a single span, so a span is
        -- one or more bracket characters and nothing else.
        btv.test.expect(b[1]).to_match("^[%(%)%[%]{}]+$")
        painted = painted + 1
      end
    end
    btv.test.expect(painted > 0).to_be(true)
  end)

  btv.test.it("a line's every bracket is painted", function(t)
    open(t)
    -- A line the spec writes itself, so the count is exact and independent of what
    -- the sample happens to hold.
    t:feed("ggOlocal t = { a = (1), b = [ [2] ] }<Esc>")
    local want = 0
    for i = 1, #t:line(1) do
      if t:line(1):sub(i, i):match("[%(%)%[%]{}]") then
        want = want + 1
      end
    end
    btv.test.expect(want).to_be(8)
    t:wait_for(function()
      return #brackets(t, 1) == want
    end, { message = "not every bracket on the line was painted" })
  end)

  -- "colour by nesting DEPTH"
  btv.test.it("a bracket's colour follows its nesting depth", function(t)
    open(t)
    -- Find a row with a nested pair and check the inner pair differs from the outer.
    local row, spans
    for i = 1, #t:screen() do
      local b = brackets(t, i)
      if #b >= 4 then
        row, spans = i, b
        break
      end
    end
    btv.test.expect(row).never.to_be_nil()
    -- The first opener and the one nested inside it cannot share a colour.
    btv.test.expect(spans[1][2]).never.to_be(spans[2][2])
    -- …and a matching pair does share one.
    local depth, opener = 0, {}
    for _, s in ipairs(spans) do
      if s[1]:match("[%(%[{]") then
        depth = depth + 1
        opener[depth] = s[2]
      else
        btv.test.expect(s[2]).to_be(opener[depth])
        depth = depth - 1
      end
    end
  end)

  -- "scroll … and the newly-revealed lines colour as they come into view"
  btv.test.it("scrolling colours the newly-revealed lines", function(t)
    open(t)
    t:feed("G")
    t:wait_for(function()
      for row = 1, #t:screen() do
        if #brackets(t, row) > 0 then
          return true
        end
      end
      return false
    end, { message = "the scrolled-to lines never coloured" })
    -- …and what is painted there is brackets, as at the top.
    for row = 1, #t:screen() do
      for _, b in ipairs(brackets(t, row)) do
        btv.test.expect(b[1]).to_match("^[%(%)%[%]{}]+$")
      end
    end
  end)

  -- "bufs = { filetype = { 'lua', 'rust', 'json', 'javascript', 'c' } }"
  btv.test.it("the provider is scoped to the filetypes it declared", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("lua")
    -- A buffer of another filetype gets nothing, however many brackets it holds.
    t:cmd("enew")
    t:cmd("setlocal filetype=text")
    t:feed("i(a (b) c)<Esc>")
    t:sleep(80)
    btv.test.expect(#brackets(t, 1)).to_be(0)
  end)

  btv.test.it("…and a buffer of a declared filetype does get it", function(t)
    open(t)
    t:cmd("enew")
    t:cmd("setlocal filetype=json")
    t:feed("i{ \"a\": [1] }<Esc>")
    t:wait_for(function()
      return #brackets(t, 1) > 0
    end, { message = "a json buffer got no rainbow" })
    btv.test.expect(#brackets(t, 1)).to_be(4)
  end)

  -- "an edit reflow wakes it too"
  btv.test.it("an edit re-runs the provider", function(t)
    open(t)
    t:feed("ggOlocal x = 1<Esc>")
    local before = #brackets(t, 1)
    btv.test.expect(before).to_be(0)
    t:feed("A ((()))<Esc>")
    t:wait_for(function()
      local n = 0
      for _, b in ipairs(brackets(t, 1)) do
        n = n + #b[1]
      end
      return n == 6
    end, { message = "the edited line never coloured" })
    -- Three nesting levels: the outermost pair shares a colour, and differs from
    -- the pair inside it.
    local spans = brackets(t, 1)
    btv.test.expect(#spans > 0).to_be(true)
    btv.test.expect(spans[1][2]).never.to_be(spans[2][2])
  end)
end)
