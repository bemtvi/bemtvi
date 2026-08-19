-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-complete-lsp
--
-- The tour needs `lua-language-server`, and the notes say it takes ~20s to index
-- before it answers anything — far longer than a spec suite may run. So what is
-- pinned here is everything that is true the moment the config loads: the engine's
-- sources and gates, the server registration, and the buffer-local maps `on_attach`
-- installs. The live completions and the docs float are covered natively, in the
-- server's LSP-completion suite; a machine with the binary also gets the one cheap
-- live check — that the server actually spawns.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.lua")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/ui-complete-lsp", function()
  -- "Enable completion with the `lsp` source first (priority 100, above `buffer`),
  --  and the native `buffer` word-scan as a fallback."
  btv.test.it("the engine draws from lsp first and buffer second", function(t)
    open(t)
    local cfg = btv.complete._config
    btv.test.expect(cfg.builtins.lsp).never.to_be(nil)
    btv.test.expect(cfg.builtins.buffer).never.to_be(nil)
    -- The `lsp` source outranks the buffer scan…
    btv.test.expect(cfg.builtins.lsp.priority > cfg.builtins.buffer.priority).to_be(true)
    -- …and each carries the gate the config asked for.
    btv.test.expect(cfg.builtins.buffer.min_chars).to_be(2)
    btv.test.expect(cfg.top_min).to_be(1)
  end)

  -- "`docs = true` is the default … `docs_wrap` (default true)"
  btv.test.it("the docs sidebar is left on, wrapping", function(t)
    open(t)
    btv.test.expect(btv.complete._config.docs).never.to_be(false)
    btv.test.expect(btv.complete._config.docs_wrap).never.to_be(false)
  end)

  -- "btv.lsp.config registers the server, btv.lsp.enable activates it."
  btv.test.it("lua_ls is registered with the cmd and markers the notes name", function(t)
    open(t)
    local cfg = btv.lsp.get_config("lua_ls")
    btv.test.expect(cfg.cmd).to_equal({ "lua-language-server" })
    btv.test.expect(cfg.filetypes).to_equal({ "lua" })
    btv.test.expect(cfg.root_markers).to_equal({ ".luarc.json", ".luarc.jsonc", ".git" })
    btv.test.expect(type(cfg.on_attach)).to_be("function")
  end)

  btv.test.it("the sample really is a lua buffer, so the server applies to it", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("lua")
  end)

  -- "on_attach runs once the server has bound the buffer — the place to set
  --  buffer-local LSP keymaps."
  btv.test.it("on_attach installs every documented buffer-local map", function(t)
    open(t)
    local buf = btv.buf.current()
    -- Run the config's own `on_attach` against this buffer: the maps it installs
    -- are the thing under test, not the spawn that would normally call it.
    t:exec(function()
      btv.lsp.get_config("lua_ls").on_attach(nil, buf)
    end)
    local normal = {}
    for _, m in ipairs(btv.keymap.buf_get(buf, "n")) do
      normal[m.lhs] = true
    end
    for _, lhs in ipairs({ "gd", "gr", "gO", "\\ws", "K", "\\rn", "\\ca", "\\ih" }) do
      btv.test.expect(normal[lhs]).to_be(true)
    end
    -- "<C-k> signature help for the call under the cursor" — insert mode.
    local insert = {}
    for _, m in ipairs(btv.keymap.buf_get(buf, "i")) do
      insert[m.lhs] = true
    end
    btv.test.expect(insert["<C-k>"]).to_be(true)
  end)

  -- "Inlay hints are off by default — turn them on for this buffer."
  btv.test.it("on_attach turns inlay hints on, and the toggle flips them", function(t)
    open(t)
    local buf = btv.buf.current()
    t:exec(function()
      btv.lsp.get_config("lua_ls").on_attach(nil, buf)
    end)
    btv.test.expect(btv.lsp.inlay_hint.is_enabled({ bufnr = buf })).to_be(true)
    t:feed("<Bslash>ih")
    btv.test.expect(btv.lsp.inlay_hint.is_enabled({ bufnr = buf })).to_be(false)
    t:feed("<Bslash>ih")
    btv.test.expect(btv.lsp.inlay_hint.is_enabled({ bufnr = buf })).to_be(true)
  end)

  -- The one live check that is cheap: with the binary installed, opening a `lua`
  -- buffer spawns the server. (Answering takes ~20s of indexing — the notes say
  -- so — which is why nothing here waits for a completion.)
  btv.test.it("the server spawns for a lua buffer when it is installed", function(t)
    open(t)
    local spawned = false
    for _ = 1, 100 do
      if #btv.lsp.clients() > 0 then
        spawned = true
        break
      end
      t:sleep(20)
    end
    if not spawned then
      print("skip: lua-language-server is not installed")
      return
    end
    local names = {}
    for _, c in ipairs(btv.lsp.clients()) do
      names[c.name] = true
    end
    btv.test.expect(names["lua_ls"]).to_be(true)
  end)
end)
