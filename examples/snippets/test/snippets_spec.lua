-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/snippets
--
-- Every case drives the popup the notes tell a reader to drive — type a trigger,
-- `<C-n>` onto the row, `<C-y>` to accept — so what is asserted is the body a
-- reader would actually get, not a direct call to the engine.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- A scratch `lua` buffer: the snippets are registered per filetype, and the
--- sample is `sample.lua`.
local function scratch(t)
  t:cmd("enew!")
  t:cmd("setlocal filetype=lua expandtab tabstop=2 shiftwidth=2")
  t:feed("gg")
end

--- Type `trigger` in insert mode and accept its popup row, the way the notes say.
local function expand(t, trigger)
  t:feed("i" .. trigger)
  t:sleep(50)
  t:feed("<C-n><C-y>")
end

btv.test.describe("examples/snippets", function()
  -- "TYPE `fn` … expands a function, cursor on the name; <Tab> jumps name -> args
  --  -> body."
  btv.test.it("fn expands, and <Tab> walks its tabstops", function(t)
    scratch(t)
    expand(t, "fn")
    btv.test.expect(t:lines()).to_equal({ "function name()", "\t", "end" })
    -- The first tabstop's default lands SELECTED, so typing replaces it.
    t:feed("greet")
    btv.test.expect(t:line(1)).to_be("function greet()")
    -- …then <Tab> is the argument list, and <Tab> again the body.
    t:feed("<Tab>arg")
    btv.test.expect(t:line(1)).to_be("function greet(arg)")
    t:feed("<Tab>")
    btv.test.expect(t:cursor()[1]).to_be(2)
    t:feed("<Esc>")
  end)

  btv.test.it("<S-Tab> walks back the way it came", function(t)
    scratch(t)
    expand(t, "fn")
    t:feed("greet<Tab>arg")
    btv.test.expect(t:line(1)).to_be("function greet(arg)")
    t:feed("<S-Tab>")
    -- Back on the name, whose text is selected again: typing replaces it.
    t:feed("other")
    btv.test.expect(t:line(1)).to_be("function other(arg)")
    t:feed("<Esc>")
  end)

  -- "type `for` … a numeric for loop with three tabstops."
  btv.test.it("for expands with its three tabstops", function(t)
    scratch(t)
    expand(t, "for")
    btv.test.expect(t:lines()).to_equal({ "for i = 1, n do", "\t", "end" })
    t:feed("row<Tab>0<Tab>10<Tab>")
    btv.test.expect(t:line(1)).to_be("for row = 0, 10 do")
    btv.test.expect(t:cursor()[1]).to_be(2)
    t:feed("<Esc>")
  end)

  btv.test.it("if expands with its condition selected", function(t)
    scratch(t)
    expand(t, "if")
    btv.test.expect(t:lines()).to_equal({ "if cond then", "\t", "end" })
    t:feed("ok")
    btv.test.expect(t:line(1)).to_be("if ok then")
    t:feed("<Esc>")
  end)

  -- "type `loc` … `local x = x`; edit the name and watch the mirror on the right
  --  update in lockstep."
  btv.test.it("loc mirrors the name into the assignment", function(t)
    scratch(t)
    expand(t, "loc")
    btv.test.expect(t:line(1)).to_be("local x = x")
    t:feed("count")
    btv.test.expect(t:line(1)).to_be("local count = count")
    t:feed("<Esc>")
  end)

  -- "<Esc> instead to keep the default and edit it"
  btv.test.it("<Esc> keeps a selected default instead of replacing it", function(t)
    scratch(t)
    expand(t, "loc")
    t:feed("<Esc>")
    btv.test.expect(t:line(1)).to_be("local x = x")
  end)

  -- "A CHOICE tabstop opens a DROPDOWN of its alternatives on land: expand `alts`,
  --  then <C-n>/<C-p> to move and <C-y> to pick — `local aaa = b`."
  btv.test.it("alts opens a choice dropdown, and <C-y> picks", function(t)
    scratch(t)
    expand(t, "alts")
    btv.test.expect(t:line(1)).to_be("local aaa = a")
    t:sleep(50)
    local menu = t:menu()
    btv.test.expect(menu).never.to_be(nil)
    btv.test.expect(table.concat(menu.items, ",")).to_be("a,b,c")
    t:feed("<C-n><C-y>")
    btv.test.expect(t:line(1)).to_be("local aaa = b")
    t:feed("<Esc>")
  end)

  -- "Each snippet row shows a right-aligned 'Snippet' KIND label … a plain buffer
  --  word, which is labelled 'Text'."
  btv.test.it("a snippet row and a buffer word wear different kinds", function(t)
    scratch(t)
    -- A buffer word to compete with, on its own line.
    t:feed("iforever<Esc>o")
    t:feed("for")
    t:sleep(50)
    local menu = t:menu()
    btv.test.expect(menu).never.to_be(nil)
    local kinds = {}
    for i, item in ipairs(menu.items) do
      kinds[item] = kinds[item] or menu.kinds[i]
    end
    btv.test.expect(kinds["for"]).to_be("Snippet")
    btv.test.expect(kinds["forever"]).to_be("Text")
    t:feed("<Esc>")
  end)

  -- "a DOCS FLOAT opens beside the popup previewing the body you're about to
  --  expand"
  btv.test.it("moving onto a snippet row previews its body", function(t)
    scratch(t)
    t:feed("ifn")
    t:sleep(50)
    t:feed("<C-n>")
    -- The sidebar is a REAL window over a `[CompletionDocs]` buffer (the hover
    -- model), not the transient content float — so it is read like any buffer.
    local doc
    t:wait_for(function()
      for _, w in ipairs(vim.api.nvim_list_wins()) do
        local buf = vim.api.nvim_win_get_buf(w)
        if btv.buf.name(buf) == "[CompletionDocs]" then
          doc = table.concat(btv.buf.lines(buf, 0, -1), "\n")
          return true
        end
      end
      return false
    end, { message = "no docs float opened beside the popup" })
    btv.test.expect(doc).to_contain("function ${1:name}(${2})")
    t:feed("<Esc>")
  end)

  -- "press <C-s> anywhere in insert mode -> expands print(${1:value})$0 directly."
  btv.test.it("<C-s> expands a body straight from a mapping", function(t)
    scratch(t)
    t:feed("i<C-s>")
    btv.test.expect(t:line(1)).to_be("print(value)")
    t:feed("42")
    btv.test.expect(t:line(1)).to_be("print(42)")
    t:feed("<Esc>")
  end)

  -- "Unsupported constructs (variables like $TM_FILENAME, regex transforms) fail
  --  loud rather than inserting raw `$1` text."
  btv.test.it("an unsupported construct fails loud", function(t)
    scratch(t)
    local ok, err = pcall(btv.snippet.expand, "name: $TM_FILENAME$0")
    if ok then
      -- The engine may report it on the message line instead of raising.
      t:cmd("echo ''")
      t:feed("i")
      t:exec(function()
        btv.snippet.expand("name: $TM_FILENAME$0")
      end)
      t:feed("<Esc>")
      btv.test.expect(t:message()).never.to_be("")
      btv.test.expect(t:line(1)).never.to_contain("$TM_FILENAME")
    else
      btv.test.expect(tostring(err)).to_contain("TM_FILENAME")
    end
  end)
end)
