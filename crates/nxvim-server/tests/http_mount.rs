//! Behavior tests for `nx.http.mount` — the plugin HTTP *server* surface (one
//! editor-owned origin, a subroute per plugin at `/plugin/<name>/*`). Black-box per the
//! project conventions: a real server over RPC, driven with `nvim_exec_lua`, and — the
//! point of the feature — hit with a **real HTTP client** on the bound port. A passing
//! assertion here means the whole round-trip works: axum accepted, the actor parked, the
//! plugin's Lua handler ran on the editor thread, and its `respond` unparked the socket.
//!
//! Off-tick observation: mounting binds on the event-loop actor and settles the promise a
//! tick later, so each test polls a `_G.*` marker its `:next` continuation sets
//! (`await_truthy`) rather than sleeping a fixed amount — which would flake under the
//! parallel load of `cargo test --workspace`.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, start_attached};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Poll `return (<expr>) and true or false` until truthy (the off-tick mount settled and
/// its continuation ran), or the budget runs out.
async fn await_truthy(rpc: &Rpc, expr: &str) -> bool {
    let code = format!("return ({expr}) and true or false");
    for _ in 0..200 {
        if lua_bool(rpc, &code).await == Some(true) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// `return`-style Lua chunk evaluated for a string.
async fn lua_string(rpc: &Rpc, code: &str) -> Option<String> {
    match exec_lua(rpc, code).await {
        Value::String(s) => s.into_str(),
        _ => None,
    }
}

/// Mount `name` with the given `on_request` body and wait for the promise to settle.
/// Returns the mount's base URL (`http://127.0.0.1:53124/plugin/<name>/`).
async fn mount(rpc: &Rpc, name: &str, on_request: &str) -> String {
    let marker = format!("_G.mounted_{name}");
    exec_lua(
        rpc,
        &format!(
            r#"
            nx.http.mount({{
              name = "{name}",
              on_request = {on_request},
            }}):next(function(m)
              _G.mount_{name} = m
              {marker} = m:url()
            end, function(err)
              {marker} = "ERROR: " .. tostring(err.message)
            end)
            "#
        ),
    )
    .await;
    assert!(
        await_truthy(rpc, &marker).await,
        "the {name} mount never settled"
    );
    let url = lua_string(rpc, &format!("return {marker}")).await.unwrap();
    assert!(
        !url.starts_with("ERROR:"),
        "mounting {name} rejected: {url}"
    );
    url
}

/// A minimal blocking HTTP/1.1 client: send `method` + `path` (+ optional body) to `addr`
/// and return the raw response text. Deliberately hand-rolled and dependency-free — the
/// point is to exercise the listener from *outside* the editor, over a real socket.
fn http_request(url: &str, method: &str, path_suffix: &str, body: Option<&str>) -> String {
    let (host_port, base_path) = split_url(url);
    let addr = host_port
        .to_socket_addrs()
        .expect("resolve")
        .next()
        .expect("one addr");
    let mut stream = TcpStream::connect(addr).expect("connect to the mount listener");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let path = format!("{base_path}{path_suffix}");
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n");
    if let Some(body) = body {
        req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    req.push_str("\r\n");
    if let Some(body) = body {
        req.push_str(body);
    }
    stream.write_all(req.as_bytes()).expect("write request");
    let mut response = Vec::new();
    // `Connection: close` means EOF ends the body — no chunk/length parsing needed.
    stream.read_to_end(&mut response).expect("read response");
    String::from_utf8_lossy(&response).into_owned()
}

/// Split `http://127.0.0.1:53124/plugin/x/` into (`127.0.0.1:53124`, `/plugin/x/`).
fn split_url(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").expect("an http:// url");
    match rest.find('/') {
        Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
        None => (rest.to_string(), "/".to_string()),
    }
}

fn get(url: &str) -> String {
    http_request(url, "GET", "", None)
}

/// The status line's code (`200` from `HTTP/1.1 200 OK`).
fn status_of(response: &str) -> u16 {
    response
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

/// The body — everything past the blank line.
fn body_of(response: &str) -> String {
    response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_string())
        .unwrap_or_default()
}

/// Nothing binds until a plugin mounts — the invariant that keeps a config with no HTTP
/// plugin byte-for-byte the editor it was. Asserted through the public surface:
/// `nx.http.origin()` is nil, so there is no origin to connect to.
#[tokio::test]
async fn no_listener_until_something_mounts() {
    let (rpc, _incoming) = start().await;
    assert_eq!(
        exec_lua(&rpc, "return nx.http.origin() == nil").await,
        Value::Boolean(true),
        "an unmounted session must report no origin — nothing should have bound"
    );
    // …and setting the options still binds nothing: they are inert until a mount.
    exec_lua(&rpc, "nx.o.httpport = 0; nx.o.httphost = '127.0.0.1'").await;
    assert_eq!(
        exec_lua(&rpc, "return nx.http.origin() == nil").await,
        Value::Boolean(true),
        "setting 'httpport' must not bind anything on its own"
    );
}

/// The headline path: mount, get a real URL back with the ephemeral port resolved, and
/// serve a real GET over a real socket.
#[tokio::test]
async fn mount_serves_a_real_get() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "hello",
        r#"function(req, respond)
             respond({ body = "hi from lua" })
           end"#,
    )
    .await;

    assert!(
        url.starts_with("http://127.0.0.1:"),
        "expected a loopback url with a resolved port, got {url}"
    );
    assert!(
        url.ends_with("/plugin/hello/"),
        "unexpected mount url: {url}"
    );

    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 200);
    assert_eq!(body_of(&response), "hi from lua");

    // The public origin now reports the live listener rather than nil.
    let origin = lua_string(&rpc, "return nx.http.origin()").await.unwrap();
    assert!(origin.starts_with("http://127.0.0.1:"), "origin: {origin}");
}

/// Everything the handler reads off `req` survives the trip: method, the MOUNT-RELATIVE
/// path, decoded query params, headers, and a POST body.
#[tokio::test]
async fn request_fidelity() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "echo",
        r#"function(req, respond)
             _G.seen = {
               method = req.method,
               path = req.path,
               raw_path = req.raw_path,
               name = req.name,
               q = req.query.v,
               ct = req.headers["content-type"],
               body = req.body,
             }
             respond({ body = "ok" })
           end"#,
    )
    .await;

    let u = url.clone();
    let response = tokio::task::spawn_blocking(move || {
        // Trailing-slashed mount url + "sub/page.css?v=2" → /plugin/echo/sub/page.css?v=2
        let mut s =
            TcpStream::connect(split_url(&u).0.to_socket_addrs().unwrap().next().unwrap()).unwrap();
        let body = "hello=world";
        let req = format!(
            "POST /plugin/echo/sub/page.css?v=2 HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\
             Content-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        String::from_utf8_lossy(&out).into_owned()
    })
    .await
    .unwrap();
    assert_eq!(status_of(&response), 200);

    assert_eq!(
        lua_string(&rpc, "return _G.seen.method").await.as_deref(),
        Some("POST")
    );
    // Mount-relative: the handler never sees its own /plugin/echo prefix.
    assert_eq!(
        lua_string(&rpc, "return _G.seen.path").await.as_deref(),
        Some("/sub/page.css")
    );
    assert_eq!(
        lua_string(&rpc, "return _G.seen.raw_path").await.as_deref(),
        Some("/plugin/echo/sub/page.css")
    );
    assert_eq!(
        lua_string(&rpc, "return _G.seen.name").await.as_deref(),
        Some("echo")
    );
    assert_eq!(
        lua_string(&rpc, "return _G.seen.q").await.as_deref(),
        Some("2")
    );
    assert_eq!(
        lua_string(&rpc, "return _G.seen.ct").await.as_deref(),
        Some("text/plain")
    );
    assert_eq!(
        lua_string(&rpc, "return _G.seen.body").await.as_deref(),
        Some("hello=world")
    );
}

/// A bare `/plugin/<name>` (no trailing slash) normalizes to `path == "/"`, so a handler's
/// root check works with or without the slash.
#[tokio::test]
async fn bare_mount_path_normalizes_to_root() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "root",
        r#"function(req, respond) respond({ body = req.path }) end"#,
    )
    .await;
    let base = url.trim_end_matches('/').to_string(); // …/plugin/root  (no trailing slash)
    let response = tokio::task::spawn_blocking(move || get(&base))
        .await
        .unwrap();
    assert_eq!(body_of(&response), "/");
}

/// Two plugins on one origin route independently by name — the core of the mount model.
#[tokio::test]
async fn two_mounts_route_independently() {
    let (rpc, _incoming) = start().await;
    let a = mount(
        &rpc,
        "alpha",
        r#"function(req, respond) respond({ body = "I am alpha" }) end"#,
    )
    .await;
    let b = mount(
        &rpc,
        "beta",
        r#"function(req, respond) respond({ body = "I am beta" }) end"#,
    )
    .await;

    // One listener: both mounts share an origin (and so, deliberately, one security
    // boundary — see the plan).
    assert_eq!(split_url(&a).0, split_url(&b).0);

    let (ra, rb) = tokio::task::spawn_blocking(move || (get(&a), get(&b)))
        .await
        .unwrap();
    assert_eq!(body_of(&ra), "I am alpha");
    assert_eq!(body_of(&rb), "I am beta");
}

/// `respond` may be called on a LATER tick — the reason it is a callback rather than a
/// return value. A handler that awaits (here, a timer) still answers.
#[tokio::test]
async fn async_handler_responds_later() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "slow",
        r#"function(req, respond)
             nx.timer(function() respond({ body = "answered later" }) end, 30)
           end"#,
    )
    .await;
    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 200);
    assert_eq!(body_of(&response), "answered later");
}

/// A custom status and headers reach the client verbatim.
#[tokio::test]
async fn status_and_headers_reach_the_client() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "teapot",
        r#"function(req, respond)
             respond({
               status = 418,
               headers = { ["content-type"] = "text/plain", ["x-nx"] = "mounted" },
               body = "short and stout",
             })
           end"#,
    )
    .await;
    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 418);
    let lower = response.to_lowercase();
    assert!(
        lower.contains("x-nx: mounted"),
        "missing header:\n{response}"
    );
    assert!(
        lower.contains("content-type: text/plain"),
        "ct:\n{response}"
    );
    assert_eq!(body_of(&response), "short and stout");
}

/// An unmounted name is a 404 the editor answers itself — Lua is never entered.
#[tokio::test]
async fn unmounted_path_is_404() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "present",
        r#"function(req, respond) respond({ body = "here" }) end"#,
    )
    .await;
    let origin = split_url(&url).0;
    let absent = format!("http://{origin}/plugin/absent/");
    let outside = format!("http://{origin}/not-a-mount");
    let (r1, r2) = tokio::task::spawn_blocking(move || (get(&absent), get(&outside)))
        .await
        .unwrap();
    assert_eq!(status_of(&r1), 404, "an unmounted name must 404");
    assert_eq!(status_of(&r2), 404, "a path outside /plugin/ must 404");
}

/// `mount:close()` retires the route; the origin stays up (an idle listener is free and a
/// stable URL survives a plugin reload) but the closed name starts 404ing.
#[tokio::test]
async fn close_retires_the_route() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "transient",
        r#"function(req, respond) respond({ body = "still here" }) end"#,
    )
    .await;
    let before = tokio::task::spawn_blocking({
        let url = url.clone();
        move || get(&url)
    })
    .await
    .unwrap();
    assert_eq!(status_of(&before), 200);

    exec_lua(&rpc, "_G.mount_transient:close()").await;
    assert_eq!(
        exec_lua(&rpc, "return _G.mount_transient:is_open()").await,
        Value::Boolean(false)
    );
    // Idempotent — a second close must not error.
    exec_lua(&rpc, "_G.mount_transient:close()").await;

    let after = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(
        status_of(&after),
        404,
        "a closed mount must stop answering:\n{after}"
    );
}

/// A duplicate name REJECTS rather than silently letting the second mount win.
#[tokio::test]
async fn duplicate_name_rejects() {
    let (rpc, _incoming) = start().await;
    mount(
        &rpc,
        "dup",
        r#"function(req, respond) respond({ body = "first" }) end"#,
    )
    .await;
    exec_lua(
        &rpc,
        r#"
        nx.http.mount({
          name = "dup",
          on_request = function(req, respond) respond({ body = "second" }) end,
        }):next(function() _G.dup2 = "RESOLVED" end, function(err) _G.dup2 = err.message end)
        "#,
    )
    .await;
    assert!(
        await_truthy(&rpc, "_G.dup2").await,
        "the duplicate never settled"
    );
    let msg = lua_string(&rpc, "return _G.dup2").await.unwrap();
    assert!(
        msg.contains("already mounted"),
        "a duplicate name must reject loudly, got: {msg}"
    );
}

/// A malformed `name` is rejected at the Lua boundary, before any op is queued.
#[tokio::test]
async fn bad_name_errors() {
    let (rpc, _incoming) = start().await;
    let err = lua_string(
        &rpc,
        r#"
        local ok, err = pcall(function()
          nx.http.mount({ name = "has/slash", on_request = function() end })
        end)
        return tostring(err)
        "#,
    )
    .await
    .unwrap();
    assert!(
        err.contains("name"),
        "expected a name complaint, got: {err}"
    );
}

/// A handler that throws answers 500 — never a hung socket, and never a silent success.
#[tokio::test]
async fn throwing_handler_is_500() {
    let (rpc, _incoming) = start().await;
    let url = mount(
        &rpc,
        "boom",
        r#"function(req, respond) error("handler blew up") end"#,
    )
    .await;
    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 500);
}

/// A handler that never responds yields a 504 at `opts.timeout` and reclaims the slot,
/// rather than leaking a parked request forever.
#[tokio::test]
async fn silent_handler_times_out() {
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        r#"
        nx.http.mount({
          name = "silent",
          timeout = 150,                     -- ms; short so the test isn't slow
          on_request = function(req, respond) end,   -- never responds
        }):next(function(m) _G.silent_url = m:url() end)
        "#,
    )
    .await;
    assert!(
        await_truthy(&rpc, "_G.silent_url").await,
        "mount never settled"
    );
    let url = lua_string(&rpc, "return _G.silent_url").await.unwrap();
    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 504);
}

/// `'httpport'` is honored — the user's option decides the port, not the plugin.
#[tokio::test]
async fn httpport_option_is_honored() {
    let (rpc, _incoming) = start().await;
    // Ask the OS for a free port, then release it: a fixed literal would flake if the port
    // happened to be taken on the test machine.
    let port = free_port();
    exec_lua(&rpc, &format!("nx.o.httpport = {port}")).await;
    let url = mount(
        &rpc,
        "fixed",
        r#"function(req, respond) respond({ body = "on my port" }) end"#,
    )
    .await;
    assert_eq!(
        split_url(&url).0,
        format!("127.0.0.1:{port}"),
        "the listener must bind the port 'httpport' asked for"
    );
    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(body_of(&response), "on my port");
}

/// Setting `'httpport'` while mounts are live REBINDS: every mount stays live on the new
/// origin, and the old address stops answering. A silent no-op would be a lie — the
/// listener lives for the session, so there is no "next bind" to take effect at.
#[tokio::test]
async fn rebind_moves_live_mounts() {
    let (rpc, _incoming) = start().await;
    let old_url = mount(
        &rpc,
        "movable",
        r#"function(req, respond) respond({ body = "moved with you" }) end"#,
    )
    .await;
    let old_origin = split_url(&old_url).0;

    let port = free_port();
    exec_lua(&rpc, &format!("nx.o.httpport = {port}")).await;
    // The rebind is off-tick: wait for the mount's url to report the new port.
    assert!(
        await_truthy(
            &rpc,
            &format!("_G.mount_movable:url():find(':{port}/', 1, true)")
        )
        .await,
        "the live mount's url never moved to the new port"
    );

    let new_url = lua_string(&rpc, "return _G.mount_movable:url()")
        .await
        .unwrap();
    assert_eq!(split_url(&new_url).0, format!("127.0.0.1:{port}"));

    // The mount still serves — on the new origin.
    let response = tokio::task::spawn_blocking(move || get(&new_url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 200);
    assert_eq!(body_of(&response), "moved with you");

    // …and the old address is gone (the listener was actually torn down, not leaked).
    let old = format!("http://{old_origin}/plugin/movable/");
    let dead = tokio::task::spawn_blocking(move || {
        TcpStream::connect_timeout(
            &old.strip_prefix("http://")
                .unwrap()
                .split('/')
                .next()
                .unwrap()
                .to_socket_addrs()
                .unwrap()
                .next()
                .unwrap(),
            Duration::from_millis(500),
        )
        .is_err()
    })
    .await
    .unwrap();
    assert!(dead, "the old listener must be torn down after a rebind");
}

/// A rebind onto a TAKEN port fails without moving anything: the old listener keeps
/// serving AND the option is reverted to the value that is actually bound, so
/// `nx.o.httpport` can never disagree with reality.
///
/// Mounts on a CONCRETE port first (not the ephemeral default) so the revert has something
/// meaningful to restore: reverting `0` would prove nothing, since `0` makes no claim about
/// which port is bound.
#[tokio::test]
async fn failed_rebind_reverts_the_option() {
    let (rpc, _incoming) = start().await;
    let good = free_port();
    exec_lua(&rpc, &format!("nx.o.httpport = {good}")).await;
    let url = mount(
        &rpc,
        "stayput",
        r#"function(req, respond) respond({ body = "unmoved" }) end"#,
    )
    .await;
    assert_eq!(split_url(&url).0, format!("127.0.0.1:{good}"));

    // Hold a port so the rebind cannot have it.
    let blocker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = blocker.local_addr().unwrap().port();

    exec_lua(&rpc, &format!("nx.o.httpport = {taken}")).await;
    // The failure is off-tick; wait for the option to snap back to the port being served.
    assert!(
        await_truthy(&rpc, &format!("nx.o.httpport == {good}")).await,
        "a failed rebind must revert 'httpport' to the port actually being served"
    );

    // The mount is untouched and still serving on the original origin.
    let response = tokio::task::spawn_blocking(move || get(&url))
        .await
        .unwrap();
    assert_eq!(status_of(&response), 200);
    assert_eq!(body_of(&response), "unmoved");
    drop(blocker);
}

/// Ask the OS for a free port and release it. Racy in principle, fine in practice: the
/// window is microseconds and the alternative (a hard-coded port) flakes for real.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}
