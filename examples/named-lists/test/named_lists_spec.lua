-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/named-lists
--
-- A named list's dock tab is a real buffer, so its rows are `t:lines()` once the
-- tab is focused. Every claim in the notes — many lists side by side, storage on
-- the editor rather than a window, never colliding with the quickfix — is driven
-- through the three verbs the example uses.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- The main-area window, so a test can always get back out of a dock.
local main_win

--- Open the sample in the main area, re-reading it so each test starts the same.
local function open(t)
  btv.layer.main()
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  main_win = vim.api.nvim_get_current_win()
end

--- The rows of the currently-focused list tab.
local function rows(t)
  return t:lines()
end

btv.test.describe("examples/named-lists", function()
  -- The per-test baseline runs `enew!` in whatever window is current; never leave
  -- it inside a dock, or the next test finds the list's buffer replaced.
  btv.test.after_each(function()
    btv.layer.main()
  end)

  -- "\tl — rebuild the 'todos' named list from the live buffer and show its tab."
  btv.test.it("\\tl collects the TODO/FIXME lines and shows the list", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return (rows(t)[1] or ""):find("TODO", 1, true) ~= nil
    end, { message = "the todos list never showed" })
    local text = table.concat(rows(t), "\n")
    btv.test.expect(text).to_contain("TODO")
    btv.test.expect(text).to_contain("FIXME")
    -- Only the flagged lines are in it.
    for _, row in ipairs(rows(t)) do
      btv.test.expect(row:find("TODO") or row:find("FIXME")).never.to_be_nil()
    end
  end)

  -- "Press it again after editing the file: btv.qf.list replaces the contents in
  --  place, so the open tab repaints (no stale snapshot, no duplicate tab)."
  btv.test.it("\\tl again replaces the contents in place", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    local before = #rows(t)
    btv.layer.main()
    t:feed("GoTODO added by the spec<Esc>")
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) == before + 1
    end, { message = "the list did not repaint" })
    btv.test.expect(table.concat(rows(t), "\n")).to_contain("added by the spec")
  end)

  -- "Two named lists sit side by side as separate dock tabs."
  btv.test.it("\\ll is a second, independent list", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    btv.layer.main()
    t:feed("<Bslash>ll")
    t:wait_for(function()
      return (table.concat(rows(t), "\n")):find("cols:", 1, true) ~= nil
    end, { message = "the long-lines list never showed" })
    -- The other list is still there, unchanged — showing it again proves it.
    btv.qf.show("todos")
    t:feed("<Esc>")
    btv.test.expect(table.concat(rows(t), "\n")).to_contain("TODO")
  end)

  -- "Storage lives on the editor, not a window, so a named list survives closing
  --  any window."
  btv.test.it("a list survives closing the window that showed it", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    local before = table.concat(rows(t), "\n")
    btv.layer.main()
    t:cmd("split")
    t:cmd("only")
    -- Re-show by name: the list is still exactly what it was.
    btv.qf.show("todos")
    t:feed("<Esc>")
    btv.test.expect(table.concat(rows(t), "\n")).to_be(before)
  end)

  -- "…and never collides with the single quickfix list."
  btv.test.it("a named list is not the quickfix list", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    local named = table.concat(rows(t), "\n")
    btv.layer.main()
    -- Fill the quickfix with something else entirely.
    vim.fn.setqflist({ { filename = DIR .. "/sample.txt", lnum = 1, text = "a quickfix entry" } })
    t:feed("<Esc>")
    t:cmd("copen")
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("a quickfix entry")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("FIXME")
    t:cmd("cclose")
    -- …and the named list is untouched.
    btv.qf.show("todos")
    t:feed("<Esc>")
    btv.test.expect(table.concat(rows(t), "\n")).to_be(named)
  end)

  -- "\td — drop the 'todos' list: its dock tab closes and the list is forgotten."
  btv.test.it("\\td drops the list", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    btv.layer.main()
    t:feed("<Bslash>td")
    t:feed("<Esc>")
    -- Showing it again finds nothing to show: it was forgotten, not hidden.
    btv.qf.show("todos")
    t:feed("<Esc>")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("FIXME")
  end)

  -- "<CR> on a row jumps to the entry in the main editing layer; the dock tab
  --  stays put."
  btv.test.it("<CR> on a row jumps to the entry in the main area", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    t:feed("gg")
    local row = rows(t)[1]
    local want = tonumber(row:match("|(%d+)"))
    btv.test.expect(want).never.to_be_nil()
    t:feed("<CR>")
    t:wait_for(function()
      return vim.api.nvim_get_current_win() == main_win
    end, { message = "<CR> never crossed back to the main area" })
    btv.test.expect(t:cursor()[1]).to_be(want)
    btv.test.expect(t:current_line():find("TODO") or t:current_line():find("FIXME"))
      .never.to_be_nil()
  end)

  -- The entry shape the notes name: "filename / lnum / col / text".
  btv.test.it("each row carries the file, line and column of its entry", function(t)
    open(t)
    t:feed("<Bslash>tl")
    t:wait_for(function()
      return #rows(t) > 0
    end, { message = "the todos list never showed" })
    for _, row in ipairs(rows(t)) do
      btv.test.expect(row).to_contain("sample.txt")
      btv.test.expect(row).to_match("|%d+ col %d+|")
    end
  end)

  -- The long-lines list sets a column of its own, which the rows must carry.
  btv.test.it("the second list's own column reaches its rows", function(t)
    open(t)
    t:feed("<Bslash>ll")
    t:wait_for(function()
      return (table.concat(rows(t), "\n")):find("cols:", 1, true) ~= nil
    end, { message = "the long-lines list never showed" })
    for _, row in ipairs(rows(t)) do
      btv.test.expect(row).to_contain("col 41|")
    end
  end)
end)
