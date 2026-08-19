-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/endofline
--
-- Every numbered TRY-IT is about the file's LAST BYTE, so each one writes into a
-- temp copy and reads the bytes back — never touching the checked-in sample,
-- which is deliberately stored without a trailing newline.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- The raw bytes of a file.
local function bytes(path)
  return btv.await(btv.fs.read(path))
end

--- A temp copy of the sample, so a `:w` in a test is harmless.
local function scratch()
  local dst = btv.test.tempdir() .. "/sample.txt"
  btv.await(btv.fs.write(dst, bytes(DIR .. "/sample.txt")))
  return dst
end

--- Open `path` and settle the read.
local function open(t, path)
  t:cmd("e " .. path)
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/endofline", function()
  btv.test.it("the sample really is stored without a trailing newline", function(t)
    local raw = bytes(DIR .. "/sample.txt")
    btv.test.expect(raw:sub(-1)).never.to_be("\n")
  end)

  -- 1. "READ DETECTION — the flag came off the bytes, nothing guessed it"
  btv.test.it("try-it 1 — the read sets 'endofline' from the bytes", function(t)
    open(t, DIR .. "/sample.txt")
    btv.test.expect(btv.bo.endofline).to_be(false)
    t:cmd("set endofline?")
    btv.test.expect(t:message()).to_contain("noendofline")
  end)

  btv.test.it("try-it 1 — the config turned 'fixendofline' off", function(t)
    open(t, DIR .. "/sample.txt")
    btv.test.expect(btv.bo.fixendofline).to_be(false)
    t:cmd("set fixendofline?")
    btv.test.expect(t:message()).to_contain("nofixendofline")
  end)

  -- 1. "SEE: the status line's right side reads 'utf-8[noeol]'"
  btv.test.it("try-it 1 — the status line marks the unterminated file", function(t)
    open(t, DIR .. "/sample.txt")
    btv.test.expect(t:statusline()).to_contain("[noeol]")
  end)

  -- "An empty document … is not a file missing a terminator — so they are not
  --  marked."
  btv.test.it("a scratch buffer is not marked [noeol]", function(t)
    open(t, DIR .. "/sample.txt")
    t:cmd("enew")
    btv.test.expect(btv.bo.endofline).to_be(false)
    btv.test.expect(t:statusline()).never.to_contain("[noeol]")
  end)

  -- 2. "BYTE-FOR-BYTE ROUND TRIP — save an untouched buffer and the file is
  --     unchanged, trailing byte included"
  btv.test.it("try-it 2 — :w round-trips the file byte for byte", function(t)
    local path = scratch()
    local before = bytes(path)
    open(t, path)
    t:cmd("w")
    local after = bytes(path)
    btv.test.expect(after).to_be(before)
    btv.test.expect(after:sub(-1)).never.to_be("\n")
    btv.test.expect(after:sub(-1)).to_be(".")
  end)

  -- 3. "THE DEFAULT, FOR CONTRAST — turn the fixer back on and save again"
  btv.test.it("try-it 3 — :set fixeol supplies the terminator on write", function(t)
    local path = scratch()
    open(t, path)
    t:cmd("set fixeol")
    t:cmd("w")
    btv.test.expect(bytes(path):sub(-1)).to_be("\n")
    -- "the flag reports what is actually on disk"
    btv.test.expect(btv.bo.endofline).to_be(true)
    t:cmd("set endofline?")
    btv.test.expect(t:message()).to_contain("endofline")
    btv.test.expect(t:message()).never.to_contain("noendofline")
    -- "…and the [noeol] marker is gone from the status line"
    btv.test.expect(t:statusline()).never.to_contain("[noeol]")
  end)

  btv.test.it("try-it 3 — and the experiment undoes cleanly", function(t)
    local path = scratch()
    local original = bytes(path)
    open(t, path)
    t:cmd("set fixeol")
    t:cmd("w")
    btv.test.expect(bytes(path)).never.to_be(original)
    t:cmd("set noeol nofixeol")
    t:cmd("w")
    btv.test.expect(bytes(path)).to_be(original)
  end)

  -- 4. "ONLY THE LAST LINE IS AFFECTED."
  btv.test.it("try-it 4 — only the document's end is bare", function(t)
    local path = scratch()
    open(t, path)
    t:cmd("set nofixeol")
    t:feed("Gonew last line<Esc>")
    t:cmd("w")
    local raw = bytes(path)
    btv.test.expect(raw:sub(-#"new last line")).to_be("new last line")
    -- Every earlier line is still terminated: the line before it ends in a break.
    btv.test.expect(raw).to_contain(".\nnew last line")
  end)

  -- 5. "AN EMPTY FILE STAYS EMPTY."
  btv.test.it("try-it 5 — an empty file is written back at 0 bytes", function(t)
    local path = btv.test.tempdir() .. "/empty-demo"
    btv.await(btv.fs.write(path, ""))
    open(t, path)
    t:cmd("w")
    btv.test.expect(bytes(path)).to_be("")
  end)

  -- The config's own §1: the autocmd carries the opt-out to every later buffer.
  btv.test.it("the BufReadPost autocmd carries nofixeol to every buffer", function(t)
    local other = btv.test.tempdir() .. "/other.txt"
    btv.await(btv.fs.write(other, "just a line\n"))
    open(t, other)
    btv.test.expect(btv.bo.fixendofline).to_be(false)
    -- …and a terminated file keeps its terminator, of course.
    btv.test.expect(btv.bo.endofline).to_be(true)
    t:cmd("w")
    btv.test.expect(bytes(other)).to_be("just a line\n")
  end)
end)
