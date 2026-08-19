-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/lspconfig
--
-- This tour's first step is `:PluginSync` — it CLONES `bemtvi-lspconfig` from the
-- network — and its `init.lua` `require`s that plugin at load. A spec suite must
-- not clone anything, so the config is sourced under a guard: with the plugin
-- present every case runs against it, and without it the suite reports the one
-- fact it can (that the config declines to load, loudly, naming what is missing)
-- rather than pretending.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

--- Load the config, tolerating an un-cloned plugin.
local loaded, load_err = pcall(dofile, DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.lua")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

btv.test.describe("examples/lspconfig", function()
  btv.test.it("the config loads, or says which plugin it is missing", function(t)
    open(t)
    if loaded then
      btv.test.expect(package.loaded["bemtvi-lspconfig"]).never.to_be(nil)
      return
    end
    -- Not cloned: the failure names the plugin rather than dying anonymously.
    btv.test.expect(tostring(load_err)).to_contain("bemtvi-lspconfig")
    print("skip: bemtvi-lspconfig is not cloned (run :PluginSync in a real session)")
  end)

  -- "1. Install the plugin, and enable ONE server." — `btv.plugins` is what the
  -- config declares, whether or not the clone has happened yet.
  btv.test.it("the two plugins the tour needs are declared", function(t)
    open(t)
    if not loaded then
      return
    end
    local declared = {}
    for _, spec in ipairs(btv.plugins.list()) do
      declared[spec.name or spec[1]] = true
    end
    btv.test.expect(declared["bemtvi/bemtvi-lspconfig"] or declared["bemtvi-lspconfig"]).to_be(true)
    btv.test.expect(declared["bemtvi/bemtvi-help"] or declared["bemtvi-help"]).to_be(true)
  end)

  -- "2. Override a bundled config. Your table is deep-merged OVER the bundled one."
  btv.test.it("the lua_ls override is merged, not a replacement", function(t)
    open(t)
    if not loaded then
      return
    end
    local cfg = btv.lsp.get_config("lua_ls")
    -- What this file set…
    btv.test.expect(cfg.settings.Lua.runtime.version).to_be("Lua 5.4")
    btv.test.expect(cfg.settings.Lua.diagnostics.globals).to_equal({ "btv", "vim" })
    btv.test.expect(cfg.settings.Lua.hint.enable).to_be(true)
    -- …and what it did NOT: the bundled `cmd` / `filetypes` / markers survive.
    btv.test.expect(cfg.cmd).never.to_be(nil)
    btv.test.expect(cfg.filetypes).to_contain("lua")
    btv.test.expect(#cfg.root_markers > 0).to_be(true)
  end)

  -- "3. The '*' layer — settings every server inherits."
  btv.test.it("the star layer carries an on_attach for every server", function(t)
    open(t)
    if not loaded then
      return
    end
    btv.test.expect(type(btv.lsp.get_config("*").on_attach)).to_be("function")
  end)

  -- "8. `:lua print(#require('bemtvi-lspconfig').available())` → 407"
  btv.test.it("the plugin bundles a config per server, lua_ls among them", function(t)
    open(t)
    if not loaded then
      return
    end
    local lspconfig = require("bemtvi-lspconfig")
    btv.test.expect(#lspconfig.available() > 300).to_be(true)
    local lua_servers = table.concat(lspconfig.for_filetype("lua"), ", ")
    btv.test.expect(lua_servers).to_contain("lua_ls")
  end)

  -- "4. The convenience path: setup() … the keymaps it installs."
  btv.test.it("setup() installed the extended keymap set", function(t)
    open(t)
    if not loaded then
      return
    end
    local normal = {}
    for _, m in ipairs(t:keymaps("n")) do
      normal[m.lhs] = true
    end
    for _, lhs in ipairs({ "grn", "gra", "grr", "gri", "grt", "gO", "<C-]>" }) do
      btv.test.expect(normal[lhs]).to_be(true)
    end
  end)

  -- "6. A few conveniences for driving the steps above."
  btv.test.it("the driving maps are wired", function(t)
    open(t)
    if not loaded then
      return
    end
    local normal = {}
    for _, m in ipairs(t:keymaps("n")) do
      normal[m.lhs] = m
    end
    btv.test.expect(normal["\\li"]).never.to_be(nil)
    btv.test.expect(normal["\\li"].desc).to_be("LSP: clients on this buffer")
    btv.test.expect(normal["\\lr"]).never.to_be(nil)
  end)

  btv.test.it("the sample is a lua buffer, so lua_ls applies to it", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("lua")
  end)
end)
