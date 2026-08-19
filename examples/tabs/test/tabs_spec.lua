-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/tabs
--
-- The lifecycle log the config keeps (`_G.tab_log`) is the observable for the
-- autocmd ordering; everything else is the `nvim_tabpage_*` read surface and the
-- painted tab strip (`t:tabs("main")`).

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- One tab on the sample, with a fresh lifecycle log. The per-test baseline
--- restores options and buffers, not the tab stack, so any leftover tab is closed.
local function open(t)
  while #vim.api.nvim_list_tabpages() > 1 do
    t:cmd("tabclose")
  end
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set showtabline=2")
  t:feed("gg")
  _G.tab_log = {}
end

--- Everything the log has recorded since `open`, joined.
local function log()
  return table.concat(_G.tab_log, " ")
end

btv.test.describe("examples/tabs", function()
  btv.test.it("the config always shows the tab bar", function(t)
    open(t)
    btv.test.expect(btv.o.showtabline).to_be(2)
    btv.test.expect(#t:tabs("main").labels).to_be(1)
  end)

  -- ":tabnew / :tabedit FILE — open a new tab"
  btv.test.it(":tabnew opens a tab and the bar lists both", function(t)
    open(t)
    t:cmd("tabnew")
    btv.test.expect(#vim.api.nvim_list_tabpages()).to_be(2)
    btv.test.expect(#t:tabs("main").labels).to_be(2)
    btv.test.expect(t:tabs("main").current).to_be(2)
    t:cmd("tabclose")
  end)

  -- "every tab create, switch, and close announces itself through the tab autocmds
  --  … ordered … as TabLeave -> WinLeave -> ... -> WinEnter -> TabEnter"
  btv.test.it("a new tab fires TabNew, TabLeave and TabEnter, in that order", function(t)
    open(t)
    t:cmd("tabnew")
    local fired = log()
    btv.test.expect(fired).to_contain("TabNew")
    btv.test.expect(fired).to_contain("TabLeave")
    btv.test.expect(fired).to_contain("TabEnter")
    -- The leave of the old tab precedes the enter of the new one.
    btv.test.expect(fired:find("TabLeave") < fired:find("TabEnter")).to_be(true)
    t:cmd("tabclose")
  end)

  btv.test.it("closing a tab fires TabClosed", function(t)
    open(t)
    t:cmd("tabnew")
    _G.tab_log = {}
    t:cmd("tabclose")
    btv.test.expect(log()).to_contain("TabClosed")
    btv.test.expect(#vim.api.nvim_list_tabpages()).to_be(1)
  end)

  -- "gt / gT — next / previous tab; {count}gt jumps to tab N"
  btv.test.it("gt, gT and {count}gt walk the tabs", function(t)
    open(t)
    t:cmd("tabnew")
    t:cmd("tabnew")
    btv.test.expect(vim.fn.tabpagenr()).to_be(3)
    t:feed("gt")
    btv.test.expect(vim.fn.tabpagenr()).to_be(1)
    t:feed("gT")
    btv.test.expect(vim.fn.tabpagenr()).to_be(3)
    t:feed("2gt")
    btv.test.expect(vim.fn.tabpagenr()).to_be(2)
    t:cmd("tabonly")
  end)

  -- ":tabnext / :tabprevious / :tablast / :tabfirst"
  btv.test.it("the :tab* navigation commands agree with gt", function(t)
    open(t)
    t:cmd("tabnew")
    t:cmd("tabnew")
    t:cmd("tabfirst")
    btv.test.expect(vim.fn.tabpagenr()).to_be(1)
    t:cmd("tabnext")
    btv.test.expect(vim.fn.tabpagenr()).to_be(2)
    t:cmd("tabprevious")
    btv.test.expect(vim.fn.tabpagenr()).to_be(1)
    t:cmd("tablast")
    btv.test.expect(vim.fn.tabpagenr()).to_be(3)
    t:cmd("tabonly")
  end)

  -- "<C-w>T — move the focused window to its own new tab"
  btv.test.it("<C-w>T sends the focused window to its own tab", function(t)
    open(t)
    t:cmd("vsplit")
    btv.test.expect(#vim.api.nvim_tabpage_list_wins(vim.api.nvim_get_current_tabpage())).to_be(2)
    t:feed("<C-w>T")
    btv.test.expect(#vim.api.nvim_list_tabpages()).to_be(2)
    btv.test.expect(#vim.api.nvim_tabpage_list_wins(vim.api.nvim_get_current_tabpage())).to_be(1)
    t:cmd("tabclose")
    t:cmd("only")
  end)

  -- ":tab split — clone the current buffer + cursor into a new tab"
  btv.test.it(":tab split clones the buffer and cursor into a new tab", function(t)
    open(t)
    t:feed("5G")
    t:cmd("tab split")
    btv.test.expect(#vim.api.nvim_list_tabpages()).to_be(2)
    btv.test.expect(btv.buf.name(0)).to_contain("sample.txt")
    btv.test.expect(t:cursor()[1]).to_be(5)
    t:cmd("tabclose")
  end)

  -- ":drop FILE — jump to a window already showing FILE (in any tab), else :edit"
  btv.test.it(":drop jumps to the tab already showing the file", function(t)
    open(t)
    local home = vim.api.nvim_get_current_tabpage()
    t:cmd("tabnew")
    btv.test.expect(vim.api.nvim_get_current_tabpage()).never.to_be(home)
    t:cmd("drop " .. DIR .. "/sample.txt")
    btv.test.expect(vim.api.nvim_get_current_tabpage()).to_be(home)
    t:cmd("tabonly")
  end)

  -- ":tabclose — close the current tab (refuses the last one)"
  btv.test.it(":tabclose refuses the last tab", function(t)
    open(t)
    t:cmd("tabclose")
    btv.test.expect(#vim.api.nvim_list_tabpages()).to_be(1)
    btv.test.expect(t:message()).to_contain("E784")
  end)

  -- ":tabonly — close every tab but this one"
  btv.test.it(":tabonly leaves one tab standing", function(t)
    open(t)
    t:cmd("tabnew")
    t:cmd("tabnew")
    t:cmd("tabonly")
    btv.test.expect(#vim.api.nvim_list_tabpages()).to_be(1)
  end)

  -- ":TabList — lists every tab page with its 1-based number, window count, and
  --  focused window"
  btv.test.it(":TabList reports the live tab table", function(t)
    open(t)
    t:cmd("tabnew")
    local got
    local prev_vim, prev_btv = vim.notify, btv.notify
    vim.notify = function(msg)
      got = tostring(msg)
    end
    btv.notify = vim.notify
    t:cmd("TabList")
    vim.notify, btv.notify = prev_vim, prev_btv
    btv.test.expect(got).to_contain("tab 1 (id")
    btv.test.expect(got).to_contain("tab 2 (id")
    btv.test.expect(got).to_contain("(current)")
    btv.test.expect(got).to_contain("wins=1")
    t:cmd("tabclose")
  end)

  -- ":TabFirst — jumps to the first tab via nvim_set_current_tabpage"
  btv.test.it(":TabFirst jumps to the first tab from Lua", function(t)
    open(t)
    t:cmd("tabnew")
    t:cmd("tabnew")
    btv.test.expect(vim.fn.tabpagenr()).to_be(3)
    t:cmd("TabFirst")
    btv.test.expect(vim.fn.tabpagenr()).to_be(1)
    t:cmd("tabonly")
  end)

  -- ":set showtabline=0/1/2 — never / only-with-2+ / always"
  btv.test.it("'showtabline' decides whether the bar is drawn", function(t)
    open(t)
    t:cmd("set showtabline=0")
    btv.test.expect(#t:tabs("main").labels).to_be(0)
    t:cmd("set showtabline=1")
    btv.test.expect(#t:tabs("main").labels).to_be(0)
    t:cmd("tabnew")
    btv.test.expect(#t:tabs("main").labels).to_be(2)
    t:cmd("tabclose")
    t:cmd("set showtabline=2")
    btv.test.expect(#t:tabs("main").labels).to_be(1)
  end)
end)
