-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/picker-priority
--
-- Each case presses the keys a numbered note in init.lua tells a reader to press and
-- checks what that note says they will see, on the picker's rows (`t:menu()`).

local DIR = btv.utils.dirname(btv.utils.dirname(debug.getinfo(1, "S").source:sub(2)))

dofile(DIR .. "/init.lua")

--- Back to the sample with nothing floating, the shipped deprioritize list in force.
local function open(t)
  for _ = 1, 3 do
    t:feed("<Esc>")
    t:sleep(30)
  end
  btv.picker.forget_history()
  t:cmd("cd " .. DIR)
  t:cmd("e " .. DIR .. "/sample.txt")
end

--- The row index (1-based) of the first row mentioning `needle`, separators folded to
--- `/` (the file pickers list native paths), or nil.
local function row_of(t, needle)
  for i, row in ipairs((t:menu() or { items = {} }).items) do
    if row:gsub("\\", "/"):find(needle, 1, true) then
      return i
    end
  end
  return nil
end

--- Press `keys` (and type `query`), then wait until every one of `needles` is listed.
local function search(t, keys, query, needles)
  t:feed(keys)
  t:wait_for(function()
    return t:menu()
  end, { message = "the picker never opened" })
  if query then
    t:feed(query)
  end
  local ok = pcall(t.wait_for, t, function()
    for _, n in ipairs(needles) do
      if not row_of(t, n) then
        return nil
      end
    end
    return true
  end, { tries = 150, interval = 20 })
  if not ok then
    error(
      "never listed all of "
        .. table.concat(needles, ", ")
        .. "; rows: "
        .. table.concat((t:menu() or { items = {} }).items, " | "),
      0
    )
  end
end

--- Every `first` row lists above every `then_` row.
local function above(t, first, then_)
  for _, a in ipairs(first) do
    for _, b in ipairs(then_) do
      btv.test.expect(row_of(t, a) < row_of(t, b)).to_be(true)
    end
  end
end

-- The search §1 names, spelled so this file is not a hit itself.
local QUERY = "use" .. "Auth"

local VENDORED = { "dist/auth.min.js", "node_modules/auth-lib/index.js", "yarn.lock" }

btv.test.describe("examples/picker-priority", function()
  -- 1. "the project's own hits first … then `dist/auth.min.js`,
  --     `node_modules/auth-lib/index.js` and `yarn.lock` below them."
  btv.test.it("§1 — live grep lists the vendored hits last", function(t)
    open(t)
    search(t, "<Bslash>fg", QUERY, { "src/auth.ts", "src/auth.test.ts", table.unpack(VENDORED) })
    above(t, { "src/auth.ts", "src/auth.test.ts", "init.lua" }, VENDORED)
    t:feed("<Esc>")
  end)

  -- "the same order for files … Type `auth` and the vendored files stay below `src/`"
  btv.test.it("§1 — and so does the file finder, query or not", function(t)
    open(t)
    search(t, "<Bslash>ff", nil, { "src/auth.ts", table.unpack(VENDORED) })
    above(t, { "src/auth.ts", "src/auth.test.ts" }, VENDORED)
    t:feed("auth")
    t:wait_for(function()
      return row_of(t, "sample.txt") == nil and row_of(t, "src/auth.ts")
    end, { message = "the query never narrowed the list" })
    above(
      t,
      { "src/auth.ts", "src/auth.test.ts" },
      { "dist/auth.min.js", "node_modules/auth-lib/index.js" }
    )
    t:feed("<Esc>")
  end)

  -- 2. "`src/auth.test.ts` has moved down among the vendored hits … `\pt` again puts
  --     the shipped list back."
  btv.test.it("§2 — \\pt lists test files last, and toggles back", function(t)
    open(t)
    t:feed("<Bslash>pt")
    search(t, "<Bslash>fg", QUERY, { "src/auth.ts", "src/auth.test.ts", table.unpack(VENDORED) })
    above(t, { "src/auth.ts", "init.lua" }, { "src/auth.test.ts" })
    t:feed("<Esc>")
    t:sleep(30)
    t:feed("<Bslash>pt")
    search(t, "<Bslash>fg", QUERY, { "src/auth.ts", "src/auth.test.ts", table.unpack(VENDORED) })
    above(t, { "src/auth.test.ts" }, VENDORED)
    t:feed("<Esc>")
  end)

  -- 3. "no `generated/bundle.js`: `\fg` respects `.gitignore` … `\fG` searches every
  --     file, ignored ones too."
  btv.test.it("§3 — \\fg skips the ignored file, \\fG finds it", function(t)
    open(t)
    local bundle = btv.utils.path_join(DIR, "generated", "bundle.js")
    for _ = 1, 50 do
      if btv.await(btv.fs.exists(bundle)) then
        break
      end
      t:sleep(20)
    end
    btv.test.expect(btv.await(btv.fs.exists(bundle))).to_be(true)
    search(t, "<Bslash>fg", QUERY, { "src/auth.ts", table.unpack(VENDORED) })
    btv.test.expect(row_of(t, "generated/bundle.js")).to_be_nil()
    t:feed("<Esc>")
    t:sleep(30)
    search(t, "<Bslash>fG", QUERY, { "src/auth.ts", "generated/bundle.js" })
    t:feed("<Esc>")
  end)

  -- "The box title says which one is open"
  btv.test.it("§3 — the box titles name the two modes", function(t)
    open(t)
    btv.test.expect(btv.picker._sources.live_grep_git.title).to_equal("Live Grep")
    btv.test.expect(btv.picker._sources.live_grep.title).to_equal("Live Grep (All Files)")
  end)
end)
