-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/treesitter-start
--
-- The two nouns the tour is about are buffer options, so most of this is
-- declarative state. Whether the buffer actually LIGHTS UP additionally needs a
-- Rust parser on disk — the notes say so — and those cases check `t:highlights()`
-- only when one is installed, rather than failing on a missing grammar.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:exec(function()
    btv.bo.filetype = "rust"
    btv.bo.ts_highlight = true
  end)
  t:feed("gg")
end

--- Whether the engine really painted the buffer (needs the grammar on disk).
local function painted(t)
  for row = 1, 6 do
    if #t:highlights(row) > 0 then
      return true
    end
  end
  return false
end

btv.test.describe("examples/treesitter-start", function()
  -- "Set both and the buffer lights up … These two lines are the whole story."
  btv.test.it("the config sets the two buffer nouns", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(btv.bo.filetype).to_be("rust")
    btv.test.expect(btv.bo.ts_highlight).to_be(true)
  end)

  -- ":TSStop — turn highlighting off for this buffer … The filetype is kept."
  btv.test.it(":TSStop clears ts_highlight and keeps the filetype", function(t)
    open(t)
    t:cmd("TSStop")
    btv.test.expect(btv.bo.ts_highlight).to_be(false)
    btv.test.expect(btv.bo.filetype).to_be("rust")
    btv.test.expect(t:message()).to_contain("treesitter: stopped")
  end)

  -- ":TSStart — turn it back on (optionally pass a language; defaults to rust)."
  btv.test.it(":TSStart turns it back on, defaulting to rust", function(t)
    open(t)
    t:cmd("TSStop")
    t:cmd("TSStart")
    btv.test.expect(btv.bo.ts_highlight).to_be(true)
    btv.test.expect(btv.bo.filetype).to_be("rust")
    btv.test.expect(t:message()).to_contain("started in 'rust'")
  end)

  btv.test.it(":TSStart takes a language argument", function(t)
    open(t)
    t:cmd("TSStart lua")
    btv.test.expect(btv.bo.filetype).to_be("lua")
    btv.test.expect(btv.bo.ts_highlight).to_be(true)
    btv.test.expect(t:message()).to_contain("started in 'lua'")
  end)

  -- "`btv.bo.<opt>` targets the current buffer; `btv.bo[buf].<opt>` targets a
  --  specific one."
  btv.test.it("the nouns are per buffer", function(t)
    open(t)
    local sample = btv.buf.current()
    t:cmd("TSStop")
    t:cmd("enew")
    -- The fresh buffer carries neither the sample's filetype nor its stopped
    -- engine: `ts_highlight` is on out of the box (the "highlight floor"), and it
    -- is the FILETYPE a `.txt` is missing.
    btv.test.expect(btv.bo.filetype).to_be("")
    btv.test.expect(btv.bo.ts_highlight).to_be(true)
    btv.test.expect(btv.bo[sample].filetype).to_be("rust")
    btv.test.expect(btv.bo[sample].ts_highlight).to_be(false)
    -- …and a write through the indexed form lands on that buffer alone.
    t:exec(function()
      btv.bo[sample].ts_highlight = true
    end)
    btv.test.expect(btv.bo[sample].ts_highlight).to_be(true)
    btv.test.expect(btv.bo.filetype).to_be("")
  end)

  -- "The `.txt` buffer lights up with Rust highlighting on startup." Needs the
  -- grammar: without it the notes promise the buffer simply stays un-highlighted.
  btv.test.it("with a rust grammar installed the buffer is painted", function(t)
    open(t)
    t:sleep(120)
    if not painted(t) then
      print("skip: no rust treesitter grammar installed")
      return
    end
    -- …and stopping really darkens it.
    t:cmd("TSStop")
    t:sleep(120)
    btv.test.expect(painted(t)).to_be(false)
    t:cmd("TSStart")
    t:wait_for(function()
      return painted(t)
    end, { tries = 100, interval = 20, message = ":TSStart re-lit nothing" })
  end)

  -- "Without it the buffer simply stays un-highlighted (best-effort, no error)."
  btv.test.it("an unknown language is best-effort, not an error", function(t)
    open(t)
    t:cmd("TSStart nosuchlang")
    btv.test.expect(btv.bo.filetype).to_be("nosuchlang")
    btv.test.expect(t:message()).to_contain("started in 'nosuchlang'")
    btv.test.expect(t:message()).never.to_contain("E")
  end)
end)
