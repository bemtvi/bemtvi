-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/input-method
--
-- What the option changes is the GUI's IME, which a headless spec can't drive.
-- What it can hold to account is the option itself: the value each step of the
-- notes leaves 'imdisable' at, which is what the GUI follows.

-- On Windows the chunk name is a `\\?\D:\…` path: drop the prefix and turn the
-- separators around, so the match below and the `dofile` both work.
local SRC = (debug.getinfo(1, "S").source:gsub("^@\\\\%?\\", "@"):gsub("\\", "/"))
local DIR = SRC:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/input-method", function()
  -- §3. "with this config as shipped, :ImeAuto is on … the IME is off in normal mode"
  btv.test.it("§3 — the IME starts off in normal mode", function(t)
    open(t)
    btv.test.expect(btv.o.imdisable).to_be(true)
  end)

  -- §3. "日本 is typed in insert mode, and `jj` moves again after <Esc>"
  btv.test.it("§3 — on in insert mode, off again after <Esc>", function(t)
    open(t)
    t:feed("i")
    btv.test.expect(btv.o.imdisable).to_be(false)
    t:feed("日本<Esc>")
    btv.test.expect(btv.o.imdisable).to_be(true)
    btv.test.expect(t:line(1)).to_contain("日本")
  end)

  -- §3. "on in insert, replace, select and terminal mode and on the command line"
  btv.test.it("§3 — on in replace mode and on the command line, off in visual", function(t)
    open(t)
    t:feed("R")
    btv.test.expect(btv.o.imdisable).to_be(false)
    t:feed("<Esc>v")
    btv.test.expect(btv.o.imdisable).to_be(true)
    t:feed("<Esc>")
    -- A command-line mapping reads the option while `:` is open.
    local seen
    btv.keymap.set("c", "<F9>", function()
      seen = btv.o.imdisable
    end)
    t:feed(":<F9><Esc>")
    btv.test.expect(seen).to_be(false)
    btv.test.expect(btv.o.imdisable).to_be(true)
  end)

  -- §3 → §1. ":ImeAuto → ImeAuto off, and the IME is back on everywhere"
  btv.test.it("§1 — :ImeAuto off leaves the IME on in every mode", function(t)
    open(t)
    t:cmd("ImeAuto")
    btv.test.expect(t:message()).to_contain("ImeAuto off")
    btv.test.expect(btv.o.imdisable).to_be(false)
    t:feed("i<Esc>v<Esc>")
    btv.test.expect(btv.o.imdisable).to_be(false)
  end)

  -- §2. "<F7> … the IME is off. <F7> once more turns it back on."
  btv.test.it("§2 — <F7> toggles it", function(t)
    open(t)
    t:cmd("ImeAuto")
    t:feed("<F7>")
    btv.test.expect(btv.o.imdisable).to_be(true)
    btv.test.expect(t:message()).to_contain("imdisable=true")
    t:feed("<F7>")
    btv.test.expect(btv.o.imdisable).to_be(false)
  end)

  -- §2. "The plain ex form works … typed directly"
  btv.test.it("§2 — :set imdisable! toggles it too", function(t)
    open(t)
    t:cmd("ImeAuto")
    t:cmd("set imdisable!")
    btv.test.expect(btv.o.imdisable).to_be(true)
    t:cmd("set noimd")
    btv.test.expect(btv.o.imdisable).to_be(false)
  end)

  -- §4. "It's painted with `BtvImePreedit` … unless a config defines it, as this one does"
  btv.test.it("§4 — the composition's colour is the config's", function()
    local hl = btv.hl.get(0, { name = "BtvImePreedit" })
    btv.test.expect(hl.fg).to_be(0x1e1e2e)
    btv.test.expect(hl.bg).to_be(0xf9e2af)
  end)
end)
