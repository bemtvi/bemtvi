-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/window-local-options
--
-- The whole point is that two windows onto the SAME buffer disagree, so every
-- case reads the option per window id — and `t:gutter()` for the column the
-- server actually reserves, which is the thing a reader sees.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("setlocal number relativenumber")
  t:feed("gg")
end

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

btv.test.describe("examples/window-local-options", function()
  -- "a split inherits them from the window it splits off"
  btv.test.it("a split starts as a clone of the window it came from", function(t)
    open(t)
    t:cmd("vsplit")
    btv.test.expect(btv.wo.number).to_be(true)
    btv.test.expect(btv.wo.relativenumber).to_be(true)
    t:cmd("only")
  end)

  -- ":setlocal nonumber norelativenumber — drop the gutter in THIS split only"
  btv.test.it(":setlocal touches the focused split alone", function(t)
    open(t)
    local original = vim.api.nvim_get_current_win()
    t:cmd("vsplit")
    local fresh = vim.api.nvim_get_current_win()
    t:cmd("setlocal nonumber norelativenumber")
    btv.test.expect(vim.wo[fresh].number).to_be(false)
    btv.test.expect(vim.wo[original].number).to_be(true)
    -- The reserved gutter follows: none here…
    btv.test.expect(t:gutter().number_width).to_be(0)
    -- …and still there in the sibling.
    t:feed("<C-w>w")
    btv.test.expect(vim.api.nvim_get_current_win()).to_be(original)
    btv.test.expect(t:gutter().number_width > 0).to_be(true)
    t:cmd("only")
  end)

  btv.test.it("both windows still show the one buffer", function(t)
    open(t)
    local buf = btv.buf.current()
    t:cmd("vsplit")
    t:cmd("setlocal nonumber norelativenumber")
    for _, w in ipairs(vim.api.nvim_list_wins()) do
      btv.test.expect(vim.api.nvim_win_get_buf(w)).to_be(buf)
    end
    t:cmd("only")
  end)

  -- "1. :GutterDemo -> a vertical split; left window has NO gutter, the right
  --  (original) keeps the hybrid numbers."
  btv.test.it(":GutterDemo gives the two windows different gutters", function(t)
    open(t)
    local original = vim.api.nvim_get_current_win()
    local got
    local prev_vim, prev_btv = vim.notify, btv.notify
    vim.notify = function(msg)
      got = tostring(msg)
    end
    btv.notify = vim.notify
    t:cmd("GutterDemo")
    -- The split is queued, so the report lands on a LATER tick.
    t:wait_for(function()
      return got ~= nil
    end, { message = ":GutterDemo never reported" })
    vim.notify, btv.notify = prev_vim, prev_btv
    local fresh = vim.api.nvim_get_current_win()
    btv.test.expect(fresh).never.to_be(original)
    btv.test.expect(vim.wo[fresh].number).to_be(false)
    btv.test.expect(vim.wo[fresh].relativenumber).to_be(false)
    btv.test.expect(vim.wo[original].number).to_be(true)
    btv.test.expect(vim.wo[original].relativenumber).to_be(true)
    btv.test.expect(got).to_contain("same buffer, two gutters")
    t:cmd("only")
  end)

  -- "2. :GutterReport -> 'win N: number=false ... | win M: number=true ...'"
  btv.test.it(":GutterReport reads the option back per window", function(t)
    open(t)
    t:cmd("vsplit")
    t:cmd("setlocal nonumber norelativenumber")
    local got = notified(function()
      t:cmd("GutterReport")
    end)
    btv.test.expect(got).to_contain("number=false")
    btv.test.expect(got).to_contain("number=true")
    t:cmd("only")
  end)

  -- "the nvim_win_get_option / nvim_get_option_value getters, which all agree"
  btv.test.it("the three getters agree on one window", function(t)
    open(t)
    t:cmd("vsplit")
    t:cmd("setlocal nonumber")
    local win = vim.api.nvim_get_current_win()
    btv.test.expect(vim.wo[win].number).to_be(false)
    btv.test.expect(vim.api.nvim_win_get_option(win, "number")).to_be(false)
    btv.test.expect(vim.api.nvim_get_option_value("number", { win = win })).to_be(false)
    t:cmd("only")
  end)

  -- "vim.wo — window-local writes … change only the named window's gutter"
  btv.test.it("a vim.wo write targets the window it names", function(t)
    open(t)
    local original = vim.api.nvim_get_current_win()
    t:cmd("vsplit")
    local fresh = vim.api.nvim_get_current_win()
    t:exec(function()
      vim.wo[original].number = false
    end)
    btv.test.expect(vim.wo[original].number).to_be(false)
    btv.test.expect(vim.wo[fresh].number).to_be(true)
    -- …and the focused window is the fresh one, which still has its gutter.
    btv.test.expect(t:gutter().number_width > 0).to_be(true)
    t:cmd("only")
  end)
end)
