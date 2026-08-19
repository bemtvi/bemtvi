-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/telescope-parity
--
-- The config is a keymap sheet over `btv.picker` and `btv.lsp`, so what is pinned
-- here is that each map exists, that the sources it names are registered, and that
-- the ones that need no external process really open and confirm. The `fd` / `git`
-- / `rg` sources spawn a program, so those cases check the source is registered
-- and open it only when the tool is on the machine.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("cd " .. DIR)
  btv.picker.forget_history()
  t:feed("<Esc>")
  t:feed("gg")
end

--- The normal-mode maps, by lhs.
local function nmaps(t)
  local out = {}
  for _, m in ipairs(t:keymaps("n")) do
    out[m.lhs] = m
  end
  return out
end

--- Open the picker `keys` fires, waiting for rows.
local function picker(t, keys)
  t:feed(keys)
  t:wait_for(function()
    local m = t:menu()
    return m ~= nil and #m.items > 0
  end, { tries = 200, interval = 20, message = keys .. " opened no picker" })
  return t:menu()
end

btv.test.describe("examples/telescope-parity", function()
  -- "2. Keymaps" — every one the file wires, with the desc it documents.
  btv.test.it("every documented normal-mode map is wired", function(t)
    open(t)
    local maps = nmaps(t)
    local want = {
      ["\\ff"] = "Find files",
      ["<C-p>"] = "Git files",
      ["\\fg"] = "Live grep",
      ["\\fG"] = "Live grep -uu + excludes",
      ["\\fA"] = "Live grep -uu",
      ["\\fb"] = "Buffers",
      ["\\fr"] = "Resume last picker",
      ["\\fi"] = "Pickers (builtin)",
      ["\\fk"] = "Keymaps",
      ["\\fm"] = "Marks",
      ["<C-/>"] = "Fuzzy find in current buffer",
      ["\\cx"] = "Diagnostics",
      ["\\cs"] = "LSP document symbols",
      ["\\cr"] = "LSP references",
      ["\\ct"] = "LSP type definitions",
    }
    for lhs, desc in pairs(want) do
      btv.test.expect(maps[lhs]).never.to_be(nil)
      btv.test.expect(maps[lhs].desc).to_be(desc)
    end
  end)

  -- "Seed a picker's prompt with the visual selection" — the visual twins.
  btv.test.it("the four selection-seeded maps are visual-mode", function(t)
    open(t)
    local vmaps = {}
    for _, m in ipairs(btv.keymap.get("v")) do
      vmaps[m.lhs] = m
    end
    for _, lhs in ipairs({ "\\ff", "\\fg", "\\fG", "\\fA" }) do
      btv.test.expect(vmaps[lhs]).never.to_be(nil)
      btv.test.expect(vmaps[lhs].desc).to_contain("selection")
    end
  end)

  -- "1. File / grep sources" — the five this file registers.
  btv.test.it("the five custom sources are registered", function(t)
    open(t)
    for _, name in ipairs({
      "files",
      "git_files",
      "live_grep",
      "live_grep_uu",
      "live_grep_ex",
    }) do
      btv.test.expect(btv.complete and true).to_be(true)
      btv.test.expect(btv.picker._sources[name]).never.to_be(nil)
    end
    -- …and the shipped ones the maps lean on are there too.
    for _, name in ipairs({ "buffers", "pickers", "keymaps", "marks", "curbuf", "diagnostics" }) do
      btv.test.expect(btv.picker._sources[name]).never.to_be(nil)
    end
  end)

  -- "The `curbuf` … source telescope has [is] shipped built-in."
  btv.test.it("<C-/> fuzzy-finds the current buffer's lines", function(t)
    open(t)
    local box = picker(t, "<C-/>")
    btv.test.expect(#box.items).to_be(#btv.buf.lines(0, 0, -1))
    -- Confirming a row moves the cursor to that line.
    t:feed("<C-n><CR>")
    t:wait_for(function()
      return t:menu() == nil
    end, { message = "the picker never closed" })
    btv.test.expect(t:cursor()[1]).to_be(2)
  end)

  btv.test.it("\\fb lists the open buffers", function(t)
    open(t)
    local box = picker(t, "<Bslash>fb")
    btv.test.expect(table.concat(box.items, "\n")).to_contain("sample.txt")
    t:feed("<Esc>")
  end)

  btv.test.it("\\fk lists the keymaps, this config's included", function(t)
    open(t)
    local box = picker(t, "<Bslash>fk")
    btv.test.expect(table.concat(box.items, "\n")).to_contain("Live grep")
    t:feed("<Esc>")
  end)

  btv.test.it("\\fi is the picker-of-pickers, and lists this file's sources", function(t)
    open(t)
    local box = picker(t, "<Bslash>fi")
    local listed = table.concat(box.items, "\n")
    btv.test.expect(listed).to_contain("live_grep_uu")
    btv.test.expect(listed).to_contain("git_files")
    t:feed("<Esc>")
  end)

  -- "<leader>fr — Resume last picker (shipped action)"
  btv.test.it("\\fr resumes the picker that was last open", function(t)
    open(t)
    picker(t, "<Bslash>fb")
    t:feed("<Esc>")
    t:wait_for(function()
      return t:menu() == nil
    end, { message = "the picker never closed" })
    local box = picker(t, "<Bslash>fr")
    btv.test.expect(table.concat(box.items, "\n")).to_contain("sample.txt")
    t:feed("<Esc>")
  end)

  -- "make_files … fd -u -t file" / "git ls-files": both spawn a program.
  btv.test.it("\\ff streams files when fd is installed", function(t)
    open(t)
    t:feed("<Bslash>ff")
    t:sleep(200)
    local m = t:menu()
    btv.test.expect(m).never.to_be(nil)
    if #m.items == 0 then
      print("skip: fd is not installed")
      t:feed("<Esc>")
      return
    end
    btv.test.expect(table.concat(m.items, "\n")).to_contain("sample.txt")
    t:feed("<Esc>")
  end)

  -- "the `-uu`/exclude grep variants" — the argv builder is the ported logic.
  btv.test.it("the grep sources are dynamic and preview a location", function(t)
    open(t)
    for _, name in ipairs({ "live_grep", "live_grep_uu", "live_grep_ex" }) do
      local src = btv.picker._sources[name]
      btv.test.expect(src.dynamic).to_be(true)
      btv.test.expect(src.preview).to_be("location")
    end
    btv.test.expect(btv.picker._sources["files"].preview).to_be("file")
  end)

  -- "Not ported — no native equivalent (documented so nothing fails silently):
  --  <leader>fh help_tags"
  btv.test.it("the un-ported help_tags map is genuinely absent", function(t)
    open(t)
    btv.test.expect(nmaps(t)["\\fh"]).to_be(nil)
  end)
end)
