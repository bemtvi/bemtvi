-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/bench
--
-- A benchmark suite can't assert on timings — they are the thing being measured.
-- What it CAN assert is everything the README promises around them: that every
-- `:benchN` exists and reports a line in the documented shape, and above all that
-- the `chk=` checksums are deterministic, since the whole A/B rests on "same
-- checksum ⇒ both VMs did identical work".
--
-- The suite reports through `print`, so the spec records that rather than reading
-- the one message line each result immediately overwrites.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

--- Record everything printed while `body` runs, in order.
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

-- The load-time banner is printed too, so record the sourcing as well.
local loading = transcript(function()
  dofile(DIR .. "/init.lua")
end)

--- The `chk=` value from a result line, or nil.
local function checksum(line)
  return line:match("chk=(%S+)")
end

btv.test.describe("examples/bench", function()
  btv.test.it("the suite announces itself and its backend", function(t)
    local banner = loading[#loading] or ""
    btv.test.expect(banner).to_contain("bench suite loaded")
    -- The backend is what the whole comparison is *about*, so it is named.
    btv.test.expect(banner).to_contain(_VERSION)
    btv.test.expect(_VERSION).to_be("Lua 5.4")
  end)

  btv.test.it("every :benchN in the README table is registered", function(t)
    -- `btv.user_command.get()` is keyed BY name (neovim's `nvim_get_commands`
    -- shape), not a list.
    local known = btv.user_command.get()
    for i = 1, 10 do
      btv.test.expect(known["bench" .. i]).to_be_truthy()
    end
    btv.test.expect(known["benchall"]).to_be_truthy()
  end)

  btv.test.it(":bench1 reports one line in the documented shape", function(t)
    local out = transcript(function()
      t:cmd("bench1")
    end)
    btv.test.expect(#out).to_be(1)
    local line = out[1]
    btv.test.expect(line).to_contain("pattern tokenize")
    btv.test.expect(line).to_match("%d+ it")
    btv.test.expect(line).to_match("ms")
    btv.test.expect(line).to_match("us/it")
    btv.test.expect(checksum(line)).never.to_be_nil()
  end)

  -- The claim the A/B rests on: identical work every run, so a differing checksum
  -- across backends means a real behavioral divergence rather than noise.
  btv.test.it("a bench's checksum is deterministic across runs", function(t)
    for _, cmd in ipairs({ "bench1", "bench5", "bench9" }) do
      local first = checksum(transcript(function()
        t:cmd(cmd)
      end)[1])
      local second = checksum(transcript(function()
        t:cmd(cmd)
      end)[1])
      btv.test.expect(first).never.to_be_nil()
      btv.test.expect(second).to_be(first)
    end
  end)

  -- `:bench5` reseeds explicitly so its data is rebuilt identically each call —
  -- without that, its checksum would drift with the shared LCG's state and the
  -- comparison would be worthless. Interleaving another bench proves the reseed.
  btv.test.it("bench5's checksum survives an interleaved bench", function(t)
    local first = checksum(transcript(function()
      t:cmd("bench5")
    end)[1])
    t:cmd("bench9")
    local second = checksum(transcript(function()
      t:cmd("bench5")
    end)[1])
    btv.test.expect(second).to_be(first)
  end)

  btv.test.it(":benchall runs the whole table, with a header and a total", function(t)
    local out = transcript(function()
      t:cmd("benchall")
    end)
    -- header + 10 result lines + total
    btv.test.expect(#out).to_be(12)
    btv.test.expect(out[1]).to_contain("bemtvi Lua microbench")
    btv.test.expect(out[1]).to_contain(_VERSION)
    btv.test.expect(out[1]).to_contain("SCALE=1")
    for i = 2, 11 do
      btv.test.expect(checksum(out[i])).never.to_be_nil()
    end
    btv.test.expect(out[12]).to_contain("total CPU:")
    btv.test.expect(out[12]).to_contain(":messages")
  end)

  btv.test.it("the sample buffer explains how to drive it", function(t)
    t:cmd("e " .. DIR .. "/sample.txt")
    local text = table.concat(t:lines(), "\n")
    btv.test.expect(text).to_contain(":benchall")
    btv.test.expect(text).to_contain(":messages")
  end)
end)
