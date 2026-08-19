-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/encoding
--
-- Every claim in the notes is about BYTES, so the spec reads and writes them:
-- each round trip is done into a temp copy and compared with the original file,
-- byte for byte, through `btv.fs`.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The config announces each read through `vim.notify`, and the `:e` that triggers
-- it repaints the message line as it finishes — so record the notifications at
-- the source. (`vim.notify` is its own binding, captured when the prelude loaded,
-- so wrapping `btv.notify` would not catch a config that calls the vim spelling.)
local notified = {}
do
  local real = vim.notify
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

--- The most recent notification, or "".
local function last_notify()
  return notified[#notified] or ""
end

--- The raw bytes of a file.
local function bytes(path)
  return btv.await(btv.fs.read(path))
end

--- Copy one of the example's files into a fresh temp dir and return its path, so
--- a `:w` in a test can never touch the checked-in sample.
local function scratch(name)
  local dst = btv.test.tempdir() .. "/" .. name
  btv.await(btv.fs.write(dst, bytes(DIR .. "/" .. name)))
  return dst
end

--- Open `path` and wait for the read to settle.
local function open(t, path)
  t:cmd("e " .. path)
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/encoding", function()
  -- "`latin1.txt` is real ISO-8859-1/windows-1252 text. The single byte 0xe9 shows
  --  as `é`, and `:set fenc?` reports `fileencoding=latin1`."
  btv.test.it("latin1.txt decodes through the fileencodings fallback", function(t)
    open(t, DIR .. "/latin1.txt")
    btv.test.expect(btv.bo.fileencoding).to_be("latin1")
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("é")
    -- The bytes on disk are single-byte, the rope is utf-8: that is the seam.
    btv.test.expect(bytes(DIR .. "/latin1.txt")).to_contain("\xe9")
    btv.test.expect(bytes(DIR .. "/latin1.txt")).never.to_contain("\xc3\xa9")
  end)

  btv.test.it("the config announces the encoding it opened with", function(t)
    open(t, DIR .. "/latin1.txt")
    btv.test.expect(last_notify()).to_contain("fileencoding=latin1")
  end)

  btv.test.it("the detection list is the documented default", function(t)
    open(t, DIR .. "/latin1.txt")
    btv.test.expect(btv.o.fileencodings).to_be("ucs-bom,utf-8,latin1")
  end)

  -- "Save with `:w` and the file stays latin1 — `é` is written back as the one
  --  byte 0xe9."
  btv.test.it("a latin1 file round-trips byte for byte on :w", function(t)
    local path = scratch("latin1.txt")
    local before = bytes(path)
    open(t, path)
    t:cmd("w")
    btv.test.expect(bytes(path)).to_be(before)
  end)

  -- "`invalid-utf8.txt` … falls through `'fileencodings'` to the latin1 fallback,
  --  opens, and `:w` reproduces the original bytes EXACTLY."
  btv.test.it("a file with invalid UTF-8 opens rather than refusing", function(t)
    open(t, DIR .. "/invalid-utf8.txt")
    btv.test.expect(btv.bo.fileencoding).to_be("latin1")
    btv.test.expect(#t:lines() > 0).to_be(true)
  end)

  btv.test.it("…and round-trips byte for byte", function(t)
    local path = scratch("invalid-utf8.txt")
    local before = bytes(path)
    open(t, path)
    t:cmd("w")
    btv.test.expect(bytes(path)).to_be(before)
  end)

  -- "those paint vim-style as the highlighted tokens `^A` and `<81>` … The display
  --  is purely cosmetic — the buffer keeps the raw bytes."
  btv.test.it("control bytes paint as ^A / <81> without changing the buffer", function(t)
    open(t, DIR .. "/invalid-utf8.txt")
    local painted = table.concat(t:screen(), "\n")
    btv.test.expect(painted).to_contain("^A")
    btv.test.expect(painted).to_contain("<81>")
    -- …and the buffer keeps the raw bytes, not the tokens: the C0 0x01 and the
    -- decoded C1 are both still in the text, which is what makes the round trip
    -- byte-identical. (The sample's prose happens to spell "^A" out too, so the
    -- token's absence is not something to assert on.)
    local text = table.concat(t:lines(), "\n")
    btv.test.expect(text).to_contain("\1")
    btv.test.expect(painted).never.to_contain("\1")
  end)

  -- "Converting on save: `:set fenc=utf-8` and `:w` — the file is rewritten as
  --  UTF-8."
  btv.test.it("converting on save rewrites the file in the new encoding", function(t)
    local path = scratch("latin1.txt")
    open(t, path)
    t:cmd("set fileencoding=utf-8")
    t:cmd("w")
    local after = bytes(path)
    btv.test.expect(after).to_contain("\xc3\xa9")
    btv.test.expect(after).never.to_contain("\xe9\x20")
  end)

  -- "Writing a character the target encoding can't represent … aborts the write
  --  with `E513` and leaves the file untouched."
  btv.test.it("an unrepresentable character fails the write loudly", function(t)
    local path = scratch("latin1.txt")
    local before = bytes(path)
    open(t, path)
    t:feed("ggO中<Esc>")
    t:cmd("w")
    btv.test.expect(t:message()).to_contain("E513")
    -- The file is untouched — no partial write, no HTML character reference.
    btv.test.expect(bytes(path)).to_be(before)
  end)

  -- "`shift_jis.txt` is real Shift_JIS … With shift_jis in the list it decodes to
  --  the Japanese text."
  btv.test.it("a CJK encoding is opt-in through fileencodings", function(t)
    -- Not in the default list, so it mis-detects first…
    open(t, DIR .. "/shift_jis.txt")
    btv.test.expect(btv.bo.fileencoding).to_be("latin1")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("こんにちは")
    -- …and decodes once you ask for it.
    t:cmd("set fileencodings=ucs-bom,utf-8,shift_jis,latin1")
    open(t, DIR .. "/shift_jis.txt")
    btv.test.expect(btv.bo.fileencoding).to_be("shift_jis")
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("こんにちは")
    t:cmd("set fileencodings=ucs-bom,utf-8,latin1")
  end)

  btv.test.it("a Shift_JIS file round-trips byte for byte", function(t)
    local path = scratch("shift_jis.txt")
    local before = bytes(path)
    t:cmd("set fileencodings=ucs-bom,utf-8,shift_jis,latin1")
    open(t, path)
    btv.test.expect(btv.bo.fileencoding).to_be("shift_jis")
    t:cmd("w")
    btv.test.expect(bytes(path)).to_be(before)
    t:cmd("set fileencodings=ucs-bom,utf-8,latin1")
  end)

  -- "`:e ++enc=` reloads a file decoding it with an explicit encoding, bypassing
  --  detection entirely … With no filename it re-edits the current file."
  btv.test.it(":e ++enc= fixes a file that opened garbled", function(t)
    open(t, DIR .. "/shift_jis.txt")
    btv.test.expect(table.concat(t:lines(), "\n")).never.to_contain("日本語")
    t:cmd("e ++enc=shift_jis")
    t:wait_for(function()
      return btv.bo.fileencoding == "shift_jis"
    end, { message = "++enc= never reloaded the file" })
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("日本語")
  end)

  btv.test.it(":e ++enc= takes a filename too, and vim's codepage aliases", function(t)
    t:cmd("enew")
    t:cmd("e ++enc=cp932 " .. DIR .. "/shift_jis.txt")
    t:wait_for(function()
      return btv.bo.fileencoding == "shift_jis"
    end, { message = "cp932 did not resolve to shift_jis" })
    btv.test.expect(table.concat(t:lines(), "\n")).to_contain("こんにちは")
  end)

  -- "The forced encoding is recorded as `'fileencoding'`, so a following `:w`
  --  writes the file back in that same encoding."
  btv.test.it("a forced read is written back in the same encoding", function(t)
    local path = scratch("shift_jis.txt")
    local before = bytes(path)
    t:cmd("e ++enc=shift_jis " .. path)
    t:wait_for(function()
      return btv.bo.fileencoding == "shift_jis"
    end, { message = "++enc= never took" })
    t:cmd("w")
    btv.test.expect(bytes(path)).to_be(before)
  end)

  btv.test.it("a bogus ++enc= fails loud", function(t)
    open(t, DIR .. "/latin1.txt")
    t:cmd("e ++enc=not-an-encoding")
    btv.test.expect(t:message()).to_contain("E474")
  end)
end)
