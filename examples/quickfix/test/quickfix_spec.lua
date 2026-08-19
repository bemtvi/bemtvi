-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/quickfix
--
-- The quickfix window is a real buffer, so a list's rows are `t:lines()` once it
-- is open, and a jump is the cursor. The producers that shell out (`:make`,
-- `:grep`) run this directory's own fake compiler and plain `grep`, so nothing
-- here reaches past the repo.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")
local ROOT = DIR .. "/../.."

dofile(DIR .. "/init.lua")

--- Open the sample from the repo root — the paths the fake compiler prints are
--- relative to it, exactly as the notes say.
local function open(t)
  t:cmd("cclose")
  t:cmd("lclose")
  t:cmd("cd " .. ROOT)
  t:cmd("e " .. DIR .. "/sample.c")
  t:cmd("e!")
  t:feed("gg")
end

--- The quickfix window's rows.
local function qf_rows(t)
  return t:lines()
end

btv.test.describe("examples/quickfix", function()
  btv.test.it("the config sets the three tool options the tour uses", function(t)
    open(t)
    t:cmd("set errorformat?")
    btv.test.expect(t:message()).to_contain("%f:%l:%c:")
    t:cmd("set makeprg?")
    btv.test.expect(t:message()).to_contain("fakecc.sh")
    t:cmd("set grepprg?")
    btv.test.expect(t:message()).to_contain("grep -n")
  end)

  -- ":make — it prints three gcc-style diagnostics against sample.c; bemtvi parses
  --  them with 'errorformat', opens the quickfix window, and jumps to the first
  --  error (line 9)."
  btv.test.it(":make parses the compiler output and jumps to the first error", function(t)
    open(t)
    t:cmd("make")
    t:wait_for(function()
      return #btv.qf.getqflist() >= 3
    end, { tries = 300, interval = 20, message = ":make produced no entries" })
    local list = btv.qf.getqflist()
    btv.test.expect(#list).to_be(3)
    btv.test.expect(list[1].lnum).to_be(9)
    -- `%t` takes the first letter of the kind word as written — `error` gives `e`.
    btv.test.expect(list[1].type).to_be("e")
    btv.test.expect(list[1].text).to_contain("expected ';'")
    btv.test.expect(list[2].type).to_be("w")
    btv.test.expect(list[3].lnum).to_be(15)
    -- The window opened…
    t:wait_for(function()
      return btv.bo.filetype == "qf" or t:cursor()[1] == 9
    end, { message = ":make opened no quickfix window and did not jump" })
  end)

  -- ":cnext / :cprev step through the errors."
  btv.test.it(":cnext and :cprev step through the entries", function(t)
    open(t)
    t:cmd("make")
    t:wait_for(function()
      return #btv.qf.getqflist() >= 3
    end, { tries = 300, interval = 20, message = ":make produced no entries" })
    t:cmd("cclose")
    t:cmd("cfirst")
    local first = t:cursor()[1]
    t:cmd("cnext")
    btv.test.expect(t:cursor()[1] > first).to_be(true)
    t:cmd("cprev")
    btv.test.expect(t:cursor()[1]).to_be(first)
  end)

  -- ":copen / :cclose toggle it."
  btv.test.it(":copen and :cclose toggle the window", function(t)
    open(t)
    t:cmd("make")
    t:wait_for(function()
      return #btv.qf.getqflist() >= 3
    end, { tries = 300, interval = 20, message = ":make produced no entries" })
    t:cmd("cclose")
    local closed = #vim.api.nvim_list_wins()
    t:cmd("copen")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(closed + 1)
    btv.test.expect(btv.bo.filetype).to_be("qf")
    btv.test.expect(#qf_rows(t)).to_be(3)
    t:cmd("cclose")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(closed)
  end)

  -- ":vimgrep /TODO/ % — the in-process searcher (no external process)."
  btv.test.it(":vimgrep fills a list without a subprocess", function(t)
    open(t)
    t:cmd("vimgrep /TODO/ %")
    t:wait_for(function()
      return #btv.qf.getqflist() > 0
    end, { message = ":vimgrep produced no entries" })
    for _, entry in ipairs(btv.qf.getqflist()) do
      btv.test.expect(entry.text).to_contain("TODO")
    end
    t:cmd("cclose")
  end)

  -- ":colder walks back to the previous list; :cnewer walks forward again."
  btv.test.it(":colder and :cnewer walk the list history", function(t)
    open(t)
    t:cmd("make")
    t:wait_for(function()
      return #btv.qf.getqflist() == 3
    end, { tries = 300, interval = 20, message = ":make produced no entries" })
    t:cmd("cclose")
    t:cmd("vimgrep /TODO/ %")
    t:wait_for(function()
      local list = btv.qf.getqflist()
      return #list > 0 and (list[1].text or ""):find("TODO", 1, true) ~= nil
    end, { message = ":vimgrep produced no entries" })
    local grep_count = #btv.qf.getqflist()
    t:cmd("cclose")
    t:cmd("colder")
    btv.test.expect(#btv.qf.getqflist()).to_be(3)
    btv.test.expect(btv.qf.getqflist()[1].text).to_contain("expected ';'")
    t:cmd("cnewer")
    btv.test.expect(#btv.qf.getqflist()).to_be(grep_count)
    t:cmd("cclose")
  end)

  -- "<leader>q populate + open the quickfix list from a Lua table via btv.qf."
  btv.test.it("\\q builds a list from Lua and opens it", function(t)
    open(t)
    t:feed("<Bslash>q")
    t:wait_for(function()
      return btv.bo.filetype == "qf"
    end, { message = "\\q opened no quickfix window" })
    btv.test.expect(#qf_rows(t)).to_be(3)
    local text = table.concat(qf_rows(t), "\n")
    btv.test.expect(text).to_contain("missing ';'")
    btv.test.expect(text).to_contain("typo: totl")
    t:cmd("cclose")
  end)

  -- "<CR> in the quickfix window jumps to the entry under the cursor."
  btv.test.it("<CR> in the quickfix window jumps to the entry", function(t)
    open(t)
    t:feed("<Bslash>q")
    t:wait_for(function()
      return btv.bo.filetype == "qf"
    end, { message = "\\q opened no quickfix window" })
    t:feed("gg<CR>")
    t:wait_for(function()
      return btv.bo.filetype ~= "qf"
    end, { message = "<CR> never left the quickfix window" })
    btv.test.expect(btv.buf.name(0)).to_contain("sample.c")
    btv.test.expect(t:cursor()[1]).to_be(9)
  end)

  -- ":LDiag fill this window's LOCATION list … Location lists are per-window."
  btv.test.it(":LDiag fills this window's location list", function(t)
    open(t)
    local owner = btv.win.current()
    local wins = #vim.api.nvim_list_wins()
    t:cmd("LDiag")
    t:wait_for(function()
      return btv.bo.filetype == "qf"
    end, { message = ":LDiag opened no location window" })
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(wins + 1)
    -- The list belongs to the window `:LDiag` ran in, not to the window that
    -- displays it — so it is read back by that window's id.
    local list = vim.fn.getloclist(owner)
    btv.test.expect(#list).to_be(2)
    btv.test.expect(list[1].text).to_contain("this window's note")
    btv.test.expect(list[2].text).to_contain("another note")
    -- The display window has none of its own.
    btv.test.expect(#vim.fn.getloclist(0)).to_be(0)
    t:cmd("lclose")
  end)

  btv.test.it("a location list belongs to its window, not to the editor", function(t)
    open(t)
    local owner = btv.win.current()
    t:cmd("LDiag")
    t:wait_for(function()
      return btv.bo.filetype == "qf"
    end, { message = ":LDiag opened no location window" })
    btv.test.expect(#vim.fn.getloclist(owner)).to_be(2)
    -- A window opened fresh has no location list of its own: the list belongs to
    -- the window, not to the editor.
    t:cmd("tabnew")
    btv.test.expect(#vim.fn.getloclist(0)).to_be(0)
    t:cmd("tabclose")
    t:cmd("lclose")
  end)

  -- ":lnext / :lprev navigate the location list."
  btv.test.it(":lnext and :lprev navigate the location list", function(t)
    open(t)
    t:cmd("LDiag")
    t:wait_for(function()
      return btv.bo.filetype == "qf"
    end, { message = ":LDiag opened no location window" })
    -- `:lclose` puts focus back on the window that owns the list; the location
    -- window itself carries no list, so navigating has to happen from the owner.
    t:cmd("lclose")
    -- `:ll 1` jumps to the first entry; the list's own commands walk from there.
    t:cmd("ll 1")
    btv.test.expect(t:cursor()[1]).to_be(14)
    t:cmd("lnext")
    btv.test.expect(t:cursor()[1]).to_be(15)
    t:cmd("lprev")
    btv.test.expect(t:cursor()[1]).to_be(14)
  end)

  -- ":lopen / :lclose toggle its window."
  btv.test.it(":lopen and :lclose toggle the location window", function(t)
    open(t)
    t:cmd("LDiag")
    t:wait_for(function()
      return btv.bo.filetype == "qf"
    end, { message = ":LDiag opened no location window" })
    local open_count = #vim.api.nvim_list_wins()
    t:cmd("lclose")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(open_count - 1)
    btv.test.expect(btv.bo.filetype).never.to_be("qf")
    t:cmd("lopen")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(open_count)
    btv.test.expect(#t:lines()).to_be(2)
    t:cmd("lclose")
  end)
end)
