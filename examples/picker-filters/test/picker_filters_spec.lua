-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/picker-filters
--
-- The filters decide which candidates a source produces at all, so the assertion
-- is always the picker's row list — `t:menu()` — and the fixture is this
-- directory's deliberate mess (a `target/`, a `vendor/`, a dotfile).

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, with nothing floating and normal mode current.
local function open(t)
  -- Unconditional escapes: with the filter boxes revealed (`filters = "open"`) an
  -- <Esc> dismisses the BOX first, so one is not always enough to close the picker
  -- — and `t:menu()` can already read nil while it is still up.
  for _ = 1, 4 do
    t:feed("<Esc>")
    t:sleep(30)
  end
  -- Every filterable picker opens seeded from the most recent line you used (§7's
  -- own note says so), so clear the history — otherwise one test's `src/**`
  -- silently scopes the next one.
  btv.picker.forget_history()
  t:cmd("cd " .. DIR)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Wait for the open picker's rows.
local function rows(t)
  return t:wait_for(function()
    local m = t:menu()
    return m and #m.items > 0 and m or nil
  end, { message = "the picker never opened" })
end

--- Open a picker by pressing its key, and wait for its rows.
local function picker(t, keys)
  t:feed(keys)
  return rows(t)
end

--- Open a picker through the API, and wait for its rows.
local function open_picker(t, name, opts)
  btv.picker.open(name, opts)
  return rows(t)
end

--- Whether any row mentions `needle`.
local function lists(t, needle)
  for _, row in ipairs((t:menu() or { items = {} }).items) do
    if row:find(needle, 1, true) then
      return true
    end
  end
  return false
end

btv.test.describe("examples/picker-filters", function()
  -- 1. "the picker opens … with neither `target/junk.rs` nor `vendor/lib.lock` in
  --     the list."
  btv.test.it("§1 — the configured excludes hide the build output", function(t)
    open(t)
    picker(t, "<Bslash>ff")
    btv.test.expect(lists(t, "sample.txt")).to_be(true)
    btv.test.expect(lists(t, "src/main.rs")).to_be(true)
    btv.test.expect(lists(t, "junk.rs")).to_be(false)
    btv.test.expect(lists(t, "lib.lock")).to_be(false)
    t:feed("<Esc>")
  end)

  -- "This is the 'stop showing me build output' knob; set it once and every picker
  --  honors it."
  btv.test.it("§1 — every filterable picker honours the same setup", function(t)
    open(t)
    open_picker(t, "mine")
    btv.test.expect(lists(t, "src/main.rs")).to_be(true)
    btv.test.expect(lists(t, "target/debug/junk.rs")).to_be(false)
    btv.test.expect(lists(t, "vendor/lib.lock")).to_be(false)
    t:feed("<Esc>")
  end)

  -- 5. "<leader>fs -> a picker showing ONLY the `src/` tree, with the rows already
  --     revealed (`filters = 'open'`)."
  btv.test.it("§5 — a picker can be opened already scoped", function(t)
    open(t)
    picker(t, "<Space>fs")
    btv.test.expect(lists(t, "src/main.rs")).to_be(true)
    btv.test.expect(lists(t, "src/deep/nested.rs")).to_be(true)
    btv.test.expect(lists(t, "sample.txt")).to_be(false)
    t:feed("<Esc>")
  end)

  -- 3. "with that in the EXCLUDE box, every Rust file vanishes"
  btv.test.it("§3 — an exclude pattern drops what it names", function(t)
    open(t)
    open_picker(t, "mine", { exclude = "*.rs", filters = "open" })
    btv.test.expect(lists(t, ".rs")).to_be(false)
    btv.test.expect(lists(t, "vendor/lib.lock")).to_be(true)
    t:feed("<Esc>")
  end)

  btv.test.it("§3 — …and an include pattern keeps only what it names", function(t)
    open(t)
    open_picker(t, "mine", { include = "*.rs", filters = "open" })
    btv.test.expect(lists(t, "src/main.rs")).to_be(true)
    btv.test.expect(lists(t, "vendor/lib.lock")).to_be(false)
    t:feed("<Esc>")
  end)

  -- "a pattern with a `/` is root-anchored"
  btv.test.it("§3 — a pattern with a slash is root-anchored", function(t)
    open(t)
    open_picker(t, "mine", { include = "src/**", filters = "open" })
    btv.test.expect(lists(t, "src/main.rs")).to_be(true)
    btv.test.expect(lists(t, "src/deep/nested.rs")).to_be(true)
    btv.test.expect(lists(t, "sample.txt")).to_be(false)
    t:feed("<Esc>")
  end)

  -- 2. "the query and the filters compose: the fuzzy match narrows what the filter
  --     left."
  btv.test.it("§2 — the query and the filters compose", function(t)
    open(t)
    open_picker(t, "mine", { include = "src/**", filters = "open" })
    btv.test.expect(lists(t, "src/main.rs")).to_be(true)
    t:feed("deep")
    t:wait_for(function()
      return t:menu() and not lists(t, "src/main.rs")
    end, { message = "the query never narrowed the filtered list" })
    btv.test.expect(lists(t, "nested.rs")).to_be(true)
    t:feed("<Esc>")
  end)

  -- 7. "Declaring `filter = true` is all it took."
  btv.test.it("§7 — the example's own source gets the boxes by declaring filter", function(t)
    open(t)
    open_picker(t, "mine")
    t:feed("<C-g>")
    t:sleep(60)
    -- The boxes are up: the picker still lists rows, and the message did not say
    -- this source has no filters.
    btv.test.expect(t:menu()).never.to_be_nil()
    btv.test.expect(t:message()).never.to_contain("no include/exclude")
    t:feed("<Esc>")
  end)

  -- 8. "<C-g> on a picker that has no boxes … a message saying this picker has no
  --     include/exclude filters."
  btv.test.it("§8 — <C-g> on an unfilterable picker says so", function(t)
    open(t)
    picker(t, "<Bslash>fb")
    t:feed("<C-g>")
    t:wait_for(function()
      return (t:message() or ""):find("filter", 1, true) ~= nil
    end, { message = "the buffers picker said nothing about filters" })
    t:feed("<Esc>")
  end)

  -- 4. "`btv.picker.history('exclude')` reads the list; `btv.picker.forget_history()`
  --     clears it."
  btv.test.it("§4 — the per-box line history is readable and clearable", function(t)
    open(t)
    btv.picker.forget_history()
    t:feed("<Esc>")
    btv.test.expect(btv.picker.history("exclude")).to_equal({})
    open_picker(t, "mine", { exclude = "*.lock", filters = "open" })
    t:feed("<Esc>")
    t:sleep(60)
    btv.test.expect(btv.picker.history("exclude")).to_contain("*.lock")
    -- "The history is per box": an exclude line never surfaces in the include box.
    btv.test.expect(btv.picker.history("include")).never.to_contain("*.lock")
    btv.picker.forget_history()
    t:feed("<Esc>")
    btv.test.expect(btv.picker.history("exclude")).to_equal({})
  end)

  btv.test.it("the setup kept the history depth the config asked for", function(t)
    open(t)
    -- `history = 20` in `btv.picker.setup`; the box keeps lines rather than
    -- discarding them.
    btv.picker.forget_history()
    t:feed("<Esc>")
    for _, pat in ipairs({ "*.a", "*.b", "*.c" }) do
      open_picker(t, "mine", { exclude = pat, filters = "open" })
      t:feed("<Esc>")
      t:sleep(40)
    end
    local hist = btv.picker.history("exclude")
    btv.test.expect(#hist >= 3).to_be(true)
    btv.picker.forget_history()
    t:feed("<Esc>")
  end)
end)
