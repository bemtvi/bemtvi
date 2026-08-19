-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/picker-rows
--
-- A picker row's SHAPE — a head, a pinned tag, a colour — is what the widget was
-- handed, so the rows themselves are `t:menu()`.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

local notified = {}
do
  local real_btv, real_vim = btv.notify, vim.notify
  btv.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_btv(msg, ...)
  end
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real_vim(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  for _ = 1, 4 do
    if t:menu() == nil and t:mode() == "n" then
      break
    end
    t:feed("<Esc>")
    t:sleep(40)
  end
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Open a picker and wait for its rows.
local function picker(t, keys)
  t:feed(keys)
  return t:wait_for(function()
    local m = t:menu()
    return m and #m.items > 0 and m or nil
  end, { message = "the picker never opened" })
end

btv.test.describe("examples/picker-rows", function()
  -- 2. "<leader>ft -> `TODO` rows painted like errors and `NOTE` rows like hints,
  --     each with its file:line head aligned down the list."
  btv.test.it("§2 — a row carries its tag, its head and its body", function(t)
    open(t)
    local m = picker(t, "<Space>ft")
    btv.test.expect(#m.items).to_be(3)
    local joined = table.concat(m.items, "\n")
    -- The tag leads…
    btv.test.expect(joined).to_contain("TODO")
    btv.test.expect(joined).to_contain("NOTE")
    -- …the head names the location…
    btv.test.expect(joined).to_contain("sample.txt:3")
    btv.test.expect(joined).to_contain("sample.txt:9")
    -- …and the body is the text.
    btv.test.expect(joined).to_contain("rewrite this in terms of the rope")
    t:feed("<Esc>")
  end)

  btv.test.it("§2 — the heads line up down the list", function(t)
    open(t)
    local m = picker(t, "<Space>ft")
    local at
    for _, row in ipairs(m.items) do
      local col = row:find("sample%.txt")
      btv.test.expect(col).never.to_be_nil()
      if at then
        btv.test.expect(col).to_be(at)
      else
        at = col
      end
    end
    t:feed("<Esc>")
  end)

  -- "`hl` names a highlight group, so the colors come from the THEME."
  btv.test.it("§2 — the fallback groups are installed from the palette", function(t)
    open(t)
    for _, group in ipairs({ "ExampleTodo", "ExampleNote" }) do
      local def = btv.hl.get(0, { name = group }) or {}
      btv.test.expect(def.fg).never.to_be_nil()
    end
    -- "Re-derive on `ColorScheme` and the marks track whatever you load."
    local listening = false
    for _, au in ipairs(btv.autocmd.get({ event = "ColorScheme" })) do
      listening = true
    end
    btv.test.expect(listening).to_be(true)
  end)

  -- 3. "<leader>fp -> a single-column list — no head, no tag, no color."
  btv.test.it("§3 — a plain row is exactly its text", function(t)
    open(t)
    local m = picker(t, "<Space>fp")
    btv.test.expect(m.items).to_equal({ "alpha", "beta", "gamma" })
    t:feed("<Esc>")
  end)

  btv.test.it("§3 — …and confirming one reports it", function(t)
    open(t)
    picker(t, "<Space>fp")
    -- The picker preselects the first row, so <CR> takes it.
    t:feed("<CR>")
    t:wait_for(function()
      return last_notify():find("picked ", 1, true) ~= nil
    end, { message = "the plain picker never confirmed" })
    btv.test.expect(last_notify()).to_be("picked alpha")
  end)

  -- 1. "<leader>fd -> every row leads with its severity letter … then
  --     file:line:col, then the message — `source: text`, folded onto ONE line."
  btv.test.it("§1 — the diagnostics picker rows carry severity, location and message", function(t)
    open(t)
    -- The config seeds them on VimEnter; re-seed here so the spec does not depend
    -- on whether that already fired for this buffer.
    btv.diagnostic.set(btv.ns.create("example"), btv.buf.current(), {
      {
        lnum = 2,
        col = 0,
        severity = btv.diagnostic.severity.ERROR,
        source = "ty",
        message = "expected `String`,\n   found `&str`",
      },
      {
        lnum = 4,
        col = 6,
        severity = btv.diagnostic.severity.WARN,
        source = "ty",
        message = "unused variable `total`",
      },
    })
    t:feed("<Esc>")
    -- The SHIPPED pickers keep the default `\` leader — their maps predate any
    -- config's `mapleader`.
    local m = picker(t, "<Bslash>fd")
    local joined = table.concat(m.items, "\n")
    btv.test.expect(joined).to_contain("sample.txt")
    btv.test.expect(joined).to_contain("ty:")
    btv.test.expect(joined).to_contain("expected `String`")
    -- "folded onto ONE line however many lines the server sent"
    for _, row in ipairs(m.items) do
      btv.test.expect(row).never.to_contain("\n")
    end
    -- "Errors sort first"
    btv.test.expect(m.items[1]).to_match("^%s*E")
    t:feed("<Esc>")
  end)

  -- "the fuzzy match highlights inside the BODY while the head keeps its color"
  btv.test.it("§2 — typing narrows on the body", function(t)
    open(t)
    local m = picker(t, "<Space>ft")
    btv.test.expect(#m.items).to_be(3)
    t:feed("rope")
    t:wait_for(function()
      local cur = t:menu()
      return cur and #cur.items == 1
    end, { message = "the query never narrowed the list" })
    btv.test.expect(t:menu().items[1]).to_contain("rope")
    t:feed("<Esc>")
  end)
end)
