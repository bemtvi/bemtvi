-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/btvchecklist
--
-- It sources `init.lua` as a session would and drives exactly the keys the
-- TRY-IT notes list, so the dialog cannot rot into an instruction that no longer
-- works. The dialog is a real (floating) window with a real buffer, so its rows
-- are `t:lines()` — no special surface needed.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The dialog reports through `btv.notify`, and it does so as it CLOSES — the
-- repaint that tears the float down takes the message line with it, and the
-- config's own startup mount holds the focus lock for the whole run, so the
-- `:messages` panel can never be focused to read the log back. Recording the
-- notifications at the source is both simpler and exact. Wrapped before the
-- config is sourced so nothing it emits is missed.
local notified = {}
do
  local real = btv.notify
  btv.notify = function(msg, level)
    notified[#notified + 1] = tostring(msg)
    return real(msg, level)
  end
end

dofile(DIR .. "/init.lua")

--- The most recent notification, or "" when there has been none.
local function last_notify()
  return notified[#notified] or ""
end

-- No `mapleader` is set, so `<leader>` is the default backslash.
local OPEN = "<Bslash>c"

--- Open the dialog and wait for its first render.
local function open(t)
  t:feed(OPEN)
  t:wait_for(function()
    return (t:line(1) or ""):find("Format on save", 1, true) ~= nil
  end, { message = "the checklist never rendered" })
end

--- The item rows only (the trailing blank + hint row dropped).
local function items(t)
  local rows, out = t:lines(), {}
  for i = 1, #rows - 2 do
    out[i] = rows[i]
  end
  return out
end

--- The hint row — the last line, which carries the live selected count.
local function hint(t)
  local rows = t:lines()
  return rows[#rows]
end

--- The labels currently ticked, in order — what `<CR>` promises to report.
local function checked(t)
  local out = {}
  for _, row in ipairs(items(t)) do
    local label = row:match("^☑%s+(.*)$")
    if label then
      out[#out + 1] = label
    end
  end
  return out
end

btv.test.describe("examples/btvchecklist", function()
  btv.test.it("<leader>c opens the dialog with one row per item", function(t)
    open(t)
    local rows = items(t)
    btv.test.expect(#rows).to_be(5)
    btv.test.expect(rows[1]).to_contain("Format on save")
    btv.test.expect(rows[5]).to_contain("Trim trailing whitespace")
    -- Every row is one of the two checkbox glyphs, nothing half-rendered.
    for _, row in ipairs(rows) do
      btv.test.expect(row:sub(1, 3) == "☑" or row:sub(1, 3) == "☐").to_be(true)
    end
    t:feed("<Esc>")
  end)

  btv.test.it("the hint row reports the live selected count", function(t)
    open(t)
    btv.test.expect(hint(t)).to_contain(#checked(t) .. " selected")
    btv.test.expect(hint(t)).to_contain("<Tab> move")
    btv.test.expect(hint(t)).to_contain("<Esc> cancel")
    t:feed("<Esc>")
  end)

  btv.test.it("<Tab> and <S-Tab> move between items, wrapping", function(t)
    open(t)
    btv.test.expect(t:cursor()[1]).to_be(1)
    t:feed("<Tab>")
    btv.test.expect(t:cursor()[1]).to_be(2)
    t:feed("<S-Tab>")
    btv.test.expect(t:cursor()[1]).to_be(1)
    -- Backwards off the top wraps to the last ITEM row, never onto the hint.
    t:feed("<S-Tab>")
    btv.test.expect(t:cursor()[1]).to_be(5)
    t:feed("<Tab>")
    btv.test.expect(t:cursor()[1]).to_be(1)
    t:feed("<Esc>")
  end)

  btv.test.it("j and k move too", function(t)
    open(t)
    t:feed("jj")
    btv.test.expect(t:cursor()[1]).to_be(3)
    t:feed("k")
    btv.test.expect(t:cursor()[1]).to_be(2)
    t:feed("<Esc>")
  end)

  -- The reactive write: `it.checked = not it.checked` re-renders on its own —
  -- both the row's glyph and the derived count in the hint.
  btv.test.it("<Space> toggles the row under the cursor and re-renders", function(t)
    open(t)
    local before = items(t)[1]
    local n = #checked(t)
    t:feed("<Space>")
    local after = items(t)[1]
    btv.test.expect(after).never.to_be(before)
    btv.test.expect(#checked(t)).to_be(before:sub(1, 3) == "☑" and n - 1 or n + 1)
    -- The computed count followed the same write, with no manual re-render.
    btv.test.expect(hint(t)).to_contain(#checked(t) .. " selected")
    -- Put it back so the shared item list leaves the next test where it found it.
    t:feed("<Space>")
    btv.test.expect(items(t)[1]).to_be(before)
    t:feed("<Esc>")
  end)

  btv.test.it("a toggle only touches the row under the cursor", function(t)
    open(t)
    local before = items(t)
    t:feed("<Tab><Space>")
    local after = items(t)
    for i = 1, #before do
      if i == 2 then
        btv.test.expect(after[i]).never.to_be(before[i])
      else
        btv.test.expect(after[i]).to_be(before[i])
      end
    end
    t:feed("<Space><Esc>")
  end)

  -- The checked rows carry the example's own highlight group; the hint row its
  -- dimmer one. Neither is buffer text, so this is `t:highlights()` work.
  btv.test.it("a ticked row's checkbox is painted, and the hint is dimmed", function(t)
    open(t)
    local rows = items(t)
    local ticked
    for i, row in ipairs(rows) do
      if row:sub(1, 3) == "☑" then
        ticked = i
        break
      end
    end
    btv.test.expect(ticked).never.to_be_nil()
    local spans = t:highlights(ticked)
    btv.test.expect(#spans >= 1).to_be(true)
    btv.test.expect(spans[1][3]).to_be("BtvChecklistOn")
    local hint_spans = t:highlights(#rows + 2)
    btv.test.expect(#hint_spans >= 1).to_be(true)
    btv.test.expect(hint_spans[1][3]).to_be("BtvChecklistHint")
    t:feed("<Esc>")
  end)

  btv.test.it("<CR> confirms and reports the checked labels", function(t)
    open(t)
    local want = table.concat(checked(t), ", ")
    _G.btvchecklist_result = nil
    t:feed("<CR>")
    t:wait_for(function()
      return _G.btvchecklist_result ~= nil
    end, { message = "<CR> never reported a result" })
    btv.test.expect(_G.btvchecklist_result).to_be(want)
    btv.test.expect(last_notify()).to_be("enabled: " .. want)
  end)

  btv.test.it("<CR> closes the dialog", function(t)
    open(t)
    local dialog = t:buf()
    t:feed("<CR>")
    t:wait_for(function()
      return t:buf() ~= dialog
    end, { message = "the dialog stayed open after <CR>" })
  end)

  btv.test.it("<Esc> cancels without reporting a selection", function(t)
    open(t)
    local dialog = t:buf()
    _G.btvchecklist_result = nil
    t:feed("<Esc>")
    t:wait_for(function()
      return _G.btvchecklist_result ~= nil
    end, { message = "<Esc> never reported a cancellation" })
    btv.test.expect(_G.btvchecklist_result).to_be("<cancelled>")
    btv.test.expect(last_notify()).to_be("checklist cancelled")
    btv.test.expect(t:buf()).never.to_be(dialog)
  end)

  btv.test.it("re-opening keeps the ticks — the dialog remembers", function(t)
    open(t)
    t:feed("<Space>")
    local want = checked(t)
    t:feed("<Esc>")
    open(t)
    btv.test.expect(checked(t)).to_equal(want)
    t:feed("<Space><Esc>")
  end)
end)
