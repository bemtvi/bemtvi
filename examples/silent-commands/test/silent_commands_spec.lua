-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/silent-commands
--
-- The whole feature is about what is *not* said, so most cases assert on an empty
-- message line — and, for the `:silent` half the notes call out, on `:messages`
-- holding no entry either.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

dofile(DIR .. "/init.lua")

--- Open the sample with a cleared message line, so "nothing was said" is a fact
--- about the command under test rather than about whatever ran before it.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:cmd("echo ''")
  t:feed("gg")
end

--- The `:messages` history, as one string. It is a real buffer in a panel, so it
--- is read like any other — and closed again, since the per-test baseline's
--- `enew!` runs in whatever window is current.
local function history(t)
  t:cmd("messages")
  local text = table.concat(t:lines(), "\n")
  t:cmd("close")
  return text
end

btv.test.describe("examples/silent-commands", function()
  -- "TYPE: :echom 'hello' SEE: `hello` on the message line"
  btv.test.it("1 — a plain :echom is heard", function(t)
    open(t)
    t:cmd("echom 'hello'")
    btv.test.expect(t:message()).to_be("hello")
    btv.test.expect(history(t)).to_contain("hello")
  end)

  -- "TYPE: :silent echom 'hello' SEE: nothing — and :messages has no entry for it"
  btv.test.it("1 — :silent drops the output, history included", function(t)
    open(t)
    t:cmd("silent echom 'quiet one'")
    btv.test.expect(t:message()).to_be("")
    btv.test.expect(history(t)).never.to_contain("quiet one")
  end)

  -- "TYPE: :silent NotACommand SEE: E492 still reported — errors survive"
  btv.test.it("1 — :silent still reports an error", function(t)
    open(t)
    t:cmd("silent NotACommand")
    btv.test.expect(t:message()).to_contain("E492")
    btv.test.expect(t:message()).to_contain("NotACommand")
  end)

  -- "TYPE: :silent! NotACommand SEE: nothing at all"
  btv.test.it("1 — :silent! swallows the error too", function(t)
    open(t)
    t:cmd("silent! NotACommand")
    btv.test.expect(t:message()).to_be("")
  end)

  -- "2. TYPE: <Space>w SEE: the buffer is saved with no '…written' message"
  btv.test.it("2 — <Space>w writes without the chatter", function(t)
    open(t)
    -- A plain `:w` says so…
    t:feed("ox<Esc>")
    t:cmd("write")
    btv.test.expect(t:message()).to_contain("written")
    -- …and the mapped, silent one does not, while still clearing 'modified'.
    t:feed("ox<Esc>")
    btv.test.expect(btv.bo.modified).to_be(true)
    t:cmd("echo ''")
    t:feed("<Space>w")
    btv.test.expect(t:message()).to_be("")
    btv.test.expect(btv.bo.modified).to_be(false)
    -- Put the file back the way it was found.
    t:cmd("undo")
    t:cmd("undo")
    t:cmd("write")
  end)

  -- "3. TYPE: <Space>o SEE: nothing happens and nothing is reported"
  btv.test.it("3 — <Space>o runs an absent command quietly", function(t)
    open(t)
    t:feed("<Space>o")
    btv.test.expect(t:message()).to_be("")
  end)

  -- "4. TYPE: <Space>e SEE: E492: Not an editor command: OptionalPluginCommand"
  btv.test.it("4 — <Space>e is silent but still reports the error", function(t)
    open(t)
    t:feed("<Space>e")
    btv.test.expect(t:message()).to_contain("E492")
    btv.test.expect(t:message()).to_contain("OptionalPluginCommand")
  end)

  -- "5. TYPE: <Space>v SEE: the cursor jumps to the last line … with no output
  --  from the three `echo`s that ran alongside it"
  btv.test.it("5 — every vim.cmd form takes the same mods", function(t)
    open(t)
    t:feed("<Space>v")
    btv.test.expect(t:cursor()[1]).to_be(#btv.buf.lines(0, 0, -1))
    btv.test.expect(t:message()).to_be("")
  end)

  -- "6. TYPE: <Space>x SEE: an error naming `keepjumps`"
  btv.test.it("6 — an unsupported modifier raises by name", function(t)
    open(t)
    t:feed("<Space>x")
    btv.test.expect(t:message()).to_contain("keepjumps")
  end)

  btv.test.it("6 — the raise is the Lua call's, not a silent drop", function(t)
    open(t)
    local ok, err = pcall(btv.cmd, "normal! G", { keepjumps = true })
    btv.test.expect(ok).to_be(false)
    btv.test.expect(tostring(err)).to_contain("keepjumps")
  end)
end)
