-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/async-runtime
--
-- Everything this example demonstrates happens on wall-clock time with no
-- keypresses, so the spec mostly waits: it sources `init.lua` the way a session
-- would and then watches `_G.async_demo` — the very table the notes tell a
-- reader to inspect with `:lua print(vim.inspect(_G.async_demo))`.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- Demo 1's claim is about ORDER, and it is settled while the config is being
-- sourced — so the recorder has to be in place for the `dofile` itself.
local sourcing = {}
do
  local real = print
  _G.print = function(...)
    local parts = {}
    for i = 1, select("#", ...) do
      parts[i] = tostring((select(i, ...)))
    end
    sourcing[#sourcing + 1] = table.concat(parts, " ")
  end
  local ok, err = pcall(dofile, DIR .. "/init.lua")
  _G.print = real
  if not ok then
    error(err, 0)
  end
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

btv.test.describe("examples/async-runtime", function()
  btv.test.it("the config exposes the state the notes tell you to inspect", function(t)
    btv.test.expect(type(_G.async_demo)).to_be("table")
    btv.test.expect(type(_G.async_demo.timer_ticks)).to_be("number")
  end)

  -- Demo 1. `vim.schedule` defers to the END of the convergence rather than
  -- running inline — so the line printed *after* the call is emitted first, and
  -- the scheduled one had not run at all by the time sourcing returned.
  btv.test.it("demo 1 — vim.schedule does not run inline", function(t)
    btv.test.expect(index_of(sourcing, "[schedule] this 'direct' line")).never.to_be_nil()
    btv.test.expect(index_of(sourcing, "[schedule] ran after the config")).to_be_nil()
  end)

  btv.test.it("demo 1 — the scheduled callback does run, once sourcing settles", function(t)
    t:wait_for(function()
      return _G.async_demo.schedule == "ran"
    end, { message = "the vim.schedule callback never ran" })
  end)

  -- Demo 2. A one-shot wall-clock delay on the loop, off the input tick.
  btv.test.it("demo 2 — vim.defer_fn fires after its delay", function(t)
    t:wait_for(function()
      return _G.async_demo.defer == "fired"
    end, { tries = 200, interval = 10, message = "vim.defer_fn never fired" })
  end)

  -- Demo 3. A self-rescheduling timer that stops itself after four ticks.
  btv.test.it("demo 3 — the repeating timer ticks four times and stops", function(t)
    t:wait_for(function()
      return _G.async_demo.timer_ticks >= 4
    end, { tries = 300, interval = 10, message = "the timer never reached 4 ticks" })
    -- It stopped itself: no fifth tick lands.
    t:sleep(400)
    btv.test.expect(_G.async_demo.timer_ticks).to_be(4)
  end)

  -- The whole point of the loop: none of that blocked the editor.
  btv.test.it("the editor stays responsive while the timers run", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    t:feed("ggOtyped while the timers ran<Esc>")
    btv.test.expect(t:line(1)).to_be("typed while the timers ran")
    btv.test.expect(t:mode()).to_be("n")
  end)
end)
