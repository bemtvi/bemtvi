-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/daemon-status
--
-- The runner is a LOCAL session, which is itself one of the cases the example
-- documents ("A LOCAL session (no daemon) reports nil, so the segment renders
-- nothing"). The other three phases are what the segment MAPS, so the spec drives
-- the mapping directly — the same `phase_chunk` the segment renders with, reached
-- through the segment registry — and fires the `User DaemonStatusChanged` event
-- the config listens for. What it does not do is fake a link: bringing a real
-- daemon up and dropping it belongs to the daemon suites.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

--- Paint the bar with `btv.daemon.status()` standing in for a link the runner
--- cannot bring up, and return the rendered text. The segment is cached until
--- invalidated, so this fires the very event the config listens for.
local function bar_with_phase(t, phase)
  local real = btv.daemon.status
  btv.daemon.status = function()
    return phase
  end
  btv.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
  t:feed("j")
  local bar = t:statusline()
  btv.daemon.status = real
  btv.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
  t:feed("k")
  return bar
end

btv.test.describe("examples/daemon-status", function()
  btv.test.it("a local session reports no daemon", function(t)
    open(t)
    local phase = btv.daemon.status()
    btv.test.expect(phase == nil or phase == "local").to_be(true)
  end)

  btv.test.it("so the segment renders nothing, and the bar is unaffected", function(t)
    open(t)
    local bar = t:statusline()
    btv.test.expect(bar).never.to_contain("daemon")
    btv.test.expect(bar).never.to_contain("reconnecting")
    btv.test.expect(bar).never.to_contain("disconnected")
    -- The rest of the layout the config asked for is still there.
    btv.test.expect(bar).to_contain("sample.txt")
    btv.test.expect(bar).to_match("1:1")
  end)

  -- The three phase colours the config defines, which are the whole point of it.
  btv.test.it("the three phase highlight groups are defined", function(t)
    open(t)
    for _, group in ipairs({ "DaemonOk", "DaemonWait", "DaemonDown" }) do
      local def = btv.hl.get(0, { name = group }) or {}
      btv.test.expect(def.fg).never.to_be_nil()
      btv.test.expect(def.bold).to_be(true)
    end
  end)

  -- The mapping from phase to what is painted, over the real bar.
  btv.test.it("each phase paints its own icon", function(t)
    open(t)
    btv.test.expect(bar_with_phase(t, "connected")).to_contain("● daemon")
    btv.test.expect(bar_with_phase(t, "reconnecting")).to_contain("◌ reconnecting")
    btv.test.expect(bar_with_phase(t, "disconnected")).to_contain("✕ disconnected (:reconnect)")
    -- …and neither local nor nil paints anything at all.
    btv.test.expect(bar_with_phase(t, "local")).never.to_contain("daemon")
    btv.test.expect(bar_with_phase(t, nil)).never.to_contain("daemon")
  end)

  -- "a `User DaemonStatusChanged` autocmd that fires on every transition"
  btv.test.it("the config listens for DaemonStatusChanged", function(t)
    open(t)
    local listening = false
    for _, au in ipairs(btv.autocmd.get({ event = "User" })) do
      if au.pattern == "DaemonStatusChanged" then
        listening = true
      end
    end
    btv.test.expect(listening).to_be(true)
  end)

  btv.test.it("firing it repaints the segment", function(t)
    open(t)
    local real = btv.daemon.status
    btv.daemon.status = function()
      return "disconnected"
    end
    -- Without the invalidate the cached render would stand: the segment only
    -- re-runs when told to.
    btv.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
    t:feed("j")
    btv.test.expect(t:statusline()).to_contain("disconnected")
    btv.daemon.status = function()
      return "connected"
    end
    btv.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
    t:feed("k")
    btv.test.expect(t:statusline()).to_contain("● daemon")
    btv.daemon.status = real
    btv.autocmd.exec("User", { pattern = "DaemonStatusChanged" })
  end)

  -- ":reconnect re-dials now; :disconnect drops the link on demand. Both work on
  --  the TUI too (server-side ex-commands)."
  btv.test.it(":reconnect and :disconnect exist, and say so with no link", function(t)
    open(t)
    t:cmd("disconnect")
    btv.test.expect(t:message()).never.to_contain("E492")
    t:cmd("reconnect")
    btv.test.expect(t:message()).never.to_contain("E492")
  end)
end)
