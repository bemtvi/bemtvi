-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/indent-detect
--
-- Every case types exactly the keys a numbered note in `init.lua` tells a reader to
-- type and asserts what it promises they will see. The config sets the *opposite* of
-- every sample file (8-wide tabs), so each assertion below fails if the detection
-- stops running — the file's own style is the only reason these numbers appear.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open one of the sample files as if for the first time. The wipe is about the runner,
--- not the feature: `btv.test` restores each test's options but not the buffers an
--- earlier test left open, and `:e` on an already-loaded buffer just switches to it
--- (vim's behavior) — no read, so nothing to detect from. Wiping first makes every case
--- a genuine first read, which is what a reader following the notes actually does.
local function open(t, name)
  local path = DIR .. "/" .. name
  t:cmd("bwipeout! " .. path)
  t:cmd("e " .. path)
end

--- The `(expandtab, shiftwidth)` the focused buffer ended up with.
local function style()
  return { btv.bo.expandtab, btv.bo.shiftwidth }
end

btv.test.describe("examples/indent-detect", function()
  btv.test.it("1 — the config really does ask for 8-wide tabs", function(t)
    -- A buffer with no file behind it has nothing to detect from, so it shows the
    -- config's own settings — the thing every case below overrules.
    t:cmd("enew!")
    btv.test.expect(style()).to_equal({ false, 8 })
  end)

  -- "3. TYPE: :set expandtab? shiftwidth?  SEE: expandtab / shiftwidth=2"
  btv.test.it("3 — the 2-space sample overrules the config", function(t)
    open(t, "sample.txt")
    btv.test.expect(style()).to_equal({ true, 2 })
  end)

  -- "3. TYPE: gg>>  SEE: the first line moves right by exactly two SPACES"
  btv.test.it("3 — >> inserts two spaces, not a tab", function(t)
    open(t, "sample.txt")
    t:feed("gg>>")
    btv.test.expect(t:line(1)).to_be("  root:")
  end)

  -- "4. TYPE: :e tabbed.txt  SEE: tabs ; gg>> moves right by one real TAB"
  btv.test.it("4 — the tab-indented file indents with a real tab", function(t)
    open(t, "tabbed.txt")
    btv.test.expect(style()).to_equal({ false, 0 })
    t:feed("gg>>")
    btv.test.expect(t:line(1)).to_be("\tfn main() {")
  end)

  -- "4. TYPE: :b#  SEE: back on sample.txt the statusline says spaces:2 again"
  btv.test.it("4 — the verdict is per buffer, not a global mode", function(t)
    open(t, "sample.txt")
    open(t, "tabbed.txt")
    btv.test.expect(style()).to_equal({ false, 0 })
    t:cmd("b#")
    btv.test.expect(style()).to_equal({ true, 2 })
  end)

  -- "5. TYPE: :e four-space.txt  SEE: spaces:4"
  btv.test.it("5 — the width is read too, not just tabs-vs-spaces", function(t)
    open(t, "four-space.txt")
    btv.test.expect(style()).to_equal({ true, 4 })
  end)

  -- "5. TYPE: ggo then <Tab>  SEE: four spaces"
  btv.test.it("5 — <Tab> inserts the detected width", function(t)
    open(t, "four-space.txt")
    t:feed("ggo<Tab>x<Esc>")
    btv.test.expect(t:line(2)).to_be("    x")
  end)

  -- "6. TYPE: :setlocal noexpandtab shiftwidth=8  SEE: >> inserts a tab"
  btv.test.it("6 — a setting made after the read is the last word", function(t)
    open(t, "four-space.txt")
    -- The read got there first…
    btv.test.expect(style()).to_equal({ true, 4 })
    -- …and setting the options by hand afterwards still wins.
    t:cmd("setlocal noexpandtab shiftwidth=8")
    t:feed("gg>>")
    btv.test.expect(t:line(1)).to_be("\tdef outer():")
  end)

  -- "6. TYPE: :e!  SEE: back to spaces:4"
  btv.test.it("6 — re-reading the file runs the detection again", function(t)
    open(t, "four-space.txt")
    t:cmd("setlocal noexpandtab shiftwidth=8")
    btv.test.expect(style()).to_equal({ false, 8 })
    t:cmd("e!")
    btv.test.expect(style()).to_equal({ true, 4 })
  end)

  -- "7. TYPE: :set noindentdetect | e tabbed.txt  SEE: the config's style stands"
  btv.test.it("7 — :set noindentdetect leaves the config in charge", function(t)
    t:cmd("set noindentdetect")
    open(t, "sample.txt")
    btv.test.expect(style()).to_equal({ false, 8 })
    t:cmd("set indentdetect")
    t:cmd("e!")
    btv.test.expect(style()).to_equal({ true, 2 })
  end)

  btv.test.it("2 — the statusline segment reports the detected style", function(t)
    open(t, "sample.txt")
    btv.test.expect(t:screen()[#t:screen()] ~= nil).to_be(true)
    local seg = btv.statusline._segments.indent
    btv.test.expect(seg.render({})[1].text).to_be("spaces:2")
    open(t, "tabbed.txt")
    btv.test.expect(seg.render({})[1].text).to_be("tabs")
  end)
end)
