-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/diagnostic-insert
--
-- The setting is about TIMING, so the spec types and then looks at the clock: it
-- checks what is on screen WHILE the keys are still coming, and what is there
-- after the quiet period or after `<Esc>`. The diagnostic layer is `t:decor()`.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text, and
--- wait for the BufEnter lint to seed.
local function open(t)
  t:cmd("DiagDebounce")
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("G")
  t:wait_for(function()
    return #btv.diagnostic.get(0) >= 0
  end, { message = "the demo linter never ran" })
end

--- How many diagnostics the editor is currently SHOWING (the applied set, not
--- whatever the linter last produced).
local function shown(t)
  local n = 0
  for row = 1, #t:screen() do
    if t:decor(row).diagnostic then
      n = n + 1
    end
  end
  return n
end

btv.test.describe("examples/diagnostic-insert", function()
  btv.test.it("the demo linter flags `bug` and `todo` with no language server", function(t)
    open(t)
    t:feed("Gobug and todo<Esc>")
    t:wait_for(function()
      return shown(t) > 0
    end, { message = "the linter flagged nothing" })
    local kinds = {}
    for _, d in ipairs(btv.diagnostic.get(0)) do
      kinds[d.severity] = (kinds[d.severity] or 0) + 1
    end
    btv.test.expect(kinds[btv.diagnostic.severity.ERROR] > 0).to_be(true)
    btv.test.expect(kinds[btv.diagnostic.severity.WARN] > 0).to_be(true)
  end)

  -- §2. "Nothing moves while the keys are coming; a second after you stop, the
  --      error appears — you are still in insert mode."
  btv.test.it("§2 — a debounced update is HELD while you type", function(t)
    open(t)
    t:feed("Go")
    local before = shown(t)
    t:feed(" -- bug", { insert = true })
    -- Straight after the keys: the linter has run (it fires per keystroke) but
    -- its answer is being held.
    btv.test.expect(shown(t)).to_be(before)
    btv.test.expect(t:mode()).to_be("i")
    t:feed("<Esc>")
  end)

  btv.test.it("§2 — …then lands on its own, still in insert mode", function(t)
    open(t)
    t:feed("Go")
    local before = shown(t)
    t:feed(" -- bug", { insert = true })
    t:wait_for(function()
      return shown(t) > before
    end, { tries = 300, interval = 20, message = "the held update never caught up" })
    -- No key was pressed to make it appear, and insert mode never ended.
    btv.test.expect(t:mode()).to_be("i")
    t:feed("<Esc>")
  end)

  -- "leaving insert mode applies it immediately whatever the interval is"
  btv.test.it("§2 — <Esc> applies the held update at once", function(t)
    open(t)
    t:feed("Go")
    local before = shown(t)
    t:feed(" -- bug", { insert = true })
    btv.test.expect(shown(t)).to_be(before)
    t:feed("<Esc>")
    btv.test.expect(shown(t) > before).to_be(true)
  end)

  -- §3. ":DiagLive → the error appears and re-flows on every single key."
  btv.test.it("§3 — :DiagLive applies every update as it lands", function(t)
    open(t)
    t:cmd("DiagLive")
    t:feed("Go")
    local before = shown(t)
    t:feed(" -- bug", { insert = true })
    btv.test.expect(shown(t) > before).to_be(true)
    btv.test.expect(t:mode()).to_be("i")
    t:feed("<Esc>")
    t:cmd("DiagDebounce")
  end)

  -- §3. ":DiagHold → nothing happens until you press <Esc>, however long you pause."
  btv.test.it("§3 — :DiagHold holds until InsertLeave, however long you wait", function(t)
    open(t)
    t:cmd("DiagHold")
    t:feed("Go")
    local before = shown(t)
    t:feed(" -- bug", { insert = true })
    t:sleep(1200)
    btv.test.expect(shown(t)).to_be(before)
    btv.test.expect(t:mode()).to_be("i")
    -- Nothing is lost: leaving insert applies the newest one.
    t:feed("<Esc>")
    btv.test.expect(shown(t) > before).to_be(true)
    t:cmd("DiagDebounce")
  end)

  btv.test.it("§3 — each command says which mode it put the editor in", function(t)
    open(t)
    t:cmd("DiagLive")
    btv.test.expect(t:message()).to_contain("every keystroke")
    t:cmd("DiagHold")
    btv.test.expect(t:message()).to_contain("InsertLeave")
    t:cmd("DiagDebounce")
    btv.test.expect(t:message()).to_contain("debounced")
  end)

  -- "while an update is held the newest one is kept"
  btv.test.it("the newest held update is the one that lands", function(t)
    open(t)
    t:cmd("DiagHold")
    t:feed("Go")
    local before = shown(t)
    -- Type a flag, then type it away again: the newest answer has no flag, so
    -- nothing appears on leaving insert.
    t:feed(" -- bug", { insert = true })
    t:feed("<BS><BS><BS>", { insert = true })
    t:feed("<Esc>")
    btv.test.expect(shown(t)).to_be(before)
    t:cmd("DiagDebounce")
  end)

  -- §1. The three surfaces the config turned on.
  btv.test.it("§1 — signs, inline messages and squiggles are all on", function(t)
    open(t)
    t:feed("Gobug here<Esc>")
    t:wait_for(function()
      return shown(t) > 0
    end, { message = "nothing was flagged" })
    local row
    for i, text in ipairs(t:screen()) do
      if text:find("bug here", 1, true) then
        row = i
      end
    end
    btv.test.expect(t:decor(row).sign).to_be("E")
    btv.test.expect(t:decor(row).diagnostic).to_contain("the word `bug` is not allowed")
  end)
end)
