-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/substitute-preview
--
-- The preview is a *paint*, not an edit: while the command line is open the buffer
-- is untouched and the removed/added sides are ephemeral highlight spans. So each
-- case types the command WITHOUT submitting it, then reads `t:highlights()` for
-- the struck-through side and `t:decor()` for the inline addition — `t:lines()`
-- would show nothing at all.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Every highlight group painted over `row`, deduplicated.
local function groups(t, row)
  local seen = {}
  for _, span in ipairs(t:highlights(row)) do
    seen[span[3]] = true
  end
  return seen
end

btv.test.describe("examples/substitute-preview", function()
  -- "it is on whenever `'incsearch'` is (the default)"
  btv.test.it("the preview rides 'incsearch', which is on", function(t)
    open(t)
    btv.test.expect(btv.o.incsearch).to_be(true)
  end)

  -- ":%s/teh/the/g — every 'teh' struck red, 'the' shown green after it"
  btv.test.it("a half-typed :s paints the removal and the addition", function(t)
    open(t)
    -- Typed, NOT submitted.
    t:feed(":%s/teh/the/g")
    -- The removal is a struck-through span in the config's own group…
    btv.test.expect(groups(t, 1)["BtvSubstituteDelete"]).to_be(true)
    -- …and the addition is spliced in beside it as virtual text, so the buffer
    -- itself still reads "teh".
    btv.test.expect(t:decor(1).virt_text).to_contain("the")
    btv.test.expect(t:line(1)).to_contain("teh")
    t:feed("<Esc>")
  end)

  -- "Press <Esc> to abandon (the preview vanishes, the buffer is untouched)"
  btv.test.it("<Esc> takes the preview with it and leaves the buffer alone", function(t)
    open(t)
    local before = table.concat(t:lines(), "\n")
    t:feed(":%s/teh/the/g")
    btv.test.expect(groups(t, 1)["BtvSubstituteDelete"]).to_be(true)
    t:feed("<Esc>")
    btv.test.expect(groups(t, 1)["BtvSubstituteDelete"]).to_be(nil)
    btv.test.expect(t:decor(1).virt_text).to_be(nil)
    btv.test.expect(table.concat(t:lines(), "\n")).to_be(before)
  end)

  btv.test.it("<CR> applies exactly what was previewed", function(t)
    open(t)
    t:feed(":%s/teh/the/g<CR>")
    btv.test.expect(t:line(1)).to_contain("the preview")
    btv.test.expect(t:line(1)).never.to_contain("teh")
    t:cmd("undo")
  end)

  -- ":2,4s/color/colour — confined to lines 2-4; first match per line (no /g)"
  btv.test.it("a range confines the preview to its lines", function(t)
    open(t)
    t:feed(":3,4s/color/colour")
    btv.test.expect(groups(t, 3)["BtvSubstituteDelete"]).to_be(true)
    btv.test.expect(groups(t, 4)["BtvSubstituteDelete"]).to_be(true)
    -- Line 5 has "color" too, and is outside the range.
    btv.test.expect(t:line(5)).to_contain("color")
    btv.test.expect(groups(t, 5)["BtvSubstituteDelete"]).to_be(nil)
    t:feed("<Esc>")
  end)

  btv.test.it("without /g only the first match on a line is previewed", function(t)
    open(t)
    -- Line 3 carries four "color"s; without the flag one is struck, with it all.
    local function struck(row)
      local n = 0
      for _, span in ipairs(t:highlights(row)) do
        if span[3] == "BtvSubstituteDelete" then
          n = n + 1
        end
      end
      return n
    end
    t:feed(":3s/color/colour")
    local one = struck(3)
    btv.test.expect(one).to_be(1)
    t:feed("<Esc>")
    t:feed(":3s/color/colour/g")
    btv.test.expect(struck(3) > one).to_be(true)
    t:feed("<Esc>")
  end)

  -- ":%s/foo// — an empty replacement previews a pure deletion"
  btv.test.it("an empty replacement previews a pure deletion", function(t)
    open(t)
    t:feed(":%s/foo//")
    local painted = groups(t, 7)
    btv.test.expect(painted["BtvSubstituteDelete"]).to_be(true)
    btv.test.expect(painted["BtvSubstituteAdd"]).to_be(nil)
    t:feed("<Esc>")
  end)

  -- "Before the second `/` is typed the plain pattern preview (the yellow match
  --  highlight) shows instead; opening the replacement hands off to the diff."
  btv.test.it("the plain match highlight shows until the replacement opens", function(t)
    open(t)
    t:feed(":%s/teh")
    -- The pattern preview is the SEARCH-match layer, which no highlight span
    -- carries — the diff has not taken over yet.
    btv.test.expect(#t:matches(1)).to_be(1)
    btv.test.expect(groups(t, 1)["BtvSubstituteDelete"]).to_be(nil)
    -- The second `/` hands off: the diff paints and the plain match steps aside.
    t:feed("/")
    btv.test.expect(groups(t, 1)["BtvSubstituteDelete"]).to_be(true)
    btv.test.expect(#t:matches(1)).to_be(0)
    t:feed("<Esc>")
  end)

  -- "The `c` (confirm) flag carries the diff into the walk … the match being
  --  decided shows the same diff while the pending matches keep the plain yellow."
  btv.test.it("the confirm walk keeps the diff on the match being decided", function(t)
    open(t)
    t:feed(":3s/color/colour/gc<CR>")
    btv.test.expect(t:message()).to_contain("replace with")
    btv.test.expect(groups(t, 3)["BtvSubstituteDelete"]).to_be(true)
    -- Answer for real: `y` takes this one, `q` stops the walk.
    t:feed("y")
    t:feed("q")
    btv.test.expect(t:line(3)).to_contain("colour")
    btv.test.expect(t:line(3)).to_contain("color;")
    t:cmd("undo")
  end)
end)
