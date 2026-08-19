-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/treesitter-textobjects
--
-- Every object here reads the language's `textobjects.scm`, so the cases that
-- select one check first that the grammar is on disk — the notes say `:TSInstall
-- rust` is the prerequisite — and skip rather than fail without it.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

local function open(t)
  t:cmd("only")
  t:cmd("e " .. DIR .. "/sample.rs")
  t:cmd("e!")
  t:feed("gg")
end

--- Whether the rust grammar (and its queries) are loaded for this buffer.
local function has_grammar(t)
  t:sleep(60)
  for row = 5, 12 do
    if #t:highlights(row) > 0 then
      return true
    end
  end
  return false
end

--- The 1-based line range a visual selection covers, from the `<` / `>` marks.
local function selection(t)
  t:feed("<Esc>")
  return vim.fn.line("'<"), vim.fn.line("'>")
end

btv.test.describe("examples/treesitter-textobjects", function()
  -- "This config only makes the demo comfortable — it does NOT enable the feature."
  btv.test.it("the config sets the demo's three comfort options", function(t)
    t:cmd("e " .. DIR .. "/sample.rs")
    btv.test.expect(btv.o.timeoutlen).to_be(300)
    btv.test.expect(btv.wo.number).to_be(true)
    btv.test.expect(btv.wo.cursorline).to_be(true)
  end)

  -- "btv.textobject.map gives them keys."
  btv.test.it("the custom objects are bound to the captures the notes name", function(t)
    open(t)
    local bound = btv.textobject.list and btv.textobject.list() or nil
    if bound == nil then
      -- No read surface for the registry; the behaviour cases below cover it.
      return
    end
    btv.test.expect(bound["il"]).to_be("@loop.inner")
    btv.test.expect(bound["ak"]).to_be("@call.outer")
  end)

  -- "Cursor inside `distance`'s body, `vif` → selects the function body. `vaf` →
  --  selects the whole `fn distance … }` including the signature."
  btv.test.it("vif takes the function body, vaf the whole function", function(t)
    open(t)
    if not has_grammar(t) then
      print("skip: no rust treesitter grammar installed")
      return
    end
    t:feed("13G")
    t:feed("vif")
    local first, last = selection(t)
    btv.test.expect(first > 11).to_be(true)
    btv.test.expect(last < 20).to_be(true)
    t:feed("13G")
    t:feed("vaf")
    local afirst, alast = selection(t)
    -- Around takes the signature line and the closing brace as well.
    btv.test.expect(afirst).to_be(11)
    btv.test.expect(alast).to_be(20)
  end)

  -- "Cursor on the nested closure, `vif` → the closure; `2vif` → `distance`."
  btv.test.it("a count reaches the enclosing function", function(t)
    open(t)
    if not has_grammar(t) then
      return
    end
    t:feed("17G")
    t:feed("f{l")
    t:feed("vif")
    local _, inner_last = selection(t)
    btv.test.expect(inner_last).to_be(17)
    t:feed("17G")
    t:feed("f{l")
    t:feed("2vif")
    local _, outer_last = selection(t)
    btv.test.expect(outer_last > 17).to_be(true)
  end)

  -- "Cursor on `target` in `main`, `dia` → deletes just that argument."
  btv.test.it("dia deletes one argument", function(t)
    open(t)
    if not has_grammar(t) then
      return
    end
    t:feed("36G")
    t:feed("f target")
    t:feed("dia")
    btv.test.expect(t:line(36)).to_contain("distance(origin")
    btv.test.expect(t:line(36)).never.to_contain("target")
    t:cmd("undo")
  end)

  -- "Cursor on the `struct Point` block, `vit` → the type; `dat` → deletes it."
  btv.test.it("vit selects the type and dat deletes it", function(t)
    open(t)
    if not has_grammar(t) then
      return
    end
    t:feed("6G")
    t:feed("vit")
    local first, last = selection(t)
    btv.test.expect(first >= 5).to_be(true)
    btv.test.expect(last <= 8).to_be(true)
    t:feed("6G")
    t:feed("dat")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("struct Point")
    t:cmd("undo")
  end)

  -- "Cursor on a `//` comment line, `vic` / `dac` → the comment object."
  btv.test.it("vic selects the comment under the cursor", function(t)
    open(t)
    if not has_grammar(t) then
      return
    end
    t:feed("10G")
    t:feed("vic")
    local first, last = selection(t)
    btv.test.expect(first).to_be(10)
    btv.test.expect(last).to_be(10)
  end)

  -- "Cursor in `total`'s for-loop, `vil` → inside the loop; `vak` on a call → the
  --  whole call (both are CUSTOM objects mapped via btv.textobject.map above)."
  btv.test.it("the custom loop and call objects select", function(t)
    open(t)
    if not has_grammar(t) then
      return
    end
    t:feed("27G")
    t:feed("vil")
    local first, last = selection(t)
    btv.test.expect(first >= 25).to_be(true)
    btv.test.expect(last <= 28).to_be(true)
    -- `vak` on the `distance(…)` call takes the whole call.
    t:feed("27G")
    t:feed("f di")
    t:feed("vak")
    local kfirst, klast = selection(t)
    btv.test.expect(kfirst).to_be(27)
    btv.test.expect(klast).to_be(27)
  end)

  -- ":TextObjects reprints the cheatsheet at any time."
  btv.test.it(":TextObjects prints the cheatsheet", function(t)
    open(t)
    local got
    local prev_vim, prev_btv = vim.notify, btv.notify
    vim.notify = function(msg)
      got = tostring(msg)
    end
    btv.notify = vim.notify
    t:cmd("TextObjects")
    vim.notify, btv.notify = prev_vim, prev_btv
    btv.test.expect(got).to_contain("f function")
    btv.test.expect(got).to_contain("l loop")
    btv.test.expect(got).to_contain("2vif")
  end)
end)
