-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/windows
--
-- The lifecycle log the config keeps (`_G.win_log`) is the observable for the
-- autocmds; the rest is the `nvim_win_*` read surface and the two commands.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- One window on the sample, with a fresh lifecycle log. The per-test baseline
--- restores options and buffers, not the layout, so extra windows are closed.
local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  _G.win_log = {}
end

local function log()
  return table.concat(_G.win_log, " ")
end

--- Whatever the next `vim.notify` is handed while `body` runs.
local function notified(body)
  local got
  local prev_vim, prev_btv = vim.notify, btv.notify
  local record = function(msg)
    got = tostring(msg)
  end
  vim.notify, btv.notify = record, record
  local ok, err = pcall(body)
  vim.notify, btv.notify = prev_vim, prev_btv
  if not ok then
    error(err, 0)
  end
  return got
end

btv.test.describe("examples/windows", function()
  btv.test.it("the config turns on the hybrid number gutter", function(t)
    open(t)
    btv.test.expect(btv.wo.number).to_be(true)
    btv.test.expect(btv.wo.relativenumber).to_be(true)
  end)

  -- "<C-w>s / <C-w>v — split the focused window"
  btv.test.it("<C-w>s and <C-w>v split, and each fires WinNew", function(t)
    open(t)
    t:feed("<C-w>s")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(2)
    btv.test.expect(log()).to_contain("WinNew")
    t:feed("<C-w>v")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(3)
    t:cmd("only")
  end)

  -- "ordered WinLeave -> (buffer events) -> WinEnter around a focus change"
  btv.test.it("a focus move fires WinLeave then WinEnter", function(t)
    open(t)
    t:cmd("split")
    _G.win_log = {}
    t:feed("<C-w>w")
    local fired = log()
    btv.test.expect(fired).to_contain("WinLeave")
    btv.test.expect(fired).to_contain("WinEnter")
    btv.test.expect(fired:find("WinLeave") < fired:find("WinEnter")).to_be(true)
    t:cmd("only")
  end)

  btv.test.it("closing a window fires WinClosed", function(t)
    open(t)
    t:cmd("split")
    _G.win_log = {}
    t:feed("<C-w>c")
    btv.test.expect(log()).to_contain("WinClosed")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(1)
  end)

  -- "<C-w>h/j/k/l — move focus by direction; <C-w>w cycles"
  btv.test.it("<C-w>h and <C-w>l move focus by direction", function(t)
    open(t)
    local first = vim.api.nvim_get_current_win()
    t:feed("<C-w>v")
    local second = vim.api.nvim_get_current_win()
    btv.test.expect(second).never.to_be(first)
    t:feed("<C-w>l")
    btv.test.expect(vim.api.nvim_get_current_win()).to_be(first)
    t:feed("<C-w>h")
    btv.test.expect(vim.api.nvim_get_current_win()).to_be(second)
    t:cmd("only")
  end)

  -- "<C-w>+ <C-w>- grow / shrink height; <C-w>< <C-w>> width (take a count!)"
  btv.test.it("<C-w>+ and <C-w>- resize, and take a count", function(t)
    open(t)
    t:cmd("split")
    local win = vim.api.nvim_get_current_win()
    local tall = vim.api.nvim_win_get_height(win)
    t:feed("3<C-w>+")
    btv.test.expect(vim.api.nvim_win_get_height(win)).to_be(tall + 3)
    t:feed("3<C-w>-")
    btv.test.expect(vim.api.nvim_win_get_height(win)).to_be(tall)
    t:cmd("only")
  end)

  btv.test.it("<C-w>> and <C-w>< resize the width", function(t)
    open(t)
    t:cmd("vsplit")
    local win = vim.api.nvim_get_current_win()
    local wide = vim.api.nvim_win_get_width(win)
    t:feed("4<C-w>>")
    btv.test.expect(vim.api.nvim_win_get_width(win)).to_be(wide + 4)
    t:feed("4<C-w><")
    btv.test.expect(vim.api.nvim_win_get_width(win)).to_be(wide)
    t:cmd("only")
  end)

  -- "<C-w>= equalize; <C-w>_ / <C-w>| maximize height / width"
  btv.test.it("<C-w>_ maximizes and <C-w>= equalizes", function(t)
    open(t)
    t:cmd("split")
    local win = vim.api.nvim_get_current_win()
    t:feed("<C-w>_")
    local tallest = vim.api.nvim_win_get_height(win)
    t:feed("<C-w>=")
    local shared = vim.api.nvim_win_get_height(win)
    btv.test.expect(tallest > shared).to_be(true)
    t:cmd("only")
  end)

  -- "<C-w>c close the focused window; <C-w>o keep only it"
  btv.test.it("<C-w>o keeps only the focused window", function(t)
    open(t)
    t:cmd("split")
    t:cmd("vsplit")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(3)
    t:feed("<C-w>o")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(1)
  end)

  -- ":resize 12 / :vertical resize 30"
  btv.test.it(":resize and :vertical resize set an exact size", function(t)
    open(t)
    t:cmd("split")
    t:cmd("resize 12")
    btv.test.expect(vim.api.nvim_win_get_height(0)).to_be(12)
    t:cmd("vsplit")
    t:cmd("vertical resize 30")
    btv.test.expect(vim.api.nvim_win_get_width(0)).to_be(30)
    t:cmd("only")
  end)

  -- ":q closes a window when several are open"
  btv.test.it(":q closes a window while several are open", function(t)
    open(t)
    t:cmd("split")
    t:cmd("q")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(1)
  end)

  -- ":WinList — lists every window with its buffer, cursor, and size"
  btv.test.it(":WinList reports the live window table", function(t)
    open(t)
    t:feed("3G")
    t:cmd("split")
    local got = notified(function()
      t:cmd("WinList")
    end)
    btv.test.expect(got).to_contain("(current)")
    btv.test.expect(got).to_contain("cursor=3,0")
    btv.test.expect(select(2, got:gsub("win %d+", ""))).to_be(2)
    t:cmd("only")
  end)

  -- ":WinDemo — opens a vertical split … parks its cursor a few lines down, then
  --  reports the layout"
  btv.test.it(":WinDemo splits, moves the cursor, and reports", function(t)
    open(t)
    local before = #vim.api.nvim_list_wins()
    local got
    local prev_vim, prev_btv = vim.notify, btv.notify
    vim.notify = function(msg)
      got = tostring(msg)
    end
    btv.notify = vim.notify
    t:cmd("WinDemo")
    -- The split is queued, so the report lands on a LATER tick.
    t:wait_for(function()
      return got ~= nil
    end, { message = ":WinDemo never reported" })
    vim.notify, btv.notify = prev_vim, prev_btv
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before + 1)
    btv.test.expect(got).to_contain("[WinDemo] opened window")
    btv.test.expect(got).to_contain(#vim.api.nvim_list_wins() .. " windows")
    btv.test.expect(t:cursor()[1]).to_be(3)
    t:cmd("only")
  end)
end)
