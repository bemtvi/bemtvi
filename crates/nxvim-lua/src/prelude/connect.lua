-- nxvim Lua prelude — `nx.connect`: the connect-provider registry + live `:connect`
-- routing (§C of the remote-connectors plan). A **connector** (e.g. a remote-containers /
-- remote-ssh system plugin) registers an async resolver for a URL scheme; `:connect <url>`
-- then routes through the local VM: a matching resolver runs (it may provision a
-- remote/container and stream progress), returns a transport spec, and the client swaps the
-- window onto it via `nx.session.reconnect` (§B). With no matching provider, `:connect`
-- falls back to the client's built-in direct dial (an `nxvim://…` QUIC URI, or an
-- `[user@]host[:port]` ssh target), so nothing regresses.
--
-- Loads AFTER nx.lua (needs `nx.session.reconnect`, `nx.command`, `nx.notify`) and after
-- promise.lua (a resolver may be asynchronous — return a promise — and `nx.promise.try`
-- folds a synchronous-throw or an async-reject into one chain). See
-- docs/plans/2026-07-05-remote-connectors-and-system-plugins.md → §C.

local vim = vim
nx = nx or {}

nx.connect = nx.connect or {}

-- The registered providers, in registration order:
--   { match = fn(url) -> boolean, resolve = fn(url) -> spec|promise, label = string }
-- `_resolve` scans this list in REVERSE, so a later registration overrides an earlier one
-- for the same scheme (a user's `init.lua` can shadow a system connector).
nx.connect._providers = nx.connect._providers or {}

-- The scheme of a URL (`"container"` from `"container://ubuntu"`), or `nil` if it carries
-- no `scheme://` prefix (a bare `[user@]host` ssh target). The scheme grammar follows
-- RFC 3986: an ASCII letter followed by letters / digits / `+` / `-` / `.`.
local function url_scheme(url)
  if type(url) ~= "string" then
    return nil
  end
  return url:match("^(%a[%w%+%-%.]*)://")
end

-- `nx.connect.register(scheme_or_matcher, resolver)` — register a connector.
--
--   * `scheme_or_matcher` — either a **scheme** string (`"container"`, `"ssh"`), matched
--     against the URL's `scheme://` prefix, or a **matcher** function `fn(url) -> boolean`
--     for host-pattern routing (e.g. only `*.internal` hosts).
--   * `resolver` — `fn(url) -> spec`. It resolves `url` to a session-swap spec (the table
--     `nx.session.reconnect` takes: `{ transport = …, config_source = …, keep_buffers = … }`).
--     `spec.config_source` (§D) picks whose config the swapped session runs — `"remote"`
--     (default; the daemon's config + plugins) or `"local"` (this machine's config, the
--     daemon backing only the seams — the dev-container shape). It MAY provision
--     asynchronously — return a **promise** that fulfils with the spec — and MAY stream
--     progress with `nx.notify` ("detecting arch…", "starting daemon…").
--
-- Registering the same scheme twice keeps both; the later wins (reverse scan in `_resolve`).
-- Fails LOUD on a bad argument (a mistyped registration is a bug, not a silent no-op).
function nx.connect.register(scheme_or_matcher, resolver)
  if type(resolver) ~= "function" then
    error("nx.connect.register: resolver must be a function", 2)
  end
  local matcher, label
  if type(scheme_or_matcher) == "string" then
    if scheme_or_matcher == "" then
      error("nx.connect.register: scheme must be a non-empty string", 2)
    end
    local scheme = scheme_or_matcher
    label = scheme .. "://"
    matcher = function(url)
      return url_scheme(url) == scheme
    end
  elseif type(scheme_or_matcher) == "function" then
    matcher = scheme_or_matcher
    label = "<matcher>"
  else
    error(
      "nx.connect.register: first argument must be a scheme string or a matcher function, got "
        .. type(scheme_or_matcher),
      2
    )
  end
  table.insert(nx.connect._providers, { match = matcher, resolve = resolver, label = label })
end

-- The first provider whose matcher accepts `url` (scanned newest-first so a later
-- registration overrides), or `nil` if none match. A matcher that throws is treated as a
-- non-match (a buggy connector must not block every other provider or the fallback).
function nx.connect._resolve(url)
  for i = #nx.connect._providers, 1, -1 do
    local p = nx.connect._providers[i]
    local ok, matched = pcall(p.match, url)
    if ok and matched then
      return p
    end
  end
  return nil
end

-- `nx.connect.connect(url)` — the `:connect` entry point (also callable directly). Route
-- `url` through the provider registry: a matching resolver runs (async-aware) and its spec
-- swaps the window (§B); with no provider, hand `url` to the client's built-in direct dial.
-- A resolver error / a non-spec return surfaces LOUD (a message), leaving the session intact
-- — the swap only happens once a valid spec resolves.
function nx.connect.connect(url)
  if type(url) ~= "string" or url == "" then
    return nx.notify(
      "usage: :connect {nxvim://host:port/token?cert=hash | [user@]host[:port][/file] | scheme://…}",
      vim.log.levels.ERROR
    )
  end
  local provider = nx.connect._resolve(url)
  if not provider then
    -- No connector owns this URL — let the client dial it directly (QUIC URI / ssh host).
    nx._connect_fallback(url)
    return
  end
  -- `nx.promise.try` runs the resolver now: a thrown error becomes a rejection, a returned
  -- promise is adopted, a plain spec fulfils immediately — one chain for all three.
  nx.promise
    .try(provider.resolve, url)
    :next(function(spec)
      if type(spec) ~= "table" then
        error(
          "connect resolver for "
            .. provider.label
            .. " must return a spec table, got "
            .. type(spec)
        )
      end
      -- Hand off to §B: validates + normalizes the spec, then swaps (fails loud on a bad one).
      nx.session.reconnect(spec)
    end)
    :catch(function(err)
      nx.notify(
        "connect (" .. provider.label .. ") failed: " .. tostring(err),
        vim.log.levels.ERROR
      )
    end)
end

-- The `:connect` ex-command. The whole argument is the URL (a URL / ssh target carries no
-- spaces; `o.args` is everything after the command name, trimmed of surrounding space).
-- Routes through the local VM (this file) rather than being client-intercepted, so a
-- connector's resolver gets a shot and both front ends (TUI + GUI) share one code path.
nx.command("connect", function(o)
  nx.connect.connect((o.args or ""):match("^%s*(.-)%s*$"))
end, {
  desc = "Connect to a remote/daemon: :connect {nxvim://… | [user@]host[:port][/file] | scheme://…}",
})

return nx
