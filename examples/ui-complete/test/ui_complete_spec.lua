-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-complete
--
-- The popup floats over the text while your typing flows on to the document, so
-- `t:menu()` is the view of what was offered and `t:line()` of what was accepted.
-- The async sources are debounced, so a case pauses for them rather than assuming.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- A scratch line at the end of the sample, in insert mode, so the buffer source
--- has the sample's own words to offer.
local function typing(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("G")
  t:feed("o")
end

--- Wait for the popup to carry at least one row.
local function rows(t)
  t:wait_for(function()
    local m = t:menu()
    return m ~= nil and #m.items > 0
  end, { tries = 200, interval = 20, message = "no completion popup" })
  return t:menu()
end

btv.test.describe("examples/ui-complete", function()
  -- "type `con` on a blank line: the popup offers `configuration`, `connection`,
  --  `completion`, `concatenate`, and `consequently`."
  btv.test.it("the buffer source offers the sample's own words", function(t)
    typing(t)
    t:feed("con")
    local offered = table.concat(rows(t).items, " ")
    for _, word in ipairs({
      "configuration",
      "connection",
      "completion",
      "concatenate",
      "consequently",
    }) do
      btv.test.expect(offered).to_contain(word)
    end
    t:feed("<C-e><Esc>")
  end)

  -- "It opens with NOTHING selected (noselect) … <C-n> / <Tab> / <Down> select"
  btv.test.it("the popup opens noselect and <C-n> moves into it", function(t)
    typing(t)
    t:feed("con")
    btv.test.expect(rows(t).selected).to_be_nil()
    t:feed("<C-n>")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<C-n>")
    btv.test.expect(t:menu().selected).to_be(2)
    t:feed("<C-p>")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<C-e><Esc>")
  end)

  -- "<C-y> / <CR> accept the highlighted row"
  btv.test.it("<C-y> accepts the highlighted row", function(t)
    typing(t)
    t:feed("con")
    local first = rows(t).items[1]
    t:feed("<C-n><C-y>")
    btv.test.expect(t:line(t:cursor()[1])).to_be(first)
    t:feed("<Esc>")
  end)

  -- "the default leaves <CR> as a literal newline … here we ALSO bind <CR>"
  btv.test.it("<CR> accepts too, since the config binds it", function(t)
    typing(t)
    t:feed("con")
    local first = rows(t).items[1]
    t:feed("<C-n><CR>")
    btv.test.expect(t:line(t:cursor()[1])).to_be(first)
    t:feed("<Esc>")
  end)

  -- "<C-e> dismiss the popup (keep what you typed)"
  btv.test.it("<C-e> dismisses and keeps the typed prefix", function(t)
    typing(t)
    t:feed("con")
    rows(t)
    t:feed("<C-e>")
    btv.test.expect(t:menu()).to_be_nil()
    btv.test.expect(t:line(t:cursor()[1])).to_be("con")
    t:feed("<Esc>")
  end)

  -- "the `keywords` async source (e.g. type `func` to get `function`)"
  btv.test.it("the async keywords source answers the live prefix", function(t)
    typing(t)
    t:feed("func")
    -- It runs ~80 ms after the last keystroke.
    t:wait_for(function()
      local m = t:menu()
      if m == nil then
        return false
      end
      for _, item in ipairs(m.items) do
        if item == "function" then
          return true
        end
      end
      return false
    end, { tries = 200, interval = 20, message = "the async source never answered" })
    t:feed("<C-e><Esc>")
  end)

  -- "Only offer keywords that actually extend the prefix — a faithful source
  --  reacts to its input."
  btv.test.it("the keywords source reacts to the prefix it is given", function(t)
    typing(t)
    t:feed("req")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items > 0 and m.items[1] == "require"
    end, { tries = 200, interval = 20, message = "the async source never answered" })
    local offered = table.concat(t:menu().items, " ")
    btv.test.expect(offered).never.to_contain("function")
    t:feed("<C-e><Esc>")
  end)

  -- "Land on a `keywords` row and its docs appear beside the popup, fetched
  --  lazily via `resolve`."
  btv.test.it("landing on a keywords row resolves its docs", function(t)
    typing(t)
    t:feed("req")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items > 0 and m.items[1] == "require"
    end, { tries = 200, interval = 20, message = "the async source never answered" })
    t:feed("<C-n>")
    local doc
    t:wait_for(function()
      for _, w in ipairs(vim.api.nvim_list_wins()) do
        local buf = vim.api.nvim_win_get_buf(w)
        if btv.buf.name(buf) == "[CompletionDocs]" then
          doc = table.concat(btv.buf.lines(buf, 0, -1), "\n")
          return doc ~= ""
        end
      end
      return false
    end, { tries = 200, interval = 20, message = "no docs sidebar opened" })
    btv.test.expect(doc).to_contain("keyword: require")
    btv.test.expect(doc).to_contain("7 chars")
    t:feed("<C-e><Esc>")
  end)

  -- "type a `:` and a letter (`:sm`, `:ro`, …): the emoji source wakes on the
  --  colon — the buffer/keyword sources go quiet."
  btv.test.it("a trigger char wakes the emoji source alone", function(t)
    typing(t)
    t:feed(":sm")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items > 0
    end, { tries = 200, interval = 20, message = "the trigger source never answered" })
    btv.test.expect(t:menu().items).to_equal({ ":smile:" })
    t:feed("<C-e><Esc>")
  end)

  -- "<C-y> replaces `:sm` with 😄"
  btv.test.it("accepting an emoji row replaces from the colon", function(t)
    typing(t)
    t:feed(":sm")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items > 0
    end, { tries = 200, interval = 20, message = "the trigger source never answered" })
    t:feed("<C-n><C-y>")
    btv.test.expect(t:line(t:cursor()[1])).to_be("😄")
    t:feed("<Esc>")
  end)

  -- "Each item carries inline `doc`, shown in the docs sidebar beside the popup."
  btv.test.it("an emoji row carries its doc inline", function(t)
    typing(t)
    t:feed(":ro")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items > 0
    end, { tries = 200, interval = 20, message = "the trigger source never answered" })
    t:feed("<C-n>")
    local doc
    t:wait_for(function()
      for _, w in ipairs(vim.api.nvim_list_wins()) do
        local buf = vim.api.nvim_win_get_buf(w)
        if btv.buf.name(buf) == "[CompletionDocs]" then
          doc = table.concat(btv.buf.lines(buf, 0, -1), "\n")
          return doc ~= ""
        end
      end
      return false
    end, { tries = 200, interval = 20, message = "no docs sidebar opened" })
    btv.test.expect(doc).to_contain(":rocket:")
    btv.test.expect(doc).to_contain("🚀")
    t:feed("<C-e><Esc>")
  end)

  -- "`min_chars` gates how long a prefix must be before the popup opens"
  btv.test.it("one character is below the buffer source's min_chars", function(t)
    typing(t)
    t:feed("c")
    t:sleep(150)
    local m = t:menu()
    if m ~= nil then
      -- The `keywords` source has no gate, so it may answer at one character —
      -- but `concatenate` is a buffer word and nothing else offers it.
      local offered = table.concat(m.items, " ")
      btv.test.expect(offered).never.to_contain("concatenate")
      btv.test.expect(offered).never.to_contain("consequently")
    end
    t:feed("<C-e><Esc>")
  end)

  btv.test.it("two characters let the buffer source in", function(t)
    typing(t)
    t:feed("co")
    t:sleep(150)
    btv.test.expect(table.concat(rows(t).items, " ")).to_contain("concatenate")
    t:feed("<C-e><Esc>")
  end)

  -- "A manual <C-Space> trigger preselects the first row"
  btv.test.it("<C-Space> forces the popup and preselects", function(t)
    typing(t)
    t:feed("c")
    t:feed("<C-Space>")
    local m = rows(t)
    btv.test.expect(m.selected).to_be(1)
    -- The manual trigger bypasses the gate, so the buffer's own words are in.
    btv.test.expect(table.concat(m.items, " ")).to_contain("concatenate")
    t:feed("<C-e><Esc>")
  end)
end)
