-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/dock-winhighlight
--
-- `'winhighlight'` is a per-window REMAP, so what it changes is which group a
-- painted cell resolves to — which is exactly what `t:highlights()` reports. The
-- spec crosses into the dock and back and reads the groups on each side.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample in the main area, so each test starts from the same place.
---
--- The dock is queued at config time and its content is written a tick later, so
--- the first test has to let that land before it can rely on either side.
local function open(t)
  t:wait_for(function()
    return btv.dock.opt("left").winhighlight ~= ""
  end, { message = "the sidebar dock never came up" })
  btv.layer.main()
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Focus the sidebar dock and wait for its content.
local function enter_dock(t)
  btv.layer.focus("left")
  t:wait_for(function()
    return (t:line(1) or ""):find("src/", 1, true) ~= nil
  end, { message = "the sidebar never took focus" })
end

btv.test.describe("examples/dock-winhighlight", function()
  -- Always leave focus in the main area: the per-test baseline runs `enew!` in
  -- whatever window is current, and inside the dock that would wipe the sidebar.
  btv.test.after_each(function()
    btv.layer.main()
  end)

  -- §2. The listing must land in the SIDEBAR, not in the file you are editing —
  -- the dock's buffer only exists a tick after `btv.dock.open` queues it.
  btv.test.it("§2 — the file listing lands in the sidebar, not the main buffer", function(t)
    open(t)
    btv.test.expect(t:line(1)).to_contain("winhighlight — per-window highlight remap")
    btv.test.expect(t:lines()).never.to_contain("  src/")
    enter_dock(t)
    btv.test.expect(t:lines()).to_equal({
      "  src/",
      "    main.rs",
      "    lib.rs",
      "  Cargo.toml",
      "  README.md",
    })
  end)

  btv.test.it("§2 — the dock is open, sized and titled", function(t)
    open(t)
    btv.test.expect(btv.dock.opt("left").size).to_be(30)
    btv.test.expect(btv.dock.opt("left").title).to_be("EXPLORER")
    btv.test.expect(btv.dock.opt("left").showtabline).to_be(2)
  end)

  -- §1. The remap only renames groups, so the targets must exist.
  btv.test.it("§1 — the sidebar groups the remap points at are defined", function(t)
    open(t)
    for _, group in ipairs({ "NormalSB", "SidebarEob", "SidebarLineNr" }) do
      local def = btv.hl.get(0, { name = group }) or {}
      btv.test.expect(def.fg or def.bg).never.to_be_nil()
    end
  end)

  -- §3. The star: the dock's chrome resolves through the remap.
  btv.test.it("§3 — the dock carries the winhighlight the config set", function(t)
    open(t)
    btv.test
      .expect(btv.dock.opt("left").winhighlight)
      .to_be("Normal:NormalSB,EndOfBuffer:SidebarEob,LineNr:SidebarLineNr")
  end)

  btv.test.it("§3 — only the dock is recoloured; the main area is untouched", function(t)
    open(t)
    btv.test.expect(btv.wo.winhighlight).to_be("")
  end)

  -- The remap's EFFECT is a colour, and colour is where a spec stops: a `Normal` /
  -- `EndOfBuffer` background is not a highlight span, and the wire carries a
  -- per-frame palette id rather than a group name for it — so `t:highlights()`
  -- cannot see which group a filler row resolved through. What is checkable is the
  -- contract around it: the remap is set on the dock and on nothing else, the
  -- groups it points at exist, and it survives a collapse.

  -- ":DockToggle left  collapse / restore the styled sidebar"
  btv.test.it("try-it — :DockToggle collapses and restores it, remap and all", function(t)
    open(t)
    t:cmd("DockToggle left")
    t:sleep(30)
    -- Focus cannot reach a collapsed dock, so the main buffer stays current.
    btv.layer.focus("left")
    btv.test.expect(t:line(1)).to_contain("winhighlight — per-window highlight remap")
    t:cmd("DockToggle left")
    enter_dock(t)
    btv.test.expect(t:lines()).to_contain("  Cargo.toml")
    btv.test.expect(btv.dock.opt("left").winhighlight).to_contain("Normal:NormalSB")
  end)

  -- "it is also exposed on `btv.wo`, so a single window can remap its own groups"
  btv.test.it("winhighlight is per-window, settable outside a dock too", function(t)
    open(t)
    t:cmd("split")
    btv.wo.winhighlight = "Normal:NormalSB"
    btv.test.expect(btv.wo.winhighlight).to_be("Normal:NormalSB")
    t:feed("<C-w>j")
    btv.test.expect(btv.wo.winhighlight).to_be("")
    t:cmd("only")
  end)
end)
