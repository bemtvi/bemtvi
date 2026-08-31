-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/folds
--
-- A closed fold replaces its lines with a single placeholder ROW, which exists
-- only on the painted screen — so the fold state is read with `t:screen()`, and
-- what an operator did to the buffer with `t:lines()`.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.lua")
  t:cmd("e!")
  t:feed("gg")
  t:feed("zR") -- every fold open, so each test starts from the same picture
end

--- A closed fold paints one placeholder row in place of its lines, spelled
--- `+--  N lines: <first line>`. These two read the fold picture off the screen —
--- the only place it exists.
local function fold_rows(t)
  local n = 0
  for _, text in ipairs(t:screen()) do
    if text:find("%d+ lines:") then
      n = n + 1
    end
  end
  return n
end

--- How many buffer lines are hidden behind placeholders right now.
local function folded_away(t)
  local n = 0
  for _, text in ipairs(t:screen()) do
    local count = text:match("(%d+) lines:")
    if count then
      n = n + tonumber(count)
    end
  end
  return n
end

btv.test.describe("examples/folds", function()
  btv.test.it("the config turns on the fold gutter and the indent source", function(t)
    open(t)
    btv.test.expect(btv.o.foldcolumn).to_be(1)
    btv.test.expect(btv.o.foldenable).to_be(true)
    btv.test.expect(btv.bo.foldmethod).to_be("indent")
  end)

  -- "Computed folds open at `foldlevel` (default 0, so nesting shows collapsed on
  --  open — `zR` opens everything, `zM` re-closes)."
  btv.test.it("zM closes every fold and zR opens them all", function(t)
    open(t)
    btv.test.expect(fold_rows(t)).to_be(0)
    t:feed("zM")
    btv.test.expect(fold_rows(t) > 0).to_be(true)
    btv.test.expect(folded_away(t) > 0).to_be(true)
    t:feed("zR")
    btv.test.expect(fold_rows(t)).to_be(0)
  end)

  -- "za / zo / zc  toggle / open / close the fold under the cursor"
  btv.test.it("zc closes the fold under the cursor, zo opens it", function(t)
    open(t)
    -- Line 7 is `window = {`, the head of an indented block.
    t:feed("8G")
    btv.test.expect(fold_rows(t)).to_be(0)
    t:feed("zc")
    btv.test.expect(fold_rows(t)).to_be(1)
    btv.test.expect(folded_away(t)).to_be(3)
    t:feed("zo")
    btv.test.expect(fold_rows(t)).to_be(0)
  end)

  btv.test.it("za toggles the same fold", function(t)
    open(t)
    t:feed("8G")
    t:feed("za")
    btv.test.expect(fold_rows(t)).to_be(1)
    t:feed("za")
    btv.test.expect(fold_rows(t)).to_be(0)
  end)

  -- "zj / zk  jump to the next / previous fold"
  btv.test.it("zj and zk step between folds", function(t)
    open(t)
    t:feed("gg")
    t:feed("zj")
    local first = t:cursor()[1]
    btv.test.expect(first > 1).to_be(true)
    t:feed("zj")
    local second = t:cursor()[1]
    btv.test.expect(second > first).to_be(true)
    -- A third step, so `zk` below has a previous SIBLING fold to land in. The second
    -- fold here opens inside the first (`window = {` nested in `config = {`), and from
    -- inside a nest there is no earlier fold above to step back to — `zk` correctly
    -- stays put. Which folds nest depends on the file's own indent width, now that
    -- `'indentdetect'` reads it off the file, so the step count is what makes this
    -- assertion about the MOTION rather than about one particular fold picture.
    t:feed("zj")
    local third = t:cursor()[1]
    btv.test.expect(third > second).to_be(true)
    -- `zk` lands on the END of the previous fold, not its start (vim's rule), so
    -- the assertion is direction, not symmetry.
    t:feed("zk")
    btv.test.expect(t:cursor()[1] < third).to_be(true)
  end)

  -- ":set foldlevel=1  show only the top level of nesting"
  btv.test.it(":set foldlevel controls how deep the picture goes", function(t)
    open(t)
    t:cmd("set foldlevel=0")
    local at_zero = folded_away(t)
    btv.test.expect(at_zero > 0).to_be(true)
    t:cmd("set foldlevel=1")
    btv.test.expect(folded_away(t) < at_zero).to_be(true)
    t:cmd("set foldlevel=99")
    btv.test.expect(fold_rows(t)).to_be(0)
  end)

  -- "zi  toggle folding off/on entirely"
  btv.test.it("zi turns folding off and on", function(t)
    open(t)
    t:feed("zM")
    btv.test.expect(fold_rows(t) > 0).to_be(true)
    t:feed("zi")
    btv.test.expect(btv.o.foldenable).to_be(false)
    btv.test.expect(fold_rows(t)).to_be(0)
    t:feed("zi")
    btv.test.expect(btv.o.foldenable).to_be(true)
    btv.test.expect(fold_rows(t) > 0).to_be(true)
  end)

  -- "zf{motion}  create a MANUAL fold … manual folds coexist with the computed ones"
  btv.test.it("zf creates a manual fold", function(t)
    open(t)
    t:cmd("set foldmethod=manual")
    t:feed("4G")
    t:feed("zfj")
    -- Two lines collapsed into one placeholder row.
    btv.test.expect(fold_rows(t)).to_be(1)
    btv.test.expect(folded_away(t)).to_be(2)
    t:feed("zo")
    btv.test.expect(fold_rows(t)).to_be(0)
  end)

  -- "With the cursor on a CLOSED fold, linewise operators act on the whole fold:
  --  dd deletes every line in the fold · yy yanks it"
  btv.test.it("dd on a closed fold deletes every line in it", function(t)
    open(t)
    local before = #t:lines()
    t:feed("8G")
    t:feed("zc")
    local folded = folded_away(t)
    btv.test.expect(folded).to_be(3)
    t:feed("dd")
    -- The whole fold went, not one line.
    btv.test.expect(#t:lines()).to_be(before - folded)
    btv.test.expect(t:lines()).never.to_contain("    width = 80,")
  end)

  btv.test.it("yy on a closed fold yanks every line in it", function(t)
    open(t)
    t:feed("8G")
    t:feed("zc")
    local folded = folded_away(t)
    t:feed("yy")
    local yanked = vim.fn.getreg('"')
    local n = select(2, yanked:gsub("\n", "\n"))
    btv.test.expect(n).to_be(folded)
    btv.test.expect(yanked).to_contain("width = 80,")
  end)

  -- "or fold by explicit markers you place in the text" — the UPGRADE note's
  -- grammar-free alternative, which must work as written.
  btv.test.it("the marker source in the UPGRADE note works as written", function(t)
    open(t)
    t:cmd("set foldmethod=marker")
    t:feed("ggOstart {{{<CR>middle<CR>end }}}<Esc>")
    t:feed("ggzc")
    btv.test.expect(fold_rows(t)).to_be(1)
    btv.test.expect(folded_away(t)).to_be(3)
  end)
end)
