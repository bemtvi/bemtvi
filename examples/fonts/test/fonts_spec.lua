-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/fonts
--
-- The runner has no window, so what a font looks like isn't testable here (the GUI's
-- tests/fonts.rs and the browser's verify-guifont.mjs cover that). These check what the
-- notes promise of the option: the value each `btv.ui.font` call sets, and that a typo
-- is refused.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

btv.test.describe("examples/fonts", function()
  btv.test.it("1, 4, 5: the config sets the list with each font's options", function()
    btv.test.expect(btv.o.guifont).to_be(
      "JetBrains Mono[features=+zero],Symbols Nerd Font Mono[scale=0.9],"
        .. "Noto Sans CJK JP,Noto Color Emoji:h15"
    )
  end)

  btv.test.it("2: another order is another list", function()
    btv.ui.font({ "Noto Sans CJK JP", "JetBrains Mono" })
    btv.test.expect(btv.o.guifont).to_be("Noto Sans CJK JP,JetBrains Mono")
  end)

  btv.test.it("3: a font's own weight", function()
    btv.ui.font({ { "JetBrains Mono", weight = "bold" } })
    btv.test.expect(btv.o.guifont).to_be("JetBrains Mono[weight=bold]")
  end)

  btv.test.it("6: a typo is E474 and the font you had stays", function(t)
    t:cmd("set guifont=JetBrains\\ Mono[wieght=500]")
    btv.test
      .expect(t:message())
      .to_contain("E474: Invalid argument: guifont: unknown option wieght")
    btv.test.expect(btv.o.guifont).to_be(
      "JetBrains Mono[features=+zero],Symbols Nerd Font Mono[scale=0.9],"
        .. "Noto Sans CJK JP,Noto Color Emoji:h15"
    )
  end)
end)
