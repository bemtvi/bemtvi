-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/fragment-highlighting
--
-- The demo's eight rows are a completion source, so `t:menu()` sees what is
-- offered and the `[CompletionDocs]` buffer what the float shows. The COLOURING
-- itself is inside that float — a spec cannot read a non-focused window's spans —
-- so what is pinned here is the wiring: the rows, the fence each one carries, the
-- framings the config installs, and the two commands that turn the ladder off and
-- on. (The colouring is covered natively, in the server's fragment suite.)

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- The window showing the completion docs, if one is up.
local function docs_win()
  for _, w in ipairs(vim.api.nvim_list_wins()) do
    if btv.buf.name(vim.api.nvim_win_get_buf(w)) == "[CompletionDocs]" then
      return w
    end
  end
end

--- A scratch line at the end of the sample, in insert mode, with any popup and
--- docs float from the previous case gone — the float is REUSED, so a stale one
--- still up would answer the next read with the last row's text.
local function typing(t)
  t:feed("<C-e>")
  t:feed("<Esc>")
  t:wait_for(function()
    return t:menu() == nil and docs_win() == nil
  end, { tries = 100, interval = 20, message = "a popup outlived the previous case" })
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("G")
  t:feed("o")
end

--- Type `prefix`, select the row, and return the docs float's text.
local function doc_for(t, prefix)
  t:feed(prefix)
  t:wait_for(function()
    local m = t:menu()
    return m ~= nil and #m.items > 0
  end, { tries = 200, interval = 20, message = "no popup for " .. prefix })
  t:feed("<C-n>")
  local doc
  t:wait_for(function()
    local w = docs_win()
    if w == nil then
      return false
    end
    doc = table.concat(btv.buf.lines(vim.api.nvim_win_get_buf(w), 0, -1), "\n")
    return doc ~= ""
  end, { tries = 200, interval = 20, message = "no docs float for " .. prefix })
  return doc
end

btv.test.describe("examples/fragment-highlighting", function()
  -- "Type `fie`, `let`, `sig`, `dia`, `pyd`, `pyc`, `pym` or `pyo` in insert mode."
  btv.test.it("each trigger offers its own row", function(t)
    for _, prefix in ipairs({ "fie", "let", "sig", "dia", "pyd", "pyc", "pym", "pyo" }) do
      typing(t)
      t:feed(prefix)
      t:wait_for(function()
        local m = t:menu()
        return m ~= nil and #m.items > 0
      end, { tries = 200, interval = 20, message = "no popup for " .. prefix })
      -- The row's own label is the prefix's first three letters, which is the
      -- source's whole matching rule.
      btv.test.expect(t:menu().items[1]).to_be(prefix == "fie" and "field" or prefix)
      t:feed("<C-e><Esc>")
    end
  end)

  -- "1. A FRAGMENT gets its real structure back … `field: Vec<String>`"
  btv.test.it("the field row carries the rust fragment", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "fie")).to_contain("field: Vec<String>")
  end)

  btv.test.it("the statement row carries its fragment", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "let")).to_contain("let total = counts.len();")
  end)

  btv.test.it("the signature row carries its fragment", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "sig")).to_contain("pub fn frobnicate(x: &str) -> Option<String>")
  end)

  -- "2. A DIALECT is left alone rather than guessed at."
  btv.test.it("the dialect row is the display text a server would send", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "dia")).to_contain("(method) Registry::get(name: &str) -> bool")
  end)

  -- "2b. INDENTATION-SENSITIVE languages."
  btv.test.it("the python def row carries its header", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "pyd")).to_contain("def frobnicate(name: str, count: int) -> bool")
  end)

  btv.test.it("the python class row carries its header", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "pyc")).to_contain("class Registry(Mapping)")
  end)

  -- "2c. A DISPLAY LABEL, and a block that is a LIST."
  btv.test.it("the display-label row keeps its label", function(t)
    typing(t)
    btv.test.expect(doc_for(t, "pym")).to_contain("(method) join(self, sep: str) -> str")
  end)

  btv.test.it("the overload row carries every signature", function(t)
    typing(t)
    local overloads = doc_for(t, "pyo")
    btv.test.expect(overloads).to_contain("def join(self, x: str) -> str")
    btv.test.expect(overloads).to_contain("def join(self, x: bytes) -> bytes")
  end)

  -- "3. Teach it a framing of your own. `fragment_context` replaces a language's
  --  list of framings."
  btv.test.it("the config installs its own framings without complaint", function(t)
    typing(t)
    t:feed("<Esc>")
    t:cmd("echo ''")
    t:exec(function()
      btv.treesitter.fragment_context("rust", {
        "struct __btv {\n%s\n}",
        "fn __btv() {\n%s\n}",
        "impl __btv {\n%s\n}",
        "trait __btv {\n%s\n}",
      })
    end)
    btv.test.expect(t:message()).to_be("")
  end)

  -- "4. Turn the ladder OFF to see the difference … `:FragmentLadderOn` puts the
  --  framings back."
  btv.test.it(":FragmentLadderOff and :FragmentLadderOn report their state", function(t)
    typing(t)
    t:feed("<Esc>")
    t:cmd("FragmentLadderOff")
    btv.test.expect(t:message()).to_be("rust fragment framings: off")
    t:cmd("FragmentLadderOn")
    btv.test.expect(t:message()).to_be("rust fragment framings: on")
  end)

  -- The row still renders with the ladder off — it is the COLOURING that changes,
  -- not the content.
  btv.test.it("the ladder toggles nothing about the doc's text", function(t)
    typing(t)
    local before = doc_for(t, "fie")
    typing(t)
    t:cmd("FragmentLadderOff")
    typing(t)
    btv.test.expect(doc_for(t, "fie")).to_be(before)
    typing(t)
    t:cmd("FragmentLadderOn")
  end)
end)
