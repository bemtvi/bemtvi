-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/decor-invalidate
--
-- The whole subject is a repaint that happens with NO viewport change and NO
-- keypress, so every assertion is on the highlight layer before and after — the
-- one place a `btv.decor` publish shows up.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The TooLong spans on screen row `row`, as `{ first, last }` pairs.
local function flagged(t, row)
  local out = {}
  for _, span in ipairs(t:highlights(row)) do
    if span[3] == "TooLong" then
      out[#out + 1] = { span[1], span[2] }
    end
  end
  return out
end

--- How many rows on screen carry a flag at all.
local function flagged_rows(t)
  local n = 0
  for row = 1, #t:screen() do
    if #flagged(t, row) > 0 then
      n = n + 1
    end
  end
  return n
end

--- Wait until the async limit has landed and the first repaint is on screen.
local function await_limit(t)
  t:wait_for(function()
    return flagged_rows(t) > 0
  end, { tries = 200, interval = 20, message = "the async limit never repainted" })
end

btv.test.describe("examples/decor-invalidate", function()
  -- 2. "open the sample and sit still … then light up on their own. No key was
  --     pressed."
  btv.test.it("§2 — the late-arriving limit repaints with no input at all", function(t)
    open(t)
    local before = t:cursor()
    await_limit(t)
    -- Nothing moved and nothing was typed: same cursor, same text.
    btv.test.expect(t:cursor()).to_equal(before)
    btv.test.expect(btv.bo.modified).to_be(false)
  end)

  btv.test.it("§1 — the flag runs from the limit column to the end of the line", function(t)
    open(t)
    await_limit(t)
    -- Line 5 is the first over-48 line in the sample.
    local spans = flagged(t, 5)
    btv.test.expect(#spans).to_be(1)
    btv.test.expect(spans[1][1]).to_be(48)
    btv.test.expect(spans[1][2]).to_be(#t:line(5))
  end)

  btv.test.it("§1 — a short line carries nothing", function(t)
    open(t)
    await_limit(t)
    btv.test.expect(#t:line(4) <= 48).to_be(true)
    btv.test.expect(#flagged(t, 4)).to_be(0)
  end)

  -- 3. ":Limit 20  → far more of each line is flagged, at once."
  btv.test.it("§3 — :Limit 20 flags more, immediately", function(t)
    open(t)
    await_limit(t)
    local before = flagged_rows(t)
    t:cmd("Limit 20")
    btv.test.expect(flagged_rows(t) > before).to_be(true)
    btv.test.expect(flagged(t, 5)[1][1]).to_be(20)
  end)

  -- ":Limit 70  → almost nothing is flagged."
  btv.test.it("§3 — :Limit 70 flags less", function(t)
    open(t)
    await_limit(t)
    t:cmd("Limit 20")
    local wide = flagged_rows(t)
    t:cmd("Limit 70")
    btv.test.expect(flagged_rows(t) < wide).to_be(true)
  end)

  btv.test.it("§3 — the repaint needs no scroll and no edit", function(t)
    open(t)
    await_limit(t)
    local before = t:cursor()
    t:cmd("Limit 20")
    btv.test.expect(t:cursor()).to_equal(before)
    btv.test.expect(btv.bo.modified).to_be(false)
    btv.test.expect(flagged(t, 5)[1][1]).to_be(20)
  end)

  btv.test.it("§3 — :Limit refuses a non-number, loudly", function(t)
    open(t)
    await_limit(t)
    local at = flagged(t, 5)[1][1]
    t:cmd("Limit banana")
    btv.test.expect(t:message()).to_contain("Limit: expects a number")
    -- …and the limit is unchanged.
    btv.test.expect(flagged(t, 5)[1][1]).to_be(at)
  end)

  -- "the scope wakes EVERY window showing it — try `:split` first and watch both
  --  halves repaint from the one call"
  btv.test.it("§3 — the buf scope repaints every window showing the buffer", function(t)
    open(t)
    await_limit(t)
    t:cmd("split")
    t:cmd("Limit 20")
    -- The focused half repainted…
    btv.test.expect(flagged(t, 5)[1][1]).to_be(20)
    -- …and so did the other one.
    t:feed("<C-w>j")
    btv.test.expect(flagged(t, 5)[1][1]).to_be(20)
    t:cmd("only")
  end)

  -- 4. "repeated asks for the same window coalesce … asking to be re-run from
  --     inside your own on_range cannot spin the editor"
  btv.test.it("§4 — repeated invalidates coalesce rather than piling up", function(t)
    open(t)
    await_limit(t)
    for _ = 1, 50 do
      btv.decor.invalidate({ buf = 0 })
    end
    -- The editor is still answering, and the marks are the same ones — a
    -- republish replaces, it does not stack.
    t:sleep(60)
    btv.test.expect(#flagged(t, 5)).to_be(1)
    t:feed("ix<Esc>u")
    btv.test.expect(t:mode()).to_be("n")
  end)
end)
