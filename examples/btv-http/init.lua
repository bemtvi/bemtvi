-- ~~~ bemtvi btv.http playground: a fetch-modeled, promise-only HTTP client ~~~
--
-- Run it (from the repo root; needs network access for the demo endpoints):
--
--     BEMTVI_CONFIG=examples/btv-http \
--       cargo run -p bemtvi -- examples/btv-http/sample.txt
--
-- `btv.http.fetch(url[, opts])` is modeled on the browser's `fetch`: it returns a
-- PROMISE of a Response and is fully async / non-blocking (ADR 0002). Like `fetch`,
-- the promise RESOLVES for any HTTP status — a 404 or 500 is a resolved response you
-- inspect via `response.ok` / `response.status`, NOT a rejection. Only a network /
-- transport failure (DNS, connect, TLS, timeout, a bad URL) REJECTS.
--
-- A Response has: `status` (number), `ok` (boolean, true for 2xx), `statusText`
-- (string), `headers` (a `{ [lowercased-name] = value }` map), `body` (raw string),
-- plus `:text()` (the body) and `:json()` (the body decoded from JSON).
--
-- The same API works everywhere bemtvi runs: natively it's a `ureq` round-trip off the
-- editor tick; over a daemon it runs on the daemon (which owns the network); in a
-- serverless browser build it's the browser's own `fetch()`.

vim.g.mapleader = "\\"

-- Small helper: show a one-line result on the message line.
local function show(msg)
  btv.notify(msg)
end

--------------------------------------------------------------------------------
-- 1. <leader>h — a plain GET. Fetch example.com and report the status + size.
--    The reaction runs on a LATER tick (the request is off the editor tick), so you
--    chain it with `:next` (or await it inside `btv.async`, below).
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>h", function()
  show("GET https://example.com …")
  btv.http.fetch("https://example.com")
    :next(function(res)
      show(("example.com → %d %s (%d bytes, %s)"):format(
        res.status,
        res.ok and "OK" or "not-ok",
        #res:text(),
        res.headers["content-type"] or "?"
      ))
    end)
    :catch(function(err)
      show("request failed: " .. tostring(err.message))
    end)
end)

--------------------------------------------------------------------------------
-- 2. <leader>j — a GET that decodes JSON, with QUERY PARAMETERS. `opts.query` builds
--    and percent-encodes the `?a=1&b=2` for you (backed by the `form_urlencoded`
--    crate) — you never concatenate a URL by hand. Fetch a GitHub search and pull a
--    field out with `res:json()`. Shown with the `btv.async` / `btv.await` style, which
--    reads like straight-line code.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>j", btv.async(function()
  show("GET api.github.com/search …")
  local ok, res = pcall(btv.await, btv.http.fetch("https://api.github.com/search/repositories", {
    query = { q = "language:rust stars:>10000", per_page = 1 },
    headers = { ["User-Agent"] = "bemtvi-btv-http-example" },
  }))
  if not ok then
    show("request failed: " .. tostring(res))
    return
  end
  if res.ok then
    local top = res:json().items[1]
    show(("top rust repo: %s — ★ %d"):format(top.full_name, top.stargazers_count))
  else
    show("HTTP " .. res.status .. " (" .. res.statusText .. ")")
  end
end))

--------------------------------------------------------------------------------
-- 3. <leader>p — a POST with a JSON body. A non-string `body` is JSON-encoded for
--    you and a `Content-Type: application/json` header is added. httpbin echoes the
--    request back, so we can read our own payload out of the response.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>p", function()
  show("POST httpbin.org/anything …")
  btv.http.fetch("https://httpbin.org/anything", {
    method = "POST",
    body = { editor = "bemtvi", feature = "btv.http" },
  })
    :next(function(res)
      if res.ok then
        local echoed = res:json()
        show("server saw json.editor = " .. tostring(echoed.json and echoed.json.editor))
      else
        show("HTTP " .. res.status)
      end
    end)
    :catch(function(err)
      show("request failed: " .. tostring(err.message))
    end)
end)

--------------------------------------------------------------------------------
-- 4. <leader>x — a deliberate transport failure, to show the REJECT path. Nothing
--    listens on this port, so the connect fails and the promise rejects with a
--    `{ message }` table (a 404, by contrast, would RESOLVE with `ok == false`).
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>x", function()
  show("GET http://127.0.0.1:1 (expected to fail) …")
  btv.http.fetch("http://127.0.0.1:1/nope", { timeout = 2000 })
    :next(function(res)
      show("unexpectedly resolved: " .. res.status)
    end)
    :catch(function(err)
      show("rejected (as expected): " .. tostring(err.message))
    end)
end)

--------------------------------------------------------------------------------
-- 5. <leader>r — REDIRECT control. `redirect = "manual"` returns the 3xx response
--    itself instead of following it (the default is "follow"; "error" would reject).
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>r", function()
  show("GET httpbin.org/redirect/1 with redirect='manual' …")
  btv.http.fetch("https://httpbin.org/redirect/1", { redirect = "manual" })
    :next(function(res)
      show(("redirect not followed: %d → %s"):format(res.status, res.headers["location"] or "?"))
    end)
    :catch(function(err)
      show("request failed: " .. tostring(err.message))
    end)
end)

--------------------------------------------------------------------------------
-- 6. <leader>l — LOCAL fetch. `btv.http.fetch_local` is identical to `btv.http.fetch`
--    but always runs on THIS machine's network (never routed to a daemon in a remote
--    session) — the HTTP analogue of the plugin manager's local-only `btv.fs`. In a
--    plain local session it behaves exactly like `\h`.
--------------------------------------------------------------------------------
btv.keymap.set("n", "<leader>l", function()
  show("GET https://example.com (forced local) …")
  btv.http.fetch_local("https://example.com")
    :next(function(res)
      show(("local fetch → %d (%d bytes)"):format(res.status, #res:text()))
    end)
    :catch(function(err)
      show("request failed: " .. tostring(err.message))
    end)
end)

show("btv.http demo loaded — try  \\h  \\j  \\p  \\x  \\r  \\l  (needs network)")
