-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/phase4-config
--
-- The whole playground is about the PAUSE: a key that is a live prefix of a
-- mapping is withheld until the next keystroke, and a client's `'timeoutlen'`
-- timer nudges the server to resolve it on idle. `t:idle()` is that nudge — the
-- spec's way of saying "the user stopped typing here".

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

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/phase4-config", function()
  -- 1. "gg then STOP … after ~1s the cursor jumps to the FIRST line."
  btv.test.it("§1 — an ambiguous mapped prefix is HELD, then flushes", function(t)
    open(t)
    t:feed("G")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    t:feed("gg")
    -- Held: `gg` is a live prefix of the `ggx` MAPPING, so nothing has happened.
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    t:idle()
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)

  -- "TYPE `ggx` quickly instead and the MAPPING fires."
  btv.test.it("§1 — …but ggx typed through fires the mapping", function(t)
    open(t)
    t:feed("G")
    local at = t:cursor()[1]
    t:feed("ggx")
    btv.test.expect(last_print()).to_contain("ggx fired")
    -- The mapping ran instead of the built-in: the cursor never moved.
    btv.test.expect(t:cursor()[1]).to_be(at)
  end)

  -- 2. "j then STOP -> after ~1s 'j (the shorter map)'."
  btv.test.it("§2 — an ambiguous short/long pair resolves to the SHORTER map", function(t)
    open(t)
    printed[#printed + 1] = "marker"
    t:feed("j")
    btv.test.expect(last_print()).to_be("marker")
    t:idle()
    btv.test.expect(last_print()).to_be("j (the shorter map)")
  end)

  btv.test.it("§2 — …and jk typed through takes the LONGER map", function(t)
    open(t)
    t:feed("jk")
    btv.test.expect(last_print()).to_be("jk (the longer map)")
  end)

  -- 3. "`,` … is marked nowait, so it fires the INSTANT you press it."
  btv.test.it("§3 — nowait fires without waiting for the longer map", function(t)
    open(t)
    t:feed(",")
    btv.test.expect(last_print()).to_contain("comma (nowait")
  end)

  btv.test.it("§3 — …so the longer ,x map can never be reached", function(t)
    open(t)
    t:feed(",x")
    -- The comma fired first; the `x` that followed is an ordinary delete.
    btv.test.expect(last_print()).to_contain("comma (nowait")
    btv.test.expect(last_print()).never.to_contain("you won't see this")
  end)

  -- 4. "<Space>n -> the message. <Space>q -> nothing on the command line, BUT the
  --     output is still logged."
  btv.test.it("§4 — a silent mapping keeps the command line clean", function(t)
    open(t)
    t:feed("<Space>n")
    btv.test.expect(t:message()).to_contain("not silent: you can read me")
    t:feed("<Space>q")
    btv.test.expect(t:message()).never.to_contain("silent: only in :messages")
    -- "…the output is still logged": the print really did run.
    btv.test.expect(last_print()).to_be("silent: only in :messages")
  end)

  -- 5. "<Space>u -> 'original <leader>u (the unique re-map was refused)'."
  btv.test.it("§5 — unique refuses to clobber, and the original survives", function(t)
    open(t)
    t:feed("<Space>u")
    btv.test.expect(last_print()).to_contain("original <leader>u")
    -- The config's own breadcrumb says the clash was refused with E227.
    local breadcrumb
    for _, line in ipairs(printed) do
      if line:find("unique clash refused", 1, true) then
        breadcrumb = line
      end
    end
    btv.test.expect(breadcrumb).never.to_be_nil()
    btv.test.expect(breadcrumb).to_contain("E227")
  end)

  -- 6. "<expr>: the RHS function RETURNS the keys to feed, computed at press time."
  btv.test.it("§6 — an expr map's keys depend on state at press time", function(t)
    open(t)
    btv.test.expect(vim.g.expr_top).to_be(true)
    t:feed("G")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    t:feed("H")
    btv.test.expect(t:cursor()[1]).to_be(1)
    -- Flip the flag and the SAME key returns different keys.
    t:feed("<Space>f")
    btv.test.expect(last_print()).to_contain("bottom (G)")
    t:feed("H")
    btv.test.expect(t:cursor()[1]).to_be(#t:lines())
    -- Put it back for the next test.
    t:feed("<Space>f")
  end)

  -- "an <expr> RHS must only compute keys — if it tries to change the editor it
  --  raises a textlock error and feeds nothing."
  btv.test.it("§6 — an expr RHS that mutates is refused, and feeds nothing", function(t)
    open(t)
    t:feed("G")
    local at = t:cursor()[1]
    btv.keymap.set("n", "<Space>x", function()
      vim.cmd("normal! gg")
      return "gg"
    end, { expr = true })
    t:feed("<Space>x")
    btv.test.expect(t:cursor()[1]).to_be(at)
  end)
end)
