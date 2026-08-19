-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/smart-indent
--
-- Every case types exactly the keys a recipe in the notes tells a reader to type
-- and asserts the lines (and cursor column) it promises. The sample has no
-- filetype on purpose — tree-sitter never indents it, so what is measured here is
-- the three options and nothing else.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- A scratch buffer carrying the config's own settings, so a test types into an
--- empty file rather than into the middle of the sample's prose.
local function scratch(t)
  t:cmd("enew!")
  t:cmd("setlocal expandtab tabstop=2 shiftwidth=2 smartindent autopairs")
  t:feed("gg")
end

btv.test.describe("examples/smart-indent", function()
  btv.test.it("the config turns all three on, two spaces wide", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    btv.test.expect(btv.bo.expandtab).to_be(true)
    btv.test.expect(btv.bo.tabstop).to_be(2)
    btv.test.expect(btv.bo.smartindent).to_be(true)
    btv.test.expect(btv.bo.autopairs).to_be(true)
  end)

  -- "1. AUTO-PAIRS — TYPE: foo( -> foo(|) ; bar -> foo(bar|) ; ) -> foo(bar)|"
  btv.test.it("1 — an opener brings its closer and the cursor sits between", function(t)
    scratch(t)
    t:feed("ifoo(")
    btv.test.expect(t:line(1)).to_be("foo()")
    btv.test.expect(t:cursor()[2]).to_be(4)
    t:feed("bar")
    btv.test.expect(t:line(1)).to_be("foo(bar)")
    -- Typing the closer walks through the one already there — no doubling.
    t:feed(")")
    btv.test.expect(t:line(1)).to_be("foo(bar)")
    btv.test.expect(t:cursor()[2]).to_be(8)
    t:feed("<Esc>")
  end)

  -- "2. BLOCK EXPANSION — TYPE: if cond { … <CR> … work"
  btv.test.it("2 — an opener closes itself and <CR> lays the block out", function(t)
    scratch(t)
    t:feed("iif cond {")
    btv.test.expect(t:line(1)).to_be("if cond {}")
    t:feed("<CR>")
    btv.test.expect(t:lines()).to_equal({ "if cond {", "  ", "}" })
    btv.test.expect(t:cursor()[1]).to_be(2)
    btv.test.expect(t:cursor()[2]).to_be(2)
    t:feed("work")
    btv.test.expect(t:lines()).to_equal({ "if cond {", "  work", "}" })
    t:feed("<Esc>")
  end)

  btv.test.it("2 — the same three lines for a paren", function(t)
    scratch(t)
    t:feed("ifn(<CR>")
    btv.test.expect(t:lines()).to_equal({ "fn(", "  ", ")" })
    btv.test.expect(t:cursor()[1]).to_be(2)
    btv.test.expect(t:cursor()[2]).to_be(2)
    t:feed("<Esc>")
  end)

  -- "3. SMARTINDENT ON ITS OWN — :setlocal noautopairs, then if cond {<CR> work<CR>}"
  btv.test.it("3 — without auto-pairs the closer is yours, and it snaps back", function(t)
    scratch(t)
    t:cmd("setlocal noautopairs")
    t:feed("iif cond {<CR>")
    btv.test.expect(t:line(1)).to_be("if cond {")
    btv.test.expect(t:cursor()[2]).to_be(2)
    t:feed("work<CR>}")
    btv.test.expect(t:lines()).to_equal({ "if cond {", "  work", "}" })
    t:feed("<Esc>")
  end)

  -- "4. BACKSPACE over an empty pair removes both halves"
  btv.test.it("4 — <BS> between an empty pair deletes both", function(t)
    scratch(t)
    t:feed("i[")
    btv.test.expect(t:line(1)).to_be("[]")
    t:feed("<BS>")
    btv.test.expect(t:line(1)).to_be("")
    t:feed("<Esc>")
  end)

  -- "5. Quotes are smart about words — an apostrophe inside a word is NOT paired"
  btv.test.it("5 — an apostrophe inside a word is left alone", function(t)
    scratch(t)
    t:feed("idon't")
    btv.test.expect(t:line(1)).to_be("don't")
    t:feed("<Esc>")
  end)

  btv.test.it("5 — a fresh quote IS paired", function(t)
    scratch(t)
    t:feed('i"')
    btv.test.expect(t:line(1)).to_be('""')
    btv.test.expect(t:cursor()[2]).to_be(1)
    t:feed("<Esc>")
  end)

  -- ":setlocal noautopairs — type `(` -> just `(`"
  btv.test.it("noautopairs types the opener alone", function(t)
    scratch(t)
    t:cmd("setlocal noautopairs")
    t:feed("i(")
    btv.test.expect(t:line(1)).to_be("(")
    t:feed("<Esc>")
  end)

  -- ":setlocal nosmartindent — <CR> no longer indents after `{`"
  btv.test.it("nosmartindent stops the bracket-aware indent", function(t)
    scratch(t)
    t:cmd("setlocal nosmartindent noautopairs")
    t:feed("iif cond {<CR>")
    btv.test.expect(t:line(2)).to_be("")
    t:feed("<Esc>")
  end)

  -- "autoindent (ai) — a new line copies the previous line's indent."
  btv.test.it("autoindent alone only copies the previous indent", function(t)
    scratch(t)
    t:cmd("setlocal nosmartindent noautopairs autoindent")
    t:feed("i    deep<CR>next")
    btv.test.expect(t:line(2)).to_be("    next")
    t:feed("<Esc>")
  end)

  -- ":set smartindent? — echoes 'smartindent'"
  btv.test.it("the options answer a query", function(t)
    scratch(t)
    t:cmd("set smartindent?")
    btv.test.expect(t:message()).to_be("smartindent")
    t:cmd("setlocal nosmartindent")
    t:cmd("set smartindent?")
    btv.test.expect(t:message()).to_be("nosmartindent")
  end)
end)
