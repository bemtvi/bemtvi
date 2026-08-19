-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/tabline
--
-- `t:tabline()` is the rendered custom strip — the row the `%`-format engine
-- produced from the builder's result, with the `%#Group#` / `%nT` items already
-- resolved away. It is NOT `t:tabs()`, which reads the structured cells the client
-- formats itself: setting `'tabline'` replaces those, so only one of the two is
-- ever drawn.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- One tab on the sample, showing the strip. Any tab a previous case opened is
--- closed first: the per-test baseline restores options and buffers, not the tab
--- stack.
local function open(t)
  while #vim.api.nvim_list_tabpages() > 1 do
    t:cmd("tabclose")
  end
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("set showtabline=2")
  t:cmd("set tabline=%!v:lua.require('myutils').my_tab_line()")
  t:feed("gg")
end

btv.test.describe("examples/tabline", function()
  -- "vim.o.tabline = '%!v:lua.require(\"myutils\").my_tab_line()'"
  btv.test.it("the config points 'tabline' at the module's builder", function(t)
    open(t)
    btv.test.expect(btv.o.tabline).to_be("%!v:lua.require('myutils').my_tab_line()")
    btv.test.expect(btv.o.showtabline).to_be(2)
  end)

  -- "Always show the tabline, even with a single tab"
  btv.test.it("a single tab still draws one numbered label", function(t)
    open(t)
    btv.test.expect(t:tabline()).to_contain("1:sample.txt")
  end)

  -- ":tabedit … — a second tab appears; the tabline lists both"
  btv.test.it(":tabedit adds a label, and gt/gT move between them", function(t)
    open(t)
    t:cmd("tabedit " .. DIR .. "/lua/myutils.lua")
    local strip = t:tabline()
    btv.test.expect(strip).to_contain("1:sample.txt")
    btv.test.expect(strip).to_contain("2:myutils.lua")
    -- Which one leads is the tab page the editor is on, so `gt`/`gT` are read there.
    btv.test.expect(vim.fn.tabpagenr()).to_be(2)
    t:feed("gt")
    btv.test.expect(vim.fn.tabpagenr()).to_be(1)
    t:feed("gT")
    btv.test.expect(vim.fn.tabpagenr()).to_be(2)
    t:cmd("tabclose")
  end)

  -- "the label … a parenthesised 3-char-per-segment hint of the parent directories"
  btv.test.it("a label abbreviates the parent directories", function(t)
    open(t)
    t:cmd("tabedit " .. DIR .. "/lua/myutils.lua")
    -- `…/examples/tabline/lua/myutils.lua` -> the two parents, three chars each.
    btv.test.expect(t:tabline()).to_contain("myutils.lua(tab/lua)")
    t:cmd("tabclose")
  end)

  -- "edit a tab's buffer (i…<Esc>) — that tab's label gains a `*` modified marker"
  btv.test.it("a modified buffer marks its label", function(t)
    open(t)
    btv.test.expect(t:tabline()).never.to_contain("sample.txt*")
    t:feed("ix<Esc>")
    btv.test.expect(t:tabline()).to_contain("sample.txt*")
    t:cmd("undo")
  end)

  -- ":tabclose — back to one tab — showtabline=1 hides the line"
  btv.test.it("at showtabline=1 a single tab draws nothing", function(t)
    open(t)
    t:cmd("set showtabline=1")
    btv.test.expect(t:tabline()).to_be(nil)
    -- …and a second tab brings the strip back.
    t:cmd("tabedit " .. DIR .. "/lua/myutils.lua")
    btv.test.expect(t:tabline()).to_contain("2:myutils.lua")
    t:cmd("tabclose")
    btv.test.expect(t:tabline()).to_be(nil)
  end)

  -- "with more than one tab — a right-aligned %999X 'close' region"
  btv.test.it("the close region appears only with more than one tab", function(t)
    open(t)
    btv.test.expect(t:tabline()).never.to_contain("close")
    t:cmd("tabedit " .. DIR .. "/lua/myutils.lua")
    btv.test.expect(t:tabline()).to_contain("close")
    -- `%=` pushed it to the right end of the strip.
    btv.test.expect(t:tabline():sub(-5)).to_be("close")
    t:cmd("tabclose")
    btv.test.expect(t:tabline()).never.to_contain("close")
  end)

  -- ":set tabline= — fall back to bemtvi's built-in tab cells"
  btv.test.it("clearing the option falls back to the built-in cells", function(t)
    open(t)
    btv.test.expect(t:tabline()).to_contain("1:sample.txt")
    t:cmd("set tabline=")
    -- The custom row is gone, and the structured cells the client formats are what
    -- is left.
    btv.test.expect(t:tabline()).to_be(nil)
    btv.test.expect(t:tabs("main").labels[1].label).to_be("sample.txt")
  end)

  -- The module's own helpers, which the label is assembled from.
  btv.test.it("the module's helpers do what the label needs", function(t)
    open(t)
    local m = require("myutils")
    btv.test.expect(m.get_last_x({ "a", "b", "c", "d" }, 2)).to_equal({ "c", "d" })
    btv.test.expect(m.get_last_x({ "a" }, 3)).to_equal({ "a" })
    btv.test.expect(m.str_join("/", { "one", "two" })).to_be("one/two")
    btv.test.expect(m.str_join("-", { "abc", "def" }, function(p)
      return p:sub(1, 1)
    end)).to_be("a-d")
    btv.test.expect(m.str_join(",", {})).to_be("")
    btv.test.expect(m.match_any({ "^NvimTree_[0-9]+" }, "NvimTree_1")).to_be(true)
    btv.test.expect(m.match_any({ "^NvimTree_[0-9]+" }, "init.lua")).to_be_falsy()
  end)

  -- "skip ignored side-panel buffers" — the label picks the first buffer of the
  -- tab that is not one of them.
  btv.test.it("an ignored side-panel buffer is not chosen as the label", function(t)
    open(t)
    local m = require("myutils")
    btv.test.expect(m.match_any({ "^undotree_", "^diffpanel_" }, "undotree_2")).to_be(true)
    btv.test.expect(m.my_tab_label(1)).to_contain("sample.txt")
  end)
end)
