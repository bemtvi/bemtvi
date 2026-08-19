-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/statusline
--
-- `t:statusline()` is the rendered bar — the text a client paints, with the
-- `%#Group#` switches already resolved away — so a case asserts on the line a
-- reader would actually read, not on the format string the builder returned.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set laststatus=2")
  t:feed("gg")
end

btv.test.describe("examples/statusline", function()
  -- "vim.o.statusline = '%!v:lua.statusline()'"
  btv.test.it("the config points 'statusline' at the Lua builder", function(t)
    open(t)
    btv.test.expect(btv.o.statusline).to_be("%!v:lua.statusline()")
  end)

  btv.test.it("the bar carries every block the notes describe", function(t)
    open(t)
    local bar = t:statusline()
    btv.test.expect(bar).to_contain("NORMAL")
    btv.test.expect(bar).to_contain("sample.txt")
    btv.test.expect(bar).to_contain("1:1")
    -- `%%` rendered as a literal percent sign, next to the through-file figure.
    btv.test.expect(bar).to_match("%d+%%")
  end)

  -- "i / v / V / R then <Esc> — the mode block recolours and relabels"
  btv.test.it("the mode block follows the mode", function(t)
    open(t)
    t:feed("i")
    btv.test.expect(t:statusline()).to_contain("INSERT")
    t:feed("<Esc>")
    btv.test.expect(t:statusline()).to_contain("NORMAL")
    t:feed("v")
    btv.test.expect(t:statusline()).to_contain("VISUAL")
    t:feed("<Esc>")
    t:feed("V")
    btv.test.expect(t:statusline()).to_contain("V-LINE")
    t:feed("<Esc>")
    t:feed("R")
    btv.test.expect(t:statusline()).to_contain("REPLACE")
    t:feed("<Esc>")
  end)

  -- "move around (hjkl, w, G) — the ruler block updates live every redraw"
  btv.test.it("the ruler tracks the cursor and the way through the file", function(t)
    open(t)
    btv.test.expect(t:statusline()).to_contain("1:1")
    -- Line 3 has text to move along; line 2 is blank.
    t:feed("jjll")
    btv.test.expect(t:statusline()).to_contain("3:3")
    t:feed("G")
    btv.test.expect(t:statusline()).to_contain("100%")
  end)

  -- "edit the buffer (x, dd, i…) — a [+] modified flag appears next to the name"
  btv.test.it("%m flags the buffer once it is modified", function(t)
    open(t)
    btv.test.expect(t:statusline()).never.to_contain("[+]")
    t:feed("x")
    btv.test.expect(t:statusline()).to_contain("sample.txt[+]")
    t:cmd("undo")
  end)

  -- ":set fileencoding=latin1 — the %{&fileencoding} block switches"
  btv.test.it("the pure %{&option} block reads the buffer option", function(t)
    open(t)
    btv.test.expect(t:statusline()).to_contain("utf-8")
    t:cmd("set fileencoding=latin1")
    btv.test.expect(t:statusline()).to_contain("latin1")
    btv.test.expect(t:statusline()).never.to_contain("utf-8")
  end)

  -- ":set bomb — a '[bom]' tag appears via the %{&bomb?…} ternary"
  btv.test.it("the ternary tags a byte-order mark", function(t)
    open(t)
    btv.test.expect(t:statusline()).never.to_contain("[bom]")
    t:cmd("set bomb")
    btv.test.expect(t:statusline()).to_contain("[bom]")
    t:cmd("set nobomb")
    btv.test.expect(t:statusline()).never.to_contain("[bom]")
  end)

  -- ":e examples/tabs/sample.txt — the file block follows the current buffer"
  btv.test.it("the file block follows the current buffer", function(t)
    open(t)
    btv.test.expect(t:statusline()).to_contain("sample.txt")
    t:cmd("enew")
    btv.test.expect(t:statusline()).to_contain("[No Name]")
  end)

  -- "click the filename block — a `%@v:lua.…@…%X` click region echoes the path"
  btv.test.it("clicking the file block runs its handler", function(t)
    open(t)
    local bar = t:statusline()
    local at = bar:find("sample.txt", 1, true)
    btv.test.expect(at).never.to_be(nil)
    -- The status row is the last row of the window area; click the name's cell.
    local row = #t:screen()
    t:mouse("left", "press", row, at)
    t:mouse("left", "release", row, at)
    btv.test.expect(t:message()).to_contain("clicked")
    btv.test.expect(t:message()).to_contain("sample.txt")
  end)

  -- ":set statusline= — fall back to bemtvi's built-in default look"
  btv.test.it("clearing the option falls back to the built-in bar", function(t)
    open(t)
    t:cmd("set statusline=")
    local bar = t:statusline()
    -- The built-in bar has its own mode block and name; what tells them apart is
    -- the ruler, which the built-in spells vim's way (`1,1`) and the config's
    -- builder spells `1:1` with a through-file percentage after it.
    btv.test.expect(bar).to_contain("1,1")
    btv.test.expect(bar).never.to_contain("1:1")
    btv.test.expect(bar).to_contain("sample.txt")
  end)
end)
