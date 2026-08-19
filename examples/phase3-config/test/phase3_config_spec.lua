-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/phase3-config
--
-- Each numbered section is typed exactly as its TYPE line says.

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

local function last_print()
  return printed[#printed] or ""
end

--- The buffer the config's buffer-local map was set on: the one that was current
--- when `init.lua` ran.
local config_buf = btv.buf.current()

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/phase3-config", function()
  -- 1. "i then type some text, then jk -> you leave insert mode"
  btv.test.it("§1 — the insert-mode jk map leaves insert, inserting nothing", function(t)
    open(t)
    t:feed("1G0")
    local before = t:line(1)
    t:feed("i")
    t:feed("hello", { insert = true })
    t:feed("jk")
    btv.test.expect(t:mode()).to_be("n")
    -- Exactly what was typed, and nothing the mapping's LHS spelled: the sample's
    -- own prose happens to mention `jk`, so the check is the whole line.
    btv.test.expect(t:line(1)).to_be("hello" .. before)
  end)

  -- "A lone j (not followed by k) still inserts a literal j."
  btv.test.it("§1 — a lone j still inserts a literal j", function(t)
    open(t)
    t:feed("1G0")
    local before = t:line(1)
    t:feed("i")
    t:feed("aja", { insert = true })
    t:feed("<Esc>")
    btv.test.expect(t:line(1)).to_be("aja" .. before)
  end)

  -- 2. ": then qq -> the command line reads ':quit'"
  btv.test.it("§2 — the command-line map expands on the : line only", function(t)
    open(t)
    t:feed(":qq")
    btv.test.expect(btv._ui.cmdline).to_contain("quit")
    t:feed("<Esc>")
    -- In normal mode `qq` is unmapped — it records a macro into `q` instead.
    btv.test.expect(t:mode()).to_be("n")
  end)

  -- 3. "<Space>b -> 'buffer-local: only in sample.txt' … Open another buffer and
  --     press <Space>b there: nothing fires."
  btv.test.it("§3 — a buffer-local map fires only in its buffer", function(t)
    t:cmd("b " .. config_buf)
    t:feed("<Space>b")
    btv.test.expect(last_print()).to_be("buffer-local: only in sample.txt")
    printed[#printed + 1] = "marker"
    t:cmd("enew")
    t:feed("<Space>b")
    btv.test.expect(last_print()).to_be("marker")
    -- Coming back, it works again.
    t:cmd("b " .. config_buf)
    t:feed("<Space>b")
    btv.test.expect(last_print()).to_be("buffer-local: only in sample.txt")
  end)

  -- 4. "<Space>g -> nothing (the map was removed at startup)"
  btv.test.it("§4 — vim.keymap.del really removes the map", function(t)
    open(t)
    printed[#printed + 1] = "marker"
    t:feed("<Space>g")
    btv.test.expect(last_print()).to_be("marker")
    local mapped = {}
    for _, m in ipairs(t:keymaps("n")) do
      mapped[m.lhs] = true
    end
    btv.test.expect(mapped["<Space>g"]).to_be_falsy()
  end)

  -- 5. "T -> SEE message 'R action …' / U -> a literal R reaches core, NO message"
  btv.test.it("§5 — nvim_set_keymap is remappable by default", function(t)
    open(t)
    t:feed("T")
    btv.test.expect(last_print()).to_be("R action (low-level remap target)")
  end)

  btv.test.it("§5 — …and opts.noremap feeds the literal key", function(t)
    open(t)
    t:feed("1G0")
    printed[#printed + 1] = "marker"
    t:feed("U")
    -- A literal `R` reached core: replace-pending, so the next key overwrites.
    btv.test.expect(last_print()).to_be("marker")
    t:feed("Z")
    btv.test.expect(t:line(1):sub(1, 1)).to_be("Z")
    t:feed("<Esc>")
  end)
end)
