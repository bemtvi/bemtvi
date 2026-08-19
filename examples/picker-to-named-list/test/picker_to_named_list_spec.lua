-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/picker-to-named-list
--
-- The demo's claim is where results LAND — its own dock tab, jumpable, surviving
-- the window it was sent from — so the assertions are on the list's rows and on
-- where `<CR>` puts the cursor.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

local notified = {}
do
  local real_btv, real_vim = btv.notify, vim.notify
  btv.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_btv(msg, ...)
  end
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_vim(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

--- The main-area window, so a test can always get back out of a dock.
local main_win

--- Open the sample in the main area, with nothing floating.
local function open(t)
  for _ = 1, 4 do
    t:feed("<Esc>")
    t:sleep(25)
  end
  btv.layer.main()
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  main_win = vim.api.nvim_get_current_win()
end

--- Open the example's custom picker and wait for its rows.
local function marks(t)
  t:feed("<Bslash>fm")
  return t:wait_for(function()
    local m = t:menu()
    return m and #m.items > 0 and m or nil
  end, { message = "the marks picker never opened" })
end

btv.test.describe("examples/picker-to-named-list", function()
  btv.test.after_each(function()
    btv.layer.main()
  end)

  -- 1. "'qfdock' is ON by default (the bemtvi way: dock tabs)."
  btv.test.it("§1 — qfdock is on by default, and \\qd toggles it", function(t)
    open(t)
    btv.test.expect(btv.o.qfdock).to_be(true)
    t:feed("<Bslash>qd")
    btv.test.expect(btv.o.qfdock).to_be(false)
    btv.test.expect(last_notify()).to_be("qfdock = false")
    t:feed("<Bslash>qd")
    btv.test.expect(btv.o.qfdock).to_be(true)
  end)

  -- 2. "<C-q> send results to the named list `<picker>:<query>`."
  btv.test.it("§2 — <C-q> sends the matching rows to a named list", function(t)
    open(t)
    local m = marks(t)
    btv.test.expect(#m.items).to_be(4)
    t:feed("<C-q>")
    t:wait_for(function()
      return (t:lines()[1] or ""):find("the dock model", 1, true) ~= nil
    end, { message = "the named list never opened" })
    btv.test.expect(#t:lines()).to_be(4)
    btv.layer.main()
  end)

  -- "the MARKED rows if any, else all the rows currently matching your query (not
  --  every candidate)"
  btv.test.it("§2 — a query narrows what <C-q> sends", function(t)
    open(t)
    marks(t)
    t:feed("notes")
    t:wait_for(function()
      local cur = t:menu()
      return cur and #cur.items == 2
    end, { message = "the query never narrowed the picker" })
    t:feed("<C-q>")
    t:wait_for(function()
      return #t:lines() == 2
    end, { message = "the named list never opened" })
    local text = table.concat(t:lines(), "\n")
    btv.test.expect(text).to_contain("notes.txt")
    btv.test.expect(text).never.to_contain("sample.txt")
    btv.layer.main()
  end)

  -- "<Tab> mark / unmark this row (and advance) — multi-select"
  btv.test.it("§2 — <Tab> multi-selects, and <C-q> sends only the marks", function(t)
    open(t)
    marks(t)
    t:feed("<Tab><Tab>")
    t:feed("<C-q>")
    t:wait_for(function()
      return #t:lines() == 2
    end, { message = "the named list never opened with the marked rows" })
    btv.layer.main()
  end)

  -- "<CR> on an entry jumps into the MAIN editing area (the dock stays put)"
  btv.test.it("§2 — <CR> on a row jumps into the main area", function(t)
    open(t)
    marks(t)
    t:feed("<C-q>")
    t:wait_for(function()
      return (t:lines()[1] or ""):find("the dock model", 1, true) ~= nil
    end, { message = "the named list never opened" })
    -- The row's own line number, so the jump is checked against what it claims.
    local want = tonumber((t:lines()[1] or ""):match("|(%d+)"))
    btv.test.expect(want).never.to_be_nil()
    t:feed("gg<CR>")
    t:wait_for(function()
      return vim.api.nvim_get_current_win() == main_win
    end, { message = "<CR> never crossed into the main area" })
    btv.test.expect(btv.buf.name(0)).to_contain("sample.txt")
    btv.test.expect(t:cursor()[1]).to_be(want)
  end)

  -- "re-running the same search updates it in place"
  btv.test.it("§2 — re-sending the same search updates the tab in place", function(t)
    open(t)
    marks(t)
    t:feed("<C-q>")
    t:wait_for(function()
      return #t:lines() == 4
    end, { message = "the named list never opened" })
    local wins = #vim.api.nvim_list_wins()
    btv.layer.main()
    marks(t)
    t:feed("<C-q>")
    t:wait_for(function()
      return #t:lines() == 4
    end, { message = "the named list never re-opened" })
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(wins)
    btv.layer.main()
  end)

  -- 3. "\lt sends this buffer's TODO lines to the window's loclist."
  btv.test.it("§3 — \\lt sends the TODO lines to the window's loclist", function(t)
    open(t)
    t:feed("<Bslash>lt")
    t:wait_for(function()
      return (table.concat(t:lines(), "\n")):find("TODO", 1, true) ~= nil
    end, { message = "the loclist never opened" })
    -- A loclist keeps vim behavior: a split, owned by the window.
    btv.test.expect(#vim.api.nvim_list_wins() > 1).to_be(true)
    t:cmd("lclose")
    btv.layer.main()
  end)

  btv.test.it("§3 — …and says so when there is nothing to send", function(t)
    open(t)
    t:cmd("enew")
    t:feed("inothing to see<Esc>")
    t:feed("<Bslash>lt")
    btv.test.expect(last_notify()).to_be("no TODO lines in this buffer")
  end)

  -- "]l / [l step through the loclist"
  btv.test.it("§3 — ]l and [l step through the loclist", function(t)
    open(t)
    t:feed("<Bslash>lt")
    t:wait_for(function()
      return (table.concat(t:lines(), "\n")):find("TODO", 1, true) ~= nil
    end, { message = "the loclist never opened" })
    t:cmd("lclose")
    btv.layer.main()
    t:feed("gg")
    t:feed("]l")
    local first = t:cursor()[1]
    btv.test.expect(t:current_line()).to_contain("TODO")
    t:feed("]l")
    btv.test.expect(t:cursor()[1] > first).to_be(true)
    btv.test.expect(t:current_line()).to_contain("TODO")
    t:feed("[l")
    btv.test.expect(t:cursor()[1]).to_be(first)
  end)
end)
