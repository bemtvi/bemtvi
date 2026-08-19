-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/complete-on-demand
--
-- The eight numbered TYPE THIS / SEE THAT steps, typed exactly as written. The
-- popup is the whole subject here, and it is in none of the buffer views — so
-- this reads `t:menu()`, which is what the client is told to float.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-read, cursor on the empty last line — where the notes put it.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("G")
end

--- The popup's rows, or {} when it is closed.
local function rows(t)
  local m = t:menu()
  return m and m.items or {}
end

--- Wait until the popup is open (it is fed by an async source).
local function await_popup(t)
  t:wait_for(function()
    return t:menu() ~= nil
  end, { message = "no completion popup opened" })
end

btv.test.describe("examples/complete-on-demand", function()
  -- 1. "`o` then type `co` → NOTHING opens."
  btv.test.it("step 1 — typing opens nothing: auto = false", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:sleep(80)
    btv.test.expect(t:menu()).to_be_nil()
    t:feed("<Esc>")
  end)

  -- 2. "<C-Space> → the popup opens on the 2-char prefix (min_chars bypassed) …
  --     with the TOP ROW ALREADY HIGHLIGHTED."
  btv.test.it("step 2 — the trigger opens on a prefix under min_chars", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:feed("<C-Space>")
    await_popup(t)
    local items = rows(t)
    btv.test.expect(#items > 0).to_be(true)
    btv.test.expect(items).to_contain("connection")
    btv.test.expect(items).to_contain("completion")
    -- Matching is fuzzy (a subsequence), not a prefix — so `cursor` from the
    -- comment text is a legitimate row, while a word with no `c…o` is not.
    for _, item in ipairs(items) do
      btv.test.expect(item:lower()).to_match("c.*o")
    end
    btv.test.expect(items).never.to_contain("local")
    -- A manual session preselects, so <C-y> works with no navigation.
    btv.test.expect(t:menu().selected).to_be(1)
    t:feed("<C-e><Esc>")
  end)

  -- 3. "type `nn` → the popup STAYS UP and narrows … Your keys still land in the
  --     document."
  btv.test.it("step 3 — the session survives typing, and narrows", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:feed("<C-Space>")
    await_popup(t)
    local before = #rows(t)
    t:feed("nn", { insert = true })
    t:sleep(80)
    btv.test.expect(t:menu()).never.to_be_nil()
    local after = rows(t)
    btv.test.expect(#after < before).to_be(true)
    btv.test.expect(after).to_contain("connection")
    btv.test.expect(after).to_contain("connect_timeout")
    for _, item in ipairs(after) do
      btv.test.expect(item:lower()).to_match("c.*o.*n.*n")
    end
    -- The keys landed in the document too.
    btv.test.expect(t:current_line()).to_be("conn")
    t:feed("<C-e><Esc>")
  end)

  -- 4. "<BS><BS><BS> → back to `c`: the popup WIDENS again rather than closing,
  --     still below min_chars."
  btv.test.it("step 4 — backspacing widens the popup instead of closing it", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:feed("<C-Space>")
    await_popup(t)
    t:feed("nn", { insert = true })
    t:sleep(80)
    local narrowed = #rows(t)
    t:feed("<BS><BS><BS>")
    t:sleep(80)
    btv.test.expect(t:menu()).never.to_be_nil()
    btv.test.expect(t:current_line()).to_be("c")
    btv.test.expect(#rows(t) > narrowed).to_be(true)
    t:feed("<C-e><Esc>")
  end)

  -- 5. "<C-y> → accepts the highlighted row (no navigation step needed)."
  btv.test.it("step 5 — <C-y> accepts the preselected row", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:feed("<C-Space>")
    await_popup(t)
    local top = rows(t)[1]
    t:feed("<C-y>")
    t:feed("<Esc>")
    btv.test.expect(t:current_line()).to_be(top)
  end)

  -- 6. "`o`, type `co`, <C-j> → the Lua-API key opens the same session."
  btv.test.it("step 6 — btv.complete.trigger on <C-j> opens the same session", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:feed("<C-j>")
    await_popup(t)
    btv.test.expect(t:menu().selected).to_be(1)
    local top = rows(t)[1]
    t:feed("<C-y><Esc>")
    btv.test.expect(t:current_line()).to_be(top)
  end)

  -- 7. "<C-e> then type `nn` → aborted: the popup does NOT come back."
  btv.test.it("step 7 — <C-e> ends the session for good", function(t)
    open(t)
    t:feed("o")
    t:feed("co", { insert = true })
    t:feed("<C-Space>")
    await_popup(t)
    t:feed("<C-e>")
    t:sleep(60)
    btv.test.expect(t:menu()).to_be_nil()
    t:feed("nn", { insert = true })
    t:sleep(80)
    btv.test.expect(t:menu()).to_be_nil()
    -- …and a fresh trigger starts a new one.
    t:feed("<C-Space>")
    await_popup(t)
    t:feed("<C-e><Esc>")
  end)

  -- 8. "`o`, <C-Space>, `zzz` → nothing matches, so the popup closes on its own
  --     and stays closed."
  btv.test.it("step 8 — a prefix nothing matches closes the session", function(t)
    open(t)
    t:feed("o")
    t:feed("c", { insert = true })
    t:feed("<C-Space>")
    await_popup(t)
    t:feed("zzz", { insert = true })
    t:sleep(100)
    btv.test.expect(t:menu()).to_be_nil()
    t:feed("<Esc>")
  end)

  -- The setup the config chose, which is what makes step 1 true.
  btv.test.it("the engine is configured on demand only", function(t)
    open(t)
    -- With `auto = true` the popup would open on a 4-char prefix; it must not.
    t:feed("o")
    t:feed("conn", { insert = true })
    t:sleep(100)
    btv.test.expect(t:menu()).to_be_nil()
    t:feed("<Esc>")
  end)
end)
