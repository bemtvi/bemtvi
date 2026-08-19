-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/btv-http
--
-- The demos themselves reach the public internet, which a test may not depend on.
-- So the spec splits in two:
--
--   * the SEMANTICS the notes spell out — a 404 resolves rather than rejects, only
--     a transport failure rejects, `query` percent-encodes, a table `body` is sent
--     as JSON, `redirect = "manual"` does not follow — are proven against an
--     endpoint the editor itself serves (`btv.http.mount`), so they are exact and
--     hermetic;
--   * the six keymaps are driven for real, and the ones that need the network are
--     driven only when it is reachable (and say so when it is not).

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The example reports through `btv.notify`, and its reactions land a tick or more
-- later — so record them at the source rather than racing the message line.
local notified = {}
do
  local real = btv.notify
  btv.notify = function(msg, level)
    notified[#notified + 1] = tostring(msg)
    return real(msg, level)
  end
end

dofile(DIR .. "/init.lua")

--- Wait until some notification contains `needle`, and return it.
local function wait_notify(t, needle, opts)
  local found
  t:wait_for(function()
    for i = #notified, 1, -1 do
      if notified[i]:find(needle, 1, true) then
        found = notified[i]
        return true
      end
    end
    return false
  end, opts or { tries = 300, interval = 20, message = "no notification matched " .. needle })
  return found
end

--- A local endpoint, served by the editor itself, so the semantics below need no
--- network at all. Mounted once, lazily, and reused.
local origin
local function endpoint()
  if origin then
    return origin
  end
  local mount = btv.await(btv.http.mount({
    name = "httpspec",
    on_request = function(req, respond)
      if req.path == "/ok" then
        respond({
          headers = { ["content-type"] = "text/plain; charset=utf-8" },
          body = "hello from the editor",
        })
      elseif req.path == "/json" then
        respond({
          headers = { ["content-type"] = "application/json" },
          body = btv.json.encode({ items = { { full_name = "a/b", stargazers_count = 7 } } }),
        })
      elseif req.path == "/echo" then
        respond({
          headers = { ["content-type"] = "application/json" },
          body = btv.json.encode({
            method = req.method,
            query = req.query,
            body = req.body,
            content_type = (req.headers or {})["content-type"],
            user_agent = (req.headers or {})["user-agent"],
          }),
        })
      elseif req.path == "/moved" then
        -- Absolute: `req.path` is mount-RELATIVE, but a `Location` is resolved
        -- against the origin, so a bare "/ok" would leave this mount's prefix.
        respond({ status = 302, headers = { location = origin .. "ok" }, body = "" })
      else
        respond({ status = 404, headers = { ["content-type"] = "text/plain" }, body = "nope" })
      end
    end,
  }))
  origin = mount:url()
  return origin
end

btv.test.describe("examples/btv-http", function()
  btv.test.it("the demo announces its six keys at load", function(t)
    local banner
    for _, m in ipairs(notified) do
      if m:find("btv.http demo loaded", 1, true) then
        banner = m
      end
    end
    btv.test.expect(banner).never.to_be_nil()
    for _, key in ipairs({ "\\h", "\\j", "\\p", "\\x", "\\r", "\\l" }) do
      btv.test.expect(banner).to_contain(key)
    end
  end)

  btv.test.it("each documented key is really mapped", function(t)
    local mapped = {}
    for _, m in ipairs(t:keymaps("n")) do
      mapped[m.lhs] = true
    end
    for _, key in ipairs({ "\\h", "\\j", "\\p", "\\x", "\\r", "\\l" }) do
      btv.test.expect(mapped[key]).to_be_truthy()
    end
  end)

  -- The headline of the doc comment: a promise-of-Response, resolving for ANY
  -- status.
  btv.test.it("fetch resolves with a Response carrying status, headers and text", function(t)
    local res = btv.await(btv.http.fetch(endpoint() .. "ok"))
    btv.test.expect(res.status).to_be(200)
    btv.test.expect(res.ok).to_be(true)
    btv.test.expect(res:text()).to_be("hello from the editor")
    btv.test.expect(res.body).to_be("hello from the editor")
    -- Header names are lowercased on the way in.
    btv.test.expect(res.headers["content-type"]).to_contain("text/plain")
  end)

  btv.test.it("a 404 RESOLVES with ok == false — it does not reject", function(t)
    local res = btv.await(btv.http.fetch(endpoint() .. "no-such-page"))
    btv.test.expect(res.status).to_be(404)
    btv.test.expect(res.ok).to_be(false)
    btv.test.expect(res:text()).to_contain("nope")
  end)

  btv.test.it(":json() decodes the body", function(t)
    local res = btv.await(btv.http.fetch(endpoint() .. "json"))
    local top = res:json().items[1]
    btv.test.expect(top.full_name).to_be("a/b")
    btv.test.expect(top.stargazers_count).to_be(7)
  end)

  -- Demo 2's mechanic: `opts.query` builds and percent-encodes the query string,
  -- so a URL is never concatenated by hand.
  btv.test.it("opts.query builds and percent-encodes the query string", function(t)
    local res = btv.await(btv.http.fetch(endpoint() .. "echo", {
      query = { q = "language:rust stars:>10000", per_page = 1 },
      headers = { ["User-Agent"] = "bemtvi-btv-http-example" },
    }))
    local echoed = res:json()
    btv.test.expect(echoed.query.q).to_be("language:rust stars:>10000")
    btv.test.expect(tostring(echoed.query.per_page)).to_be("1")
    btv.test.expect(echoed.user_agent).to_be("bemtvi-btv-http-example")
  end)

  -- Demo 3's mechanic: a non-string body is JSON-encoded, with the content type
  -- filled in for you.
  btv.test.it("a table body is sent as JSON, with the header added", function(t)
    local res = btv.await(btv.http.fetch(endpoint() .. "echo", {
      method = "POST",
      body = { editor = "bemtvi", feature = "btv.http" },
    }))
    local echoed = res:json()
    btv.test.expect(echoed.method).to_be("POST")
    btv.test.expect(echoed.content_type).to_contain("application/json")
    btv.test.expect(btv.json.decode(echoed.body).editor).to_be("bemtvi")
  end)

  -- Demo 5's mechanic.
  btv.test.it("redirect = 'manual' returns the 3xx instead of following it", function(t)
    local res = btv.await(btv.http.fetch(endpoint() .. "moved", { redirect = "manual" }))
    btv.test.expect(res.status).to_be(302)
    btv.test.expect(res.headers["location"]).to_be(endpoint() .. "ok")
    -- …and the default follows it.
    local followed = btv.await(btv.http.fetch(endpoint() .. "moved"))
    btv.test.expect(followed.status).to_be(200)
    btv.test.expect(followed:text()).to_be("hello from the editor")
  end)

  -- Demo 4, driven exactly as the notes say — and hermetic, because nothing is
  -- listening on port 1 of the loopback interface.
  btv.test.it("demo 4 — \\x rejects on a transport failure", function(t)
    t:feed("<Bslash>x")
    local msg = wait_notify(t, "rejected (as expected)")
    btv.test.expect(msg).to_contain("rejected (as expected)")
    btv.test.expect(msg).never.to_contain("unexpectedly resolved")
  end)

  btv.test.it("btv.http.fetch_local is the same API, forced onto this machine", function(t)
    local res = btv.await(btv.http.fetch_local(endpoint() .. "ok"))
    btv.test.expect(res.status).to_be(200)
    btv.test.expect(res:text()).to_be("hello from the editor")
  end)

  -- The four demos that genuinely need the public internet. Driven for real when
  -- it is reachable; reported rather than silently passed when it is not.
  btv.test.it("demos 1/2/3/5/6 — the network keys, when the network is up", function(t)
    local reachable = pcall(function()
      local res = btv.await(btv.http.fetch("https://example.com", { timeout = 4000 }))
      return res.status
    end)
    if not reachable then
      btv.notify("examples/btv-http: skipping the network demos — no internet", 2)
      return
    end
    t:feed("<Bslash>h")
    btv.test.expect(wait_notify(t, "example.com →")).to_match("%d+ bytes")
    t:feed("<Bslash>l")
    btv.test.expect(wait_notify(t, "local fetch →")).to_match("%d+ bytes")
    t:feed("<Bslash>j")
    btv.test.expect(wait_notify(t, "top rust repo:")).to_contain("★")
    t:feed("<Bslash>p")
    btv.test.expect(wait_notify(t, "server saw json.editor")).to_contain("bemtvi")
    t:feed("<Bslash>r")
    btv.test.expect(wait_notify(t, "redirect not followed:")).to_contain("30")
  end)
end)
