-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-select
--
-- The widget floats over the window and grabs every key, so it is in none of the
-- buffer views — `t:menu()` is what sees the rows and which one leads. Each case
-- drives it with the very keys the notes name.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Open the menu the mapping `keys` fires, and wait for it.
local function menu(t, keys)
  t:feed(keys)
  t:wait_for(function()
    return t:menu() ~= nil
  end, { message = keys .. " opened no menu" })
  return t:menu()
end

btv.test.describe("examples/ui-select", function()
  -- "1. \\p — a plain string chooser. A menu of three fruits floats under the cursor."
  btv.test.it("\\p offers the three fruits", function(t)
    open(t)
    local box = menu(t, "<Bslash>p")
    btv.test.expect(box.items).to_equal({ "apple", "banana", "cherry" })
    t:feed("<Esc>")
  end)

  -- "The menu opens NOSELECT … nothing is highlighted until you move … The first
  --  j / k reveals the highlight at the first row."
  btv.test.it("the menu opens with nothing selected", function(t)
    open(t)
    local box = menu(t, "<Bslash>p")
    btv.test.expect(box.selected).to_be_nil()
    t:feed("j")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("j")
    btv.test.expect(t:menu().selected).to_be(2)
    t:feed("k")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<Esc>")
  end)

  -- "press <CR>. The chosen fruit is echoed"
  btv.test.it("<CR> resolves the promise with the highlighted row", function(t)
    open(t)
    menu(t, "<Bslash>p")
    t:feed("jj<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("picked", 1, true) ~= nil
    end, { message = "nothing was picked" })
    btv.test.expect(t:message()).to_be("picked banana")
  end)

  -- "the promise resolves to nil on <Esc>, so you'll see the cancel branch fire"
  btv.test.it("<Esc> cancels and the promise resolves to nil", function(t)
    open(t)
    menu(t, "<Bslash>p")
    t:feed("<Esc>")
    t:wait_for(function()
      return (t:message() or ""):find("cancel", 1, true) ~= nil
    end, { message = "the cancel branch never fired" })
    btv.test.expect(t:message()).to_contain("nothing picked (cancelled)")
    btv.test.expect(t:menu()).to_be_nil()
  end)

  btv.test.it("q also cancels", function(t)
    open(t)
    menu(t, "<Bslash>p")
    t:feed("q")
    t:wait_for(function()
      return t:menu() == nil
    end, { message = "q left the menu up" })
  end)

  -- "the alternative navigation keys: <C-n> / <C-p>, arrows, gg / G"
  btv.test.it("<C-n>/<C-p>, the arrows and gg/G all navigate", function(t)
    open(t)
    menu(t, "<Bslash>p")
    t:feed("<C-n>")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<Down>")
    btv.test.expect(t:menu().selected).to_be(2)
    t:feed("<C-p>")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("G")
    btv.test.expect(t:menu().selected).to_be(3)
    t:feed("gg")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<Esc>")
  end)

  -- "2. \\c — choose a command to run … format_item renders the display label; the
  --  promise still resolves to the ORIGINAL table."
  btv.test.it("\\c shows the labels and runs the chosen command", function(t)
    open(t)
    local box = menu(t, "<Bslash>c")
    btv.test.expect(box.items).to_equal({ "Save file", "Split window", "Show messages" })
    -- Pick "Split window": the entry's own `cmd` round-tripped through the promise.
    local before = #vim.api.nvim_list_wins()
    t:feed("jj<CR>")
    t:wait_for(function()
      return #vim.api.nvim_list_wins() == before + 1
    end, { message = "the chosen command never ran" })
    t:cmd("only")
  end)

  -- "3. A long list scrolls, AWAITED linearly. \\n opens twenty entries."
  btv.test.it("\\n awaits its choice inside btv.async", function(t)
    open(t)
    local box = menu(t, "<Bslash>n")
    btv.test.expect(#box.items).to_be(20)
    btv.test.expect(box.items[1]).to_be("line 1")
    btv.test.expect(box.items[20]).to_be("line 20")
    -- "the box caps its height" — twenty rows do not become twenty screen rows.
    btv.test.expect(box.height < 20).to_be(true)
    t:feed("G<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("chose", 1, true) ~= nil
    end, { message = "the awaited choice never landed" })
    btv.test.expect(t:message()).to_be("chose line 20")
  end)

  btv.test.it("the menu grabs every key while it is up", function(t)
    open(t)
    local line = t:line(1)
    menu(t, "<Bslash>p")
    -- `x` would delete a character in the buffer; the menu swallows it.
    t:feed("x")
    btv.test.expect(t:line(1)).to_be(line)
    t:feed("<Esc>")
  end)
end)
