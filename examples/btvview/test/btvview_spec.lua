-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/btvview
--
-- It sources `init.lua` as a session would and drives the TRY-IT keys: toggle the
-- sidebar, move inside it, open an entry into the MAIN area — plus the claim the
-- sample buffer makes about a view being inert to the editing grammar.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

-- No `mapleader` is set, so `<leader>` is the default backslash.
local TOGGLE = "<Bslash>e"

--- Focus the sidebar and wait for its rows.
local function enter_sidebar(t)
  btv.layer.focus("left")
  t:wait_for(function()
    return (t:line(1) or ""):find("sample.txt", 1, true) ~= nil
  end, { message = "the sidebar never took focus" })
end

--- Back to the main editor area.
local function enter_main(t)
  btv.layer.main()
  t:wait_for(function()
    return (t:line(1) or ""):find("sample.txt", 1, true) == nil
  end, { message = "focus never returned to the main area" })
end

--- Open the sample in the main area, so each test starts from the same place.
local function open_sample(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/btvview", function()
  -- Always leave focus in the main area. The per-test baseline runs `enew!` in
  -- whatever window is current, so a test that ends inside the sidebar would hand
  -- the next one a dock window whose view content had been replaced by a scratch
  -- buffer — and every later test would fail for that reason alone.
  btv.test.after_each(function()
    btv.layer.main()
  end)

  btv.test.it("the sidebar is mounted at startup, with focus in the main area", function(t)
    open_sample(t)
    -- The main buffer is the sample, not the view: the config calls
    -- `btv.layer.main()` after mounting precisely so the cursor doesn't start
    -- inside the sidebar.
    btv.test.expect(t:line(1)).to_contain("btv.view sample buffer")
    enter_sidebar(t)
    btv.test.expect(t:lines()).to_equal({
      "  sample.txt",
      "  init.lua",
      "  README (this repo)",
    })
    enter_main(t)
  end)

  btv.test.it("the view's buffer is a view, not a document", function(t)
    open_sample(t)
    enter_sidebar(t)
    btv.test.expect(btv.bo.filetype).to_be("btvview")
    -- A view is a surface, not a document: it is not listed, and its `'buftype'`
    -- says so — which is the canonical signal a plugin gates on.
    btv.test.expect(btv.bo.buftype).to_be("nofile")
    enter_main(t)
  end)

  -- "The sidebar can't be edited (try `i` or `dd` in it — nothing happens)."
  btv.test.it("the view is inert to the editing grammar", function(t)
    open_sample(t)
    enter_sidebar(t)
    local before = t:lines()
    t:feed("ihello<Esc>")
    btv.test.expect(t:lines()).to_equal(before)
    t:feed("dd")
    btv.test.expect(t:lines()).to_equal(before)
    t:feed("x")
    btv.test.expect(t:lines()).to_equal(before)
    -- …and it never even entered insert.
    btv.test.expect(t:mode()).to_be("n")
    enter_main(t)
  end)

  -- "Move with j / k inside it."
  btv.test.it("j and k move within the list", function(t)
    open_sample(t)
    enter_sidebar(t)
    t:feed("gg")
    btv.test.expect(t:cursor()[1]).to_be(1)
    t:feed("jj")
    btv.test.expect(t:cursor()[1]).to_be(3)
    t:feed("k")
    btv.test.expect(t:cursor()[1]).to_be(2)
    enter_main(t)
  end)

  -- The key trick: `<CR>` runs `btv.open(path, { where = "main" })`, so the file
  -- lands in the editor rather than replacing the sidebar's own content.
  btv.test.it("<CR> opens the entry in the MAIN area, not the sidebar", function(t)
    open_sample(t)
    enter_sidebar(t)
    t:feed("ggj") -- the init.lua row
    t:feed("<CR>")
    t:wait_for(function()
      return (t:line(1) or ""):find("btv.view playground", 1, true) ~= nil
    end, { message = "<CR> never opened init.lua" })
    -- Focus crossed to the main area, and the sidebar still holds its own list.
    btv.test.expect(btv.bo.filetype).to_be("lua")
    enter_sidebar(t)
    btv.test.expect(t:line(1)).to_contain("sample.txt")
    enter_main(t)
  end)

  -- The per-row icon paint, laid through the extmark layer a tick after mount.
  btv.test.it("each row's leading glyph is painted", function(t)
    open_sample(t)
    enter_sidebar(t)
    for row = 1, 3 do
      local spans = t:highlights(row)
      btv.test.expect(#spans).to_be(1)
      btv.test.expect(spans[1][3]).to_be("BtvViewIcon")
      btv.test.expect(spans[1][1]).to_be(0)
    end
    enter_main(t)
  end)

  -- "<leader>e toggles the view in the left dock" — and its content survives.
  btv.test.it("<leader>e hides the dock, and again brings it back", function(t)
    open_sample(t)
    t:feed(TOGGLE)
    t:wait_for(function()
      -- With the dock hidden, focusing "left" cannot land anywhere.
      btv.layer.focus("left")
      return (t:line(1) or ""):find("sample.txt", 1, true) == nil
        or (t:line(1) or ""):find("btv.view sample buffer", 1, true) ~= nil
    end, { message = "the dock never hid" })
    t:feed(TOGGLE)
    enter_sidebar(t)
    btv.test.expect(t:lines()).to_equal({
      "  sample.txt",
      "  init.lua",
      "  README (this repo)",
    })
    enter_main(t)
  end)
end)
