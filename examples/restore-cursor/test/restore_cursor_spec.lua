-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/restore-cursor
--
-- The tour's gesture is "quit and relaunch", which a single session cannot do.
-- The mechanism it rides on can be driven here in full, though: the `"` mark is
-- recorded when a file-backed buffer is left (and stashed by path when it is
-- wiped), and `'restorecursor'` arms the `BufReadPost` jump that reads it back. So
-- wiping the buffer and opening the file afresh exercises exactly the two halves a
-- relaunch does — shada is only what carries the mark between sessions.
--
-- The wipe is what makes each case honest: reopening a buffer that is merely
-- *hidden* restores its own saved cursor whether or not the option is on, so a test
-- that skipped it would pass with the feature turned off.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")
local SAMPLE = DIR .. "/sample.txt"

dofile(DIR .. "/init.lua")

--- Open the sample with no remembered position at all: wipe any buffer on it (so
--- its stashed marks go with it) and read the file in fresh.
local function open(t)
  t:cmd("enew!")
  t:cmd("silent! bwipeout! " .. SAMPLE)
  t:cmd("e " .. SAMPLE)
end

--- Leave the sample and open it again from scratch — the in-session stand-in for
--- quitting and relaunching.
local function reopen(t)
  t:cmd("enew!")
  t:cmd("bwipeout! " .. SAMPLE)
  t:cmd("e " .. SAMPLE)
end

btv.test.describe("examples/restore-cursor", function()
  -- "this one line is the whole feature: vim.o.restorecursor = true"
  btv.test.it("the config turns 'restorecursor' on", function(t)
    open(t)
    btv.test.expect(btv.o.restorecursor).to_be(true)
  end)

  btv.test.it("a first open still lands at the top", function(t)
    open(t)
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)

  -- "move the cursor down a few lines (say 15G) … the cursor lands back on 15"
  btv.test.it("reopening the file lands back on the line you left", function(t)
    open(t)
    t:feed("15G")
    btv.test.expect(t:cursor()[1]).to_be(15)
    -- Leave the file, then read it again from scratch — the `"` mark carries the
    -- position across the wipe.
    reopen(t)
    btv.test.expect(t:cursor()[1]).to_be(15)
  end)

  -- "It's a plain option, so you can flip it from the command line too."
  btv.test.it("with it off the reopen goes back to the top", function(t)
    open(t)
    t:feed("15G")
    t:exec(function()
      vim.o.restorecursor = false
    end)
    reopen(t)
    btv.test.expect(t:cursor()[1]).to_be(1)
  end)

  -- "Under the hood the jump runs through bemtvi's `:normal` command. You can use
  --  that yourself for any keystroke sequence, e.g. lowercase the whole file."
  btv.test.it(":%normal! guu runs the same keystroke path over every line", function(t)
    open(t)
    t:cmd("%normal! guu")
    btv.test.expect(t:line(1)).to_be("restore-cursor demo")
    btv.test.expect(t:line(4)).to_be(
      "move the cursor somewhere down this file, then quit with :q."
    )
    btv.test.expect(t:line(5)).to_contain("re-run the same command")
  end)
end)
