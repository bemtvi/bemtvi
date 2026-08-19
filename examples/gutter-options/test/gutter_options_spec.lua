-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/gutter-options
--
-- The gutter is drawn by the CLIENT from the widths the server reserves, so it is
-- read with `t:gutter()` rather than off the painted rows — and the reserved width
-- is the honest measure anyway, since `'numberwidth'` is only a minimum.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- How many cells of gutter the window has reserved. The client draws it from
--- these widths, so it is `t:gutter()` — the painted rows are the text area alone.
local function gutter_width(t)
  return t:gutter().total
end

btv.test.describe("examples/gutter-options", function()
  btv.test.it("the config starts wide, as its comment says", function(t)
    open(t)
    btv.test.expect(btv.wo.numberwidth).to_be(8)
    btv.test.expect(btv.wo.signcolumn).to_be("yes:2")
    -- 8 cells of numbers + 2 sign columns of 2 cells each.
    btv.test.expect(gutter_width(t)).to_be(8 + 4)
  end)

  -- "'numberwidth' … the MINIMUM width of the line-number column."
  btv.test.it("numberwidth is a minimum the gutter honours", function(t)
    open(t)
    t:cmd("setlocal signcolumn=no")
    btv.test.expect(gutter_width(t)).to_be(8)
    t:cmd("setlocal numberwidth=4")
    btv.test.expect(gutter_width(t)).to_be(4)
  end)

  -- "growing to fit the largest line number plus a trailing space"
  btv.test.it("…and grows past it to fit the largest line number", function(t)
    open(t)
    t:cmd("setlocal signcolumn=no numberwidth=4")
    btv.test.expect(gutter_width(t)).to_be(4)
    -- A file whose largest line number is four digits: the gutter grows past the
    -- minimum to fit it, plus the trailing space.
    local big = btv.test.tempdir() .. "/big.txt"
    btv.await(btv.fs.write(big, string.rep("x\n", 1200)))
    t:cmd("e " .. big)
    t:cmd("setlocal signcolumn=no numberwidth=4")
    btv.test.expect(#t:lines()).to_be(1200)
    btv.test.expect(gutter_width(t)).to_be(5)
  end)

  -- "Each sign column is 2 cells … yes / yes:2 ALWAYS reserve 1 (or N) columns"
  btv.test.it("signcolumn reserves two cells per column", function(t)
    open(t)
    t:cmd("setlocal numberwidth=4")
    t:cmd("setlocal signcolumn=no")
    btv.test.expect(gutter_width(t)).to_be(4)
    t:cmd("setlocal signcolumn=yes")
    btv.test.expect(gutter_width(t)).to_be(4 + 2)
    t:cmd("setlocal signcolumn=yes:2")
    btv.test.expect(gutter_width(t)).to_be(4 + 4)
  end)

  -- "auto  show 1 column when a sign is present, collapse to 0 when none are"
  btv.test.it("signcolumn=auto collapses when there is no sign", function(t)
    open(t)
    t:cmd("setlocal numberwidth=4 signcolumn=auto")
    btv.test.expect(gutter_width(t)).to_be(4)
    local ns = btv.ns.create("gutter-options-spec")
    btv.buf.set_extmark(0, ns, 0, 0, { sign_text = ">>" })
    t:feed("j")
    btv.test.expect(gutter_width(t)).to_be(4 + 2)
    btv.buf.clear_namespace(0, ns, 0, -1)
    t:feed("k")
    btv.test.expect(gutter_width(t)).to_be(4)
  end)

  -- "<leader>n cycles numberwidth 4 -> 6 -> 8 -> 4"
  btv.test.it("<leader>n cycles numberwidth and says so", function(t)
    open(t)
    t:cmd("setlocal signcolumn=no")
    t:feed("<Space>n")
    btv.test.expect(btv.wo.numberwidth).to_be(4)
    btv.test.expect(t:message()).to_contain("numberwidth = 4")
    t:feed("<Space>n")
    btv.test.expect(btv.wo.numberwidth).to_be(6)
    t:feed("<Space>n")
    btv.test.expect(btv.wo.numberwidth).to_be(8)
    t:feed("<Space>n")
    btv.test.expect(btv.wo.numberwidth).to_be(4)
  end)

  -- "<leader>s cycles signcolumn through the common policies"
  btv.test.it("<leader>s cycles signcolumn and says so", function(t)
    open(t)
    for _, want in ipairs({ "auto:1-3", "no", "auto", "yes", "yes:2" }) do
      t:feed("<Space>s")
      btv.test.expect(btv.wo.signcolumn).to_be(want)
      btv.test.expect(t:message()).to_contain("signcolumn = " .. want)
    end
  end)

  -- "Both are window-local, so two splits onto the same file can differ."
  btv.test.it("both options are per window", function(t)
    open(t)
    t:feed("<C-w>v")
    t:cmd("setlocal numberwidth=4 signcolumn=no")
    btv.test.expect(gutter_width(t)).to_be(4)
    t:feed("<C-w>w")
    btv.test.expect(btv.wo.numberwidth).to_be(8)
    btv.test.expect(btv.wo.signcolumn).to_be("yes:2")
    btv.test.expect(gutter_width(t)).to_be(12)
    t:cmd("only")
  end)
end)
