-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-prompt
--
-- Both prompts open over the editor's COMMAND LINE, so what is on it is the view
-- (`btv._ui.cmdline`), and the promise settling is what a case waits for — nothing
-- about either call is synchronous.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

--- Wait for a notification to land on the message line.
local function settled(t, what)
  t:wait_for(function()
    return (t:message() or ""):find(what, 1, true) ~= nil
  end, { message = "the promise never settled with " .. what })
  return t:message()
end

btv.test.describe("examples/ui-prompt", function()
  -- "1. \\i — btv.ui.input: a one-line text prompt. The command line opens
  --  labelled 'Your name: '."
  btv.test.it("\\i opens a labelled prompt and resolves with the text", function(t)
    open(t)
    t:feed("<Bslash>i")
    btv.test.expect(t:cmdline()).to_contain("Your name: ")
    t:feed("Ada<CR>")
    btv.test.expect(settled(t, "hello")).to_be("hello, Ada")
  end)

  -- "Press <Esc> to cancel (the callback runs with `nil`)"
  btv.test.it("\\i cancels with nil on <Esc>", function(t)
    open(t)
    t:feed("<Bslash>i")
    t:feed("Ada<Esc>")
    btv.test.expect(settled(t, "cancelled")).to_be("input cancelled")
  end)

  -- "resolves to the entered string ('' on an empty <CR>)"
  btv.test.it("an empty <CR> resolves with the empty string, not nil", function(t)
    open(t)
    t:feed("<Bslash>i")
    t:feed("<CR>")
    btv.test.expect(settled(t, "hello")).to_be("hello, ")
  end)

  -- "2. \\r — btv.ui.input with a prefilled default. The line is pre-filled with
  --  the current file name."
  btv.test.it("\\r prefills the line with the file name", function(t)
    open(t)
    t:feed("<Bslash>r")
    btv.test.expect(t:cmdline()).to_contain("Rename to: ")
    btv.test.expect(t:cmdline()).to_contain("sample.txt")
    -- The cursor sits at the end of the default, so typing appends.
    t:feed(".bak<CR>")
    btv.test.expect(settled(t, "would rename")).to_be("would rename to sample.txt.bak")
  end)

  -- "3. \\d — btv.ui.confirm: a yes/no confirmation. The command line shows
  --  'Delete this line? [Y/n]'."
  btv.test.it("\\d confirms and deletes the line", function(t)
    open(t)
    local second = t:line(2)
    t:feed("<Bslash>d")
    btv.test.expect(t:cmdline()).to_contain("Delete this line?")
    btv.test.expect(t:cmdline()).to_contain("[Y/n]")
    t:feed("y")
    btv.test.expect(settled(t, "deleted")).to_be("line deleted")
    btv.test.expect(t:line(1)).to_be(second)
    t:cmd("undo")
  end)

  -- "press <CR>, since Yes is the default"
  btv.test.it("\\d takes <CR> as Yes", function(t)
    open(t)
    local second = t:line(2)
    t:feed("<Bslash>d")
    t:feed("<CR>")
    btv.test.expect(settled(t, "deleted")).to_be("line deleted")
    btv.test.expect(t:line(1)).to_be(second)
    t:cmd("undo")
  end)

  -- "`n` or <Esc> to decline. The promise resolves to a BOOLEAN — true on Yes,
  --  false on No / cancel."
  btv.test.it("\\d declines on n and on <Esc>, leaving the line", function(t)
    open(t)
    local first = t:line(1)
    t:feed("<Bslash>d")
    t:feed("n")
    btv.test.expect(settled(t, "kept")).to_be("kept")
    btv.test.expect(t:line(1)).to_be(first)
    t:cmd("echo ''")
    t:feed("<Bslash>d")
    t:feed("<Esc>")
    btv.test.expect(settled(t, "kept")).to_be("kept")
    btv.test.expect(t:line(1)).to_be(first)
  end)

  -- "4. \\q — btv.ui.confirm defaulting to No (the safe choice on <CR>).
  --  'Quit without saving? [y/N]' — <CR> declines."
  btv.test.it("\\q defaults to No, so <CR> declines", function(t)
    open(t)
    t:feed("<Bslash>q")
    btv.test.expect(t:cmdline()).to_contain("Quit without saving?")
    btv.test.expect(t:cmdline()).to_contain("[y/N]")
    t:feed("<CR>")
    btv.test.expect(settled(t, "staying")).to_be("staying")
  end)

  -- "only one prompt at a time"
  btv.test.it("the prompt holds the command line while it is open", function(t)
    open(t)
    local line = t:line(1)
    t:feed("<Bslash>i")
    -- `x` would delete a character in the buffer; the prompt takes the key.
    t:feed("x")
    btv.test.expect(t:line(1)).to_be(line)
    btv.test.expect(t:cmdline()).to_contain("x")
    t:feed("<Esc>")
  end)
end)
