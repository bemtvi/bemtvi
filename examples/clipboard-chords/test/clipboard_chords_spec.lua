-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/clipboard-chords
--
-- Every numbered section is typed exactly as its Type-this line says. `"+` is the
-- SYSTEM clipboard, which under the test runner is the in-memory double
-- `btv.test.clipboard` reads and seeds — so a copy really does leave the editor
-- and a paste really does come back through it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
  btv.test.clipboard.clear()
end

--- Put the cursor on the first line containing `needle`.
local function goto_line(t, needle)
  for i, line in ipairs(t:lines()) do
    if line:find(needle, 1, true) then
      t:feed(i .. "G")
      return i
    end
  end
  error("no line containing " .. needle, 0)
end

btv.test.describe("examples/clipboard-chords", function()
  btv.test.it("the chords ship bound, with no config", function(t)
    open(t)
    -- The config file itself sets no mapping — every override in it is commented
    -- out — so anything bound here is the shipped default.
    local visual, normal = {}, {}
    for _, m in ipairs(t:keymaps("v")) do
      visual[m.lhs] = true
    end
    for _, m in ipairs(t:keymaps("n")) do
      normal[m.lhs] = true
    end
    btv.test.expect(visual["<C-c>"]).to_be_truthy()
    btv.test.expect(visual["<C-S-c>"]).to_be_truthy()
    btv.test.expect(normal["<C-v>"]).to_be_truthy()
    btv.test.expect(normal["<C-S-v>"]).to_be_truthy()
  end)

  -- 1. "vee<C-c>  … j0<C-v>"
  btv.test.it("§1 — <C-c> copies the selection to the SYSTEM clipboard", function(t)
    open(t)
    goto_line(t, "copy me please")
    t:feed("^vee<C-c>")
    btv.test.expect(btv.test.clipboard.peek()).to_be("copy me")
    -- The unnamed register is mirrored too — vim sets `""` on any yank whatever
    -- the explicit target — so a plain `p` still works right after the chord.
    btv.test.expect(vim.fn.getreg('"')).to_be("copy me")
  end)

  btv.test.it("§1 — <C-v> pastes AT the cursor, the way `P` does", function(t)
    open(t)
    goto_line(t, "copy me please")
    t:feed("^vee<C-c>")
    local target = goto_line(t, "paste target ->")
    t:feed("^<C-v>")
    -- At the cursor, pushing the rest right — not after it, as `p` would.
    btv.test.expect(t:line(target)).to_match("^%s*copy mepaste target %->")
  end)

  btv.test.it("§1 — a linewise copy pastes as whole lines, above the cursor", function(t)
    open(t)
    local first = goto_line(t, "first line of the pair")
    t:feed("Vj<C-c>")
    btv.test.expect(btv.test.clipboard.peek()).to_contain("first line of the pair")
    btv.test.expect(btv.test.clipboard.peek()).to_contain("second line of the pair")
    t:feed(first .. "G<C-v>")
    -- Two whole lines inserted above, so the original pair moved down by two.
    btv.test.expect(t:line(first)).to_contain("first line of the pair")
    btv.test.expect(t:line(first + 1)).to_contain("second line of the pair")
    btv.test.expect(t:line(first + 2)).to_contain("first line of the pair")
  end)

  -- 2. "A << <C-v> >><Esc>"
  btv.test.it("§2 — <C-v> in insert drops the clipboard at the caret", function(t)
    open(t)
    btv.test.clipboard.seed("PASTED", false)
    local line = goto_line(t, "append after me:")
    t:feed("A << <C-v> >><Esc>")
    btv.test.expect(t:line(line)).to_contain("<< PASTED >>")
    -- Insert mode continued: what was typed after the paste landed after it.
    btv.test.expect(t:line(line)).to_match("PASTED >>$")
    btv.test.expect(t:mode()).to_be("n")
  end)

  -- 5. ":e <C-v>" — the chord is bound on the command line too.
  btv.test.it("§5 — <C-v> inserts into the command line", function(t)
    open(t)
    goto_line(t, "examples/clipboard-chords/sample.txt")
    t:feed("0v$<C-c>")
    btv.test.expect(btv.test.clipboard.peek()).to_contain("sample.txt")
    t:feed(":e <C-v>")
    -- The path is on the command line, not yet run.
    btv.test.expect(btv._ui.cmdline).to_contain("sample.txt")
    t:feed("<Esc>")
    btv.test.expect(t:mode()).to_be("n")
  end)

  -- 6. ":registers" shows what the chords put on `"+`.
  btv.test.it("§6 — the \"+ row shows what the chord copied", function(t)
    open(t)
    goto_line(t, "copy me please")
    t:feed("^vee<C-c>")
    t:cmd("registers")
    local rows = table.concat(t:lines(), "\n")
    btv.test.expect(rows).to_contain('"+')
    btv.test.expect(rows).to_contain("copy me")
    t:feed("q")
  end)

  -- 3. "The chords are registered as `default = true` maps, so any map of your own
  --     on the same key wins — no need to unmap first."
  btv.test.it("§3 — a config map on the same key wins over the default", function(t)
    open(t)
    btv.test.clipboard.seed("SHOULD-NOT-PASTE", false)
    local hit = 0
    btv.keymap.set("n", "<C-v>", function()
      hit = hit + 1
    end, { desc = "Hijacked paste" })
    local before = t:lines()
    t:feed("<C-v>")
    btv.test.expect(hit).to_be(1)
    btv.test.expect(t:lines()).to_equal(before)
  end)

  -- 4. "Map it to an empty function: the key becomes a no-op rather than falling
  --     back to the built-in."
  btv.test.it("§4 — an empty map turns a chord off rather than falling through", function(t)
    open(t)
    btv.keymap.set("v", "<C-c>", function() end, { desc = "Disable the copy chord" })
    btv.test.clipboard.seed("UNCHANGED", false)
    goto_line(t, "copy me please")
    t:feed("^vee<C-c>")
    btv.test.expect(btv.test.clipboard.peek()).to_be("UNCHANGED")
    t:feed("<Esc>")
  end)

  -- The commented-out cut chord in §5, added the way the note shows.
  btv.test.it("§5 — the suggested <C-x> cut chord works as written", function(t)
    open(t)
    btv.keymap.set("v", "<C-x>", '"+d', { desc = "Cut the selection to the clipboard" })
    local line = goto_line(t, "copy me please")
    t:feed("^vee<C-x>")
    btv.test.expect(btv.test.clipboard.peek()).to_be("copy me")
    btv.test.expect(t:line(line)).to_match("^%s* please$")
  end)
end)
