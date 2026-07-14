//! The `nx.http.mount` listener: **one** editor-owned HTTP origin serving every plugin's
//! mounted subroute at `/plugin/<name>/*`.
//!
//! The inbound twin of [`crate::http`], which is outbound-only (`ureq`). Where a plugin's
//! `nx.http.fetch` *calls* the network, a plugin's `nx.http.mount` *answers* it — enough to
//! serve a live markdown renderer or any browser-facing UI a plugin wants to own. See
//! `docs/plans/2026-07-14-nx-http-mount.md`.
//!
//! **One origin, not a port per plugin.** A browser tab cannot bind a TCP port, so a
//! per-plugin-port API would be native-only by construction and plugin code would not port
//! to the web build. A mounted subroute is something a Service Worker satisfies exactly as
//! well as this `TcpListener` does, so the same Lua runs in every world. It also removes
//! port collisions between two nxvim instances and plugins hard-coding 8080.
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
//! socket: the mount closed (`503`), the handler threw (`500`, sent by the Lua side), or it
//! never responded (`504` at `opts.timeout`, which also notifies — see
//! [`LoopEvent::HttpServerTimeout`]).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::response::Response;
use nxvim_lua::{HttpServerReply, HttpServerRequest};
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

/// The path prefix reserved for plugin mounts. Everything under it routes by mount name;
/// the editor keeps `/` and any future `/_nx/*` for itself.
const MOUNT_PREFIX: &str = "/plugin/";

/// A parked request: the mount it routed to, and the channel its axum handler is blocked
/// on. Dropping the sender (an `unmount`) resolves that handler's `await` with an error,
/// which it turns into a `503` — so a closed mount's in-flight requests answer immediately
/// instead of waiting out a timeout for a handler that can no longer run.
struct Pending {
    mount_id: u64,
    tx: oneshot::Sender<HttpServerReply>,
}

/// One live mount: which Lua callback id owns it, and how long its handler may take.
#[derive(Clone)]
struct Route {
    /// The `nx.http.mount` callback id — what [`LoopEvent::HttpServerRequest`] carries so
    /// the Lua side can find the plugin's `on_request`.
    id: u64,
    /// `opts.timeout` — how long this mount's handler may leave a request unanswered.
    timeout: Duration,
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
}

/// The actor's `nx.http.mount` state: the routes, and the listener once something has
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
                    "nx.http.mount: {name:?} is already mounted (each mount name must be unique)"
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
        self.shared
            .routes
            .lock()
            .unwrap()
            .insert(name, Route { id, timeout });
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
        let listener = TcpListener::bind((host, port)).await.map_err(|e| {
            format!("nx.http.mount: cannot bind {host}:{port}: {e} (see 'httphost' / 'httpport')")
        })?;
        let addr = listener.local_addr().map_err(|e| {
            format!("nx.http.mount: bound {host}:{port} but cannot read it back: {e}")
        })?;
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
                eprintln!("nxvim: nx.http.mount listener on {addr} stopped: {e}");
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

/// The single axum handler behind every mount: route by name, park on the plugin's answer.
async fn handle(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = parts.uri.path().to_string();

    // Split `/plugin/<name>/<rest>`. Everything outside the reserved prefix, and every
    // unmounted name, is a 404 the editor answers itself — Lua is never entered, so an
    // unmounted path costs nothing.
    let Some(rest) = path.strip_prefix(MOUNT_PREFIX) else {
        return text_response(StatusCode::NOT_FOUND, "nxvim: no plugin mount here\n");
    };
    let (name, rel) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        // A bare `/plugin/example` is the mount root: normalize to "/" so a handler's
        // `req.path == "/"` check works with or without the trailing slash.
        None => (rest, "/"),
    };
    let Some(route) = shared.routes.lock().unwrap().get(name).cloned() else {
        return text_response(
            StatusCode::NOT_FOUND,
            &format!("nxvim: no plugin mounted at /plugin/{name}\n"),
        );
    };

    // Buffer the body — a mount handler reads it as one Lua string. Bounded: an unbounded
    // upload must not be able to make the editor allocate without limit.
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => {
            return text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!("nxvim: request body exceeds {MAX_BODY} bytes\n"),
            )
        }
    };

    let server_request = HttpServerRequest {
        name: name.to_string(),
        method: parts.method.as_str().to_uppercase(),
        path: rel.to_string(),
        raw_path: path.clone(),
        query: parts
            .uri
            .query()
            .map(|q| form_urlencoded::parse(q.as_bytes()).into_owned().collect())
            .unwrap_or_default(),
        headers: parts
            .headers
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_ascii_lowercase(),
                    // A non-UTF-8 header value is lossy rather than fatal: dropping the
                    // request over one odd header would be worse than a mangled value the
                    // handler can ignore.
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: body.to_vec(),
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
            "nxvim: the editor is shutting down\n",
        );
    }

    match tokio::time::timeout(route.timeout, rx).await {
        Ok(Ok(reply)) => build_response(reply),
        // The sender was dropped without a reply: the mount closed (or the editor is going
        // away) while this request was in flight.
        Ok(Err(_)) => text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "nxvim: the plugin mount closed while handling this request\n",
        ),
        Err(_) => {
            // The handler never responded. Reclaim the slot and tell the Lua side, which
            // notifies — a silent 504 would leave a plugin author nothing to debug from.
            shared.pending.lock().unwrap().remove(&req_id);
            let _ = shared
                .event_tx
                .send(LoopEvent::HttpServerTimeout { req_id });
            text_response(
                StatusCode::GATEWAY_TIMEOUT,
                "nxvim: the plugin mount did not respond in time\n",
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
                &format!("nxvim: could not build the mount's response: {e}\n"),
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
