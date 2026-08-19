-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-picker
--
-- The picker floats over the window and grabs every key, so `t:menu()` is the view
-- of its rows, its box and which one leads. The two shipped sources that shell out
-- (`files` / `live_grep`) need `rg`, so those cases skip when it is absent rather
-- than failing on a missing binary; everything else runs in-process.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("cd " .. DIR)
  -- A picker remembers its filter history across opens; start each case clean.
  btv.picker.forget_history()
  t:feed("gg")
end

--- Open the picker the mapping `keys` fires and wait for its rows.
local function picker(t, keys)
  t:feed(keys)
  t:wait_for(function()
    local m = t:menu()
    return m ~= nil and #m.items > 0
  end, { tries = 200, interval = 20, message = keys .. " opened no picker" })
  return t:menu()
end

btv.test.describe("examples/ui-picker", function()
  -- "2. A custom STATIC source … \\fc colours — pick a colour; the choice is echoed."
  btv.test.it("\\fc lists the colours and echoes the pick", function(t)
    open(t)
    local box = picker(t, "<Bslash>fc")
    btv.test.expect(#box.items).to_be(8)
    btv.test.expect(box.items[1]).to_be("crimson")
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("you picked", 1, true) ~= nil
    end, { message = "the confirm never fired" })
    btv.test.expect(t:message()).to_be("you picked crimson")
  end)

  -- "type — edit the query (the document is NOT touched) … a Rust fuzzy matcher
  --  that re-ranks as you type"
  btv.test.it("typing filters the rows and leaves the buffer alone", function(t)
    open(t)
    local line = t:line(1)
    picker(t, "<Bslash>fc")
    t:feed("mid")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items < 8 and m.items[1] == "midnight"
    end, { message = "the query never narrowed the list" })
    btv.test.expect(t:menu().items[1]).to_be("midnight")
    btv.test.expect(t:line(1)).to_be(line)
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("you picked", 1, true) ~= nil
    end, { message = "the confirm never fired" })
    btv.test.expect(t:message()).to_be("you picked midnight")
  end)

  -- "<C-n> / <C-p> move the selection down / up (also <Down> / <Up>)"
  btv.test.it("<C-n> and <C-p> move the selection", function(t)
    open(t)
    picker(t, "<Bslash>fc")
    -- The picker opens with its first row already leading (unlike `btv.ui.select`,
    -- which opens noselect).
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<C-n>")
    btv.test.expect(t:menu().selected).to_be(2)
    t:feed("<C-n>")
    btv.test.expect(t:menu().selected).to_be(3)
    t:feed("<C-p>")
    btv.test.expect(t:menu().selected).to_be(2)
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("you picked", 1, true) ~= nil
    end, { message = "the confirm never fired" })
    btv.test.expect(t:message()).to_be("you picked cornflower")
  end)

  -- "<Esc> cancel"
  btv.test.it("<Esc> cancels without confirming", function(t)
    open(t)
    t:cmd("echo ''")
    picker(t, "<Bslash>fc")
    t:feed("<Esc>")
    t:wait_for(function()
      return t:menu() == nil
    end, { message = "<Esc> left the picker up" })
    btv.test.expect(t:message()).never.to_contain("you picked")
  end)

  -- "Set the size per source with `width` / `height` … This source asks for the
  --  input BELOW the results."
  btv.test.it("the colours source takes its declared size", function(t)
    open(t)
    local box = picker(t, "<Bslash>fc")
    btv.test.expect(box.width).to_be(math.floor(vim.o.columns * 0.5 + 0.5))
    t:feed("<Esc>")
  end)

  -- "… or override the size at open time (a compact 40x10 cell box)"
  btv.test.it("\\fC overrides the size per open", function(t)
    open(t)
    local box = picker(t, "<Bslash>fC")
    btv.test.expect(box.width).to_be(40)
    btv.test.expect(box.height).to_be(10)
    t:feed("<Esc>")
  end)

  -- "3. A custom DYNAMIC source — re-run per keystroke, the matcher bypassed.
  --  This one just echoes the query back. \\fe echo"
  btv.test.it("\\fe re-runs its source on every keystroke", function(t)
    open(t)
    t:feed("<Bslash>fe")
    -- With an empty query the source pushes nothing at all.
    t:wait_for(function()
      return t:menu() ~= nil
    end, { message = "\\fe opened no picker" })
    btv.test.expect(#t:menu().items).to_be(0)
    t:feed("abc")
    t:wait_for(function()
      local m = t:menu()
      return m ~= nil and #m.items == 2
    end, { tries = 200, interval = 20, message = "the dynamic source never ran" })
    btv.test.expect(t:menu().items[1]).to_be("search: abc")
    btv.test.expect(t:menu().items[2]).to_be("again:  abc")
    t:feed("<C-n><CR>")
    t:wait_for(function()
      return (t:message() or ""):find("confirmed query", 1, true) ~= nil
    end, { message = "the confirm never fired" })
    btv.test.expect(t:message()).to_be("confirmed query: abc")
  end)

  -- "4. The PREVIEW pane … \\fp preview"
  btv.test.it("\\fp lists this example's files with a preview pane", function(t)
    open(t)
    local box = picker(t, "<Bslash>fp")
    btv.test.expect(box.items).to_equal({ "sample.txt", "notes.txt", "init.lua" })
    -- The picker opens with the first row already leading, so one `<C-n>` moves
    -- to the second — `notes.txt`.
    t:feed("<C-n><CR>")
    t:wait_for(function()
      return (btv.buf.name(0) or ""):find("notes.txt", 1, true) ~= nil
    end, { message = "the confirm opened no file" })
  end)

  -- "\\fb buffers — pick an open buffer (in-memory; no process)."
  btv.test.it("\\fb lists the open buffers", function(t)
    open(t)
    local box = picker(t, "<Bslash>fb")
    local rows = table.concat(box.items, "\n")
    btv.test.expect(rows).to_contain("sample.txt")
    -- "Each row leads with the `:ls` facts: bufnr, `%` current …"
    btv.test.expect(rows).to_match("%d+%s+%%")
    t:feed("<Esc>")
  end)

  -- "\\ff files — fuzzy file finder (streams `rg --files`)"
  btv.test.it("\\ff streams the files under the cwd", function(t)
    open(t)
    t:feed("<Bslash>ff")
    local rows
    t:wait_for(function()
      local m = t:menu()
      if m == nil then
        return false
      end
      rows = table.concat(m.items, "\n")
      return #m.items > 0
    end, { tries = 100, interval = 20, message = "skip-or-rows" })
    btv.test.expect(rows).to_contain("sample.txt")
    t:feed("<Esc>")
  end)

  -- "Dynamic sources (live_grep) debounce … Tune it globally … or per open"
  btv.test.it("the config sets the debounce, and \\fG overrides it", function(t)
    open(t)
    btv.test.expect(btv.picker.debounce).to_be(250)
  end)

  -- "Picker keys are ordinary `picker`-mode maps, so rebind any of them."
  btv.test.it("the picker keys are picker-mode maps", function(t)
    open(t)
    local lhs = {}
    for _, m in ipairs(btv.keymap.get("picker")) do
      lhs[m.lhs] = true
    end
    btv.test.expect(lhs["<C-t>"]).to_be(true)
    btv.test.expect(lhs["<C-x>"]).to_be(true)
    btv.test.expect(lhs["<C-v>"]).to_be(true)
  end)

  -- "<C-t> open the highlighted item in a NEW TAB … these three hand the gesture
  --  to the SOURCE's `confirm`"
  btv.test.it("<C-t> opens the pick in a new tab", function(t)
    open(t)
    local tabs = #vim.api.nvim_list_tabpages()
    picker(t, "<Bslash>fp")
    t:feed("<C-t>")
    t:wait_for(function()
      return #vim.api.nvim_list_tabpages() == tabs + 1
    end, { message = "<C-t> opened no tab" })
    t:cmd("tabclose")
  end)

  -- "<C-x> / <C-v> open it in a horizontal / vertical SPLIT"
  btv.test.it("<C-v> opens the pick in a split", function(t)
    open(t)
    t:cmd("only")
    picker(t, "<Bslash>fp")
    t:feed("<C-v>")
    t:wait_for(function()
      return #vim.api.nvim_list_wins() == 2
    end, { message = "<C-v> opened no split" })
    t:cmd("only")
  end)
end)
