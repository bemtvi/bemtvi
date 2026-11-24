-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/decor-query
--
-- Every test needs the markdown treesitter grammar, so each one skips (with the
-- reason) on a machine that doesn't have it.

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

local expect = btv.test.expect

local function needs_markdown(t)
  if not btv.tbl.contains(btv.await(btv.treesitter.installed()), "markdown") then
    t:skip("no markdown treesitter grammar installed")
  end
end

-- The gutter sign on each screen row from `first` to `last`, `" "` where there is none.
local function signs(t, first, last)
  local out = {}
  for row = first, last do
    out[#out + 1] = t:decor(row).sign or " "
  end
  return table.concat(out)
end

--- Open the sample afresh and wait for the provider's first marks.
local function open(t)
  needs_markdown(t)
  t:cmd("e " .. DIR .. "/sample.md")
  t:cmd("e!")
  t:feed("gg")
  t:wait_for(function()
    return t:decor(5).sign ~= nil
  end, { message = "the provider never marked the first table" })
end

btv.test.describe("examples/decor-query", function()
  -- 1. "every row of both tables carries a `T`, every row of the code fence a `C`;
  --    the paragraph line with a stray `|` has no sign"
  btv.test.it("§1 marks the tables and the fence from the grammar", function(t)
    open(t)
    -- rows: 1 title, 3 stray pipe, 5-8 table, 12-15 fence, 17-19 table
    expect(signs(t, 1, 19)).to_be("    TTTT   CCCC TTT")
  end)

  -- 2. "`5Gzt`, then `<C-e>` once: the rows still on screen keep their `T`"
  btv.test.it("§2 a table cut by the top of the window keeps its marks", function(t)
    open(t)
    t:feed("5Gzt<C-e>")
    t:wait_for(function()
      return t:lines()[6] == t:screen()[1] or nil
    end, { message = "the window never scrolled to the delimiter row" })
    t:wait_for(function()
      return signs(t, 1, 3) == "TTT" or nil
    end, { message = "the rows below the scrolled-off header lost their marks" })
  end)

  -- 3. "`/billing<CR>dd`: the first table loses a `T` row; `u` gets it back"
  btv.test.it("§3 an edit is re-queried", function(t)
    open(t)
    t:feed("/billing<CR>dd")
    t:wait_for(function()
      return signs(t, 1, 8) == "    TTT " or nil
    end, { message = "the deleted row's mark did not go" })
    t:feed("u")
    t:wait_for(function()
      return signs(t, 1, 8) == "    TTTT" or nil
    end, { message = "undo did not bring the mark back" })
  end)

  -- 3. "`o| x | y |<Esc>` on the table's last row: the new row is marked too"
  btv.test.it("§3 a row added to a table is part of it", function(t)
    open(t)
    t:feed("19Go| x | y |<Esc>")
    expect(t:lines()[20]).to_be("| x | y |")
    t:wait_for(function()
      return t:decor(20).sign == "T" or nil
    end, { message = "the new row was not marked" })
  end)

  -- 4. "`:lua btv.bo.ts_highlight = false`: every sign disappears"
  btv.test.it("§4 treesitter off clears the marks, and on brings them back", function(t)
    open(t)
    t:cmd("lua btv.bo.ts_highlight = false")
    t:wait_for(function()
      return signs(t, 1, 19) == ("    TTTT   CCCC TTT"):gsub("%S", " ") or nil
    end, { message = "the signs stayed with treesitter off" })
    t:cmd("lua btv.bo.ts_highlight = true")
    t:wait_for(function()
      return signs(t, 1, 19) == "    TTTT   CCCC TTT" or nil
    end, { message = "the signs did not come back" })
  end)
end)
