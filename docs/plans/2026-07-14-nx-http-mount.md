# nx.http.mount — plugin-defined HTTP, one editor-owned origin

`nx.http.mount(opts)` is the Lua HTTP **server** surface, the inbound twin of
[`nx.http.fetch`](2026-07-05-nx-http-fetch.md). A plugin does **not** bind a port. It
**mounts a subroute** on one editor-owned origin and gets a URL back — enough to serve a
live markdown renderer, a preview pane, or any browser-facing UI a plugin wants to own.

```lua
nx.http.mount({
  name = "example",
  on_request = function(req, respond)
    respond({ status = 200, headers = { ["content-type"] = "text/html" }, body = render() })
  end,
}):next(function(mount)
  nx.ui.open(mount:url())        -- http://127.0.0.1:53124/plugin/example/
end)
```

## Why mounts and not ports

The first draft of this plan gave every plugin its own listener (`nx.http.serve{ port }`).
That was wrong, and the browser is what proves it: **a tab cannot bind a TCP port**. A
per-plugin-port API is native-only by construction, so the web build would need a
*different* API, and plugin code would not port between them. `:url()` would mean two
different things.

One editor-owned origin fixes that. The plugin's contract everywhere is *"here is my
subroute handler, give me my URL"* — which a Service Worker can satisfy exactly as well as
a `TcpListener` can. Same Lua, three worlds, which is the same bet `nx.http.fetch` made.

It also removes a whole class of plugin bugs that ports invite: port collisions between two
nxvim instances, plugins hard-coding `8080`, and every plugin author re-inventing bind-retry.

**Nothing starts until a plugin asks.** The listener is **lazily bound on the first mount**
and never before — a config with no HTTP plugin opens no port, spawns no task, and is
byte-for-byte the editor it is today. The listener outlives an unmount for the rest of the
session (an idle listener is free, and a stable URL survives a plugin reload); it is torn
down at shutdown.

### The cost: mounts share one origin

Worth naming plainly rather than discovering later. `http://127.0.0.1:PORT/plugin/a/` and
`/plugin/b/` are the **same origin**, so the same-origin policy does *not* isolate two
mounts: script in mount A can `fetch` mount B and read the reply. Per-plugin *ports* would
have been separate origins and would have gotten that isolation for free.

We take the trade knowingly, because:

- The web build **forces** same-origin regardless (a Service Worker intercepts paths on the
  page's own origin). Per-plugin ports would buy isolation natively and nothing on the web
  — two security models for one API, and the weaker one still sets the ceiling for any
  plugin that wants to run in both.
- A plugin serving **untrusted** HTML (rendered markdown from a repo under review) must
  defend itself anyway — the threat there is script execution in the preview, which a port
  boundary does not stop.

So: mounts are a **trust boundary between the editor and the network**, not between
plugins. A plugin that renders untrusted content sets a restrictive `content-security-policy`
on its own responses. The docstring says this in as many words.

## Two shapes in one API, and why

`nx` is **promise-only for one-shot async; a persistent event stream stays handler-based**
(`prelude/process.lua`). A mount is both, so it uses both:

- **The mount is one-shot** → `nx.http.mount` returns a **promise of the handle**. It
  resolves once the route is live (and the lazy listener bound), and rejects loudly on a
  duplicate name or a bind failure. The promise is also what makes an ephemeral port usable:
  the concrete port only exists *after* the bind, which is cross-tick state — exactly the
  `winid` trap `nx.schedule` cannot poll (CLAUDE.md). Resolving the handle hands over a URL
  with the port already settled, so no plugin ever polls for it.
- **The requests are a persistent stream** → `opts.on_request`, a handler. Same doctrine as
  `nx.process.open` / `nx.socket.connect` / `nx.fs.watch`.

### Why `respond` is a callback, not a return value

`on_request(req, respond)` — the handler *calls* `respond(res)` rather than returning it. A
return value forces the reply same-tick; real handlers await (`nx.fs.read` a file,
`nx.http.fetch` an upstream, render a buffer). Passing `respond` lets a handler reply now or
later, with no ambiguity about whether a returned promise means "the response" or "the
response's body".

## Lua surface (`crates/nxvim-lua/src/prelude/httpmount.lua`)

- `nx.http.mount(opts)` → promise of a `Mount`.
  - `opts.name` — **required**; `[%w_-]+`. Mounts at `/plugin/<name>`. The `/plugin/`
    namespace is reserved for mounts so the editor keeps `/` and `/_nx/*` for itself.
  - `opts.on_request(req, respond)` — **required**; the request handler.
  - `opts.timeout` — ms a request may sit unanswered before the client gets a `504` and the
    slot is reclaimed (default `30000`). Guards a handler that never responds. Enforced in
    Lua, so it means the same thing on every build (see *Web*).
- `req` = `{ method, path, query, headers, body, name, raw_path }`.
  - **`req.path` is mount-relative** — `GET /plugin/example/style.css?v=2` gives
    `path = "/style.css"`, `query = { v = "2" }`, `raw_path = "/plugin/example/style.css"`.
    Relative paths are what let the same handler work under any prefix, native or web.
    A bare `GET /plugin/example` normalizes to `path = "/"`.
  - `headers` are lowercased; `body` is a binary-safe string (`""` when there is none).
- `respond(res)` — `res` = `{ status = 200, headers = {}, body = "" }`, every field
  optional. A second `respond` on the same request notifies loud (the slot is gone).
- `Mount` handle: `:url()` (`http://127.0.0.1:53124/plugin/example/`), `:path()`
  (`/plugin/example`), `:close()` — removes the route; idempotent.
- `nx.http.origin()` → the base origin, or `nil` when nothing has mounted yet (so it never
  lies about a listener that does not exist).

Unmounted paths under `/plugin/` get a `404` from the editor, not from any plugin.

A **bare mount root** (`/plugin/example`, no trailing slash) `308`-redirects to
`/plugin/example/` — the same trailing-slash redirect a web server does for a directory.
Without it, a page served at `/plugin/example` resolves a relative URL (`fetch("source")`)
against its parent `/plugin/`, hitting `/plugin/source` (the wrong mount); the slash makes
the mount root the base. The redirect is applied by the editor before the handler runs, in
both worlds (the native listener and the browser's `EditHost`, via one shared
`nxvim_lua::mount_root_redirect`), so a plugin serving an index page at its root needs no
`<base>` tag or absolute paths. A sub-path is left alone — it is the plugin's own routing.

## User options: `'httphost'` and `'httpport'`

Where mounts listen is the **user's** call, not a plugin's — it is their machine, their
firewall, their bookmark. So it is an ordinary option (nxvim's own, not standard vim),
global, settable from `:set` / `nx.o` / `vim.o` like any other:

```lua
nx.o.httphost = "127.0.0.1"   -- default; loopback only
nx.o.httpport = 0             -- default; 0 picks a free ephemeral port
```

```vim
:set httpport=8080            " a stable, bookmarkable URL
:set httphost=0.0.0.0         " expose mounts to the LAN — see the security note
```

| option | kind | default | meaning |
|---|---|---|---|
| `'httphost'` | string | `"127.0.0.1"` | interface the mount listener binds |
| `'httpport'` | number | `0` | port to bind; `0` picks a free one |

Both are `Global`, `abbrev: None` (there is no vim heritage to be short for). This replaces
the `nx.http.configure{}` call an earlier draft had: a Lua-only setter would have let the
*first plugin to load* silently decide the user's port, and would have been unreachable from
a `:set` or a vimrc.

**When they are read.** At **bind time** — the first mount. Until then they are inert, which
is the whole "nothing starts" invariant: a config that sets `'httpport'` but mounts nothing
still opens no port.

**Changing them while serving rebinds, atomically.** A `:set httpport=9000` after a mount is
live must not silently do nothing (the listener lives for the session, so "takes effect at
the next bind" would be a lie — there is no next bind). Instead the editor **binds the new
address first, and only on success swaps the router onto it and drops the old listener**.
Because the route map is shared (`Arc`), the swap keeps every mount live; only the origin
changes, and `mount:url()` reports the new one.

If the new bind **fails** (port in use), nothing has moved: the old listener is still
serving, so the editor notifies loud and **reverts the option to the live value**. That
keeps `:set httpport?` and `nx.http.origin()` in agreement — an option that reads `9000`
while the editor serves `:53124` would be exactly the silent lie the project's fail-loud
rule exists to prevent.

There is no `OptionSet` event in the tree, so the change is noticed by comparing the live
values against the bound address on the tick — **gated on a listener actually existing**, so
a config with no HTTP plugin pays nothing for it.

## Binding is a security boundary

`quic.rs` is blunt that an unauthenticated listener which executes arbitrary code "is remote
code execution by design — the TLS cert buys *encryption, not authorization*". A mount's
`on_request` is arbitrary Lua with the editor's full authority, so the port it answers on is
a capability.

- **`'httphost'` defaults to `"127.0.0.1"`** — loopback only. Reaching the LAN takes an
  explicit `:set httphost=0.0.0.0`, and the option's `doc` says what that means. Making it a
  *user* option rather than a plugin call matters here: exposing the editor to the network
  is a decision the human takes deliberately, not one a plugin can make on their behalf by
  loading.
- No token is minted. Unlike the daemon (whose protocol we define end to end), a mount
  answers ordinary browsers, so auth is the plugin's own policy. The loopback default is
  what keeps it safe by construction.
- Plaintext HTTP only. TLS on a loopback preview buys nothing, and a self-signed cert would
  only train users through a browser warning.

## The listener is always **local**, and there is no daemon leg

`nx.http.fetch` routes to the daemon (the daemon owns the network; it dodges CORS and
reaches the remote's private hosts), with `fetch_local` as the escape hatch. **A server
inverts this and needs neither.**

The Lua VM lives in the **edit-host** — the local machine — in every session, including a
`--daemon` / `--remote-config` one (plugins load into the local VM via the local rtp; see
the plugin manager's local-always seam). So `on_request` runs locally no matter what. A
listener bound on the *daemon* would round-trip every request back to the local VM to be
answered — strictly worse, and pointless for the motivating case: a markdown preview exists
to be opened by the **human's** browser, which is at the local machine.

So the mount ops carry **no `local` flag** and add **no `srv_*` daemon leg**. (A
remote-bound listener, if ever wanted, is a separate op — not a flag on this one.)

## Rust: the op round-trip

Types live in `nxvim-lua` (`ops.rs`), transport-free and wasm-safe, beside
`HttpRequest`/`HttpResponse`:

- `HttpServerRequest { name, path, method, query, headers, body, raw_path }`
- `HttpServerReply { status, headers, body }`

New `LoopOp`s — the `SockConnect`/`SockWrite`/`SockClose` triple is the template:

- `LoopOp::HttpMount { id, name, timeout_ms }` — ensure the listener, add the
  route. `host`/`port` are the `'httphost'`/`'httpport'` values, read off the editor by the
  effects layer at mount time — the actor is told the address, it never reads options.
- `LoopOp::HttpRespond { id, req_id, reply }` — answer one in-flight request.
- `LoopOp::HttpUnmount { id }` — `mount:close()`.
- `LoopOp::HttpRebind { host, port }` — an `'httphost'`/`'httpport'` write while serving.

Native flow (`effects.rs::apply_loop_op` → `LoopCommand` → the `evloop.rs` actor):

- `LoopCommand::HttpMount` → if no listener exists, bind a `tokio::net::TcpListener` and
  spawn **axum** (`axum::serve(listener, router)`, `.with_graceful_shutdown(rx)`) with a
  single fallback handler; register the route. The resolved `SocketAddr` comes back as
  `LoopEvent::HttpMountResult` → `CallbackArgs::HttpMountResult` → the promise settles with
  the URL (or rejects with the bind / duplicate-name error).
- `LoopCommand::HttpRebind` → bind the new address **before** touching the old: on success,
  spawn a fresh `axum::serve` over the *same* `Arc` route map and shut the old one down
  (every mount stays live, only the origin moves); on failure, leave the old listener
  serving and report `LoopEvent::HttpRebindErr` so the editor can notify and revert the
  option.
- The fallback handler parses `/plugin/<name>/<rest>`, looks the name up in the route map
  (miss → `404` without ever entering Lua), allocates a `req_id`, emits
  `LoopEvent::HttpServerRequest { id, req_id, request }`, and **awaits a
  `oneshot::Receiver<HttpServerReply>`** parked in the actor's pending map.
- `LoopOp::HttpRespond` completes that oneshot → axum writes the response.
- A dropped sender (unmounted, handler threw, timeout fired) synthesizes `503`/`500`/`504`
  rather than hanging the socket.

The **per-request id space** is the one genuinely new thing versus `nx.socket`, whose
`on_data` is fire-and-forget: a `u64` counter in the actor, namespaced under the mount id.
The `(id, req_id)` pair is what `respond` carries back.

### The dep

`axum = "=0.8.9"` (with its `tower`/`hyper`/`http-body-util` tree), pinned exactly per the
workspace convention and gated behind `nxvim-server`'s `native` feature exactly like `ureq`.
`tokio`'s `net` feature is already on; axum's `tokio` + `http1` features are what
`axum::serve` needs. The current-thread runtime is fine — axum wants a running reactor, not
a multi-thread one.

## Web: the Service Worker virtual origin

The web build reaches the **same** `HttpServerRequest`/`HttpServerReply` contract with a
Service Worker in place of the listener. The plugin's Lua does not change; only `:url()`
resolves differently (the page's own origin, e.g. `https://demo.nxvim.dev/plugin/example/`).

**Why a Service Worker at all.** The point of this feature is a *real URL* — one an
`<iframe>`, an `<img src>`, a `window.open`, or a stylesheet `@import` can load. Only a SW
can make ordinary browser loads resolve to editor-generated bytes; postMessage plumbing
could return data to the page, but nothing else in the platform would treat it as a URL.

**Topology.** The edit-host wasm lives in a Worker; the SW is a third context and cannot
call it. The SW relays through a window client:

```
browser load  /plugin/example/index.html
     └─► ServiceWorker  fetch handler          (nx-sw.js, scope "/")
            └─► postMessage + MessageChannel ─► window client (index.html)
                    └─► worker.mjs ─► eh_http_server_request(...)  [wasm]
                            └─► Lua on_request → respond
                    ◄─ eh_take_http_server_replies ──┘
            ◄─ port.postMessage(reply) ──┘
     ◄─ event.respondWith(new Response(...))
```

`nx-sw.js`, served from the site root so its scope covers `/plugin/`:

```js
self.addEventListener("fetch", (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;
  if (!url.pathname.startsWith("/plugin/")) return;   // reserved namespace; everything else falls through
  event.respondWith(routeToEditHost(event.request));
});

async function routeToEditHost(request) {
  const client = await pickClient();
  // Fail loud: a mount whose edit-host is gone is a 503, never a silent empty 200.
  if (!client) return new Response("nxvim: no edit-host client", { status: 503 });

  const channel = new MessageChannel();
  const reply = new Promise((resolve) => { channel.port1.onmessage = (e) => resolve(e.data); });
  client.postMessage({ type: "nx-http-request", request: { … } }, [channel.port2]);
  const res = await reply;
  return new Response(res.body, { status: res.status, headers: res.headers });
}
```

**Which window to relay to is not obvious, and getting it wrong hangs the load.** The tab a
user *opens the mount URL in* is itself a same-origin, focused window client — so a naive
`clients[0]` (or a pick-by-focus) relays the request to that very tab, which is loading the
mount page and has no relay listener, and the load spins until the backstop. So `pickClient`
does not guess: it **probes** every window (`postMessage` a `nx-edithost-probe` with a reply
port) and relays to the first that answers. Only a tab running the editor's message handler
answers, so the requesting tab — and any other non-editor page on the origin — is never
chosen. Statelessly asking each time also survives a Service Worker restart, which a
registration table would not. (This one only showed up when the mount was opened in a
*separate* tab; a same-page fetch never hit it, which is why `verify-http-mount.mjs` now
drives the multi-tab path explicitly.)

The window client forwards to the edit-host Worker and posts the reply back down the port.
`eh_take_http_server_requests` / `eh_http_server_reply` mirror the fetch pair
(`eh_take_http_requests` / `eh_http_result`) and need their `EXPORTED_FUNCTIONS` entries in
`build.sh`.

**Wasm dispatch.** `HostEffects::http_mount` / `http_respond` / `http_unmount` on the
`#[cfg(not(feature = "native"))]` side, recorded on the `Sink` and drained by the Worker —
the same shape as `http_op`. This is the **no-gate** row of the three degradation
strategies after all, not the loud-failure row the first draft assumed: a SW is always
available on the demo origin, so a serverless session needs no daemon and no listener.

**The wake is the hard part** (found in build, not design). `http_op` is *editor-initiated*:
the tick enqueues, the Worker drains. A mount request is the mirror — *browser-initiated* —
and the Worker's run loop **parks on `Atomics.wait`**, which blocks its event loop entirely.
A `postMessage` would sit undelivered until some unrelated keystroke happened to wake it. So
the relayed request rides the **SAB ring as a frame** (type 9), exactly like a keystroke:
the ring's `SEQ` bump is the only thing that can wake a parked loop. The mount result rides
back as frame type 10. Under the postMessage fallback (5c) the loop never parks, so plain
messages are used there.

**Routing lives in Rust, not JS.** The Worker relays the *full* path and the edit-host does
the `/plugin/<name>/<rest>` split, the mount lookup, and the miss→`404` — through the same
`nxvim_lua::build_server_request` the native listener calls. Doing it in JS would have been
easier and would have quietly let `req.path` mean two different things in the two worlds.

**Cross-origin isolation.** The editor page is COEP `require-corp` (the SharedArrayBuffer
prerequisite), and a document embedded in such a page must assert COEP itself — so an
`<iframe>` of a mount, the whole point of serving a real URL, fails without it. The SW
therefore defaults `cross-origin-embedder-policy: require-corp` +
`cross-origin-resource-policy: same-origin` on every mount reply, letting an explicit plugin
header win. The editor knows it is isolated; a plugin should not have to.

**Scope.** A Service Worker's scope cannot rise above the path it was served from, so
`nx-sw.js` is served from the **root** (`/nx-sw.js`) even though its source lives in `web/`
— `serve.mjs` special-cases it, `package-site.sh` stages it at the publish root. Registering
it with `scope: "/"` additionally needs a **`Service-Worker-Allowed: /`** header on the
script response, or the browser rejects the registration outright and every mount 404s; that
header is in `serve.mjs` and in `web/_headers` for the deployed site.

**Constraints, honestly.** A SW needs a secure context (HTTPS or `localhost`) — met by the
demo site, and a plain-`http://` non-localhost origin rejects at `mount`. `serviceWorker.ready`
means "there is an active worker", *not* "it controls this page": on a first visit the page
loaded uncontrolled, so the registration also waits for `controllerchange` (the SW's
`clients.claim()`), or the first mount would 404 until a reload. All of that is why `mount`
returns a promise rather than a synchronous handle — the API absorbs the wait with no
plugin-visible difference. A hard reload with the SW bypassed serves no mounts, which is
correct: no editor, no content.

**`opts.timeout` needs no transport at all.** It is enforced in **Lua**: `nx._http_request`
arms a deadline per request, and on expiry answers `504` and notifies. So the browser build
inherits the contract exactly, with no listener to enforce it and nothing to keep in sync.

That split is the point. `opts.timeout` is the **plugin's contract** — "your handler did not
answer in time" — and only Lua knows it, so one implementation there is inherited by every
transport. What each transport keeps is a far-longer **backstop** for the one case a Lua
deadline cannot cover: the editor never runs it (a wedged tick; or the postMessage fallback,
where Lua timers never fire at all). That is *resource safety* — reclaiming a socket or a
parked `fetch` — a different concern that merely shares a unit. Native's backstop is
`opts.timeout + 30s` (exact, since the listener knows the mount); the SW's is a flat 5
minutes, which caps a mount asking for more than that — worth knowing, not worth plumbing a
per-mount value through a Service Worker for.

An earlier draft had the listener enforce `opts.timeout` and the SW apply its own fixed 30s
backstop, which made the option native-only *and* let a legitimately slow web handler be cut
off at 30s. Moving it to Lua deleted a `LoopEvent`, a runtime call, and a Lua callback — the
web parity came out as a consequence rather than as more code.

## Fail loud

- Bind failure, duplicate mount name, or a bad `name` **rejects** the promise. No silent
  fallback to another port or a suffixed name.
- A throwing `on_request` responds `500` **and** notifies — never swallowed into a status.
- A handler that never responds yields `504` at `opts.timeout` and notifies — from Lua, so
  identically on every build.
- A second `respond` on the same request notifies rather than silently dropping.
- An `'httphost'`/`'httpport'` write while serving rebinds or notifies-and-reverts — it
  never silently fails to apply, and never leaves the option disagreeing with reality.
- Web with no SW support (insecure origin) rejects at `mount` rather than handing back a URL
  that will 404.

## Phases

1. **Native** — `'httphost'`/`'httpport'` in `nxvim-core`'s `options.rs` (field + set/get
   arms + `Default` + `OptionInfo`), `ops.rs` types + `LoopOp`s, the `evloop.rs` actor +
   axum + shared route map + rebind, `effects.rs` dispatch (+ the gated option-change
   check), `runtime.rs` marshalling, `install.rs` bridges, `prelude/httpmount.lua`,
   `tests/http_mount.rs`.
2. **Example** — `examples/nx-http-mount/`: a markdown renderer plugin serving the current
   buffer as styled HTML, end-to-end runnable.
3. **Web** — `nx-sw.js`, the window-client relay, the `HostEffects` seam + FFI exports,
   `verify-http-mount.mjs` in headless Chromium. **Done.**

## Tests (`crates/nxvim-server/tests/http_mount.rs`)

Black-box through the running server, driving Lua via `exec_lua` and hitting the bound port
with a real client:

- **nothing starts unmounted** — no listener exists until the first mount (the invariant)
- an ephemeral port resolves; `:url()` is reachable (GET 200, body round-trips)
- `req` fidelity: method, mount-relative `path`, `query`, headers, and a POST body reach Lua
- **two mounts on one origin** route independently by name
- an **async** handler (responds a tick later) still answers
- custom `status` + `headers` reach the client
- an unmounted `/plugin/<name>` is a `404`; `:close()` makes a live mount start 404ing
- a duplicate `name` **rejects**; a bad `name` **rejects**
- a throwing handler yields `500`; a silent handler yields `504` at a short `timeout` (which
  the listener's `opts.timeout + 30s` backstop could not have produced — so a pass proves the
  Lua deadline is what fired)
- **`'httpport'` is honored** — set it before mounting and the listener binds that exact port
- **a rebind while serving** — `:set httpport=<free>` moves the origin and every mount stays
  live on the new one; the old address stops answering
- **a failed rebind** — `:set httpport=<taken>` leaves the old listener serving *and* reverts
  the option, so `nx.o.httpport` still equals the live port
- the loopback default does not bind a public interface
