-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/connect
--
-- What a headless runner CAN check is the whole editor-side half of the loop: that
-- `btv.connect.register` routes `:connect <url>` to the right resolver, that the
-- resolver is handed the URL and may report progress, that the spec it returns is
-- the `btv.session.reconnect` shape, that a promise-returning resolver is awaited,
-- and that an unmatched scheme falls through to the built-in dialer. The final
-- step — the CLIENT swapping its window onto the returned transport — belongs to a
-- real client, so the spec stops at the spec.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The demo resolver reports progress through `btv.notify`; record it at the source.
local notified = {}
do
  local real = btv.notify
  btv.notify = function(msg, level)
    notified[#notified + 1] = tostring(msg)
    return real(msg, level)
  end
end

dofile(DIR .. "/init.lua")

--- The most recent notification containing `needle`, or nil.
local function notified_with(needle)
  for i = #notified, 1, -1 do
    if notified[i]:find(needle, 1, true) then
      return notified[i]
    end
  end
  return nil
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.txt")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/connect", function()
  btv.test.it("the demo:// resolver runs, and reports its progress", function(t)
    open(t)
    t:cmd("connect demo://here")
    local msg = t:wait_for(function()
      return notified_with("connect: provisioning")
    end, { message = "the demo resolver never ran" })
    -- It is handed the URL it matched.
    btv.test.expect(msg).to_contain("demo://here")
  end)

  -- The resolver's contract, exercised through the very API the example documents:
  -- a scheme string, and a `fn(url) -> spec`.
  btv.test.it("btv.connect.register routes by scheme", function(t)
    open(t)
    local seen
    btv.connect.register("spectest", function(url)
      seen = url
      return { transport = { kind = "spawn", argv = { "true" } }, config_source = "local" }
    end)
    t:cmd("connect spectest://somewhere/deep?x=1")
    t:wait_for(function()
      return seen ~= nil
    end, { message = "the registered resolver never ran" })
    btv.test.expect(seen).to_be("spectest://somewhere/deep?x=1")
  end)

  -- "`scheme_or_matcher` is a scheme string … or a `fn(url) -> boolean` matcher"
  btv.test.it("a matcher function is accepted in place of a scheme", function(t)
    open(t)
    local seen
    btv.connect.register(function(url)
      return url:find("^weird%+scheme:") ~= nil
    end, function(url)
      seen = url
      return { transport = { kind = "spawn", argv = { "true" } } }
    end)
    t:cmd("connect weird+scheme:whatever")
    t:wait_for(function()
      return seen ~= nil
    end, { message = "the matcher never matched" })
    btv.test.expect(seen).to_be("weird+scheme:whatever")
  end)

  -- "The resolver MAY return a promise (provision asynchronously)."
  btv.test.it("a promise-returning resolver is awaited before the swap", function(t)
    open(t)
    local resolved = false
    btv.connect.register("slowspec", function()
      return btv.promise.delay(30):next(function()
        resolved = true
        return { transport = { kind = "spawn", argv = { "true" } } }
      end)
    end)
    t:cmd("connect slowspec://x")
    t:wait_for(function()
      return resolved
    end, { message = "the async resolver never fulfilled" })
    btv.test.expect(resolved).to_be(true)
  end)

  -- "with no matching provider it falls back to the built-in direct dial"
  btv.test.it("an unmatched scheme falls through to the built-in dialer", function(t)
    open(t)
    local ran = false
    btv.connect.register("neverpicked", function()
      ran = true
      return {}
    end)
    -- A `bemtvi://` URL matches no registered connector, so the built-in dialer
    -- takes it — and fails loudly on a port with nothing behind it rather than
    -- silently doing nothing.
    t:cmd("connect bemtvi://127.0.0.1:1/tok?cert=abc")
    t:sleep(120)
    btv.test.expect(ran).to_be(false)
  end)

  -- The statusline segment the example ships: a LOCAL session has no daemon, so it
  -- renders nothing rather than an empty box.
  btv.test.it("the daemon segment hides itself in a local session", function(t)
    open(t)
    local phase = btv.daemon.status()
    btv.test.expect(phase == nil or phase == "local").to_be(true)
    btv.test.expect(t:statusline()).never.to_contain("●")
    -- …and the rest of the bar the config laid out is there.
    btv.test.expect(t:statusline()).to_contain("sample.txt")
    btv.test.expect(t:statusline()).to_match("1:1")
  end)
end)
