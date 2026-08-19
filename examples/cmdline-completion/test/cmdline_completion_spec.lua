-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/cmdline-completion
--
-- The wildmenu is the float-list widget, so what it offers is `t:menu()`; the
-- command line itself is `btv._ui.cmdline`. Between them the spec can type each
-- WHAT-TO-TRY line exactly as written and check both halves.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- The wildmenu's rows, or {} when it is closed.
local function rows(t)
  local m = t:menu()
  return m and m.items or {}
end

--- Wait for the wildmenu to float.
local function await_menu(t)
  t:wait_for(function()
    return t:menu() ~= nil
  end, { message = "the wildmenu never opened" })
end

--- The text on the command line.
local function cmdline()
  return btv._ui.cmdline or ""
end

--- Whether `list` holds `name`.
local function has(list, name)
  for _, item in ipairs(list) do
    if item == name then
      return true
    end
  end
  return false
end

btv.test.describe("examples/cmdline-completion", function()
  -- ":e<Tab>  float a list of commands matching `e` (edit, enew, …)"
  btv.test.it("<Tab> floats a fuzzy list of command names", function(t)
    open(t)
    t:feed(":e<Tab>")
    await_menu(t)
    local items = rows(t)
    btv.test.expect(has(items, "edit")).to_be(true)
    btv.test.expect(has(items, "enew")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  -- "The popup opens with NOTHING highlighted (noselect)."
  btv.test.it("the popup opens noselect", function(t)
    open(t)
    t:feed(":e<Tab>")
    await_menu(t)
    btv.test.expect(t:menu().selected).to_be_nil()
    -- The first <Tab> highlights the top match…
    t:feed("<Tab>")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it("<S-Tab> from noselect highlights the bottom row", function(t)
    open(t)
    t:feed(":e<Tab>")
    await_menu(t)
    local n = #rows(t)
    t:feed("<S-Tab>")
    btv.test.expect(t:menu().selected).to_be(n)
    t:feed("<Esc><Esc>")
  end)

  -- ":tab<Tab>  narrow to the tab-* family"
  btv.test.it(":tab<Tab> narrows to the tab family", function(t)
    open(t)
    t:feed(":tab<Tab>")
    await_menu(t)
    local items = rows(t)
    btv.test.expect(has(items, "tabnew")).to_be(true)
    btv.test.expect(has(items, "tabclose")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  -- "keep typing — the open list narrows LIVE (`:e<Tab>` then `d` → just `edit`)"
  btv.test.it("the open list narrows live as you keep typing", function(t)
    open(t)
    t:feed(":e<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "enew")).to_be(true)
    t:feed("d")
    t:sleep(40)
    local items = rows(t)
    btv.test.expect(has(items, "edit")).to_be(true)
    btv.test.expect(has(items, "enew")).to_be(false)
    t:feed("<Esc><Esc>")
  end)

  -- "<Esc>  dismiss the wildmenu but keep the command line open (a second <Esc>
  --  then cancels the line)"
  btv.test.it("<Esc> dismisses the menu but keeps the line", function(t)
    open(t)
    t:feed(":e<Tab>")
    await_menu(t)
    t:feed("<Esc>")
    t:sleep(40)
    btv.test.expect(t:menu()).to_be_nil()
    btv.test.expect(cmdline()).to_contain("e")
    t:feed("<Esc>")
    btv.test.expect(t:mode()).to_be("n")
  end)

  -- The worked example: ":ene<Tab>  <Tab>  <CR>  → a new empty buffer".
  btv.test.it("the worked example — :ene<Tab><Tab><CR> runs :enew", function(t)
    open(t)
    t:feed(":ene<Tab>")
    await_menu(t)
    btv.test.expect(rows(t)[1]).to_be("enew")
    t:feed("<Tab>")
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<CR>")
    t:sleep(40)
    -- The sample buffer was replaced by an empty one.
    btv.test.expect(t:lines()).to_equal({ "" })
    btv.test.expect(btv.buf.name(0)).to_be("")
  end)

  -- "THE UNIFIED PAYOFF — a plugin command appears like a built-in."
  btv.test.it("the plugin's :Greet joins the catalog", function(t)
    open(t)
    t:feed(":Gree<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "Greet")).to_be(true)
    t:feed("<Tab><CR>")
    t:wait_for(function()
      return (t:message() or ""):find("Hello from a plugin command", 1, true) ~= nil
    end, { message = ":Greet never ran" })
  end)

  -- "ARGUMENT COMPLETION — option names after `:set`."
  btv.test.it(":set <Tab> completes option names", function(t)
    open(t)
    t:feed(":set nu<Tab>")
    await_menu(t)
    local items = rows(t)
    btv.test.expect(has(items, "number")).to_be(true)
    btv.test.expect(has(items, "numberwidth")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":setlocal shares the option-argument completer", function(t)
    open(t)
    t:feed(":setlocal ts<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "tabstop")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it("<CR> on an option name accepts it AND runs the line", function(t)
    open(t)
    t:cmd("set nonumber")
    t:feed(":set nu<Tab>")
    await_menu(t)
    t:feed("<Tab>")
    btv.test.expect(rows(t)[t:menu().selected]).to_be("number")
    t:feed("<CR>")
    t:sleep(40)
    -- Accept-and-run, the same rule as for a command name: the line is gone and
    -- the option is set. Only the file-path picker pastes without running.
    btv.test.expect(t:mode()).to_be("n")
    btv.test.expect(cmdline()).to_be("")
    btv.test.expect(btv.o.number).to_be(true)
  end)

  btv.test.it("<Esc> first is how you finish a value form by hand", function(t)
    open(t)
    t:feed(":set nu<Tab>")
    await_menu(t)
    t:feed("<Esc>")
    btv.test.expect(t:menu()).to_be_nil()
    btv.test.expect(cmdline()).to_contain("set nu")
    t:feed("<Esc>")
  end)

  -- "NAME ARGUMENT COMPLETION — buffers, color schemes, highlight groups."
  btv.test.it(":buffer <Tab> lists the loaded buffers", function(t)
    open(t)
    t:feed(":buffer <Tab>")
    await_menu(t)
    local joined = table.concat(rows(t), "\n")
    btv.test.expect(joined).to_contain("sample.txt")
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":colorscheme <Tab> lists the available schemes", function(t)
    open(t)
    t:feed(":colorscheme <Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "bemtvi")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":highlight <Tab> lists the defined groups", function(t)
    open(t)
    btv.hl.define(0, "SpecMadeThisGroup", { fg = "#123456" })
    t:feed(":highlight SpecMade<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "SpecMadeThisGroup")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":setfiletype <Tab> lists recognized filetypes", function(t)
    open(t)
    t:feed(":setfiletype lu<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "lua")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":autocmd <Tab> lists the events, :augroup the groups", function(t)
    open(t)
    t:feed(":autocmd BufWri<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "BufWritePre")).to_be(true)
    t:feed("<Esc><Esc>")
    t:cmd("augroup SpecGroup")
    t:cmd("augroup END")
    t:feed(":augroup SpecGr<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "SpecGroup")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":put <Tab> lists the registers that hold content", function(t)
    open(t)
    t:feed("yy")
    t:feed(':put <Tab>')
    await_menu(t)
    btv.test.expect(#rows(t) > 0).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it(":move <Tab> lists address landmarks", function(t)
    open(t)
    t:feed("3Gma")
    t:feed(":move <Tab>")
    await_menu(t)
    local items = rows(t)
    btv.test.expect(has(items, ".")).to_be(true)
    btv.test.expect(has(items, "$")).to_be(true)
    btv.test.expect(has(items, "'a")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  -- "MODIFIER WRAPPERS — completion recurses through :vertical / :tab / :silent."
  btv.test.it("a modifier is stripped and the nested command completes", function(t)
    open(t)
    t:feed(":vertical spl<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "split")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  btv.test.it("chained modifiers recurse too", function(t)
    open(t)
    t:feed(":silent vertical spl<Tab>")
    await_menu(t)
    btv.test.expect(has(rows(t), "split")).to_be(true)
    t:feed("<Esc><Esc>")
  end)

  -- "any other command's arguments have no completer yet, so the wildmenu just
  --  stays closed."
  btv.test.it("an argument with no completer opens nothing", function(t)
    open(t)
    t:feed(":Greet arg<Tab>")
    t:sleep(60)
    btv.test.expect(t:menu()).to_be_nil()
    t:feed("<Esc>")
  end)
end)
