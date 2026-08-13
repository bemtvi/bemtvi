-- btv.http — a promise-always HTTP client modeled after the browser's `fetch`.
--
-- Part of the pure-Lua `btv.*` prelude (see runtime.rs); the Lua half over the
-- `btv._http_fetch` Rust bridge in install.rs. Loaded AFTER promise.lua — the one
-- entry point returns a PROMISE (there are NO callbacks, matching `btv.run` / `btv.fs`).
--
-- Shape (matches the browser `fetch`): `btv.http.fetch(url[, opts])` returns a promise
-- of a Response. Like `fetch`, the promise RESOLVES for any HTTP status — a 404 or 500
-- is a resolved response you inspect via `response.ok` / `response.status`, NOT a
-- rejection. Only a network / transport failure (DNS, connect, TLS, timeout, a bad
-- URL) REJECTS, with a `{ message }` table.
--
-- ```lua
-- btv.async(function()
--   local res = btv.await(btv.http.fetch("https://api.example.com/search", {
--     query = { q = "hello world", page = 2 },   -- ?q=hello+world&page=2 (encoded for you)
--   }))
--   if res.ok then
--     local data = res:json()               -- decoded JSON (rejects/throws on bad JSON)
--     btv.print(data.name)
--   else
--     btv.print("HTTP " .. res.status)        -- 404 / 500 / … still resolve here
--   end
-- end)()
-- ```
--
-- Query params (`opts.query`) and form bodies (`opts.form`) are encoded with the
-- `rust-url` crates (`form_urlencoded` / `percent-encoding`) via the `btv._url_encode_*`
-- bridges — an established encoder, not bespoke. The building blocks are public too:
-- `btv.http.encode_query`, `btv.http.encode_uri_component`, `btv.http.build_url`.
--
-- The bridge runs each request OFF the editor tick: `btv._http_fetch` queues a
-- LoopOp::Http and returns immediately, so the promise stays pending and SETTLES ON A
-- LATER TICK (the event-loop actor runs `ureq` on its blocking pool natively; a daemon
-- session runs it over the `http_op` leg; a serverless browser uses its own `fetch()`).
-- Non-blocking on native and the only way to reach the network on wasm.

btv.http = btv.http or {}

-- ----- Response --------------------------------------------------------------
--
-- The resolved value of `btv.http.fetch`. A plain table the Rust side fills —
-- `status` (number), `ok` (boolean, `true` for 2xx), `statusText` (string),
-- `headers` (a `{ [lowercased-name] = value }` map), `body` (the raw byte-string) —
-- with `:text()` / `:json()` conveniences bolted on via this metatable.

local Response = {}
Response.__index = Response

-- `res:text()` -> the body as a Lua string (already the raw bytes; this is the
-- identity read, present for `fetch`-parity and readability). Never rejects.
function Response:text()
  return self.body
end

-- `res:json()` -> the body decoded from JSON into a Lua value. Uses `btv.json.decode`
-- (the shared codec). Raises on malformed JSON — inside `btv.async` that surfaces as a
-- promise rejection, exactly like the browser `response.json()` rejecting.
function Response:json()
  return btv.json.decode(self.body)
end

-- ----- fetch -----------------------------------------------------------------

-- ----- URL / query encoding --------------------------------------------------
--
-- Building a request URL by hand is the common footgun a `fetch`-style client should
-- own: a query value with a space / `&` / `=` must be percent-encoded, or it silently
-- corrupts the request. These are the primitives; `opts.query` (below) wires them into
-- `fetch` so you rarely call them directly.

-- `btv.http.encode_uri_component(s)` -> `s` percent-encoded for use INSIDE a URL component
-- (one query key/value, a path segment), matching JavaScript's `encodeURIComponent`:
-- every byte outside the RFC 3986 unreserved set (`A-Z a-z 0-9 - _ . ~`) becomes `%XX`.
-- UTF-8 is encoded byte-by-byte, so a space is `%20` and `é` is `%C3%A9`. Public. The
-- encoding is the `percent-encoding` (`rust-url`) crate's, not a bespoke implementation
-- (`btv._url_encode_component`).
function btv.http.encode_uri_component(s)
  return btv._url_encode_component(tostring(s))
end

-- `btv.http.encode_query(params)` -> an encoded query string with NO leading `?`, built
-- from `params`: either a MAP (`{ q = "hi there", page = 2 }` -> `q=hi+there&page=2`) or
-- an ordered PAIR-LIST (`{ { "q", "a" }, { "q", "b" } }` -> `q=a&q=b`, for repeated /
-- fixed-order keys). A LIST value in a map repeats the key
-- (`{ tag = { "x", "y" } }` -> `tag=x&tag=y`). Keys and values are `tostring`'d. A map
-- has no guaranteed key order — use a pair-list when order matters. Public.
--
-- The Lua side only flattens the map/list shapes into a pair-list; the actual
-- `application/x-www-form-urlencoded` serialization is the `form_urlencoded`
-- (`rust-url`) crate's (`btv._url_encode_query`), so a space is `+` and a `+`/`&`/`=` in a
-- value is escaped correctly — no bespoke encoder.
function btv.http.encode_query(params)
  if type(params) ~= "table" then
    error("btv.http.encode_query: expected a table, got " .. type(params), 2)
  end
  local pairs_out = {}
  local function add(k, v)
    pairs_out[#pairs_out + 1] = { tostring(k), tostring(v) }
  end
  -- A pair-list: a sequence whose first element is itself a `{ key, value }` table.
  if params[1] ~= nil and type(params[1]) == "table" then
    for _, pair in ipairs(params) do
      add(pair[1], pair[2])
    end
  else
    for k, v in pairs(params) do
      if type(v) == "table" then
        for _, item in ipairs(v) do
          add(k, item) -- a list value repeats the key
        end
      else
        add(k, v)
      end
    end
  end
  return btv._url_encode_query(pairs_out)
end

-- `btv.http.build_url(url, query)` -> `url` with `query` (a table, see `encode_query`)
-- appended as a query string — joined with `?`, or `&` if `url` already has a `?`. A nil
-- / empty `query` returns `url` unchanged. Public — the exact join `opts.query` does.
function btv.http.build_url(url, query)
  if query == nil then
    return url
  end
  local qs = btv.http.encode_query(query)
  if qs == "" then
    return url
  end
  return url .. (url:find("?", 1, true) and "&" or "?") .. qs
end

-- Normalize a caller's `headers` table into the `{ name, value }` pair-list the bridge
-- takes. Accepts either a map (`{ ["Content-Type"] = "application/json" }`) or an
-- already-ordered list of `{ name, value }` pairs (for repeated headers / a fixed
-- order). Values are `tostring`'d so a number header (e.g. a content length) is fine.
local function build_headers(headers)
  local out = {}
  if headers == nil then
    return out
  end
  if type(headers) ~= "table" then
    error("btv.http.fetch: `headers` must be a table, got " .. type(headers), 3)
  end
  -- A pair-list: a sequence whose first element is itself a `{ name, value }` table.
  if headers[1] ~= nil and type(headers[1]) == "table" then
    for _, pair in ipairs(headers) do
      out[#out + 1] = { tostring(pair[1]), tostring(pair[2]) }
    end
  else
    for name, value in pairs(headers) do
      out[#out + 1] = { tostring(name), tostring(value) }
    end
  end
  return out
end

-- Shape a `(url, opts)` pair into the values the bridge takes — the shared prep both
-- `fetch` and `fetch_local` do: validate, apply `method` / `query` / `headers`, resolve
-- the body (`body` or `form`), and validate `redirect`. Returns
-- `url, method, headers, body, timeout, redirect, max_redirects`. `who` names the caller
-- in error messages.
local function prepare_request(who, url, opts)
  if type(url) ~= "string" then
    error(who .. ": expected a string url, got " .. type(url), 3)
  end
  opts = opts or {}
  if type(opts) ~= "table" then
    error(who .. ": `opts` must be a table, got " .. type(opts), 3)
  end

  local method = string.upper(opts.method or "GET")
  local headers = build_headers(opts.headers)

  -- Query parameters: append them to the URL (encoded), so callers never hand-build a
  -- query string. `?` if the URL has none yet, else `&`.
  url = btv.http.build_url(url, opts.query)

  -- Does `headers` already carry a `content-type` (any case)? Then we don't add ours.
  local function has_content_type()
    for _, pair in ipairs(headers) do
      if string.lower(pair[1]) == "content-type" then
        return true
      end
    end
    return false
  end

  -- Body precedence: `form` (urlencoded) or `body` (string raw, else JSON), never both.
  local body = opts.body
  if opts.form ~= nil then
    if body ~= nil then
      error(who .. ": pass either `body` or `form`, not both", 3)
    end
    body = btv.http.encode_query(opts.form)
    if not has_content_type() then
      headers[#headers + 1] = { "Content-Type", "application/x-www-form-urlencoded" }
    end
  elseif body ~= nil and type(body) ~= "string" then
    body = btv.json.encode(body)
    if not has_content_type() then
      headers[#headers + 1] = { "Content-Type", "application/json" }
    end
  end

  -- Redirect handling (fetch's `redirect`): follow (default) / manual / error.
  local redirect = opts.redirect or "follow"
  if redirect ~= "follow" and redirect ~= "manual" and redirect ~= "error" then
    error(
      who .. ": `redirect` must be 'follow', 'manual', or 'error', got " .. tostring(redirect),
      3
    )
  end

  return url, method, headers, body or "", opts.timeout, redirect, opts.max_redirects
end

-- Shared executor for `fetch` / `fetch_local`: prepare the request and queue it through
-- `bridge` (`btv._http_fetch` or `btv._local_http_fetch`), settling the promise on the reply.
local function do_fetch(who, bridge, url, opts)
  local u, method, headers, body, timeout, redirect, max_redirects = prepare_request(who, url, opts)
  return btv.promise.new(function(resolve, reject)
    local id = btv._next_cb_id()
    btv._cb_fns[id] = function(err, response)
      if err ~= nil then
        reject(err)
      else
        resolve(setmetatable(response, Response))
      end
    end
    btv._bridge(id, function()
      bridge(id, u, method, headers, body, timeout, redirect, max_redirects)
    end)
  end)
end

-- `btv.http.fetch(url[, opts])` -> promise of a Response. The one entry point of the
-- `btv.http` client — modeled on the browser's `fetch`, fully async, non-blocking.
--
-- Args:
--   * `url`  — the absolute request URL (`http://` or `https://`). Required.
--   * `opts` — an optional table:
--       * `method`  — HTTP method (default `"GET"`); upper-cased for you.
--       * `query`   — query parameters appended to `url` (encoded), so you never build
--                     `?a=1&b=2` by hand. A `{ [name] = value }` map or a
--                     `{ {name, value}, ... }` pair-list (repeated/ordered keys); a LIST
--                     value repeats the key. See `btv.http.encode_query`.
--       * `headers` — request headers, a `{ [name] = value }` map or a
--                     `{ {name, value}, ... }` pair-list (for repeated / ordered headers).
--       * `body`    — the request body. A string is sent verbatim; any other value is
--                     encoded to JSON (`btv.json.encode`) AND a
--                     `Content-Type: application/json` header is added unless the caller
--                     set one.
--       * `form`    — a table sent as an `application/x-www-form-urlencoded` body (same
--                     shape as `query`); sets the content-type header unless the caller
--                     did. Mutually exclusive with `body`.
--       * `timeout` — an overall timeout in milliseconds (default: the backend's).
--       * `redirect` — `"follow"` (default), `"manual"` (don't follow; resolve with the
--                     3xx response), or `"error"` (reject on a redirect) — as in `fetch`.
--       * `max_redirects` — cap on redirects to follow when `redirect == "follow"`.
--
-- The Response it resolves with is `{ status, ok, statusText, headers, body }` — `ok` is
-- true for a 2xx status, `headers` a `{ [lowercased-name] = value }` map — plus `:text()`
-- (the raw body) and `:json()` (the body decoded from JSON).
--
-- RESOLVES with the Response for ANY HTTP status (a 404 / 500 still resolves — check
-- `res.ok` / `res.status`); REJECTS with a `{ message }` table only on a network /
-- transport failure (DNS, connect, TLS, timeout, a bad URL) — the exact split the browser
-- `fetch` makes. Await it inside `btv.async`, or chain `:next` / `:catch`:
--
-- ```lua
-- btv.async(function()
--   local res = btv.await(btv.http.fetch("https://api.example.com/search", {
--     query = { q = "hello world", page = 2 },   -- ?q=hello+world&page=2 (encoded for you)
--   }))
--   if res.ok then btv.print(res:json().name) else btv.print("HTTP " .. res.status) end
-- end)()
-- ```
function btv.http.fetch(url, opts)
  return do_fetch("btv.http.fetch", btv._http_fetch, url, opts)
end

-- `btv.http.fetch_local(url[, opts])` — identical to `btv.http.fetch`, but the request runs
-- on THIS machine's network (the native `ureq`, or the browser's own `fetch()`) even when
-- the session's `btv.http` routes to a daemon. The HTTP analogue of the plugin manager's
-- local-only `btv.fs` (`btv._local_fs_op`): reach for it when a request must originate from
-- the client, not the daemon (`local` is a Lua keyword, hence the `_local` name rather than
-- an option). In a bare/local session it is exactly `btv.http.fetch`.
function btv.http.fetch_local(url, opts)
  return do_fetch("btv.http.fetch_local", btv._local_http_fetch, url, opts)
end
