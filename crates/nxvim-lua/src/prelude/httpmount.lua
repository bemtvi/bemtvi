-- nx.http.mount — the plugin HTTP *server* surface, the inbound twin of `nx.http.fetch`.
--
-- Part of the pure-Lua `nx.*` prelude (see runtime.rs); the Lua half over the
-- `nx._http_mount` / `nx._http_respond` / `nx._http_unmount` Rust bridges in install.rs.
-- Loaded AFTER http.lua, whose `nx.http` table it extends.
--
-- A plugin does NOT bind a port. It mounts a subroute on the editor's ONE origin and gets
-- a URL back — enough to serve a live markdown renderer, a preview pane, or any
-- browser-facing UI a plugin wants to own.
--
-- ```lua
-- nx.http.mount({
--   name = "example",
--   on_request = function(req, respond)
--     respond({ status = 200, headers = { ["content-type"] = "text/html" }, body = "<h1>hi</h1>" })
--   end,
-- }):next(function(mount)
--   nx.print(mount:url())        -- http://127.0.0.1:53124/plugin/example/
-- end)
-- ```
--
-- Why mounts and not ports: a browser tab cannot bind a TCP port, so a per-plugin-port
-- API would be native-only by construction and plugin code would not port to the web
-- build. A mounted subroute is something a Service Worker satisfies exactly as well as a
-- TCP listener does — same Lua, three worlds, the bet `nx.http.fetch` already made. It
-- also removes port collisions between two nxvim instances and plugins hard-coding 8080.
--
-- Nothing starts until a plugin asks: the listener is bound lazily on the FIRST mount and
-- never before, so a config with no HTTP plugin opens no port at all.
--
-- WHERE it listens is the user's call, not the plugin's — the `'httphost'` / `'httpport'`
-- options (`:set httpport=8080`), read when the listener binds. It is their machine,
-- their firewall, their bookmark; a plugin must not decide it by loading. `'httphost'`
-- defaults to `127.0.0.1` (loopback only).
--
-- SECURITY: mounts share ONE origin, so the same-origin policy does NOT isolate mount A
-- from mount B — script served by one can fetch the other and read the reply. Mounts are
-- a trust boundary between the EDITOR and the NETWORK, not between plugins. A mount that
-- renders untrusted content (markdown from a repo under review) should set a restrictive
-- `content-security-policy` on its own responses.
--
-- Two shapes in one API, deliberately (`nx` is promise-only for ONE-SHOT async; a
-- persistent stream of events stays handler-based — same as `nx.process.open` /
-- `nx.socket.connect` / autocmds):
--
--   * the MOUNT is one-shot, so `nx.http.mount` returns a PROMISE of the handle. It is
--     also what makes an ephemeral `'httpport' = 0` usable: the concrete port only exists
--     after the bind — cross-tick state `nx.schedule` cannot poll for (see runtime.lua) —
--     so the promise hands it over already settled and no plugin ever polls.
--   * the REQUESTS are a persistent stream, so they are a handler (`opts.on_request`).

nx.http = nx.http or {}

-- The mount id (a callback id) -> `{ on_request, name }` for every live mount. Persistent:
-- a mount serves until `:close()`, unlike the one-shot `nx._cb_fns` a fetch settles through.
nx._http_mounts = nx._http_mounts or {}

-- Request ids the editor is still waiting on, `[req_id] = true`. A slot is cleared by the
-- first `respond`, or by `nx._http_timeout` when the client already gave up. What makes a
-- double-`respond` (or a late one) notify LOUD instead of silently vanishing into an actor
-- that no longer knows the id. Ids are globally unique across mounts (one counter in the
-- actor), so one flat table covers every mount.
nx._http_open = nx._http_open or {}

-- ----- Mount handle ----------------------------------------------------------

local Mount = {}
Mount.__index = Mount

-- `mount:url()` — the mount's base URL, trailing-slashed
-- (`http://127.0.0.1:53124/plugin/example/`). Hand this to a browser, an `<iframe>`, or
-- `nx.ui.open`. Absorbs the difference between worlds: natively it is the editor's bound
-- port, on the web build the page's own origin.
function Mount:url()
  return self:origin() .. self:path() .. "/"
end

-- `mount:path()` — the mount's path on the origin (`/plugin/example`), no trailing slash.
function Mount:path()
  return "/plugin/" .. self._name
end

-- `mount:origin()` — the origin serving this mount (`http://127.0.0.1:53124`).
--
-- Read through the central `nx._http_origin` rather than captured per-handle, so a
-- `:set httpport=9000` rebind moves EVERY live mount's URL at once. A handle that cached
-- its origin at mount time would keep handing out a dead one.
function Mount:origin()
  return nx._http_origin
end

-- `mount:close()` — retire the route. Idempotent. In-flight requests are dropped (their
-- clients get a 503). The listener itself stays bound for the session: an idle listener
-- costs nothing, and a stable origin survives a plugin reload.
function Mount:close()
  if not self._alive then
    return
  end
  self._alive = false
  nx._http_mounts[self._id] = nil
  nx._http_unmount(self._id)
end

-- `mount:is_open()` — false once `:close()` ran.
function Mount:is_open()
  return self._alive == true
end

-- ----- Native callbacks ------------------------------------------------------

-- Native callback: an inbound request routed to mount `id`. Builds the one-shot `respond`
-- closure and hands both to the plugin's handler.
--
-- A handler that THROWS answers 500 and notifies — never swallowed into a bare status the
-- plugin author cannot see. A handler that never responds is caught by the actor's
-- timeout (`nx._http_timeout`), not here.
function nx._http_request(id, req_id, req)
  local mount = nx._http_mounts[id]
  if not mount then
    -- The mount closed between the actor routing this and the tick running it. Drop the
    -- slot so the actor's 503 is the whole story.
    nx._http_open[req_id] = nil
    return
  end
  nx._http_open[req_id] = true

  local ok, err = pcall(mount.on_request, req, function(res)
    nx._http_reply(req_id, res)
  end)
  if not ok then
    -- The handler threw. Answer 500 if it had not already responded, and surface the
    -- error: a plugin bug must not read as an ordinary error status to the browser.
    if nx._http_open[req_id] then
      nx._http_open[req_id] = nil
      nx._http_respond(req_id, 500, {}, "")
    end
    nx.notify(
      ("nx.http.mount(%q): on_request errored: %s"):format(mount.name, tostring(err)),
      nx.log.levels.ERROR
    )
  end
end

-- Native callback: the actor gave up on `req_id` (the client already got its 504). Closes
-- the slot and notifies — a handler that never calls `respond` is a plugin bug, and a
-- silent 504 would leave nothing to debug from.
function nx._http_timeout(req_id)
  if not nx._http_open[req_id] then
    return
  end
  nx._http_open[req_id] = nil
  nx.notify(
    "nx.http.mount: on_request did not respond in time; the client got a 504. "
      .. "Every request must call respond() exactly once.",
    nx.log.levels.ERROR
  )
end

-- The `respond` a handler is called with. Validates, closes the slot, queues the reply.
function nx._http_reply(req_id, res)
  if not nx._http_open[req_id] then
    -- Already answered, or timed out. Loud: silently dropping the second reply is how a
    -- double-respond bug survives to production.
    nx.notify(
      "nx.http.mount: respond() called for a request that was already answered "
        .. "(or timed out). Call it exactly once per request.",
      nx.log.levels.ERROR
    )
    return
  end
  res = res or {}
  if type(res) ~= "table" then
    error("nx.http.mount: respond expects a table { status, headers, body }", 2)
  end

  local status = res.status or 200
  if type(status) ~= "number" or status < 100 or status > 599 then
    error(("nx.http.mount: respond status must be 100-599, got %s"):format(tostring(status)), 2)
  end

  local body = res.body or ""
  if type(body) ~= "string" then
    error(("nx.http.mount: respond body must be a string, got %s"):format(type(body)), 2)
  end

  -- Headers as ordered `{ name, value }` pairs for the bridge (a map at the surface, so a
  -- plugin writes `{ ["content-type"] = "text/html" }`).
  local headers = {}
  for name, value in pairs(res.headers or {}) do
    if type(name) ~= "string" then
      error("nx.http.mount: respond header names must be strings", 2)
    end
    headers[#headers + 1] = { name, tostring(value) }
  end

  nx._http_open[req_id] = nil
  nx._http_respond(req_id, status, headers, body)
end

-- Native callback: the listener rebound onto a new address after an `'httphost'` /
-- `'httpport'` write. Every live mount stays mounted — only the origin moved — so this
-- updates the one place `Mount:origin()` reads.
function nx._http_rebound(origin)
  nx._http_origin = origin
end

-- ----- Public API ------------------------------------------------------------

-- `nx.http.mount(opts)` -> a promise of a `Mount`.
--
-- Publishes `opts.on_request` at `/plugin/<opts.name>/*` on the editor's one origin,
-- binding the listener if this is the first mount. The promise RESOLVES with the handle
-- once the route is live, and REJECTS (a `{ message }` table) when the listener cannot
-- bind (`'httpport'` is taken) or the name is already mounted — never a silent fallback to
-- some other port or a suffixed name.
--
-- ```lua
-- opts = {
--   name = "example",              -- required; [%w_-]+. Mounts at /plugin/example
--   on_request = function(req, respond) end,  -- required
--   timeout = 30000,               -- ms before an unanswered request gets a 504
-- }
-- ```
--
-- The handler is called `on_request(req, respond)`:
--
-- ```lua
-- req = {
--   method = "GET",                -- upper-cased
--   path = "/style.css",           -- MOUNT-RELATIVE; a bare /plugin/example is "/"
--   raw_path = "/plugin/example/style.css",
--   query = { v = "2" },           -- decoded
--   headers = { ["accept"] = "*/*" },  -- lowercased names
--   body = "",                     -- raw bytes, binary-safe
--   name = "example",              -- the mount it routed to
-- }
-- ```
--
-- `req.path` is mount-relative so the same handler works under any prefix or origin — the
-- native port and the browser Service Worker's origin included.
--
-- `respond(res)` answers it, exactly once:
--
-- ```lua
-- respond({ status = 200, headers = { ["content-type"] = "text/html" }, body = html })
-- ```
--
-- Every `res` field is optional (`{}` is a bare 200). `respond` may be called LATER — that
-- is why it is a callback rather than a return value: a handler is free to await a file
-- read or an upstream fetch first.
--
-- ```lua
-- on_request = function(req, respond)
--   nx.fs.read("/tmp/page.html"):next(function(html)
--     respond({ body = html })
--   end)
-- end
-- ```
function nx.http.mount(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("nx.http.mount: expected a table { name, on_request }", 2)
  end
  local name = opts.name
  if type(name) ~= "string" or not name:match("^[%w_-]+$") then
    error(
      ("nx.http.mount: `name` must be a non-empty [%%w_-]+ string, got %s"):format(tostring(name)),
      2
    )
  end
  if type(opts.on_request) ~= "function" then
    error("nx.http.mount: `on_request` must be a function(req, respond)", 2)
  end
  local timeout = opts.timeout or 30000
  if type(timeout) ~= "number" or timeout <= 0 then
    error(
      ("nx.http.mount: `timeout` must be a positive number of ms, got %s"):format(
        tostring(opts.timeout)
      ),
      2
    )
  end

  return nx.promise.new(function(resolve, reject)
    local id = nx._next_cb_id()
    nx._cb_fns[id] = function(err, origin)
      if err ~= nil then
        -- The mount never published: drop the registry entry so a stale handler cannot
        -- be reached by a route that does not exist.
        nx._http_mounts[id] = nil
        reject(err)
        return
      end
      -- One listener, so one origin for every mount — recorded centrally and read back
      -- through `Mount:origin()`, which is what lets a later rebind move them all.
      nx._http_origin = origin
      resolve(setmetatable({ _id = id, _name = name, _alive = true }, Mount))
    end
    -- Register BEFORE the bridge: the actor can route a request to this mount as soon as
    -- the listener is up, and the reply arrives on a later tick either way.
    nx._http_mounts[id] = { on_request = opts.on_request, name = name }
    nx._http_mount(id, name, timeout)
  end)
end

-- `nx.http.origin()` -> the origin serving plugin mounts (`http://127.0.0.1:53124`), or
-- `nil` when nothing has mounted yet. Nil rather than a guess: until the first
-- `nx.http.mount` there is no listener, so there is no origin to report and inventing one
-- from `'httpport'` would describe a port nothing is listening on.
function nx.http.origin()
  return nx._http_origin
end
