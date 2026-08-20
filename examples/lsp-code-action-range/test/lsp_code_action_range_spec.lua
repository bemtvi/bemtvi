-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/lsp-code-action-range
--
-- The refactors themselves are gopls's, and a loaded Go module takes far longer
-- than a spec suite may run — so nothing here waits for the chooser. What is
-- pinned is the part that belongs to the editor: WHICH RANGE the request carries.
-- `btv.lsp.code_action` takes it from an explicit `opts.range`, else the live
-- selection, else a point at the cursor — and that choice is observable without a
-- server, because the request is built before it is sent.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, with anything a previous case's request left on screen gone
--- first. A code-action reply lands on a LATER tick — with a real gopls attached
--- it can arrive mid-case and open its chooser over the next one — so each case
--- starts from a settled screen rather than assuming one.
local function open(t)
  t:feed("<Esc>")
  t:feed("<Esc>")
  t:wait_for(function()
    return t:menu() == nil
  end, { tries = 200, interval = 20, message = "a chooser outlived the previous case" })
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.go")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

--- Install the config's own `on_attach` maps against this buffer (the spawn that
--- would normally call it needs a warmed-up gopls).
local function attach(t)
  local buf = btv.buf.current()
  t:exec(function()
    btv.lsp.get_config("gopls").on_attach(nil, buf)
  end)
  return buf
end

btv.test.describe("examples/lsp-code-action-range", function()
  -- "Attach gopls to `go` buffers. `go.mod` is the root marker."
  btv.test.it("gopls is registered for go buffers with go.mod as its root", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("go")
    local cfg = btv.lsp.get_config("gopls")
    btv.test.expect(cfg.cmd).to_equal({ "gopls" })
    btv.test.expect(cfg.root_markers).to_equal({ "go.mod", ".git" })
  end)

  -- "The keymap is set for BOTH normal and visual mode, and that is the whole trick."
  btv.test.it("\\ca is bound in normal AND visual mode", function(t)
    open(t)
    local buf = attach(t)
    local normal, visual = {}, {}
    for _, m in ipairs(btv.keymap.buf_get(buf, "n")) do
      normal[m.lhs] = true
    end
    for _, m in ipairs(btv.keymap.buf_get(buf, "v")) do
      visual[m.lhs] = true
    end
    btv.test.expect(normal["\\ca"]).to_be(true)
    btv.test.expect(visual["\\ca"]).to_be(true)
    -- "`<leader>cr` … ranges compose with the kind filter" — also both modes.
    btv.test.expect(normal["\\cr"]).to_be(true)
    btv.test.expect(visual["\\cr"]).to_be(true)
    -- "`<leader>cx` … from wherever the cursor is, with no selection at all."
    btv.test.expect(normal["\\cx"]).to_be(true)
    btv.test.expect(visual["\\cx"]).to_be(nil)
    btv.test.expect(normal["K"]).to_be(true)
  end)

  -- "pressed in Visual, `btv.lsp.code_action()` finds the live selection and sends
  --  it as the request's range (then consumes it, dropping to Normal)."
  btv.test.it("\\ca in Visual consumes the selection and leaves the mode", function(t)
    open(t)
    attach(t)
    t:feed("9G")
    t:feed("V3j")
    btv.test.expect(t:mode()).to_be("V")
    t:feed("<Bslash>ca")
    -- Whatever the server does with it, the selection is spent: vim drops to
    -- Normal for any command that acts on one.
    t:wait_for(function()
      return t:mode() == "n"
    end, { message = "the selection was never consumed" })
    -- …and the selection marks record the lines it was built from, so `gv`
    -- reselects exactly what the request carried.
    t:feed("gv")
    btv.test.expect(t:mode()).to_be("V")
    btv.test.expect(t:cursor()[1]).to_be(12)
    t:feed("o")
    btv.test.expect(t:cursor()[1]).to_be(9)
    t:feed("<Esc>")
  end)

  -- "Press `<leader>ca` in NORMAL mode … a point request at the cursor."
  btv.test.it("\\ca in Normal leaves the buffer and the mode alone", function(t)
    open(t)
    attach(t)
    local before = table.concat(t:lines(), "\n")
    t:feed("5G")
    t:feed("<Bslash>ca")
    btv.test.expect(t:mode()).to_be("n")
    btv.test.expect(t:cursor()[1]).to_be(5)
    btv.test.expect(table.concat(t:lines(), "\n")).to_be(before)
  end)

  -- "3. Select the same lines and type `:` … then `LspCodeAction<CR>`."
  btv.test.it("the ex form takes the addressed lines", function(t)
    open(t)
    attach(t)
    -- One feed: `:` prefills the selection's address only while the selection is
    -- live, so nothing may settle between them.
    t:feed("9GV3j:")
    btv.test.expect(t:cmdline()).to_be(":'<,'>")
    t:feed("LspCodeAction<CR>")
    t:wait_for(function()
      return t:mode() == "n"
    end, { message = "the ex form never returned to Normal" })
  end)

  -- "`<leader>cx` runs the same request with an EXPLICIT range … from wherever the
  --  cursor is, with no selection at all."
  btv.test.it("\\cx needs neither a selection nor a cursor position", function(t)
    open(t)
    attach(t)
    local before = table.concat(t:lines(), "\n")
    t:feed("gg")
    t:feed("<Bslash>cx")
    btv.test.expect(t:mode()).to_be("n")
    btv.test.expect(t:cursor()[1]).to_be(1)
    btv.test.expect(table.concat(t:lines(), "\n")).to_be(before)
  end)

  -- "Rows 8..11 end-exclusive = the `sum := 0` … `}` block (file lines 9-12)."
  btv.test.it("the explicit range really is the block the notes name", function(t)
    open(t)
    btv.test.expect(t:line(9)).to_contain("sum := 0")
    btv.test.expect(t:line(12)).to_contain("}")
  end)

  -- The one cheap live check.
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
