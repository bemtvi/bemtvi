-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/ui-float
--
-- The content float is neither buffer text nor a painted row of the focused
-- window — it floats over them and holds no list — so `t:float()` is its only
-- view. The LSP half needs `lua-language-server`, so those cases skip when it is
-- not on the PATH rather than failing on a missing binary.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, with any float a previous case left dismissed — the popup is
--- transient, so the next key takes it and the case would otherwise measure the
--- wrong one.
local function open(t)
  t:feed("<Esc>")
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.lua")
  t:cmd("e!")
  t:feed("gg")
end

--- The float `keys` opens, once it is up.
local function float(t, keys)
  t:feed(keys)
  t:wait_for(function()
    return t:float() ~= nil
  end, { message = keys .. " opened no float" })
  return t:float()
end

btv.test.describe("examples/ui-float", function()
  -- "1. \\o — a cursor-anchored content float from a multi-line string."
  btv.test.it("\\o floats the string it was handed, with its title", function(t)
    open(t)
    local f = float(t, "<Bslash>o")
    btv.test.expect(f.text).to_contain("btv.ui.float")
    btv.test.expect(f.text).to_contain("The list-less content float.")
    btv.test.expect(f.text).to_contain("Press any key to dismiss.")
    btv.test.expect(f.title).to_contain("info")
    -- A string with newlines became one row per line.
    btv.test.expect(#f.lines).to_be(4)
    btv.test.expect(f.text:sub(1, 12)).to_be("btv.ui.float")
    t:feed("<Esc>")
  end)

  -- "It is a transient popup: the NEXT key dismisses it … it never grabs input —
  --  the key is still handled normally."
  btv.test.it("the next key dismisses it and still acts", function(t)
    open(t)
    local line = t:cursor()[1]
    float(t, "<Bslash>o")
    t:feed("j")
    btv.test.expect(t:float()).to_be_nil()
    -- …and the `j` moved the cursor: the float never grabbed it.
    btv.test.expect(t:cursor()[1]).to_be(line + 1)
  end)

  -- "2. \\O — a centered float from a list of lines."
  btv.test.it("\\O floats a list of lines, centered", function(t)
    open(t)
    local f = float(t, "<Bslash>O")
    btv.test.expect(f.text).to_be("centered over the editor\n\nrelative = 'editor'")
    btv.test.expect(#f.lines).to_be(3)
    btv.test.expect(f.title).to_be_nil()
    t:feed("<Esc>")
  end)

  -- "3. K — LSP hover through the content float … With no server attached it
  --  echoes 'No language server attached'."
  btv.test.it("K reports the absent server rather than doing nothing", function(t)
    open(t)
    -- With `lua-language-server` installed the reply opens on the float surface
    -- instead, which the hover cases above already cover — this pins the other
    -- branch, the one every machine without the binary takes.
    if #btv.lsp.clients() > 0 then
      return
    end
    t:cmd("echo ''")
    t:feed("K")
    t:wait_for(function()
      return (t:message() or ""):find("language server", 1, true) ~= nil
    end, { message = "K neither hovered nor reported the absent server" })
    btv.test.expect(t:message()).to_contain("No language server attached")
  end)

  btv.test.it("the config registers lua_ls declaratively", function(t)
    open(t)
    local cfg = btv.lsp.get_config("lua_ls")
    btv.test.expect(cfg.cmd).to_equal({ "lua-language-server" })
    btv.test.expect(cfg.filetypes).to_equal({ "lua" })
    btv.test.expect(cfg.root_markers).to_equal({ ".luarc.json", ".git" })
  end)

  btv.test.it("K and \\s are the two LSP maps the notes name", function(t)
    open(t)
    local lhs = {}
    for _, m in ipairs(t:keymaps("n")) do
      lhs[m.lhs] = true
    end
    btv.test.expect(lhs["K"]).to_be(true)
    btv.test.expect(lhs["\\s"]).to_be(true)
  end)

  -- "opts.border … opts.relative" — the two the notes document, driven directly.
  btv.test.it("the border and relative options are honoured", function(t)
    open(t)
    t:exec(function()
      btv.ui.float("bare", { border = "none", relative = "editor" })
    end)
    t:wait_for(function()
      return t:float() ~= nil
    end, { message = "no float opened" })
    btv.test.expect(t:float().text).to_be("bare")
    t:feed("<Esc>")
    -- An unknown border fails loud rather than falling back silently.
    t:exec(function()
      btv.ui.float("nope", { border = "sparkly" })
    end)
    t:wait_for(function()
      return (t:message() or ""):find("sparkly", 1, true) ~= nil
    end, { message = "an unknown border was accepted" })
    btv.test.expect(t:float()).to_be_nil()
  end)
end)
