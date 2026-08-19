-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/dock
--
-- Every TRY-IT line, typed as written: the layer chord that crosses into a dock,
-- the single `<C-w>` that splits within one, the ex-commands, and the difference
-- the notes make most of — TOGGLE keeps a dock's content, CLOSE drops it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- The main area's window, so "am I in the main area?" is an identity check rather
--- than a guess at the text. The WINDOW, not the buffer: a dock's scratch buffer
--- can be replaced (the per-test baseline `enew!` does exactly that if a test ends
--- inside one), and then two regions would report the same buffer.
local main_win

--- Wait for the startup docks, then park in the main area with the sample open.
local function open(t)
  t:wait_for(function()
    return btv.dock.opt("left").title == "EXPLORER"
  end, { message = "the startup docks never came up" })
  btv.layer.main()
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  main_win = vim.api.nvim_get_current_win()
end

--- Whether focus is currently in the main area.
local function in_main(t)
  return vim.api.nvim_get_current_win() == main_win
end

btv.test.describe("examples/dock", function()
  -- The baseline `enew!` runs in whatever window is current, so never end a test
  -- inside a dock — that would replace its scratch buffer for every later test.
  btv.test.after_each(function()
    btv.layer.main()
  end)

  btv.test.it("the config opens a left side bar and a bottom tray", function(t)
    open(t)
    btv.test.expect(btv.dock.opt("left").size).to_be(28)
    btv.test.expect(btv.dock.opt("left").title).to_be("EXPLORER")
    btv.test.expect(btv.dock.opt("left").showtabline).to_be(2)
    btv.test.expect(btv.dock.opt("bottom").title).to_be("TERMINAL")
    btv.test.expect(btv.dock.opt("bottom").autohide).to_be(true)
  end)

  btv.test.it("focus starts in the main area, on the sample", function(t)
    open(t)
    btv.test.expect(in_main(t)).to_be(true)
  end)

  -- "<C-w><C-w>h  cross focus INTO the left dock"
  btv.test.it("try-it — <C-w><C-w>h crosses into the left dock", function(t)
    open(t)
    t:feed("<C-w><C-w>h")
    btv.test.expect(in_main(t)).to_be(false)
    -- "Each dock starts on an empty scratch buffer … just start typing."
    t:feed("ityped in the dock<Esc>")
    btv.test.expect(t:line(1)).to_be("typed in the dock")
  end)

  -- "<C-w><C-w>l  from a dock, cross back to the main area"
  btv.test.it("try-it — <C-w><C-w>l crosses back out", function(t)
    open(t)
    t:feed("<C-w><C-w>h")
    btv.test.expect(in_main(t)).to_be(false)
    t:feed("<C-w><C-w>l")
    btv.test.expect(in_main(t)).to_be(true)
  end)

  -- "<C-w>v / <C-w>s  while focused in a dock, split WITHIN it (single <C-w>!)"
  btv.test.it("try-it — a single <C-w>s splits within the dock", function(t)
    open(t)
    t:feed("<C-w><C-w>h")
    local before = #vim.api.nvim_list_wins()
    t:feed("<C-w>s")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before + 1)
    -- Still inside the dock, not out in the main area.
    btv.test.expect(in_main(t)).to_be(false)
    t:feed("<C-w>c")
  end)

  -- ":DockOpen left 30  open/resize a dock"
  btv.test.it("try-it — :DockOpen resizes the dock", function(t)
    open(t)
    t:cmd("DockOpen left 30")
    btv.test.expect(btv.dock.opt("left").size).to_be(30)
    t:cmd("DockOpen left 28")
    btv.test.expect(btv.dock.opt("left").size).to_be(28)
  end)

  -- ":DockFocus {side}"
  btv.test.it("try-it — :DockFocus crosses into a dock", function(t)
    open(t)
    t:cmd("DockFocus left")
    btv.test.expect(in_main(t)).to_be(false)
  end)

  -- ":DockGrow {side} {n}" — the config's own command, driving the `size` option.
  btv.test.it("the config's :DockGrow resizes through the size option", function(t)
    open(t)
    t:cmd("DockGrow left 40")
    btv.test.expect(btv.dock.opt("left").size).to_be(40)
    t:cmd("DockGrow left 28")
    btv.test.expect(btv.dock.opt("left").size).to_be(28)
  end)

  -- "TOGGLE vs CLOSE: :DockToggle … collapse a dock from view while KEEPING its
  --  content — its splits, tabs, cursor and text all come back."
  btv.test.it("try-it — :DockToggle keeps the dock's content", function(t)
    open(t)
    t:feed("<C-w><C-w>h")
    t:feed("ccremember me<Esc>")
    btv.layer.main()
    local before = #vim.api.nvim_list_wins()
    t:cmd("DockToggle left")
    -- Collapsed: its window is gone from the layout…
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before - 1)
    -- …and the spatial cross skips it, because you collapsed it on purpose.
    t:feed("<C-w><C-w>h")
    btv.test.expect(in_main(t)).to_be(true)
    -- Toggling it back restores the window AND everything that was in it.
    t:cmd("DockToggle left")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before)
    t:cmd("DockFocus left")
    btv.test.expect(t:line(1)).to_be("remember me")
  end)

  -- "<leader>e  toggle the left explorer (the same, by keymap)"
  btv.test.it("try-it — <leader>e is the same toggle", function(t)
    open(t)
    local before = #vim.api.nvim_list_wins()
    t:feed("<Space>e")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before - 1)
    t:feed("<Space>e")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(before)
    t:cmd("DockFocus left")
    btv.test.expect(in_main(t)).to_be(false)
  end)

  -- ":DockFocus reaches even a collapsed dock — asking for it by name is asking
  --  for it back."
  btv.test.it("try-it — :DockFocus reveals a collapsed dock", function(t)
    open(t)
    t:cmd("DockToggle left")
    t:cmd("DockFocus left")
    btv.test.expect(in_main(t)).to_be(false)
    btv.layer.main()
  end)

  -- "The bottom tray … is set `autohide` — it collapses by itself the moment focus
  --  leaves it, and pops back when you cross into it again."
  btv.test.it("the autohide tray collapses when focus leaves it", function(t)
    open(t)
    -- Start from a known state: the tray up and focused.
    t:cmd("DockFocus bottom")
    btv.test.expect(in_main(t)).to_be(false)
    local with_tray = #vim.api.nvim_list_wins()
    t:feed("cctyped in the tray<Esc>")
    -- Leaving collapses it, on its own, with no command. (`btv.layer.main()` is
    -- queued like every other Lua write, so settle a tick before reading back.)
    btv.layer.main()
    t:feed("<Esc>")
    btv.test.expect(in_main(t)).to_be(true)
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(with_tray - 1)
  end)

  btv.test.it("…and crossing back in pops it open, content intact", function(t)
    open(t)
    t:cmd("DockFocus bottom")
    t:feed("cctyped in the tray<Esc>")
    btv.layer.main()
    -- The spatial cross reaches a collapsed AUTOHIDE dock: its collapse is
    -- transient by design, unlike one you asked for with :DockToggle.
    t:feed("<C-w><C-w>j")
    btv.test.expect(in_main(t)).to_be(false)
    btv.test.expect(t:line(1)).to_be("typed in the tray")
  end)

  -- "a dock is GLOBAL (it shows on every tab) and is never disturbed by splits /
  --  window switches / tab changes in the main editor area"
  btv.test.it("a dock survives a split and a new tab in the main area", function(t)
    open(t)
    t:feed("<C-w><C-w>h")
    t:feed("ccstill here<Esc>")
    btv.layer.main()
    t:cmd("split")
    t:cmd("tabnew")
    t:cmd("DockFocus left")
    btv.test.expect(t:line(1)).to_be("still here")
    btv.layer.main()
    t:cmd("tabclose")
    t:cmd("only")
  end)
end)
