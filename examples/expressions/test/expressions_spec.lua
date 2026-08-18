-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/expressions
--
-- It loads `init.lua` exactly as a user's session would and drives the same
-- `<leader>N` shortcuts the notes tell you to press, so the demos cannot rot
-- into instructions that no longer work.
--
-- `t:lines()` is buffer text; `t:screen()` is what the client would paint. The
-- fold demos need the second, because a closed fold's placeholder replaces the
-- lines rather than changing them.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- Load the example's config the way a session would, once.
dofile(DIR .. "/init.lua")

--- Open the sample buffer and park the cursor at the top.
---
--- Two commands, not one: the per-test baseline restores only the buffer `enew!`
--- replaces, so a test that edited the sample leaves *that* buffer modified and a
--- plain `:e` (which switches to an existing buffer without re-reading) would
--- hand the next test the edited file back. The bare `:e!` re-reads it once it is
--- current again.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/expressions", function()
  btv.test.it("the config loads and its options apply", function(t)
    open(t)
    btv.test.expect(btv.o.number).to_be(true)
    btv.test.expect(btv.bo.foldmethod).to_be("expr")
    btv.test.expect(btv.bo.foldexpr).never.to_be("")
  end)

  -- Demo 1. The braced blocks fold. A spec cannot read the painted rows, but a
  -- closed fold is *navigable* state: `j` steps over the whole thing.
  btv.test.it("demo 1 — the foldexpr folds each braced block", function(t)
    open(t)
    t:feed("<Space>1")
    -- The two blocks collapse into one painted row each…
    btv.test.expect(t:screen()[4]).to_contain("fn alpha() {")
    btv.test.expect(t:screen()[6]).to_contain("fn beta() {")
    -- …and a closed fold is navigable state too: `j` steps over the whole thing.
    t:feed("4G")
    btv.test.expect(t:cursor()[1]).to_be(4)
    t:feed("j")
    btv.test.expect(t:cursor()[1]).to_be(9)
  end)

  -- Demo 2. The collapsed row is painted, not buffered, so this reads the screen.
  btv.test.it("demo 2 — the custom foldtext paints the collapsed row", function(t)
    open(t)
    t:cmd("FoldText")
    t:feed("<Space>1")
    local row = t:screen()[4]
    btv.test.expect(row).to_contain("fn alpha() {")
    btv.test.expect(row).to_contain("… 5 lines")
  end)

  btv.test.it("demo 2 — :FoldText off restores the built-in row", function(t)
    open(t)
    t:cmd("FoldText off")
    t:feed("<Space>1")
    btv.test.expect(t:screen()[4]).to_contain("5 lines: fn alpha() {")
  end)

  -- Demo 3. `=` is a normal-mode operator, so the shortcut is `16G=3j`.
  btv.test.it("demo 3 — the indentexpr indents the flat block", function(t)
    open(t)
    t:feed("<Space>3")
    btv.test.expect(t:line(17)).to_be("    six")
    btv.test.expect(t:line(18)).to_be("    seven")
    btv.test.expect(t:line(19)).to_be("}")
  end)

  -- Demo 4. The same `.h` extension, decided by what is inside the file.
  btv.test.it("demo 4 — the sniffer alternates cpp and c", function(t)
    open(t)
    t:feed("<Space>4")
    btv.test.expect(btv.bo.filetype).to_be("cpp")
    t:feed("<Space>4")
    btv.test.expect(btv.bo.filetype).to_be("c")
    t:feed("<Space>4")
    btv.test.expect(btv.bo.filetype).to_be("cpp")
  end)

  -- Demo 5. The confirmed row is the observable: with the scorer on, `docs/`
  -- leads, so `<CR>` on the top row takes it.
  btv.test.it("demo 5 — the scorer promotes docs/ to the top", function(t)
    open(t)
    t:feed("<Space>5")
    t:sleep(50)
    t:feed("mod")
    t:sleep(50)
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("picked", 1, true) ~= nil
    end, { message = "the picker reported no pick" })
    btv.test.expect(t:message()).to_contain("picked docs/model.md")
  end)

  btv.test.it("demo 5 — :Scorer off puts the native order back", function(t)
    open(t)
    t:cmd("Scorer off")
    t:feed("<Space>5")
    t:sleep(50)
    t:feed("mod")
    t:sleep(50)
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("picked", 1, true) ~= nil
    end, { message = "the picker reported no pick" })
    -- Whatever the matcher ranks first, it is not the row the scorer promoted —
    -- which is the whole point of offering the toggle.
    btv.test.expect(t:message()).never.to_contain("picked docs/model.md")
  end)

  -- Demo 6. The expression register needs no config, so the spec types exactly
  -- what the notes tell a reader to type.
  btv.test.it("demo 6 — <C-r>= inserts a computed value", function(t)
    open(t)
    t:feed("2G")
    t:feed("o<C-r>=lnum * 10<CR><Esc>")
    -- `o` opened line 3, so `lnum` was 3 when the expression ran.
    btv.test.expect(t:line(3)).to_be("30")
  end)

  btv.test.it('demo 6 — "= stores a result the next p pastes', function(t)
    open(t)
    t:feed('gg"=("-"):rep(8)<CR>p')
    btv.test.expect(t:lines()[1]).to_contain("--------")
    btv.test.expect(vim.fn.getreg("=")).to_be("--------")
  end)

  btv.test.it("demo 6 — <C-r>= splices into a command line", function(t)
    open(t)
    t:feed("3G")
    -- The replacement is *terminated* (`…/`): an unterminated `:s` trims its
    -- trailing whitespace, which would eat the space after the computed number.
    t:feed(":s/^/<C-r>=lnum<CR>. /<CR>")
    btv.test.expect(t:line(3)).to_match("^3%. ")
  end)

  -- Demo 6b. `<C-n>` takes the *top* row and `<C-y>` accepts it, so what lands in
  -- the buffer is the popup's order — no need to read the popup itself.
  btv.test.it("demo 6b — the scorer sorts the snippet row last", function(t)
    open(t)
    t:feed("ofor")
    t:sleep(50)
    t:feed("<C-n><C-y>")
    btv.test.expect(t:line(2)).never.to_be("for_loop")
    btv.test.expect(t:line(2)).to_match("^for")
  end)

  btv.test.it("demo 6b — :CompleteScorer off puts the snippet back on top", function(t)
    open(t)
    t:cmd("CompleteScorer off")
    t:feed("ofor")
    t:sleep(50)
    t:feed("<C-n><C-y>")
    -- Its source outranks the words', so natively the snippet leads.
    btv.test.expect(t:line(2)).to_be("for_loop")
  end)

  -- Demo 7. A paint is neither buffer text nor a painted glyph, so this is the
  -- one thing only `t:highlights()` can see.
  btv.test.it("demo 7 — the paint lights up every TODO", function(t)
    open(t)
    t:feed("<Space>7")
    t:feed("ITODO: fix this<Esc>")
    local spans = t:highlights(1)
    btv.test.expect(#spans).to_be(1)
    btv.test.expect(spans[1][3]).to_be("Todo")
    -- 1-based inclusive in, 0-based end-exclusive out: `TODO` at the line start.
    btv.test.expect(spans[1][1]).to_be(0)
    btv.test.expect(spans[1][2]).to_be(4)
  end)

  btv.test.it("demo 7 — :Paint off takes the paint with it", function(t)
    open(t)
    t:feed("<Space>7")
    t:feed("ITODO<Esc>")
    btv.test.expect(#t:highlights(1)).to_be(1)
    t:cmd("Paint off")
    btv.test.expect(#t:highlights(1)).to_be(0)
  end)

  -- Demo 8. The quickfix window's rows are buffer text, so `t:lines()` reads
  -- them once the list is open and focused.
  btv.test.it("demo 8 — the quickfix rows follow btv.qf.text", function(t)
    open(t)
    t:feed("<Space>8")
    local rows = t:lines()
    btv.test.expect(rows[1]).to_be("1. [E] one — sample.txt:5")
    btv.test.expect(rows[2]).to_be("2. [W] four — sample.txt:11")
    btv.test.expect(rows[3]).to_be("3. [-] six — sample.txt:17")
  end)

  btv.test.it("demo 8 — :QfText off restores vim's rendering in place", function(t)
    open(t)
    t:feed("<Space>8")
    btv.test.expect(t:lines()[1]).to_contain("1. [E] one")
    t:cmd("QfText off")
    btv.test.expect(t:lines()[1]).to_contain("|5 col 1| one")
  end)

  -- Demo 9. The parser runs where `'errorformat'` would, so the proof is the
  -- entries the list ends up holding.
  btv.test.it("demo 9 — the Lua parser builds entries errorformat cannot", function(t)
    open(t)
    t:feed("<Space>9")
    local rows = t:lines()
    btv.test.expect(rows[1]).to_contain("|5 col 1| one")
    btv.test.expect(rows[2]).to_contain("|11 col 1| four")
    -- The prose line is declined, and kept as an unjumpable row.
    btv.test.expect(rows[3]).to_be("|| -- build finished with 1 error")
  end)

  btv.test.it("demo 9 — :QfParse off leaves the output to errorformat", function(t)
    open(t)
    t:feed("<Space>9")
    btv.test.expect(t:lines()[1]).to_contain("|5 col 1| one")
    -- `:QfParse off` re-populates from the same three lines, now unparsed: the
    -- raw text is the whole entry, with no line number and no column.
    t:cmd("QfParse off")
    btv.test.expect(t:lines()[1]).to_contain("(5,1): error: one")
    btv.test.expect(t:lines()[1]).never.to_contain("col 1|")
  end)

  btv.test.it(":Cheat opens its float", function(t)
    open(t)
    t:cmd("Cheat")
    t:wait_for(function()
      return t:float() ~= nil
    end, { message = ":Cheat opened no float" })
  end)
end)
