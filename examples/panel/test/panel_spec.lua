-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/panel
--
-- A panel is an ordinary `nomodifiable` buffer in a focus-locked overlay, so its
-- rows are `t:lines()`, its navigation is plain motions, and its selection is a
-- buffer-local keymap. Every TRY-IT key is typed as written.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The `<CR>` map reports through `vim.notify`, after the panel closes.
local notified = {}
do
  local real = vim.notify
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

local FRUITS = { "apple", "banana", "cherry", "date", "elderberry", "fig" }

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Open the fruit panel and wait for it.
local function panel(t)
  t:feed("<Bslash>p")
  t:wait_for(function()
    return btv.bo.filetype == "fruitpanel"
  end, { message = "the fruit panel never opened" })
end

btv.test.describe("examples/panel", function()
  btv.test.it("<leader>p mounts the panel with its lines", function(t)
    open(t)
    panel(t)
    btv.test.expect(t:lines()).to_equal(FRUITS)
    btv.test.expect(btv.buf.name(0)).to_be("[Fruit]")
    t:feed("q")
  end)

  -- "Its content is an ordinary `nomodifiable` buffer."
  btv.test.it("the panel buffer is read-only", function(t)
    open(t)
    panel(t)
    local before = t:lines()
    -- Not `i…<Esc>`: this panel maps `<Esc>` to close, so that would dismiss it
    -- rather than test it.
    t:feed("x")
    t:feed("dd")
    t:feed("p")
    btv.test.expect(t:lines()).to_equal(before)
    btv.test.expect(btv.bo.filetype).to_be("fruitpanel")
    t:feed("q")
  end)

  -- "j / k  move within the list (ordinary motions)"
  btv.test.it("try-it — ordinary motions navigate it", function(t)
    open(t)
    panel(t)
    btv.test.expect(t:cursor()[1]).to_be(1)
    t:feed("jj")
    btv.test.expect(t:cursor()[1]).to_be(3)
    t:feed("G")
    btv.test.expect(t:cursor()[1]).to_be(#FRUITS)
    t:feed("gg")
    btv.test.expect(t:current_line()).to_be("apple")
    t:feed("q")
  end)

  -- "opening it … **locks focus** to the panel — `<C-w>` navigation is inert"
  btv.test.it("the panel is modal: <C-w> cannot leave it", function(t)
    open(t)
    panel(t)
    t:feed("<C-w>k")
    btv.test.expect(btv.bo.filetype).to_be("fruitpanel")
    t:feed("<C-w>w")
    btv.test.expect(btv.bo.filetype).to_be("fruitpanel")
    t:feed("q")
  end)

  -- "<CR>  'choose' the line under the cursor (echoes it) and close the panel"
  btv.test.it("try-it — <CR> chooses the line and closes the panel", function(t)
    open(t)
    panel(t)
    t:feed("jj")
    btv.test.expect(t:current_line()).to_be("cherry")
    t:feed("<CR>")
    t:wait_for(function()
      return last_notify():find("you chose: cherry", 1, true) ~= nil
    end, { message = "<CR> never reported a choice" })
    btv.test.expect(btv.bo.filetype).never.to_be("fruitpanel")
  end)

  -- "q / <Esc>  dismiss the panel"
  btv.test.it("try-it — q and <Esc> both dismiss it", function(t)
    open(t)
    panel(t)
    t:feed("q")
    btv.test.expect(btv.bo.filetype).never.to_be("fruitpanel")
    panel(t)
    t:feed("<Esc>")
    btv.test.expect(btv.bo.filetype).never.to_be("fruitpanel")
  end)

  -- "The `name` makes it unique: re-opening replaces its content."
  btv.test.it("re-opening replaces the panel rather than stacking one", function(t)
    open(t)
    panel(t)
    local wins = #vim.api.nvim_list_wins()
    t:feed("<Bslash>p")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(wins)
    btv.test.expect(t:lines()).to_equal(FRUITS)
    t:feed("q")
  end)

  -- "The built-in listings ride the very same mechanism."
  btv.test.it("the built-in listings are the same surface", function(t)
    open(t)
    t:cmd("messages")
    btv.test.expect(btv.buf.name(0)).to_be("[Messages]")
    -- The built-in ftplugin gives them `q` for free.
    t:feed("q")
    btv.test.expect(btv.buf.name(0)).never.to_be("[Messages]")
    t:cmd("registers")
    btv.test.expect(btv.buf.name(0)).to_be("[Registers]")
    t:feed("q")
  end)

  -- "Panel buffers are hidden from `:ls` (they're surfaces, not documents)."
  btv.test.it("a panel buffer is not listed in :ls", function(t)
    open(t)
    panel(t)
    t:feed("q")
    t:cmd("ls")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("[Fruit]")
    t:feed("q")
  end)

  -- "…and always open as panels — `:b [Fruit]` re-opens the panel rather than
  --  showing it in the main window."
  btv.test.it(":b on a panel buffer re-opens it as a panel", function(t)
    open(t)
    panel(t)
    t:feed("q")
    t:cmd("b [Fruit]")
    btv.test.expect(btv.bo.filetype).to_be("fruitpanel")
    btv.test.expect(t:lines()).to_equal(FRUITS)
    -- Still modal, so it really is the panel and not a plain window.
    t:feed("<C-w>k")
    btv.test.expect(btv.bo.filetype).to_be("fruitpanel")
    t:feed("q")
  end)

  -- ":lspanels to list the panels themselves"
  btv.test.it(":lspanels lists the panels", function(t)
    open(t)
    panel(t)
    t:feed("q")
    t:cmd("lspanels")
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("[Fruit]")
    t:feed("q")
  end)
end)
