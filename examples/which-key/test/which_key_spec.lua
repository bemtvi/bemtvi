-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/which-key
--
-- The popup is a non-focus content float, so `t:float()` is its view. It is
-- DEBOUNCED: it appears only once you pause, which is exactly what `t:sleep`
-- expresses — a fast sequence must leave it hidden.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- The debounce the config waits out before showing, plus a margin.
local DELAY = 200
local PAUSE = DELAY + 150

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("<Esc>")
  t:feed("gg")
end

--- Type `keys`, pause long enough for the popup, and return it.
local function popup(t, keys)
  t:feed(keys)
  t:wait_for(function()
    return t:float() ~= nil
  end, { tries = 100, interval = 20, message = "no popup after " .. keys })
  return t:float()
end

btv.test.describe("examples/which-key", function()
  -- "press <leader> (Space) and pause. A bordered popup appears … listing every
  --  key that can follow — `w write`, `q quit`, `f +file`, `g +git`."
  btv.test.it("the leader popup lists every key that can follow", function(t)
    open(t)
    local f = popup(t, "<Space>")
    btv.test.expect(f.text).to_contain("w")
    btv.test.expect(f.text).to_contain("write")
    btv.test.expect(f.text).to_contain("quit")
    -- `f` and `g` lead deeper, so they read as groups.
    btv.test.expect(f.text).to_contain("+")
    t:feed("<Esc>")
  end)

  -- "Keep typing into a group (`f`) and the popup REFRESHES to that group's keys"
  btv.test.it("descending into a group refreshes the popup", function(t)
    open(t)
    popup(t, "<Space>")
    t:feed("f")
    t:sleep(PAUSE)
    local f = t:float()
    btv.test.expect(f).never.to_be(nil)
    btv.test.expect(f.text).to_contain("find file")
    btv.test.expect(f.text).to_contain("live grep")
    -- The sibling group's keys are gone.
    btv.test.expect(f.text).never.to_contain("git status")
    t:feed("<Esc>")
  end)

  -- "complete a mapping … and it closes"
  btv.test.it("completing a mapping closes the popup and fires it", function(t)
    open(t)
    popup(t, "<Space>")
    t:feed("w")
    t:wait_for(function()
      return t:float() == nil
    end, { message = "the popup outlived the mapping" })
    btv.test.expect(t:message()).to_be("write")
  end)

  -- "Debounce … so a fast, deliberate sequence never flashes the popup"
  btv.test.it("a fast sequence never shows the popup", function(t)
    open(t)
    -- Both keys in one feed: no pause between them, so the debounce never fires.
    t:feed("<Space>w")
    btv.test.expect(t:float()).to_be_nil()
    btv.test.expect(t:message()).to_be("write")
  end)

  -- "break the sequence … and it closes"
  btv.test.it("breaking the sequence closes the popup", function(t)
    open(t)
    popup(t, "<Space>")
    t:feed("<Esc>")
    t:wait_for(function()
      return t:float() == nil
    end, { message = "the popup survived the break" })
  end)

  -- "pause after `z` for the viewport commands (zt/zz/zb…)"
  btv.test.it("the built-in z prefix feeds the same popup", function(t)
    open(t)
    local f = popup(t, "z")
    btv.test.expect(f.text).to_contain("z")
    btv.test.expect(f.text:lower()).to_contain("center")
    t:feed("<Esc>")
  end)

  -- "after `<C-w>` for the window commands"
  btv.test.it("the built-in <C-w> prefix feeds it too", function(t)
    open(t)
    local f = popup(t, "<C-w>")
    btv.test.expect(f.text).to_contain("Close window")
    btv.test.expect(f.text).to_contain("Focus left")
    t:feed("<Esc>")
  end)

  -- "mid-`f` or after a lone `d` for an 'awaiting input' hint card"
  btv.test.it("an open continuation set renders its label as a hint card", function(t)
    open(t)
    local f = popup(t, "f")
    btv.test.expect(f.text).to_contain("Find character")
    btv.test.expect(f.title).to_contain("f")
    t:feed("<Esc>")
  end)

  btv.test.it("a lone operator shows its own hint", function(t)
    open(t)
    local f = popup(t, "d")
    btv.test.expect(f.title).to_contain("d")
    btv.test.expect(f.text).never.to_be("")
    t:feed("<Esc>")
  end)

  -- "Title the popup `keys — label` so the prefix isn't cryptic"
  btv.test.it("the title carries the keys, and the label when there is one", function(t)
    open(t)
    btv.test.expect(popup(t, "<Space>").title).to_contain("<Space>")
    t:feed("<Esc>")
    t:sleep(60)
    btv.test.expect(popup(t, "f").title).to_contain("—")
    t:feed("<Esc>")
  end)

  -- "it must never take focus or bind keys"
  btv.test.it("the popup takes no focus and swallows no key", function(t)
    open(t)
    local win = vim.api.nvim_get_current_win()
    local line = t:cursor()[1]
    popup(t, "z")
    btv.test.expect(vim.api.nvim_get_current_win()).to_be(win)
    -- `zz` completes the viewport command; the cursor never moved to the popup.
    t:feed("z")
    btv.test.expect(t:cursor()[1]).to_be(line)
  end)

  -- "which-key's own highlight groups"
  btv.test.it("the config defines its three highlight groups", function(t)
    open(t)
    for _, group in ipairs({ "WhichKey", "WhichKeyGroup", "WhichKeyDesc" }) do
      btv.test.expect(btv.hl.exists(group)).to_be(true)
    end
  end)
end)
