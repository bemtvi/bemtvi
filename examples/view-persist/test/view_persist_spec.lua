-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/view-persist
--
-- The payoff is a RESTART, which one run cannot stage — but everything the restart
-- restores FROM is live here: the component mounts itself, its keys mutate the
-- reactive list, and every mutation re-renders the view. (The slot round-trip and
-- the store's own persistence are covered natively, in the server's session and
-- shada suites; the component's store is namespaced to the config, so a spec in
-- another namespace cannot read it — the rendered rows are the honest view.)
--
-- Every case hands focus back to the main layer before it ends: the per-test
-- baseline's `enew!` runs in whatever window is current, and left in the dock it
-- would replace the sidebar's own buffer.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Whether a case has already waited for the mount. The sidebar is mounted ONCE, by
--- the config, so only the first case can legitimately be waiting on it — every
--- later one is re-focusing a window that already exists. Splitting the two lets the
--- first wait generously (a loaded machine can take seconds to get the dock up)
--- without multiplying that patience by the number of cases.
---
--- It flips on the first ATTEMPT, not the first success. The suite has its own 30s
--- budget, and a long per-case wait that survives a miss is what overruns it: eight
--- cases each patiently waiting out the same absent sidebar spend minutes and report
--- a bare "did not finish" naming nothing, instead of one named failure.
local waited_for_mount = false

--- Focus the sidebar the config mounted, waiting for it to appear, and return its
--- rows with the cursor on the first one.
local function sidebar(t)
  local first = not waited_for_mount
  waited_for_mount = true
  t:wait_for(function()
    for _, w in ipairs(vim.api.nvim_list_wins()) do
      if btv.buf.name(vim.api.nvim_win_get_buf(w)) == "Notes" then
        btv.win.set_current(w)
        return true
      end
    end
    return false
  end, {
    tries = first and 500 or 50,
    interval = 20,
    message = first and "the Notes sidebar never mounted" or "the Notes sidebar went away",
  })
  t:feed("gg")
  return t:lines()
end

--- Hand focus back to the editor, and WAIT for the cross to land.
---
--- `btv.layer.main()` queues the move like every other mutation, and `t:exec` runs
--- its function without settling — so on a slow tick a case can end with focus still
--- in the dock. The next case's baseline `enew!` then runs THERE and replaces the
--- sidebar's own buffer, and every case after it hunts a window that no longer
--- exists. That was this spec's flake: not a wrong assertion anywhere, but a suite
--- that ran out of time looking for a buffer an earlier case had destroyed.
local function leave(t)
  t:exec(function()
    btv.layer.main()
  end)
  t:wait_for(function()
    return btv.buf.name(btv.buf.current()) ~= "Notes"
  end, { tries = 100, interval = 20, message = "focus never left the Notes sidebar" })
end

btv.test.describe("examples/view-persist", function()
  -- "the component re-ran `setup`, which rebuilt the list from this plugin's own
  --  store" — or, on a first run, the friendly default.
  btv.test.it("the sidebar mounts with the first-run default", function(t)
    local lines = sidebar(t)
    btv.test.expect(lines[1]).to_contain("Press <leader>na to add a note")
    leave(t)
  end)

  -- "Mount it in the left dock under a stable persist id."
  btv.test.it("it is mounted as its own filetype in the dock", function(t)
    sidebar(t)
    btv.test.expect(btv.bo.filetype).to_be("btvnotes")
    btv.test.expect(btv.buf.name(0)).to_be("Notes")
    leave(t)
  end)

  -- "<leader>na add a note to the list (saved immediately)"
  btv.test.it("<leader>na appends a note and re-renders", function(t)
    local before = #sidebar(t)
    t:feed("<Bslash>na")
    t:wait_for(function()
      return #t:lines() == before + 1
    end, { message = "the note never appeared" })
    btv.test.expect(t:lines()[before + 1]).to_be("note " .. (before + 1))
    leave(t)
  end)

  btv.test.it("adding twice appends twice", function(t)
    local before = #sidebar(t)
    t:feed("<Bslash>na")
    t:feed("<Bslash>na")
    t:wait_for(function()
      return #t:lines() == before + 2
    end, { message = "the notes never appeared" })
    leave(t)
  end)

  -- "<leader>nd delete the note under the cursor"
  btv.test.it("<leader>nd removes the note under the cursor", function(t)
    sidebar(t)
    t:feed("<Bslash>na")
    t:wait_for(function()
      return #t:lines() >= 2
    end, { message = "the note never appeared" })
    local before = #t:lines()
    local second = t:lines()[2]
    t:feed("gg")
    t:feed("<Bslash>nd")
    t:wait_for(function()
      return #t:lines() == before - 1
    end, { message = "the note was never removed" })
    -- The row under the cursor went, not some other one.
    btv.test.expect(t:lines()[1]).to_be(second)
    leave(t)
  end)

  -- "<CR> echo the note under the cursor"
  btv.test.it("<CR> echoes the note under the cursor", function(t)
    local lines = sidebar(t)
    t:feed("gg")
    t:feed("<CR>")
    t:wait_for(function()
      return (t:message() or ""):find("note:", 1, true) ~= nil
    end, { message = "<CR> echoed nothing" })
    btv.test.expect(t:message()).to_be("note: " .. lines[1])
    leave(t)
  end)

  -- "`render` is pure … the framework re-runs it automatically on every mutation."
  btv.test.it("the rows always mirror the list, add after delete included", function(t)
    sidebar(t)
    t:feed("<Bslash>na")
    t:wait_for(function()
      return #t:lines() >= 2
    end, { message = "the note never appeared" })
    local grown = #t:lines()
    t:feed("gg")
    t:feed("<Bslash>nd")
    t:wait_for(function()
      return #t:lines() == grown - 1
    end, { message = "the note was never removed" })
    t:feed("<Bslash>na")
    t:wait_for(function()
      return #t:lines() == grown
    end, { message = "the re-added note never appeared" })
    leave(t)
  end)

  -- "`btv.shada.save_layout(true)` … opts the layout capture in."
  btv.test.it("the config opted the layout into the session capture", function(t)
    sidebar(t)
    leave(t)
    btv.test.expect(type(btv.shada.save_layout)).to_be("function")
  end)
end)
