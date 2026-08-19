-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/async-events
--
-- It loads `init.lua` exactly as a session would, then drives the same keys and
-- commands the numbered notes tell a reader to type — so a demo cannot rot into
-- an instruction that no longer works.
--
-- Most of what this example demonstrates is *ordering*, which the config reports
-- through `print`. So the spec swaps in a recording `print` around the action it
-- is measuring and asserts on the transcript, rather than on the one message
-- line that only ever holds the last of them.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Record everything the config prints while `body` runs, in order.
---
--- `print` is looked up as a global at call time, so replacing it here really
--- does capture the config's own lines; the original is always put back.
local function transcript(body)
  local seen, real = {}, print
  _G.print = function(...)
    local parts = {}
    for i = 1, select("#", ...) do
      parts[i] = tostring((select(i, ...)))
    end
    seen[#seen + 1] = table.concat(parts, " ")
  end
  local ok, err = pcall(body)
  _G.print = real
  if not ok then
    error(err, 0)
  end
  return seen
end

--- Index of the first recorded line containing `needle`, or nil.
local function index_of(lines, needle)
  for i, line in ipairs(lines) do
    if line:find(needle, 1, true) then
      return i
    end
  end
  return nil
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Re-read the file that is already current: exactly ONE more read sequence.
local function reread(t)
  t:cmd("e!")
end

--- How many lines of the transcript contain `needle`.
local function count_of(lines, needle)
  local n = 0
  for _, line in ipairs(lines) do
    if line:find(needle, 1, true) then
      n = n + 1
    end
  end
  return n
end

btv.test.describe("examples/async-events", function()
  btv.test.it("the config registers a handler for each demo", function(t)
    open(t)
    local events = {}
    for _, au in ipairs(btv.autocmd.get({})) do
      events[au.event] = true
    end
    for _, want in ipairs({ "CursorMoved", "BufReadPost", "FileType", "BufWinEnter", "User" }) do
      btv.test.expect(events[want]).to_be_truthy()
    end
  end)

  -- Demo 1. A hot-path handler must be synchronous. This one starts async work
  -- but does not RETURN it, so moving the cursor is quiet.
  btv.test.it("demo 1 — moving the cursor does not raise", function(t)
    open(t)
    t:feed("jklh")
    btv.test.expect(t:message()).never.to_contain("E5108")
    btv.test.expect(t:mode()).to_be("n")
  end)

  btv.test.it("demo 1 — the handler's async work still runs, off the tick", function(t)
    open(t)
    local lines = transcript(function()
      t:feed("jjkk")
      t:sleep(30)
    end)
    btv.test.expect(index_of(lines, "cursor moves:")).never.to_be_nil()
  end)

  -- Demo 2. The read sequence advances one stage at a time, waiting for each
  -- stage's async handlers — so FileType sees the filetype BufReadPost set.
  btv.test.it("demo 2 — the async BufReadPost handler sets the filetype", function(t)
    open(t)
    btv.test.expect(btv.bo.filetype).to_be("demo")
  end)

  btv.test.it("demo 2 — FileType runs only after BufReadPost settles", function(t)
    local lines = transcript(function()
      open(t)
      t:sleep(120)
    end)
    local started = index_of(lines, "2. BufReadPost start")
    local done = index_of(lines, "2. BufReadPost done")
    local ft = index_of(lines, "2. FileType = demo")
    btv.test.expect(started).never.to_be_nil()
    btv.test.expect(done).never.to_be_nil()
    btv.test.expect(ft).never.to_be_nil()
    btv.test.expect(started < done).to_be(true)
    btv.test.expect(done < ft).to_be(true)
  end)

  -- Demo 3. A handler registered *while* the event was being delivered still
  -- receives it.
  btv.test.it("demo 3 — the late subscriber receives the same event", function(t)
    local lines = transcript(function()
      open(t)
      t:sleep(150)
    end)
    local ft = index_of(lines, "2. FileType = demo")
    local late = index_of(lines, "3. late subscriber ran for demo")
    btv.test.expect(late).never.to_be_nil()
    -- It ran after the fire that registered it, not before.
    btv.test.expect(ft < late).to_be(true)
  end)

  btv.test.it("demo 3 — a handler that already ran is never re-run", function(t)
    -- Every read registers one more late subscriber, and they are never removed,
    -- so the transcript grows by exactly ONE line per read. It would DOUBLE if
    -- delivery were not filtered by registration order — each already-delivered
    -- subscriber running again on the replay the newcomer triggers.
    open(t)
    local function reads()
      return count_of(
        transcript(function()
          reread(t)
          t:sleep(150)
        end),
        "3. late subscriber ran for demo"
      )
    end
    local first = reads()
    local second = reads()
    btv.test.expect(second).to_be(first + 1)
  end)

  -- Demo 4. The budget bounds the WAIT, not the delivery: the slow handler still
  -- finishes, it just does not hold the sequence up.
  btv.test.it("demo 4 — the over-budget handler still completes", function(t)
    local lines = transcript(function()
      open(t)
      t:sleep(500)
    end)
    btv.test.expect(index_of(lines, "4. the slow handler finally finished")).never.to_be_nil()
  end)

  -- Demo 5. A handler that never settles has nothing to report, so `pending()`
  -- is where it stays visible.
  btv.test.it("demo 5 — a hung handler is listed by btv.autocmd.pending()", function(t)
    open(t)
    btv.autocmd.exec("User", { pattern = "NeverSettles" })
    local entry = t:wait_for(function()
      for _, e in ipairs(btv.autocmd.pending()) do
        if e.event == "User" then
          return e
        end
      end
      return nil
    end, { message = "the hung User handler never showed up in pending()" })
    btv.test.expect(entry.budget).to_be(50)
    btv.test.expect(entry.site).to_contain("init.lua")
    -- `pending()` only lists what is already past its budget; the reported
    -- elapsed time is floored, so it can equal the budget on the first poll.
    btv.test.expect(entry.elapsed_ms >= entry.budget).to_be(true)
  end)
end)
