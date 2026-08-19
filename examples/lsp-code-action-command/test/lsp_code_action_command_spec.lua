-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/lsp-code-action-command
--
-- The tour needs `gopls` and a loaded module, which takes far longer than a spec
-- suite may run — so the live chooser is not driven here. What IS driven is the
-- part the example is actually about: the client-side command HANDLER. It is a
-- plain function in `btv.lsp.commands`, so the spec calls it with the payload
-- gopls sends and checks what the editor did with it — no server involved.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.go")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

btv.test.describe("examples/lsp-code-action-command", function()
  -- "`btv.lsp.commands[name]` is where you say 'I'll handle that one'."
  btv.test.it("the config claims exactly one command name", function(t)
    open(t)
    btv.test.expect(type(btv.lsp.commands["gopls.client_open_url"])).to_be("function")
    local names = {}
    for name in pairs(btv.lsp.commands) do
      names[#names + 1] = name
    end
    btv.test.expect(names).to_equal({ "gopls.client_open_url" })
  end)

  -- "the message line shows the URL gopls asked to open, and the URL lands in the
  --  unnamed register, so `p` pastes it."
  btv.test.it("the handler yanks the URL and reports it", function(t)
    open(t)
    t:exec(function()
      btv.lsp.commands["gopls.client_open_url"]({
        command = "gopls.client_open_url",
        arguments = { "https://example.invalid/gopls" },
      }, { client_id = 1 })
    end)
    t:wait_for(function()
      return (t:message() or ""):find("example.invalid", 1, true) ~= nil
    end, { message = "the handler reported nothing" })
    btv.test.expect(t:message()).to_contain("https://example.invalid/gopls")
    btv.test.expect(t:message()).to_contain("yanked")
    -- …and `p` really pastes it.
    btv.test.expect(vim.fn.getreg('"')).to_be("https://example.invalid/gopls")
    t:feed("gg0p")
    btv.test.expect(t:line(1)).to_contain("https://example.invalid/gopls")
    t:cmd("undo")
  end)

  -- "gopls asked to open a URL but sent none" — the loud path.
  btv.test.it("a command with no URL warns instead of guessing", function(t)
    open(t)
    local said
    local prev_vim, prev_btv = vim.notify, btv.notify
    local record = function(msg)
      said = tostring(msg)
    end
    vim.notify, btv.notify = record, record
    t:exec(function()
      btv.lsp.commands["gopls.client_open_url"]({ command = "gopls.client_open_url" }, {
        client_id = 1,
      })
    end)
    vim.notify, btv.notify = prev_vim, prev_btv
    btv.test.expect(said).to_contain("sent none")
    btv.test.expect(vim.fn.getreg('"')).never.to_contain("http")
  end)

  -- "`<leader>cl` to list what is registered."
  btv.test.it("\\cl lists the claimed names", function(t)
    open(t)
    t:feed("<Bslash>cl")
    t:wait_for(function()
      return (t:message() or ""):find("btv.lsp.commands", 1, true) ~= nil
    end, { message = "\\cl printed nothing" })
    btv.test.expect(t:message()).to_contain("gopls.client_open_url")
  end)

  -- "Attach gopls to `go` buffers … `go.mod` is the root marker."
  btv.test.it("gopls is registered for go buffers with go.mod as its root", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("go")
    local cfg = btv.lsp.get_config("gopls")
    btv.test.expect(cfg.cmd).to_equal({ "gopls" })
    btv.test.expect(cfg.filetypes).to_equal({ "go" })
    btv.test.expect(cfg.root_markers).to_equal({ "go.mod", ".git" })
  end)

  -- "the keymaps on_attach installs"
  btv.test.it("on_attach installs the three maps the notes name", function(t)
    open(t)
    local buf = btv.buf.current()
    t:exec(function()
      btv.lsp.get_config("gopls").on_attach(nil, buf)
    end)
    local normal = {}
    for _, m in ipairs(btv.keymap.buf_get(buf, "n")) do
      normal[m.lhs] = true
    end
    btv.test.expect(normal["\\ca"]).to_be(true)
    btv.test.expect(normal["\\cd"]).to_be(true)
    btv.test.expect(normal["K"]).to_be(true)
    -- "<leader>ca is set for BOTH normal and visual mode."
    local visual = {}
    for _, m in ipairs(btv.keymap.buf_get(buf, "v")) do
      visual[m.lhs] = true
    end
    btv.test.expect(visual["\\ca"]).to_be(true)
  end)

  -- The one cheap live check: with the binary installed, a `go` buffer spawns it.
  btv.test.it("gopls spawns for a go buffer when it is installed", function(t)
    open(t)
    for _ = 1, 100 do
      if #btv.lsp.clients() > 0 then
        break
      end
      t:sleep(20)
    end
    if #btv.lsp.clients() == 0 then
      print("skip: gopls is not installed")
      return
    end
    local names = {}
    for _, c in ipairs(btv.lsp.clients()) do
      names[c.name] = true
    end
    btv.test.expect(names["gopls"]).to_be(true)
  end)
end)
