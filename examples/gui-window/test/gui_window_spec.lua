-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/gui-window
--
-- What these options change is the native GUI's WINDOW — the notes say so, and
-- `crates/bemtvi-gui/tests/window_state.rs` drives the real one. What a headless
-- spec can hold to account is everything around it: the config's values land,
-- each mode the notes type is taken and reads back, a typo is rejected loudly and
-- changes nothing, and `&` goes back to the default.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

btv.test.describe("examples/gui-window", function()
  -- §1. "this config leaves them at `remember`"
  btv.test.it("§1 — the defaults are remember", function()
    btv.test.expect(btv.o.guiposition).to_be("remember")
    btv.test.expect(btv.o.guisize).to_be("remember")
    btv.test.expect(btv.o.guimaximize).to_be("remember")
    btv.test.expect(vim.o.guisize).to_be("remember")
  end)

  -- §2. ":set guiposition=centered"
  btv.test.it("§2 — centered", function(t)
    t:cmd("set guiposition=centered")
    btv.test.expect(btv.o.guiposition).to_be("centered")
  end)

  -- §3. ":set guisize=60%x70%", then ":set guisize?" says so.
  btv.test.it("§3 — a share of the screen, and the query says so", function(t)
    t:cmd("set guisize=60%x70%")
    btv.test.expect(btv.o.guisize).to_be("60%x70%")
    t:cmd("set guisize?")
    btv.test.expect(t:message()).to_contain("guisize=60%x70%")
  end)

  -- §4. ":set guimaximize=always", then "never".
  btv.test.it("§4 — always, then never, maximized", function(t)
    t:cmd("set guimaximize=always")
    btv.test.expect(btv.o.guimaximize).to_be("always")
    t:cmd("set guimaximize=never")
    btv.test.expect(btv.o.guimaximize).to_be("never")
  end)

  -- §5. ":set guisize=150%x50%" → E474, unchanged; so does "guiposition=left";
  -- ":set guisize&" → remember.
  btv.test.it("§5 — a typo is rejected loudly, and & resets", function(t)
    t:cmd("set guisize=60%x70%")
    t:cmd("set guisize=150%x50%")
    btv.test.expect(t:message()).to_contain("E474: Invalid argument: guisize=150%x50%")
    btv.test.expect(btv.o.guisize).to_be("60%x70%")
    t:cmd("set guiposition=left")
    btv.test.expect(t:message()).to_contain("E474: Invalid argument: guiposition=left")
    btv.test.expect(btv.o.guiposition).to_be("remember")
    t:cmd("set guisize&")
    t:cmd("set guisize?")
    btv.test.expect(t:message()).to_contain("guisize=remember")
  end)
end)
