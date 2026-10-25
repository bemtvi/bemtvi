-- btv.http.mount — the plugin HTTP *server* surface, the inbound twin of `btv.http.fetch`.
--
-- Part of the pure-Lua `btv.*` prelude (see runtime.rs); the Lua half over the
-- `btv._http_mount` / `btv._http_respond` / `btv._http_unmount` Rust bridges in install.rs.
-- Loaded AFTER http.lua, whose `btv.http` table it extends.
--
-- A plugin does NOT bind a port. It mounts a subroute on the editor's ONE origin and gets
-- a URL back — enough to serve a live markdown renderer, a preview pane, or any
-- browser-facing UI a plugin wants to own.
--
-- ```lua
-- btv.http.mount({
--   name = "example",
--   on_request = function(req, respond)
--     respond({ status = 200, headers = { ["content-type"] = "text/html" }, body = "<h1>hi</h1>" })
--   end,
-- }):next(function(mount)
--   btv.print(mount:url())        -- http://127.0.0.1:53124/<token>/plugin/example/
-- end)
-- ```
--
-- Why mounts and not ports: a browser tab cannot bind a TCP port, so a per-plugin-port
-- API would be native-only by construction and plugin code would not port to the web
-- build. A mounted subroute is something a Service Worker satisfies exactly as well as a
-- TCP listener does — same Lua, three worlds, the bet `btv.http.fetch` already made. It
-- also removes port collisions between two bemtvi instances and plugins hard-coding 8080.
--
-- Nothing starts until a plugin asks: the listener is bound lazily on the FIRST mount and
-- never before, so a config with no HTTP plugin opens no port at all.
--
-- WHERE it listens is the user's call, not the plugin's — the `'httphost'` / `'httpport'`
-- options (`:set httpport=8080`), read when the listener binds. It is their machine,
-- their firewall, their bookmark; a plugin must not decide it by loading. `'httphost'`
-- defaults to `127.0.0.1` (loopback only).
--
-- SECURITY, three layers:
--
--   * Natively every mounted path sits under a per-session secret segment
--     (`http://127.0.0.1:<port>/<token>/plugin/<name>/`), so another local process or user
--     cannot call a mount without being handed the URL. Always build URLs from
--     `mount:url()` / `mount:path()` (or relative to the page) — never hard-code
--     `/plugin/<name>`, which natively 404s. (The web build has no token: its Service Worker
--     only answers the editor's own origin, which no other process can reach.)
--   * The listener refuses foreign `Origin`s and DNS-rebound `Host`s, so other websites
--     cannot drive a mount either.
--   * Every mount response gets a `content-security-policy: sandbox …` (no
--     `allow-same-origin`), so a mount's documents run in an OPAQUE origin. That matters
--     most on the web build, where mounts share the editor's own origin: an unsandboxed
--     page could read the editor's OPFS, drive the wasm, and use its daemon link. The cost:
--     a sandboxed page cannot `fetch()` back to its own mount (on the web its requests
--     bypass the Service Worker; natively they carry `Origin: null` and are refused), has
--     no cookies or `localStorage`, and cannot touch `window.opener`/`parent`. A plugin
--     whose OWN page needs that opts out with `same_origin = true` — and then must not put
--     untrusted HTML in that document (render it in a nested `<iframe sandbox>` instead).
--
-- Two shapes in one API, deliberately (`btv` is promise-only for ONE-SHOT async; a
-- persistent stream of events stays handler-based — same as `btv.process.open` /
-- `btv.socket.connect` / autocmds):
--
--   * the MOUNT is one-shot, so `btv.http.mount` returns a PROMISE of the handle. It is
--     also what makes an ephemeral `'httpport' = 0` usable: the concrete port only exists
--     after the bind — cross-tick state `btv.schedule` cannot poll for (see runtime.lua) —
--     so the promise hands it over already settled and no plugin ever polls.
--   * the REQUESTS are a persistent stream, so they are a handler (`opts.on_request`).

btv.http = btv.http or {}

-- The mount id (a callback id) -> `{ on_request, name }` for every live mount. Persistent:
-- a mount serves until `:close()`, unlike the one-shot `btv._cb_fns` a fetch settles through.
btv._http_mounts = btv._http_mounts or {}

-- Request ids the editor is still waiting on: `[req_id] = <the deadline timer>`. A slot is
-- cleared (and its timer stopped) by the first `respond`, or by the timer itself firing.
-- What makes a double-`respond` (or a late one) notify LOUD instead of silently vanishing
-- into a transport that no longer knows the id. Ids are globally unique across mounts (one
-- counter per listener), so one flat table covers every mount.
--
-- The VALUE being the timer is what lets `opts.timeout` live here rather than in each
-- transport — see `btv._http_request`.
btv._http_open = btv._http_open or {}
-- `req_id -> mount id` for every slot in `btv._http_open`, so closing a mount can retire
-- its own in-flight requests.
btv._http_open_mount = btv._http_open_mount or {}

-- ----- Mount handle ----------------------------------------------------------

local Mount = {}
Mount.__index = Mount

-- The policy appended to every response of a mount without `same_origin = true`: script,
-- forms, popups, dialogs and downloads still work, but the document runs in an opaque
-- origin. Appended as its own header, never merged into (or replacing) the plugin's CSP —
-- both policies are enforced.
local SANDBOX_CSP =
  "sandbox allow-scripts allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals allow-downloads"

-- Record the base URL a transport reported (`http://127.0.0.1:53124/<token>` natively,
-- the page's bare origin on the web) as its origin and the path prefix mounts sit under.
local function set_base(base)
  local origin, prefix = tostring(base):match("^(%a[%w+.-]*://[^/]*)(.*)$")
  btv._http_origin = origin or base
  btv._http_prefix = (prefix or ""):gsub("/+$", "")
end

-- `mount:url()` — the mount's base URL, trailing-slashed
-- (`http://127.0.0.1:53124/<token>/plugin/example/`). Hand this to a browser, an
-- `<iframe>`, or `btv.ui.open`. Absorbs the difference between worlds: natively it is the
-- editor's bound port plus the session token, on the web build the page's own origin.
-- It is the only way to address a mount — build every URL from it.
function Mount:url()
  return self:origin() .. self:path() .. "/"
end

-- `mount:path()` — the mount's path on the origin (`/<token>/plugin/example` natively,
-- `/plugin/example` on the web build), no trailing slash.
function Mount:path()
  return (btv._http_prefix or "") .. "/plugin/" .. self._name
end

-- `mount:origin()` — the origin serving this mount (`http://127.0.0.1:53124`).
--
-- Read through the central `btv._http_origin` rather than captured per-handle, so a
-- `:set httpport=9000` rebind moves EVERY live mount's URL at once. A handle that cached
-- its origin at mount time would keep handing out a dead one.
function Mount:origin()
  return btv._http_origin
end

-- `mount:close()` — retire the route. Idempotent. In-flight requests are dropped (their
-- clients get a 503). The listener itself stays bound for the session: an idle listener
-- costs nothing, and a stable origin survives a plugin reload.
function Mount:close()
  if not self._alive then
    return
  end
  self._alive = false
  btv._http_mounts[self._id] = nil
  -- Its in-flight requests go with it (the transport answers them 503): stop their
  -- deadlines so none fires later with a 504 and a false timeout report.
  for req_id, owner in pairs(btv._http_open_mount) do
    if owner == self._id then
      local deadline = btv._http_open[req_id]
      if deadline then
        deadline:stop()
      end
      btv._http_open[req_id] = nil
      btv._http_open_mount[req_id] = nil
    end
  end
  btv._http_unmount(self._id)
end

-- `mount:is_open()` — false once `:close()` ran.
function Mount:is_open()
  return self._alive == true
end

-- ----- Native callbacks ------------------------------------------------------

-- Native callback: an inbound request routed to mount `id`. Arms the request's deadline,
-- builds the one-shot `respond` closure, and hands both to the plugin's handler.
--
-- A handler that THROWS answers 500 and notifies — never swallowed into a bare status the
-- plugin author cannot see. A handler that never RESPONDS is caught by the deadline armed
-- here.
--
-- `opts.timeout` is enforced HERE, in Lua, and not by whatever is carrying the bytes. It is
-- the plugin's contract ("your handler did not answer in time"), so it belongs where the
-- contract is — one implementation every transport inherits, rather than one per transport
-- that must each be told the mount's timeout and each drift from it. (The transports keep
-- their own far-longer backstop for the case Lua cannot run AT ALL — a wedged tick, or a
-- build whose timers cannot fire. That is resource safety, a different concern that merely
-- shares a unit.)
function btv._http_request(id, req_id, req)
  local mount = btv._http_mounts[id]
  if not mount then
    -- The mount closed between the transport routing this and the tick running it. Drop the
    -- slot so the transport's 503 is the whole story.
    btv._http_open[req_id] = nil
    return
  end

  -- The deadline. Fires only if nothing answered first — `btv._http_reply` stops it.
  btv._http_open_mount[req_id] = id
  btv._http_open[req_id] = btv.timer(function()
    if not btv._http_open[req_id] then
      return
    end
    btv._http_open[req_id] = nil
    btv._http_open_mount[req_id] = nil
    btv._http_respond(req_id, 504, { { "content-type", "text/plain; charset=utf-8" } }, "")
    btv.notify(
      ("btv.http.mount(%q): on_request did not respond within %dms; the client got a 504. "):format(
        mount.name,
        mount.timeout
      ) .. "Every request must call respond() exactly once.",
      btv.log.levels.ERROR
    )
  end, mount.timeout)

  local ok, err = pcall(mount.on_request, req, function(res)
    btv._http_reply(req_id, res)
  end)
  if not ok then
    -- The handler threw. Answer 500 if it had not already responded, and surface the
    -- error: a plugin bug must not read as an ordinary error status to the browser.
    local deadline = btv._http_open[req_id]
    if deadline then
      deadline:stop()
      btv._http_open[req_id] = nil
      btv._http_open_mount[req_id] = nil
      btv._http_respond(req_id, 500, {}, "")
    end
    btv.notify(
      ("btv.http.mount(%q): on_request errored: %s"):format(mount.name, tostring(err)),
      btv.log.levels.ERROR
    )
  end
end

-- The `respond` a handler is called with. Validates, closes the slot, queues the reply.
function btv._http_reply(req_id, res)
  local deadline = btv._http_open[req_id]
  if not deadline then
    -- Already answered, or timed out. Loud: silently dropping the second reply is how a
    -- double-respond bug survives to production.
    btv.notify(
      "btv.http.mount: respond() called for a request that was already answered "
        .. "(or timed out). Call it exactly once per request.",
      btv.log.levels.ERROR
    )
    return
  end
  -- Retire the request: stop its deadline so a live timer per in-flight request cannot
  -- pile up (or fire a false timeout for a request already answered).
  local function close()
    deadline:stop()
    btv._http_open[req_id] = nil
    btv._http_open_mount[req_id] = nil
  end
  -- A reply that fails validation still answers the client — a 500 now, not a 504
  -- from the deadline later — before the error reaches the caller.
  local function refuse(msg)
    close()
    btv._http_respond(req_id, 500, {}, "")
    error(msg, 3)
  end
  res = res or {}
  if type(res) ~= "table" then
    refuse("btv.http.mount: respond expects a table { status, headers, body }")
  end

  local status = res.status or 200
  if type(status) ~= "number" or status < 100 or status > 599 then
    refuse(("btv.http.mount: respond status must be 100-599, got %s"):format(tostring(status)))
  end

  local body = res.body or ""
  if type(body) ~= "string" then
    refuse(("btv.http.mount: respond body must be a string, got %s"):format(type(body)))
  end

  -- Headers as ordered `{ name, value }` pairs for the bridge (a map at the surface, so a
  -- plugin writes `{ ["content-type"] = "text/html" }`).
  local headers = {}
  -- Refused LOUD here, not dropped: the transport cannot put a header with CR/LF or an
  -- invalid name on the wire, and silently losing one (a `content-security-policy`)
  -- serves the page without it.
  for name, value in pairs(res.headers or {}) do
    if type(name) ~= "string" or not name:match("^[%w!#$%%&'*+%-.^_`|~]+$") then
      refuse("btv.http.mount: invalid response header name " .. tostring(name))
    end
    value = tostring(value)
    if value:find("[\r\n%z]") then
      refuse("btv.http.mount: response header " .. name .. " contains CR, LF or NUL")
    end
    headers[#headers + 1] = { name, value }
  end

  local mount = btv._http_mounts[btv._http_open_mount[req_id]]
  if not (mount and mount.same_origin) then
    headers[#headers + 1] = { "content-security-policy", SANDBOX_CSP }
  end

  close()
  btv._http_respond(req_id, status, headers, body)
end

-- Native callback: the listener rebound onto a new address after an `'httphost'` /
-- `'httpport'` write. Every live mount stays mounted — only the origin moved — so this
-- updates the one place `Mount:origin()` reads.
function btv._http_rebound(base)
  set_base(base)
end

-- ----- Public API ------------------------------------------------------------

-- `btv.http.mount(opts)` -> a promise of a `Mount`.
--
-- Publishes `opts.on_request` at `/plugin/<opts.name>/*` on the editor's one origin
-- (natively under the session-token segment — address it only through `mount:url()`),
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
--   same_origin = false,           -- DANGEROUS when true: serve WITHOUT the sandbox CSP
-- }
-- ```
--
-- Every response is sandboxed into an opaque origin by default (a `content-security-policy`
-- of `sandbox allow-scripts …` is added beside any CSP the plugin sets). `same_origin = true`
-- turns that off so the page can `fetch()` its own mount and use storage — on the web build
-- it then runs with the EDITOR's privileges (its OPFS, the wasm, the daemon link), so only
-- use it for a page made entirely of the plugin's own code, never for untrusted HTML.
--
-- The handler is called `on_request(req, respond)`:
--
-- ```lua
-- req = {
--   method = "GET",                -- upper-cased
--   path = "/style.css",           -- MOUNT-RELATIVE; a bare /plugin/example is "/"
--   raw_path = "/plugin/example/style.css",  -- below the session token, in every world
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
--   btv.fs.read("/tmp/page.html"):next(function(html)
--     respond({ body = html })
--   end)
-- end
-- ```
function btv.http.mount(opts)
  opts = opts or {}
  if type(opts) ~= "table" then
    error("btv.http.mount: expected a table { name, on_request }", 2)
  end
  local name = opts.name
  if type(name) ~= "string" or not name:match("^[%w_-]+$") then
    error(
      ("btv.http.mount: `name` must be a non-empty [%%w_-]+ string, got %s"):format(tostring(name)),
      2
    )
  end
  if type(opts.on_request) ~= "function" then
    error("btv.http.mount: `on_request` must be a function(req, respond)", 2)
  end
  local timeout = opts.timeout or 30000
  if type(timeout) ~= "number" or timeout <= 0 then
    error(
      ("btv.http.mount: `timeout` must be a positive number of ms, got %s"):format(
        tostring(opts.timeout)
      ),
      2
    )
  end

  local same_origin = opts.same_origin
  if same_origin ~= nil and type(same_origin) ~= "boolean" then
    error(("btv.http.mount: `same_origin` must be a boolean, got %s"):format(type(same_origin)), 2)
  end

  return btv.promise.new(function(resolve, reject)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(err, base)
      if err ~= nil then
        -- The mount never published: drop the registry entry so a stale handler cannot
        -- be reached by a route that does not exist.
        btv._http_mounts[id] = nil
        reject(err)
        return
      end
      -- One listener, so one origin for every mount — recorded centrally and read back
      -- through `Mount:origin()`, which is what lets a later rebind move them all.
      set_base(base)
      resolve(setmetatable({ _id = id, _name = name, _alive = true }, Mount))
    end
    -- Register BEFORE the bridge: the actor can route a request to this mount as soon as
    -- the listener is up, and the reply arrives on a later tick either way.
    btv._http_mounts[id] = {
      on_request = opts.on_request,
      name = name,
      timeout = timeout,
      same_origin = same_origin == true,
    }
    -- If the bridge throws (a conversion error), drop BOTH registrations: the
    -- callback could never fire, and the mount table describes a listener that
    -- does not exist. Rethrow so the promise executor rejects.
    local ok, err = pcall(btv._http_mount, id, name, timeout)
    if not ok then
      btv._cb_fns[id] = nil
      btv._http_mounts[id] = nil
      error(err, 0)
    end
  end)
end

-- `btv.http.origin()` -> the origin serving plugin mounts (`http://127.0.0.1:53124`), or
-- `nil` when nothing has mounted yet. Only the origin: natively a mount also needs the
-- session-token path segment, so address mounts with `mount:url()`, not this. Nil rather than a guess: until the first
-- `btv.http.mount` there is no listener, so there is no origin to report and inventing one
-- from `'httpport'` would describe a port nothing is listening on.
function btv.http.origin()
  return btv._http_origin
end
