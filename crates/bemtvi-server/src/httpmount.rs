//! The `btv.http.mount` listener: **one** editor-owned HTTP origin serving every plugin's
//! mounted subroute at `/plugin/<name>/*`.
//!
//! The inbound twin of [`crate::http`], which is outbound-only (`ureq`). Where a plugin's
//! `btv.http.fetch` *calls* the network, a plugin's `btv.http.mount` *answers* it — enough to
//! serve a live markdown renderer or any browser-facing UI a plugin wants to own. See
//! `docs/plans/2026-07-14-btv-http-mount.md`.
//!
//! **One origin, not a port per plugin.** A browser tab cannot bind a TCP port, so a
//! per-plugin-port API would be native-only by construction and plugin code would not port
//! to the web build. A mounted subroute is something a Service Worker satisfies exactly as
//! well as this `TcpListener` does, so the same Lua runs in every world. It also removes
//! port collisions between two bemtvi instances and plugins hard-coding 8080.
//!
//! **The listener is a same-origin surface.** A page on a foreign site must not be able to
//! drive a plugin's mount — its stateful handlers would be open to CSRF, and a
//! DNS-rebinding domain would bypass the loopback restriction entirely. The handler
//! rejects every request whose `Origin` does not name this listener's own address
//! ([`same_origin`]); cross-site fetches and form posts always carry `Origin`, so the
//! mutation-capable requests are closed off before any route lookup runs.
//!
//! **Nothing starts until a plugin asks.** [`HttpMounts`] holds no listener until the first
//! `HttpMount` command; a config with no HTTP plugin opens no port and spawns no task.
//!
//! **The request round-trip is the interesting part.** The editor and the Lua VM are
//! `!Send` and live on the server's one thread, so an axum handler — running on the actor's
//! tokio task — cannot call the plugin's Lua handler itself. Instead it parks: it allocates
//! a `req_id`, sends the request inbound as a [`LoopEvent::HttpServerRequest`], and awaits a
//! [`oneshot`] the server thread completes when the plugin's `respond` runs. The pending map
//! is what joins the two.
//!
//! ```text
//! GET /plugin/example/  ─► axum handler (actor task)
//!                            ├─ route lookup: name → mount id (a miss is a 404, no Lua)
//!                            ├─ pending[req_id] = oneshot::Sender
//!                            ├─ LoopEvent::HttpServerRequest ──► server thread
//!                            │                                     └─ Lua on_request(req, respond)
//!                            │      LoopCommand::HttpRespond ◄────────┘
//!                            ├─ ◄── oneshot completes
//!                            └─ 200 OK
//! ```
//!
//! Every way that round-trip can *not* complete is a distinct status rather than a hung
//! socket: the mount closed (`503`), the handler threw (`500`), or it never responded
//! (`504`) — all three sent by the Lua side, which owns the plugin-facing contract and so
//! keeps it identical on the browser build. This module's only deadline is [`Route`]'s
//! `backstop`, for when the editor never runs that Lua at all.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use bemtvi_lua::HttpServerReply;
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::oneshot;

use crate::evloop::LoopEvent;

/// The largest request body a mount will buffer, 16 MiB. A mount handler takes its body as
/// one Lua string, so the body is buffered whole either way; this bounds what an unbounded
/// upload can make the editor allocate ("the editor must never freeze"). A larger body is
/// rejected with `413` rather than truncated — a silently short body would corrupt whatever
/// the plugin does with it.
const MAX_BODY: usize = 16 * 1024 * 1024;

/// How much longer than a mount's own `opts.timeout` the listener holds a parked request
/// before reclaiming its socket. The Lua deadline answers `504` at `opts.timeout`; this only
/// fires when the editor never ran that deadline at all, so it needs enough slack that a
/// merely-busy editor is never cut off mid-answer.
const BACKSTOP_GRACE: Duration = Duration::from_secs(30);

/// A parked request: the mount it routed to, and the channel its axum handler is blocked
/// on. Dropping the sender (an `unmount`) resolves that handler's `await` with an error,
/// which it turns into a `503` — so a closed mount's in-flight requests answer immediately
/// instead of waiting out a timeout for a handler that can no longer run.
struct Pending {
    mount_id: u64,
    tx: oneshot::Sender<HttpServerReply>,
}

/// One live mount: which Lua callback id owns it, and the backstop for its requests.
#[derive(Clone)]
struct Route {
    /// The `btv.http.mount` callback id — what [`LoopEvent::HttpServerRequest`] carries so
    /// the Lua side can find the plugin's `on_request`.
    id: u64,
    /// How long a parked request may hold this listener's socket before it is reclaimed.
    ///
    /// **Not** `opts.timeout`. The Lua side owns that: it arms a deadline per request and
    /// answers `504` itself, so the browser build — which has no listener at all — inherits
    /// the identical contract. This is the strictly-longer backstop for the one case a Lua
    /// deadline cannot cover: the editor thread never runs it. `opts.timeout` +
    /// [`BACKSTOP_GRACE`], so a well-behaved editor always hits the plugin's own deadline
    /// first and this never fires.
    backstop: Duration,
}

/// State the axum handler shares with the actor. All of it is behind `Arc` so a **rebind**
/// (`:set httpport=9000`) can spawn a fresh `axum::serve` over the *same* routes and
/// pending requests — every mount stays live and only the origin moves.
struct Shared {
    /// Mount name (`example`) → its route. The axum handler reads it per request; the actor
    /// writes it on mount/unmount.
    routes: Mutex<HashMap<String, Route>>,
    /// In-flight requests: `req_id` → the parked handler's sender, tagged with the mount
    /// that owns it. The handler inserts; `HttpRespond` (or a timeout / unmount) removes.
    /// Tagged because `unmount` must be able to find *this mount's* requests and drop them.
    pending: Mutex<HashMap<u64, Pending>>,
    /// Allocates `req_id`s. One counter for the whole listener, so an id identifies a
    /// request across every mount and `HttpRespond` needs no mount id alongside it.
    next_req_id: AtomicU64,
    /// Inbound channel to the server thread — where a parked request sends its
    /// [`LoopEvent::HttpServerRequest`].
    event_tx: UnboundedSender<LoopEvent>,
    /// The address the listener is bound to, `None` until the first bind. The handler
    /// checks every request's `Origin` against it ([`same_origin`]); set at each bind so
    /// a rebind moves the gate with the origin.
    bound_addr: Mutex<Option<SocketAddr>>,
}

/// The actor's `btv.http.mount` state: the routes, and the listener once something has
/// mounted. Owned by the event-loop actor, which is the only writer.
pub(crate) struct HttpMounts {
    shared: Arc<Shared>,
    /// `None` until the first mount binds — the "nothing starts" invariant, and what makes
    /// [`origin`](Self::origin) `None` rather than a guess.
    bound: Option<Bound>,
}

/// A live listener: the address it resolved to, and the handle that stops its serve task.
struct Bound {
    addr: SocketAddr,
    /// Stops this listener's `axum::serve` — sent on explicitly by a rebind once the
    /// replacement is accepting, and fired implicitly by its own `Drop` when the actor goes
    /// away. Either way the graceful-shutdown future completes and the port is released.
    shutdown: oneshot::Sender<()>,
}

impl HttpMounts {
    pub(crate) fn new(event_tx: UnboundedSender<LoopEvent>) -> Self {
        HttpMounts {
            shared: Arc::new(Shared {
                routes: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashMap::new()),
                next_req_id: AtomicU64::new(1),
                event_tx,
                bound_addr: Mutex::new(None),
            }),
            bound: None,
        }
    }

    /// The origin mounts are served on (`http://127.0.0.1:53124`), or `None` when nothing
    /// has bound yet.
    fn origin(&self) -> Option<String> {
        self.bound.as_ref().map(|b| origin_of(b.addr))
    }

    /// Add a route, binding the listener first if this is the first mount. Sends the
    /// [`LoopEvent::HttpMountResult`] that settles the plugin's promise — with the bound
    /// origin, or the bind / duplicate-name error.
    pub(crate) async fn mount(
        &mut self,
        id: u64,
        name: String,
        host: &str,
        port: u16,
        timeout: Duration,
    ) {
        // A duplicate name is caught BEFORE any bind: two plugins fighting over `/plugin/x`
        // must not be resolved by silently letting the second win, and a failed mount must
        // not be the thing that opens a port.
        let duplicate = self.shared.routes.lock().unwrap().contains_key(&name);
        if duplicate {
            self.send_mount_result(
                id,
                Err(format!(
                    "btv.http.mount: {name:?} is already mounted (each mount name must be unique)"
                )),
            );
            return;
        }
        if self.bound.is_none() {
            match self.bind(host, port).await {
                Ok(bound) => self.bound = Some(bound),
                Err(e) => {
                    self.send_mount_result(id, Err(e));
                    return;
                }
            }
        }
        self.shared.routes.lock().unwrap().insert(
            name,
            Route {
                id,
                backstop: timeout + BACKSTOP_GRACE,
            },
        );
        let origin = self.origin().expect("bound above");
        self.send_mount_result(id, Ok(origin));
    }

    /// Retire the route owned by callback `id` (`mount:close()`), and drop any of its
    /// in-flight requests so their clients get a `503` now rather than waiting out the
    /// timeout for a handler that can no longer run.
    ///
    /// The listener stays bound: an idle listener costs nothing, and a stable origin
    /// survives a plugin reload.
    pub(crate) fn unmount(&mut self, id: u64) {
        self.shared.routes.lock().unwrap().retain(|_, r| r.id != id);
        // Drop this mount's parked requests: each sender's drop wakes its axum handler,
        // which answers 503 now rather than sitting out the full timeout.
        self.shared
            .pending
            .lock()
            .unwrap()
            .retain(|_, p| p.mount_id != id);
    }

    /// Complete an in-flight request with the plugin's reply. A `req_id` the map no longer
    /// holds (it timed out, or its mount closed) is dropped — the Lua side detects both
    /// first and notifies, so this is never a silent success.
    pub(crate) fn respond(&mut self, req_id: u64, reply: HttpServerReply) {
        if let Some(pending) = self.shared.pending.lock().unwrap().remove(&req_id) {
            let _ = pending.tx.send(reply);
        }
    }

    /// Move the listener onto `host:port` after an `'httphost'` / `'httpport'` write.
    ///
    /// Binds the new address **before** touching the old one and swaps only on success, so
    /// a failure leaves the editor exactly as it was — still serving, with the option free
    /// to be reverted rather than left describing a port nothing listens on. The routes and
    /// pending requests are shared, so every mount survives the move; only the origin
    /// changes.
    ///
    /// A no-op when nothing is bound: until a plugin mounts, the options are inert and are
    /// simply read at bind time.
    pub(crate) async fn rebind(&mut self, host: &str, port: u16) {
        if self.bound.is_none() {
            return;
        }
        match self.bind(host, port).await {
            Ok(bound) => {
                // Only now that the replacement is accepting do we stop the old listener,
                // so the origin is never down in between.
                let origin = origin_of(bound.addr);
                if let Some(old) = self.bound.replace(bound) {
                    let _ = old.shutdown.send(());
                }
                let _ = self.shared.event_tx.send(LoopEvent::HttpRebound {
                    origin,
                    host: host.to_string(),
                    port,
                });
            }
            Err(message) => {
                let _ = self.shared.event_tx.send(LoopEvent::HttpRebindErr {
                    message,
                    host: host.to_string(),
                    port,
                });
            }
        }
    }

    /// Bind `host:port` and spawn an `axum::serve` over the shared state. Returns the
    /// resolved address (an ephemeral `:0` is concrete by now — the whole reason the mount
    /// promise carries the origin) or a human error for the reject.
    async fn bind(&self, host: &str, port: u16) -> Result<Bound, String> {
        // Bounded: resolving a `host` *name* (not a literal address) can stall on a
        // dead resolver for seconds, and `mount`/`rebind` await this inline on the
        // shared event-loop actor — every timer and process would park behind it.
        let listener = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            TcpListener::bind((host, port)),
        )
        .await
        {
            Ok(Ok(listener)) => listener,
            Ok(Err(e)) => {
                return Err(format!(
                    "btv.http.mount: cannot bind {host}:{port}: {e} (see 'httphost' / 'httpport')"
                ))
            }
            Err(_) => {
                return Err(format!(
                "btv.http.mount: timed out binding {host}:{port} (host resolution may be stalled)"
            ))
            }
        };
        let addr = listener.local_addr().map_err(|e| {
            format!("btv.http.mount: bound {host}:{port} but cannot read it back: {e}")
        })?;
        // Publish the address BEFORE the serve task starts, so no request can arrive at a
        // listener whose origin gate is still unarmed. A rebind sets the new address
        // before the old listener stops — its in-flight window (sub-millisecond) checks
        // the new origin, which is the origin the mounts are moving to anyway.
        *self.shared.bound_addr.lock().unwrap() = Some(addr);
        let (shutdown, shutdown_rx) = oneshot::channel();
        let app = axum::Router::new()
            // One fallback rather than routes: axum never sees the mount names — the
            // handler splits `/plugin/<name>/<rest>` and looks `name` up in the live map,
            // which a plugin can change at any tick.
            .fallback(handle)
            .with_state(self.shared.clone());
        tokio::spawn(async move {
            let served = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
            if let Err(e) = served {
                // `axum::serve` handles per-connection errors internally, so reaching here
                // means the whole listener died. Never silent.
                eprintln!("bemtvi: btv.http.mount listener on {addr} stopped: {e}");
            }
        });
        Ok(Bound { addr, shutdown })
    }

    fn send_mount_result(&self, id: u64, result: Result<String, String>) {
        let _ = self
            .shared
            .event_tx
            .send(LoopEvent::HttpMountResult { id, result });
    }
}

/// `http://host:port` for `addr` — the origin string mounts hang off.
fn origin_of(addr: SocketAddr) -> String {
    format!("http://{addr}")
}

/// Is the request's declared `Origin` this listener's own? A page on a foreign site must
/// not be able to drive a plugin's mount — its handlers would be open to CSRF, and a
/// DNS-rebinding domain would defeat the loopback bind. Cross-site fetches and form
/// posts always carry `Origin`, so an `Origin` that doesn't name this machine is
/// rejected before any route lookup.
///
/// A request with **no** `Origin` passes: curl, a same-origin GET navigation, and the
/// plugin's own page all send none. That leaves the residual hole of a header-less GET
/// tag (`<img src>` on a foreign page) — it carries no `Origin` and cannot read the
/// response, so it can only trigger side effects on GET endpoints; a plugin's
/// state-changing endpoints should require POST, which always carries `Origin` here.
/// (`null`, `https://…`, and non-UTF-8 `Origin` values are never this listener's.)
fn same_origin(bound: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    // A serving listener always has an address; `None` would be a bug, so fail the gate
    // loud rather than silently serving without it.
    let Some(bound) = bound else { return false };
    let Some(origin) = headers.get("origin") else {
        return true;
    };
    let Some(origin) = origin.to_str().ok() else {
        return false;
    };
    let Some(authority) = origin.strip_prefix("http://") else {
        return false;
    };

    // Authority is `host[:port]`; split the port, keeping a bracketed IPv6 host whole
    // (`[::1]:53124`). A port that won't parse can't be verified — fail closed.
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if host.ends_with(']') || port.parse::<u16>().is_ok() => {
            (host, Some(port))
        }
        _ => (authority, None),
    };
    match port.and_then(|p| p.parse::<u16>().ok()) {
        // A port-less authority is the default port 80 — never this listener, which
        // binds a high ephemeral port, so nothing to match.
        Some(port) if port != bound.port() => return false,
        None if bound.port() != 80 => return false,
        _ => {}
    }
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        // Loopback aliases (127.0.0.0/8, `::1`) name pages this listener could have
        // served; a deliberately off-loopback bind additionally trusts its own exact
        // address. Any other IP — including an attacker's domain resolving to us — is a
        // foreign page.
        ip.is_loopback() || (!bound.ip().is_loopback() && ip == bound.ip())
    } else {
        host.eq_ignore_ascii_case("localhost")
    }
}

/// The single axum handler behind every mount: route by name, park on the plugin's answer.
async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();

    // The origin gate runs before ANY route work — a foreign page gets a 403 whether the
    // path is a live mount, a 404, or the root redirect, so a cross-site probe can't even
    // learn which mount names exist.
    if !same_origin(*shared.bound_addr.lock().unwrap(), &parts.headers) {
        return text_response(
            StatusCode::FORBIDDEN,
            "bemtvi: request is not same-origin with the mount listener\n",
        );
    }

    let path = parts.uri.path().to_string();

    // Split `/plugin/<name>/<rest>`. Everything outside the reserved prefix, and every
    // unmounted name, is a 404 the editor answers itself — Lua is never entered, so an
    // unmounted path costs nothing.
    // The prefix split lives in `bemtvi-lua` beside the request type, shared with the
    // browser's Service Worker path — a plugin's `req.path` must mean the same thing in
    // both worlds, so the rules have exactly one home.
    let Some((name, _)) = bemtvi_lua::split_mount_path(&path) else {
        return text_response(StatusCode::NOT_FOUND, "bemtvi: no plugin mount here\n");
    };
    let Some(route) = shared.routes.lock().unwrap().get(name).cloned() else {
        return text_response(
            StatusCode::NOT_FOUND,
            &format!("bemtvi: no plugin mounted at /plugin/{name}\n"),
        );
    };

    // A live mount's bare root (`/plugin/<name>` with no trailing slash) redirects to the
    // slash form, so a page served there resolves its relative URLs against the mount rather
    // than against `/plugin/`. The query rides along. (Shared with the browser path.)
    if let Some(location) = bemtvi_lua::mount_root_redirect(&path) {
        let location = match parts.uri.query() {
            Some(q) => format!("{location}?{q}"),
            None => location,
        };
        return Response::builder()
            .status(StatusCode::PERMANENT_REDIRECT)
            .header("location", location)
            .body(axum::body::Body::empty())
            .expect("a redirect response is always well-formed");
    }

    // Buffer the body — a mount handler reads it as one Lua string. Bounded: an unbounded
    // upload must not be able to make the editor allocate without limit.
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("bemtvi: request body exceeds {MAX_BODY} bytes\n"),
            )
        }
    };

    let headers = parts
        .headers
        .iter()
        .map(|(name, value)| {
            (
                name.as_str().to_string(),
                // A non-UTF-8 header value is lossy rather than fatal: dropping the request
                // over one odd header would be worse than a mangled value the handler can
                // ignore.
                String::from_utf8_lossy(value.as_bytes()).into_owned(),
            )
        })
        .collect();
    let Some(server_request) = bemtvi_lua::build_server_request(
        parts.method.as_str(),
        &path,
        parts.uri.query(),
        headers,
        body.to_vec(),
    ) else {
        // Unreachable — `split_mount_path` above already accepted this path — but a
        // silent unwrap here would be a panic in the listener rather than a 404.
        return text_response(StatusCode::NOT_FOUND, "bemtvi: no plugin mount here\n");
    };

    // Park: hand the request to the server thread and await the plugin's `respond`.
    let req_id = shared.next_req_id.fetch_add(1, Ordering::Relaxed);
    let (tx, rx) = oneshot::channel();
    shared.pending.lock().unwrap().insert(
        req_id,
        Pending {
            mount_id: route.id,
            tx,
        },
    );
    if shared
        .event_tx
        .send(LoopEvent::HttpServerRequest {
            id: route.id,
            req_id,
            request: server_request,
        })
        .is_err()
    {
        shared.pending.lock().unwrap().remove(&req_id);
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "bemtvi: the editor is shutting down\n",
        );
    }

    match tokio::time::timeout(route.backstop, rx).await {
        Ok(Ok(reply)) => build_response(reply),
        // The sender was dropped without a reply: the mount closed (or the editor is going
        // away) while this request was in flight.
        Ok(Err(_)) => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "bemtvi: the plugin mount closed while handling this request\n",
        ),
        Err(_) => {
            // The BACKSTOP fired, which means the editor never even ran this mount's own
            // deadline — that would have answered 504 at `opts.timeout` long before now.
            // Reclaim the socket. Nothing to tell Lua: if it is this stuck it cannot listen,
            // and its slot is cleaned up by its own deadline whenever it runs again.
            shared.pending.lock().unwrap().remove(&req_id);
            text_response(
                StatusCode::GATEWAY_TIMEOUT,
                "bemtvi: the editor never answered this request (its tick is not running)\n",
            )
        }
    }
}

/// Turn a plugin's reply into an axum response. An unusable status or header is dropped
/// with the rest of the reply intact rather than failing the whole request — the Lua side
/// has already range-checked the status, so reaching those arms means something exotic.
fn build_response(reply: HttpServerReply) -> Response {
    let status = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = Response::builder().status(status);
    for (name, value) in &reply.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::try_from(name.as_str()),
            HeaderValue::try_from(value.as_str()),
        ) {
            response = response.header(name, value);
        }
    }
    response
        .body(axum::body::Body::from(Bytes::from(reply.body)))
        .unwrap_or_else(|e| {
            text_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("bemtvi: could not build the mount's response: {e}\n"),
            )
        })
}

/// A `text/plain` response — the editor's own answers (404 / 503 / 504), never a plugin's.
fn text_response(status: StatusCode, body: &str) -> Response {
    Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .body(axum::body::Body::from(body.to_string()))
        .expect("a static text response is always well-formed")
}
