-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/lsp-multi-server
--
-- The point of the tour is ROUTING between two servers, and seeing it needs both
-- of them up and a loaded Go module — far longer than a spec suite may run. So
-- what is pinned here is the configuration that decides the routing (both servers
-- enabled for the same filetype, gopls ranked above the linter) and the maps that
-- exercise it, plus whichever servers happen to be installed on the machine.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.go")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

--- The client names attached to this buffer, once any have arrived.
local function clients(t)
  for _ = 1, 100 do
    if #btv.lsp.clients({ bufnr = 0 }) > 0 then
      break
    end
    t:sleep(20)
  end
  local names = {}
  for _, c in ipairs(btv.lsp.clients({ bufnr = 0 })) do
    names[c.name] = c
  end
  return names
end

btv.test.describe("examples/lsp-multi-server", function()
  -- "Every server enabled for a filetype attaches."
  btv.test.it("both servers are configured for the same filetype", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("go")
    local go = btv.lsp.get_config("gopls")
    local lint = btv.lsp.get_config("golangci_lint")
    btv.test.expect(go.filetypes).to_equal({ "go" })
    btv.test.expect(lint.filetypes).to_equal({ "go" })
    btv.test.expect(go.cmd).to_equal({ "gopls" })
    btv.test.expect(lint.cmd).to_equal({ "golangci-lint-langserver" })
  end)

  -- "`priority` (section 5) decides who leads … Capability is the filter,
  --  priority is the preference."
  btv.test.it("gopls is ranked above the linter", function(t)
    open(t)
    btv.test.expect(btv.lsp.get_config("gopls").priority).to_be(10)
    -- The linter states none, so it takes the default — below gopls either way.
    local lint = btv.lsp.get_config("golangci_lint").priority
    btv.test.expect(lint == nil or lint < 10).to_be(true)
  end)

  -- "`--issues-exit-code=1` matters: the adapter reads a non-zero exit as 'there
  --  were findings', not as a failure."
  btv.test.it("the linter is told to report findings as a non-zero exit", function(t)
    open(t)
    local init = btv.lsp.get_config("golangci_lint").init_options
    btv.test.expect(table.concat(init.command, " ")).to_contain("--issues-exit-code=1")
    btv.test.expect(init.command[1]).to_be("golangci-lint")
  end)

  -- The maps the notes name, on the default `<Space>` leader.
  btv.test.it("every documented map is wired", function(t)
    open(t)
    local normal, visual = {}, {}
    for _, m in ipairs(t:keymaps("n")) do
      normal[m.lhs] = true
    end
    for _, m in ipairs(btv.keymap.get("v")) do
      visual[m.lhs] = true
    end
    for _, lhs in ipairs({
      "K",
      "gd",
      "\\ca",
      "\\lf",
      "\\lF",
      "\\lh",
      "\\lr",
      "\\li",
      "\\lc",
      "\\ld",
      "\\lp",
    }) do
      btv.test.expect(normal[lhs]).to_be(true)
    end
    btv.test.expect(visual["\\ca"]).to_be(true)
  end)

  -- "`<leader>li` — how many servers are on this buffer, and which."
  btv.test.it("<leader>li names the servers actually attached", function(t)
    open(t)
    local attached = clients(t)
    t:feed("<Bslash>li")
    t:wait_for(function()
      return (t:message() or ""):find("clients:", 1, true) ~= nil
    end, { message = "<leader>li printed nothing" })
    local said = t:message()
    local n = 0
    for name in pairs(attached) do
      btv.test.expect(said).to_contain(name)
      n = n + 1
    end
    btv.test.expect(said).to_contain(n .. " clients:")
  end)

  -- "`<leader>lc` — ask them what they advertise."
  btv.test.it("<leader>lc reports each server's capabilities", function(t)
    open(t)
    local attached = clients(t)
    if next(attached) == nil then
      print("skip: neither gopls nor golangci-lint-langserver is installed")
      return
    end
    t:feed("<Bslash>lc")
    t:wait_for(function()
      return (t:message() or ""):find("=>", 1, true) ~= nil
    end, { message = "<leader>lc printed nothing" })
    local said = t:message()
    for name in pairs(attached) do
      btv.test.expect(said).to_contain(name .. " =>")
    end
    -- "golangci-lint-langserver is a pure PUBLISHER: it answers no request at all."
    if attached["golangci_lint"] then
      btv.test.expect(said).to_contain("golangci_lint => (none)")
    end
    -- "gopls => hover, definition, references, …"
    if attached["gopls"] then
      btv.test.expect(said).to_contain("hover")
      btv.test.expect(said).to_contain("definition")
    end
  end)

  -- "`<leader>lp` — who leads, and why."
  btv.test.it("<leader>lp prints each attached server's priority", function(t)
    open(t)
    local attached = clients(t)
    if next(attached) == nil then
      print("skip: no Go language server is installed")
      return
    end
    t:feed("<Bslash>lp")
    t:wait_for(function()
      return (t:message() or ""):find("priority=", 1, true) ~= nil
    end, { message = "<leader>lp printed nothing" })
    if attached["gopls"] then
      btv.test.expect(t:message()).to_contain("gopls priority=10")
    end
  end)

  -- "`<leader>ld` — the merged diagnostics, each carrying who published it."
  btv.test.it("<leader>ld attributes each diagnostic to its publisher", function(t)
    open(t)
    clients(t)
    t:cmd("echo ''")
    t:feed("<Bslash>ld")
    t:wait_for(function()
      return (t:message() or "") ~= ""
    end, { message = "<leader>ld said nothing at all" })
    -- Either there are none yet (the servers are still reading the module) or each
    -- row names the client that published it.
    local said = t:message()
    if said:find("%d+: %[") then
      btv.test.expect(said).to_match("%d+: %[%a")
    end
  end)

  -- "`<leader>lF` … `{ name = 'nosuch' }`" — the loud path for a server that is
  -- not there at all.
  btv.test.it("<leader>lF names the server it could not find", function(t)
    open(t)
    clients(t)
    t:cmd("echo ''")
    t:feed("<Bslash>lF")
    t:wait_for(function()
      return (t:message() or "") ~= ""
    end, { message = "<leader>lF said nothing at all" })
    btv.test.expect(t:message()).to_contain("nosuch")
  end)
end)
