-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/treesitter-install
--
-- Installing the grammar needs the network, so the spec doesn't do it: the tests that
-- need the LaTeX grammar skip (with the reason) on a machine that doesn't have it.
-- Install it first with `:TSInstall latex` to run them.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

local expect = btv.test.expect

local function needs_latex(t)
  if not btv.tbl.contains(btv.await(btv.treesitter.installed()), "latex") then
    t:skip("no latex treesitter grammar installed (:TSInstall latex)")
  end
end

local function open(t)
  t:cmd("e " .. DIR .. "/sample.tex")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/treesitter-install", function()
  -- The run line opens sample.tex; the grammar applies because `.tex` is `latex`.
  btv.test.it("the sample opens as latex", function(t)
    open(t)
    expect(btv.bo.filetype).to_be("latex")
  end)

  -- 1. "the document lights up as soon as it lands: commands, sections, math …"
  btv.test.it("§1 the installed grammar parses the sample's structure", function(t)
    needs_latex(t)
    open(t)
    local sections = btv.await(btv.treesitter.query(0, "(section) @s"))
    local subsections = btv.await(btv.treesitter.query(0, "(subsection) @s"))
    expect(#sections.matches).to_be(3)
    expect(#subsections.matches).to_be(2)
  end)

  -- 4. "`:set foldmethod=expr foldexpr=…`, then `zM`: each section and environment
  --    folds to one line. `zR` opens them all again."
  btv.test.it("§4 zM folds the document by its sections, zR opens it", function(t)
    needs_latex(t)
    open(t)
    t:cmd("set foldmethod=expr foldexpr=v:lua.btv.treesitter.foldexpr()")
    local function folded_rows()
      local n = 0
      for _, text in ipairs(t:screen()) do
        if text:match("^%+%-+%s+%d+ lines:") then
          n = n + 1
        end
      end
      return n
    end
    t:feed("zM")
    t:wait_for(function()
      return folded_rows() > 0 or nil
    end, { message = "zM closed no fold" })
    t:feed("zR")
    t:wait_for(function()
      return folded_rows() == 0 or nil
    end, { message = "zR left a fold closed" })
  end)
end)
