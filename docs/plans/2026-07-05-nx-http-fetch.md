# nx.http — a fetch-modeled async HTTP client

`nx.http.fetch(url[, opts])` is the Lua HTTP surface, modeled on the browser's
`fetch`: it returns a **promise of a Response** and never blocks the editor tick.
Like `fetch`, any HTTP status **resolves** (inspect `response.ok` / `response.status`);
only a network / transport failure **rejects** (a `{ message }` table). One API,
three worlds — native, native-daemon, and the serverless browser — exactly mirroring
the `nx.fs` off-tick shape.

## Lua surface (`crates/nxvim-lua/src/prelude/http.lua`)

- `nx.http.fetch(url, opts)` → promise of a `Response`.
- `opts`: `method` (default `GET`), `query` (query params, encoded + appended to the
  URL), `headers` (a `{ [name]=value }` map or a `{ {name,value}, … }` pair-list), `body`
  (a string sent raw, else JSON-encoded with a `Content-Type: application/json` header
  added), `form` (a table sent as an `application/x-www-form-urlencoded` body; mutually
  exclusive with `body`), `timeout` (ms).
- `Response`: `{ status, ok, statusText, headers = { [lowercased-name]=value }, body }`
  plus `:text()` (the body) and `:json()` (the body decoded via `nx.json.decode`).
- URL/query helpers (public, lib-backed): `nx.http.encode_query`,
  `nx.http.encode_uri_component`, `nx.http.build_url`. The encoding is the `rust-url`
  crates (`form_urlencoded` + `percent-encoding`) via the `nx._url_encode_*` bridges (in
  `nxvim-lua`, wasm-safe) — the Lua side only flattens map/list shapes into pairs.

Fetch semantics: a 404/500 resolves with `ok == false`; only a DNS/connect/TLS/timeout
failure rejects. The wrapper bridges through `nx._http_fetch(cb_id, url, method, headers,
body, timeout_ms)` and settles the promise from `nx._cb_fns[cb_id](err, response)`.

## The one-shot off-tick op (the `nx.fs` twin)

Types live in `nxvim-lua` (`ops.rs`: `HttpRequest` / `HttpResponse` / `HttpError`), so
they stay transport-free and wasm-safe (no `ureq` in `nxvim-lua`). The wire codec is
`httpwire.rs` (`http_request_{to,from}_value` / `http_result_{to,from}_value`), the HTTP
sibling of `fswire.rs`; bodies ride as msgpack `bin`.

- `LoopOp::Http { id, request }` (queued by the bridge) → drained in
  `effects.rs::apply_loop_op`.
- **Native:** → `LoopCommand::Http` → the event-loop actor's `HttpBackend`
  (`evloop.rs`) → back as `LoopEvent::HttpResult` → `CallbackArgs::HttpResult` →
  `run_callback` marshals the Response into Lua.
- `HttpBackend::Local` runs `crate::http::run_http_request` (`ureq`, blocking) on
  `spawn_blocking`; `HttpBackend::Remote` awaits the daemon `http_op` leg.

`ureq` returns a 4xx/5xx as `Err(Status)`; `run_http_request` folds that back into a
*resolved* `HttpResponse` (fetch semantics) and rejects only on `Err(Transport)`.

## Native-daemon (`daemon.rs`, the `http_op` leg)

The Control-group leg, sibling of `luafs_op`:

- **edit-host side:** `RemoteHttp` (a `req_tx` to `run_http_jobs`, which sends one
  `http_op` request per fetch and decodes the reply); built in `build_link`, driven by a
  job server spawned alongside `run_fs_jobs`, injected via `ServerInit.http_jobs`.
- **daemon side:** `serve_http_daemon_on` runs `run_http_request` on `spawn_blocking`
  and replies with the `["ok", …] | ["err", message]` envelope. Routed by
  `LegGroup::classify` (`http_*` → Control) + the `DaemonLegs` `http` sender.

The daemon owns the network, so a remote session's fetch runs there (dodging CORS) — the
same reason `nx.fs` / processes route to the daemon.

## Serverless browser (`nxvim-edithost` + `worker.mjs`)

- `HostEffects::http_op` (wasm) records `(id, request)` on the `Sink`; drained by
  `eh_take_http_requests` as JSON, landed by `eh_http_result` (msgpack bytes →
  `EditHost::http_result`). Unlike `nx.fs`, **no host gate** — the browser always has
  `fetch()`, so a serverless session needs no daemon.
- `worker.mjs::drainHttpRequests` runs `daemonHttpOp` (the `http_op` leg) when a daemon
  is connected, else `browserHttpFetch` (the browser's own `fetch()`), and re-encodes the
  `["ok"|"err", …]` envelope to msgpack for `eh_http_result`.

## Redirect control

`opts.redirect` mirrors `fetch`: `"follow"` (default, up to `opts.max_redirects`),
`"manual"` (resolve with the 3xx), `"error"` (reject on a redirect). Carried on
`HttpRequest` (crosses via `httpwire`). Native: `run_http_request` builds a
`ureq::Agent` with the right `redirects(n)` cap and turns `"error"` into a reject.
Browser: mapped straight onto the `fetch()` `redirect` option (`max_redirects` has no
`fetch()` equivalent, so it's native/daemon-only).

## Local-forced fetch (`nx.http.fetch_local`)

`nx.http.fetch_local(url, opts)` is identical to `fetch` but runs on **this machine's**
network even in a daemon session — the HTTP analogue of the plugin manager's local-only
`nx.fs` (`nx._local_fs_op`). (`local` is a Lua keyword, hence the `_local` name rather
than an option.) Implemented as a `local: bool` flag on `LoopOp::Http` /
`LoopCommand::Http` (bridged by `nx._http_fetch` = false / `nx._local_http_fetch` = true):

- Native: the actor picks `HttpBackend::Local` (its own `ureq`) when `local`, ignoring the
  session's (possibly daemon) `HttpBackend`.
- Wasm: `HostEffects::http_op(id, request, local)` → the Worker's `drainHttpRequests`
  routes `daemonUri && !local ? daemonHttpOp : browserHttpFetch`, so `local` forces the
  browser's own `fetch()` past a connected daemon.

(Streaming was prototyped but removed — its local-vs-remote asymmetry with buffered fetch
was the wrong tradeoff. `fetch` stays daemon-routed for its CORS / reachability benefit.)

## Tests

- `crates/nxvim-server/tests/http.rs` — the native path against a loopback server
  (2xx / json / 404-resolves / POST body / transport-reject).
- `crates/nxvim-server/tests/daemon_http.rs` — the `http_op` leg over an in-process
  duplex (the actor is `HttpBackend::Remote`, so a resolved response can only have crossed
  the wire).
- `crates/nxvim-edithost/web/verify-http.mjs` — the serverless browser `fetch()` leg in a
  real headless Chromium against `serve.mjs`'s same-origin `/api/*` test routes (incl.
  query building + `fetch_local`).
- Native redirect (follow/manual/error) + `fetch_local` are in `tests/http.rs`;
  `daemon_http.rs` proves `fetch` routes to the daemon while `fetch_local` bypasses it (a
  fake daemon that rejects `http_op`).
- `examples/nx-http/` — a runnable playground (`\h` GET, `\j` JSON, `\p` POST, `\x`
  reject).
