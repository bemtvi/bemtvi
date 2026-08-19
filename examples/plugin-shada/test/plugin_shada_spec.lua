-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/plugin-shada
--
-- The demo's subject is ISOLATION — a namespace assigned from where the code
-- lives, which no other code can name — and PERSISTENCE across sessions. The first
-- is fully testable in one session and is where the guarantee actually lives; the
-- second is what a single `--test-plugin` run cannot span, so the spec checks the
-- store round-trips instead of crossing a restart.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

local printed = {}
do
  local real = print
  _G.print = function(...)
    printed[#printed + 1] = tostring((...))
    return real(...)
  end
end

dofile(DIR .. "/init.lua")

-- The bundled plugin lives under `pack/demo/start/`, which a real session
-- auto-sources from the CONFIG dir — and a `--test-plugin` run is hermetic, with no
-- config dir at all. Put its directory on the runtimepath and source it, which is
-- exactly what the pack convention does: its namespace is then assigned from where
-- the code lives, as the example's whole point requires.
local PLUGIN = DIR .. "/pack/demo/start/recent-files"
btv._add_rtp(PLUGIN)
dofile(PLUGIN .. "/plugin/recent-files.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/plugin-shada", function()
  -- "From init.lua, `btv.shada.plugin()` attributes to the config root, which maps
  --  to the reserved 'user' namespace."
  btv.test.it("the config's store is the reserved user namespace", function(t)
    open(t)
    -- The spec is a file in the same config directory, so it attributes the same
    -- way the config does.
    local store = btv.shada.plugin()
    btv.test.expect(store.namespace).never.to_be_nil()
    btv.test.expect(type(store.get)).to_be("function")
  end)

  -- ":Launches — how many times this config has been launched"
  btv.test.it(":Launches reports the counter the config bumped", function(t)
    open(t)
    t:cmd("Launches")
    local last = printed[#printed] or ""
    btv.test.expect(last).to_contain("config launches so far:")
    btv.test.expect(tonumber(last:match("(%d+)$")) >= 1).to_be(true)
  end)

  -- "read last session's value, bump, persist"
  btv.test.it("the counter round-trips through the store", function(t)
    open(t)
    local store = btv.shada.plugin()
    local before = store:get("launches")
    btv.test.expect(type(before)).to_be("number")
    store:set("launches", before + 41)
    btv.test.expect(store:get("launches")).to_be(before + 41)
    store:set("launches", before)
    btv.test.expect(store:get("launches")).to_be(before)
  end)

  -- "the bundled plugin under pack/demo/start/recent-files/ -> 'recent-files'"
  btv.test.it("the bundled plugin was auto-sourced from pack/", function(t)
    open(t)
    local known = btv.user_command.get()
    btv.test.expect(known["RecentFiles"]).to_be_truthy()
  end)

  -- "it remembers the files you open"
  btv.test.it(":RecentFiles lists what the plugin remembered", function(t)
    open(t)
    t:cmd("RecentFiles")
    local text = table.concat(printed, "\n")
    btv.test.expect(text).to_contain("recent-files")
    -- The sample was opened, so it is remembered.
    btv.test.expect(text).to_contain("sample.txt")
  end)

  btv.test.it("…and a newly-opened file goes to the front", function(t)
    open(t)
    local other = btv.test.tempdir() .. "/other.txt"
    btv.await(btv.fs.write(other, "hello\n"))
    t:cmd("e " .. other)
    t:cmd("e!")
    printed[#printed + 1] = "marker"
    t:cmd("RecentFiles")
    local text = table.concat(printed, "\n")
    btv.test.expect(text).to_contain("1. " .. other)
  end)

  -- "a sourced file can't name another namespace — that is the whole point of
  --  assigned namespaces."
  btv.test.it("a caller cannot name another plugin's namespace", function(t)
    open(t)
    local ok, err = pcall(btv.shada.plugin, "recent-files")
    btv.test.expect(ok).to_be(false)
    btv.test.expect(tostring(err)).to_contain("cannot claim")
    btv.test.expect(tostring(err)).to_contain("recent-files")
  end)

  btv.test.it("…so the two stores cannot see each other", function(t)
    open(t)
    local mine = btv.shada.plugin()
    -- The config's own store holds `launches` and nothing the plugin wrote.
    local keys = {}
    for _, k in ipairs(mine:keys()) do
      keys[k] = true
    end
    btv.test.expect(keys["launches"]).to_be_truthy()
    btv.test.expect(keys["files"]).to_be_falsy()
  end)

  -- "Plugin data is also walled off from the core editor shada."
  btv.test.it("plugin data is walled off from the editor's own shada", function(t)
    open(t)
    local mine = btv.shada.plugin()
    for _, k in ipairs(mine:keys()) do
      -- No register / mark / history keys leak in.
      btv.test.expect(k).never.to_be("registers")
      btv.test.expect(k).never.to_be("marks")
    end
    -- …and the namespace is listed among the plugin namespaces, not the core's.
    local namespaces = {}
    for _, ns in ipairs(btv.shada.namespaces()) do
      namespaces[ns] = true
    end
    btv.test.expect(namespaces[mine.namespace]).to_be_truthy()
  end)
end)
