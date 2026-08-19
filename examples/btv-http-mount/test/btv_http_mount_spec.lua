-- The example's own test suite, in pure Lua against a real editor.
--
--   bemtvi --test-plugin examples/btv-http-mount
--
-- Fully hermetic: the mount is served by the editor under test, so the spec just
-- fetches its own origin. It drives the three keys the notes list and checks each
-- route the handler documents — including that `req.path` really is
-- MOUNT-RELATIVE, which is the property that lets the same handler run under any
-- prefix.

local DIR = debug.getinfo(1, "S").source:match("^@(.*)/test/[^/]+$")

-- The mount resolves a tick or two after sourcing and announces its URL through
-- `btv.notify`; record the notifications rather than racing the message line.
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

--- The mount's URL, once the mount promise has settled.
local url
local function mount_url(t)
  if url then
    return url
  end
  local msg = t:wait_for(function()
    return notified_with("markdown preview mounted at")
  end, { tries = 200, interval = 20, message = "the preview never mounted" })
  url = msg:match("mounted at (%S+)")
  return url
end

--- Open the sample, re-reading it so each test starts from the same text.
local function open(t)
  t:cmd("e " .. DIR .. "/sample.md")
  t:cmd("e!")
  t:feed("gg")
end

btv.test.describe("examples/btv-http-mount", function()
  btv.test.it("the mount comes up on the editor's own origin", function(t)
    open(t)
    local u = mount_url(t)
    btv.test.expect(u).to_contain("/plugin/preview/")
    -- A plugin never binds a port: it publishes a subroute on the ONE origin.
    btv.test.expect(u).to_contain(btv.http.origin())
  end)

  btv.test.it("the mount root serves the page shell", function(t)
    open(t)
    local res = btv.await(btv.http.fetch(mount_url(t)))
    btv.test.expect(res.status).to_be(200)
    btv.test.expect(res.headers["content-type"]).to_contain("text/html")
    btv.test.expect(res:text()).to_contain("live markdown preview")
    -- Self-contained, so the demo works offline: no CDN, no external asset.
    btv.test.expect(res:text()).never.to_contain("https://cdn")
  end)

  -- The whole point of the demo: the page follows the buffer.
  btv.test.it("/source serves the CURRENT buffer text", function(t)
    open(t)
    local res = btv.await(btv.http.fetch(mount_url(t) .. "source"))
    btv.test.expect(res.status).to_be(200)
    btv.test.expect(res.headers["content-type"]).to_contain("text/plain")
    -- The page polls, so a stale copy must never be cached.
    btv.test.expect(res.headers["cache-control"]).to_contain("no-store")
    btv.test.expect(res:text()).to_be(table.concat(t:lines(), "\n"))
  end)

  btv.test.it("/source follows an edit — the page cannot go stale", function(t)
    open(t)
    local before = btv.await(btv.http.fetch(mount_url(t) .. "source")):text()
    t:feed("ggOa line the spec typed<Esc>")
    local after = btv.await(btv.http.fetch(mount_url(t) .. "source")):text()
    btv.test.expect(after).never.to_be(before)
    btv.test.expect(after).to_contain("a line the spec typed")
  end)

  btv.test.it("/info is JSON, and req.query reaches the handler", function(t)
    open(t)
    local res = btv.await(btv.http.fetch(mount_url(t) .. "info"))
    btv.test.expect(res.headers["content-type"]).to_contain("application/json")
    local info = res:json()
    btv.test.expect(info.name).to_contain("sample.md")
    btv.test.expect(info.lines).to_be(#t:lines())
    btv.test.expect(info.mount).to_be("preview")
    btv.test.expect(info.method).to_be("GET")
    -- `?pretty=1` is read off `req.query` and changes the encoding.
    local plain = btv.await(btv.http.fetch(mount_url(t) .. "info")):text()
    local pretty = btv.await(btv.http.fetch(mount_url(t) .. "info", { query = { pretty = "1" } })):text()
    btv.test.expect(plain:find("\n", 1, true)).to_be_nil()
    btv.test.expect(pretty:find("\n", 1, true)).never.to_be_nil()
  end)

  -- The property that makes the handler portable: it never sees its own prefix.
  btv.test.it("req.path is mount-relative", function(t)
    open(t)
    local res = btv.await(btv.http.fetch(mount_url(t) .. "nowhere"))
    btv.test.expect(res.status).to_be(404)
    -- The plugin's own 404, naming the path IT saw — not the full URL.
    btv.test.expect(res:text()).to_be("no such page: /nowhere\n")
  end)

  btv.test.it("an unmounted name 404s from the editor, not the plugin", function(t)
    open(t)
    local res = btv.await(btv.http.fetch(btv.http.origin() .. "/plugin/not-mounted/x"))
    btv.test.expect(res.status).to_be(404)
    btv.test.expect(res:text()).never.to_contain("no such page:")
  end)

  -- \u prints the URL and the shared origin.
  btv.test.it("\\u reports the URL and the editor's origin", function(t)
    open(t)
    local u = mount_url(t)
    t:feed("<Bslash>u")
    local msg = t:wait_for(function()
      return notified_with("(origin:")
    end, { message = "\\u said nothing" })
    btv.test.expect(msg).to_contain(u)
    btv.test.expect(msg).to_contain(btv.http.origin())
  end)

  -- \c closes the mount: the URL starts 404ing, but the listener stays up.
  btv.test.it("\\c closes the mount, and the origin outlives it", function(t)
    open(t)
    local u = mount_url(t)
    local origin = btv.http.origin()
    t:feed("<Bslash>c")
    t:wait_for(function()
      return notified_with("preview closed")
    end, { message = "\\c said nothing" })
    local res = btv.await(btv.http.fetch(u))
    btv.test.expect(res.status).to_be(404)
    -- The listener is still there — the origin is unchanged and still answering.
    btv.test.expect(btv.http.origin()).to_be(origin)
    btv.test.expect(btv.await(btv.http.fetch(origin .. "/plugin/preview/source")).status).to_be(404)
  end)
end)
