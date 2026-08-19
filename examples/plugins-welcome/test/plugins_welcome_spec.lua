-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/plugins-welcome
--
-- Installing the set needs the network, which a test may not depend on — but
-- everything the example is actually about is local: what the config REGISTERS,
-- the three gates that decide whether the offer appears, and the two surfaces
-- (`<CR>` on the offer, `c` for the checklist) that the notes tell you to press.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open a scratch buffer with nothing floating.
local function open(t)
  for _ = 1, 4 do
    t:feed("<Esc>")
    t:sleep(25)
  end
  t:cmd("enew")
end

btv.test.describe("examples/plugins-welcome", function()
  -- "This example instead registers its OWN set with btv.plugins.recommend{…}."
  btv.test.it("the config registers a recommended set", function(t)
    open(t)
    local set = btv.plugins.recommended and btv.plugins.recommended() or nil
    if set then
      btv.test.expect(#set > 0).to_be(true)
      local names = {}
      for _, spec in ipairs(set) do
        names[spec.name or spec[1]] = spec
      end
      btv.test.expect(names["catppuccin"]).never.to_be_nil()
    end
    btv.test.expect(type(btv.plugins.recommend)).to_be("function")
  end)

  -- "gate 2: the user has declared no plugins of their own (this config declares
  --  none)"
  btv.test.it("the config declares no plugins of its own", function(t)
    open(t)
    btv.test.expect(btv.plugins.list()).to_equal({})
  end)

  -- "just run `:PluginsWelcome` any time to reopen the offer on demand."
  btv.test.it(":PluginsWelcome reopens the offer", function(t)
    open(t)
    t:cmd("PluginsWelcome")
    t:wait_for(function()
      return btv.bo.filetype == "btvpluginswelcome"
    end, { message = ":PluginsWelcome opened nothing" })
    t:feed("<Esc>")
  end)

  -- "On the offer: <CR> install all · c customize · ? reference page · <Esc> skip"
  btv.test.it("the offer summarises the set by count and origin", function(t)
    open(t)
    t:cmd("PluginsWelcome")
    t:wait_for(function()
      return btv.bo.filetype == "btvpluginswelcome" and (t:lines()[1] or "") ~= ""
    end, { message = ":PluginsWelcome opened nothing" })
    -- The offer is a real buffer, so its rows are `t:lines()`.
    local text = table.concat(t:lines(), "\n")
    -- "summarizing it by count and by the origins its code comes from rather than
    --  listing every plugin"
    btv.test.expect(text).to_contain("3 plugins")
    btv.test.expect(text).to_contain("github.com/bemtvi")
    btv.test.expect(text).never.to_contain("btv-files")
    -- …and the keys the notes list.
    btv.test.expect(text).to_contain("<CR>")
    btv.test.expect(text).to_contain("c ")
    t:feed("<Esc>")
  end)

  -- "`c` opens the CUSTOMIZE checklist behind it (every plugin with its exact
  --  source, pre-ticked and untickable)"
  btv.test.it("c opens the customize checklist, pre-ticked", function(t)
    open(t)
    t:cmd("PluginsWelcome")
    t:wait_for(function()
      return btv.bo.filetype == "btvpluginswelcome"
    end, { message = ":PluginsWelcome opened nothing" })
    t:feed("c")
    local text = t:wait_for(function()
      local rows = table.concat(t:lines(), "\n")
      return rows:find("catppuccin", 1, true) and rows or nil
    end, { message = "the customize checklist never opened" })
    -- Every plugin with its exact source and its blurb.
    btv.test.expect(text).to_contain("bemtvi/catppuccin-bemtvi")
    btv.test.expect(text).to_contain("Soothing pastel colorscheme")
    -- Pre-ticked.
    btv.test.expect(text).to_match("[☑x✓]")
    t:feed("<Esc>")
  end)

  btv.test.it("<Space> unticks an item in the checklist", function(t)
    open(t)
    t:cmd("PluginsWelcome")
    t:wait_for(function()
      return btv.bo.filetype == "btvpluginswelcome"
    end, { message = ":PluginsWelcome opened nothing" })
    t:feed("c")
    t:wait_for(function()
      return (table.concat(t:lines(), "\n")):find("catppuccin", 1, true) ~= nil
    end, { message = "the customize checklist never opened" })
    -- Move onto the row that names a plugin — the rows above it are the blurb.
    local row
    for i, line in ipairs(t:lines()) do
      if line:find("catppuccin", 1, true) then
        row = i
        break
      end
    end
    btv.test.expect(row).never.to_be_nil()
    t:feed(row .. "G")
    local before = t:line(row)
    t:feed("<Space>")
    btv.test.expect(t:line(row)).never.to_be(before)
    -- …and toggling back restores it.
    t:feed("<Space>")
    btv.test.expect(t:line(row)).to_be(before)
    t:feed("<Esc>")
  end)

  -- "A recommended spec is DATA + STRING-form hooks only … `config`/`init` must be
  --  a STRING of Lua (not a function)."
  btv.test.it("the recommended specs use string-form hooks", function(t)
    open(t)
    local set = btv.plugins.recommended and btv.plugins.recommended() or nil
    if not set then
      return
    end
    for _, spec in ipairs(set) do
      if spec.config ~= nil then
        btv.test.expect(type(spec.config)).to_be("string")
      end
      if spec.init ~= nil then
        btv.test.expect(type(spec.init)).to_be("string")
      end
      -- "`desc` … is how a user decides whether to keep the plugin."
      btv.test.expect(type(spec.desc)).to_be("string")
    end
  end)

  -- "(Call btv.plugins.recommend({}) to suppress the welcome entirely.)"
  btv.test.it("recommend({}) suppresses the offer", function(t)
    open(t)
    btv.plugins.recommend({})
    t:feed("<Esc>")
    t:cmd("PluginsWelcome")
    t:sleep(80)
    btv.test.expect(btv.bo.filetype).never.to_be("btvpluginswelcome")
    -- Put the example's own set back for the rest of the suite.
    dofile(DIR .. "/init.lua")
  end)
end)
