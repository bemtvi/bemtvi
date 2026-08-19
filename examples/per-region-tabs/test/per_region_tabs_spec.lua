-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/per-region-tabs
--
-- The claim to hold to account is INDEPENDENCE: a tab operation must reach the
-- focused region's stack and no other. So every case counts each region's tabs
-- before and after, rather than trusting the one the notes are about.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample in the main area, with the docks up.
local function open(t)
  t:wait_for(function()
    return btv.dock.opt("left").title == "EXPLORER" and btv.dock.opt("bottom").title == "TERMINAL"
  end, { message = "the startup docks never came up" })
  btv.layer.main()
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Focus a region and let the queued focus land before the next command — a
--- config-side `btv.layer.focus` and a following `:tabnew` drain on separate
--- queues, so without the settle the tab can be added to the region you just left.
local function focus(t, region)
  if region == "main" then
    btv.layer.main()
  else
    btv.layer.focus(region)
  end
  t:feed("<Esc>")
end

--- How many tab pages a region has. `nvim_list_tabpages` is no use here — it
--- reports one global list whichever region is focused, which is exactly the
--- distinction this example is about — so this reads the region's own tabline.
--- (The config sets `showtabline = 2` everywhere, so every region draws one.)
local function tabs_of(t, region)
  t:feed("<Esc>")
  return #t:tabs(region).labels
end

--- The focused region's count, by name.
local function tabs(t, region)
  return tabs_of(t, region)
end

btv.test.describe("examples/per-region-tabs", function()
  -- The baseline runs `enew!` in whatever window is current; never end inside a
  -- dock, or its buffer is replaced for the next test.
  -- Close every extra tab in every region, so one test's leftovers cannot be read
  -- as the next one's starting point, and put the left dock back if a test closed
  -- it by closing its last tab.
  btv.test.after_each(function()
    for _, region in ipairs({ "left", "bottom", "main" }) do
      for _ = 1, 6 do
        local rt = btv.test and btv._ui and btv._ui.region_tabs or {}
        local labels = (rt[region] or {}).tabs or {}
        if #labels <= 1 then
          break
        end
        if region == "main" then
          btv.layer.main()
        else
          btv.layer.focus(region)
        end
        btv._feedkeys("<Esc>", true, false, true)
        vim.cmd("tabclose")
      end
    end
    btv.layer.main()
  end)

  btv.test.it("the config opens two titled docks with always-on strips", function(t)
    open(t)
    btv.test.expect(btv.dock.opt("left").title).to_be("EXPLORER")
    btv.test.expect(btv.dock.opt("left").showtabline).to_be(2)
    btv.test.expect(btv.dock.opt("bottom").title).to_be("TERMINAL")
    btv.test.expect(btv.dock.opt("bottom").showtabline).to_be(2)
    btv.test.expect(btv.o.showtabline).to_be(2)
  end)

  -- ":tabnew  add a tab to the FOCUSED region only"
  btv.test.it("try-it — :tabnew adds a tab to the focused region only", function(t)
    open(t)
    local main_before = tabs_of(t, "main")
    local left_before = tabs_of(t, "left")
    local bottom_before = tabs_of(t, "bottom")

    focus(t, "left")
    t:cmd("tabnew")
    btv.test.expect(tabs_of(t, "left")).to_be(left_before + 1)
    -- …and neither other region moved.
    btv.test.expect(tabs_of(t, "main")).to_be(main_before)
    btv.test.expect(tabs_of(t, "bottom")).to_be(bottom_before)

    focus(t, "left")
    t:cmd("tabclose")
    btv.test.expect(tabs_of(t, "left")).to_be(left_before)
  end)

  -- "gt / gT  cycle the FOCUSED region's tabs"
  btv.test.it("try-it — gt cycles the focused region's tabs", function(t)
    open(t)
    focus(t, "bottom")
    t:cmd("tabnew")
    t:cmd("tabnew")
    btv.test.expect(tabs_of(t, "bottom")).to_be(3)
    focus(t, "bottom")
    local at = t:tabs("bottom").current
    t:feed("gt")
    btv.test.expect(t:tabs("bottom").current).never.to_be(at)
    t:feed("gT")
    btv.test.expect(t:tabs("bottom").current).to_be(at)
    t:cmd("tabclose")
    t:cmd("tabclose")
    btv.layer.main()
  end)

  -- ":T {n} — add `n` tabs (default 1) to the region that currently has focus."
  btv.test.it("the config's :T adds tabs to the focused region", function(t)
    open(t)
    local main_before = tabs_of(t, "main")
    btv.layer.focus("bottom")
    local bottom_before = tabs_of(t, "bottom")
    focus(t, "bottom")
    t:cmd("T 3")
    btv.test.expect(tabs_of(t, "bottom")).to_be(bottom_before + 3)
    btv.test.expect(tabs_of(t, "main")).to_be(main_before)
    focus(t, "bottom")
    for _ = 1, 3 do
      t:cmd("tabclose")
    end
    btv.test.expect(tabs_of(t, "bottom")).to_be(bottom_before)
    btv.layer.main()
  end)

  btv.test.it(":T defaults to one tab", function(t)
    open(t)
    local before = tabs_of(t, "main")
    focus(t, "main")
    t:cmd("T")
    btv.test.expect(tabs_of(t, "main")).to_be(before + 1)
    focus(t, "main")
    t:cmd("tabclose")
    btv.test.expect(tabs_of(t, "main")).to_be(before)
  end)

  -- "Main always keeps at least one tab."
  btv.test.it("main always keeps at least one tab", function(t)
    open(t)
    btv.layer.main()
    btv.test.expect(tabs_of(t, "main")).to_be(1)
    focus(t, "main")
    t:cmd("tabclose")
    btv.test.expect(tabs_of(t, "main")).to_be(1)
  end)

  -- "a dock's last tab closes the dock … the dock folds away and the main area
  --  reclaims the space"
  btv.test.it("try-it — closing a dock's last tab closes the dock", function(t)
    open(t)
    local wins = #vim.api.nvim_list_wins()
    btv.layer.focus("left")
    btv.test.expect(tabs_of(t, "left")).to_be(1)
    focus(t, "left")
    t:cmd("tabclose")
    btv.layer.main()
    t:feed("<Esc>")
    btv.test.expect(#vim.api.nvim_list_wins()).to_be(wins - 1)
    -- Re-open it so the rest of the suite finds the layout it expects.
    btv.dock.open({ side = "left", size = 28, title = "EXPLORER", showtabline = 2 })
    t:feed("<Esc>")
    btv.layer.main()
  end)

  -- "Switch focus between regions with the doubled window key … and the same tab
  --  keys now drive that region's own tab stack."
  btv.test.it("try-it — the doubled window key picks which stack gt drives", function(t)
    open(t)
    -- Give main two tabs and the bottom tray three, then prove `gt` in each
    -- region walks its own.
    t:cmd("tabnew")
    local main_tabs = tabs_of(t, "main")
    btv.test.expect(main_tabs).to_be(2)
    t:feed("<C-w><C-w>j")
    t:cmd("T 2")
    btv.test.expect(tabs_of(t, "bottom")).to_be(3)
    -- Each region kept its own count.
    btv.test.expect(tabs_of(t, "main")).to_be(main_tabs)
    focus(t, "main")
    t:cmd("tabclose")
    btv.layer.focus("bottom")
    for _ = 1, 2 do
      t:cmd("tabclose")
    end
    btv.layer.main()
  end)
end)
