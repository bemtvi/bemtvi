-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/format-on-save
--
-- The point of the demo is WHEN the handlers run relative to the bytes, so every
-- case writes into a temp copy and reads the file back off disk — asserting on
-- the buffer alone would not distinguish "formatted then saved" from "saved then
-- formatted".

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- §4 reports through `vim.notify`, after the write; record it at the source.
local notified = {}
do
  local real = vim.notify
  vim.notify = function(msg, ...)
    notified[#notified + 1] = tostring(msg)
    return real(msg, ...)
  end
end

dofile(DIR .. "/init.lua")

local function last_notify()
  return notified[#notified] or ""
end

--- `:w`, then wait for the write to actually land.
---
--- The whole point of §3 is that the write AWAITS an async handler — so the bytes
--- are not down when `:w` returns a tick later. `BufWritePost` fires once they
--- are, which is exactly the signal to wait on.
local function save(t)
  local before = #notified
  t:cmd("w")
  t:wait_for(function()
    return #notified > before
  end, { tries = 200, interval = 10, message = "the write never completed" })
end

--- The bytes on disk.
local function on_disk(path)
  return btv.await(btv.fs.read(path))
end

--- A temp `.txt` (the handlers' pattern) holding `text`.
local function scratch(t, text)
  local path = btv.test.tempdir() .. "/doc.txt"
  btv.await(btv.fs.write(path, text))
  t:cmd("e " .. path)
  t:cmd("e!")
  t:feed("gg")
  return path
end

btv.test.describe("examples/format-on-save", function()
  -- §1. "the trailing spaces are gone in the buffer AND on disk"
  btv.test.it("§1 — trailing whitespace is trimmed before the bytes", function(t)
    local path = scratch(t, "keep me   \nand me\t\n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("keep me\nand me\n")
    btv.test.expect(t:lines()).to_equal({ "keep me", "and me" })
  end)

  -- "each handler first checks whether there's anything to change … a no-op save
  --  stays quiet"
  btv.test.it("§1 — a save with nothing to trim is a no-op", function(t)
    local path = scratch(t, "already clean\n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("already clean\n")
    btv.test.expect(t:message()).never.to_contain("E486")
  end)

  -- §2. "handlers compose" — both mutations reach the same write.
  btv.test.it("§2 — a second handler's edit lands in the same save", function(t)
    local path = scratch(t, "todo: wire the thing   \n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("TODO: wire the thing\n")
  end)

  -- §3. "the write awaits the promise … it waits, formats, then writes the
  --      formatted bytes"
  btv.test.it("§3 — an async handler's edit is in the SAVED bytes", function(t)
    local path = scratch(t, "FIXME later\n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("TODO later\n")
    btv.test.expect(t:lines()).to_equal({ "TODO later" })
  end)

  btv.test.it("§3 — the async formatter rewrites every occurrence", function(t)
    local path = scratch(t, "FIXME one FIXME two\n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("TODO one TODO two\n")
  end)

  btv.test.it("§1+§2+§3 — all three compose in one save", function(t)
    local path = scratch(t, "todo: FIXME this   \n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("TODO: TODO this\n")
  end)

  -- §4. "BufWritePost … a `saved <name> (<n> lines)` message"
  btv.test.it("§4 — BufWritePost reports after the bytes are down", function(t)
    local path = scratch(t, "one\ntwo\nthree\n")
    save(t)
    btv.test.expect(last_notify()).to_contain("saved ")
    btv.test.expect(last_notify()).to_contain("doc.txt")
    btv.test.expect(last_notify()).to_contain("(3 lines)")
  end)

  -- The handlers are scoped to `*.txt`, so another filetype is untouched.
  btv.test.it("the handlers only claim the pattern they registered for", function(t)
    local path = btv.test.tempdir() .. "/code.lua"
    btv.await(btv.fs.write(path, "todo: not mine   \n"))
    t:cmd("e " .. path)
    t:cmd("e!")
    -- No `*.txt` handler claims this file, so nothing awaits and nothing reports.
    t:cmd("w")
    t:sleep(60)
    btv.test.expect(on_disk(path)).to_be("todo: not mine   \n")
  end)

  -- The regex note the config makes a point of: PCRE, so `\s+$` — a vim-escaped
  -- `\s\+$` would match a literal `+` and silently trim nothing.
  btv.test.it("the trim pattern is PCRE, not vim-escaped", function(t)
    local path = scratch(t, "spaces here    \n")
    save(t)
    btv.test.expect(on_disk(path)).to_be("spaces here\n")
    -- The vim spelling really would have been wrong: it matches a literal `+`.
    t:cmd([[%s/\s\+$//]])
    btv.test.expect(t:message()).to_contain("E486")
  end)
end)
