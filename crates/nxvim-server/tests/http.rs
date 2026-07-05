//! Behavior tests for `nx.http` — the promise-always, `fetch`-modeled HTTP client
//! (native path: `ureq` on the event-loop actor's blocking pool). Black-box per the
//! project conventions — a real server over RPC, driven with `nvim_exec_lua`,
//! asserting on observable Lua state. Each test spins a tiny throwaway HTTP server on
//! a loopback port (no network, no external dependency) and fetches it.
//!
//! Off-tick observation: the request runs off the editor tick (a queued `LoopOp` on the
//! actor), settling on a later tick, so each test polls a `_G.*` marker its `:next` /
//! `nx.async` continuation sets (`await_truthy`) rather than a fixed sleep — which would
//! flake against a fast loopback round-trip under parallel test load.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, lua_bool, lua_u64, start_attached};
use tokio::sync::mpsc::UnboundedReceiver;

async fn start() -> (Rpc, UnboundedReceiver<Incoming>) {
    start_attached(ServerInit::default(), 80, 24).await
}

/// Poll `return (<expr>) and true or false` until it's truthy (the off-tick fetch settled
/// and its continuation ran), or the budget runs out. A bounded retry beats a fixed sleep,
/// which flakes under the parallel load of `cargo test --workspace` (a network round-trip +
/// the off-tick settle can outrun any fixed wait).
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

/// Read one HTTP request off `stream`: the head (up to the blank line), then the body
/// its `Content-Length` announces (which may arrive in a later TCP segment). Good enough
/// for the fixed, small requests these tests send.
fn read_request(stream: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    // First, read through the end of the headers.
    let header_end = loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return String::from_utf8_lossy(&buf).into_owned();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    // Then read exactly the announced Content-Length of body (bytes already buffered
    // past `header_end` count toward it).
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            (name.trim().eq_ignore_ascii_case("content-length"))
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() - header_end < content_length {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Write one HTTP/1.1 response with an explicit `Content-Length` and `Connection: close`.
fn write_response(stream: &mut TcpStream, status_line: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// Spawn a throwaway HTTP server on a loopback port; returns its base URL
/// (`http://127.0.0.1:PORT`). It serves a fixed route table for `accepts` connections
/// then exits: `/hello` (200 text), `/data` (200 JSON), `/missing` (404), `/echo`
/// (echoes the request body back).
fn spawn_test_server(accepts: usize) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    thread::spawn(move || {
        for _ in 0..accepts {
            let Ok((mut stream, _)) = listener.accept() else {
                break;
            };
            let req = read_request(&mut stream);
            // The request-target (path + `?query`); `route` is the path before `?`.
            let target = req
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            let route = target.split('?').next().unwrap_or("/");
            match route {
                "/hello" => write_response(&mut stream, "200 OK", "text/plain", b"hello world"),
                "/data" => write_response(
                    &mut stream,
                    "200 OK",
                    "application/json",
                    br#"{"name":"nx","count":3}"#,
                ),
                "/missing" => write_response(&mut stream, "404 Not Found", "text/plain", b"nope"),
                // A 302 pointing at /hello (relative Location, resolved against the base).
                "/redirect" => {
                    let head = "HTTP/1.1 302 Found\r\nLocation: /hello\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.flush();
                }
                // Echo the full request-target back, so a test can read the query string
                // the client built + encoded.
                "/target" => write_response(&mut stream, "200 OK", "text/plain", target.as_bytes()),
                "/echo" => {
                    // The body follows the header terminator; echo whatever we got.
                    let body = req
                        .split_once("\r\n\r\n")
                        .map(|(_, b)| b.to_string())
                        .unwrap_or_default();
                    write_response(&mut stream, "200 OK", "text/plain", body.as_bytes());
                }
                _ => write_response(&mut stream, "200 OK", "text/plain", b"ok"),
            }
        }
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn fetch_resolves_with_a_2xx_response() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "_G.res = nil\n\
             nx.http.fetch('{base}/hello'):next(function(r) _G.res = r end)"
        ),
    )
    .await;
    // The request runs off-tick (a queued LoopOp on the actor), settling on a later tick;
    // poll until it resolves (a fixed barrier would race a fast loopback round-trip).
    assert!(await_truthy(&rpc, "_G.res").await, "fetch never settled");
    assert_eq!(lua_u64(&rpc, "return _G.res.status").await, Some(200));
    assert_eq!(lua_bool(&rpc, "return _G.res.ok").await, Some(true));
    assert_eq!(
        exec_lua(&rpc, "return _G.res:text()").await.as_str(),
        Some("hello world")
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.res.headers['content-type']")
            .await
            .as_str(),
        Some("text/plain")
    );
}

#[tokio::test]
async fn fetch_json_decodes_the_body() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "_G.name = nil\n\
             _G.count = nil\n\
             nx.async(function()\n\
               local r = nx.await(nx.http.fetch('{base}/data'))\n\
               local j = r:json()\n\
               _G.name = j.name\n\
               _G.count = j.count\n\
             end)()"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.name").await, "fetch never settled");
    assert_eq!(exec_lua(&rpc, "return _G.name").await.as_str(), Some("nx"));
    assert_eq!(lua_u64(&rpc, "return _G.count").await, Some(3));
}

#[tokio::test]
async fn fetch_resolves_a_404_with_ok_false() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // Fetch semantics: a 404 RESOLVES (ok == false), it does not reject.
    exec_lua(
        &rpc,
        &format!(
            "_G.status = nil\n\
             _G.ok = nil\n\
             _G.rejected = false\n\
             nx.http.fetch('{base}/missing')\n\
               :next(function(r) _G.status = r.status; _G.ok = r.ok end)\n\
               :catch(function() _G.rejected = true end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.status").await, "fetch never settled");
    assert_eq!(lua_u64(&rpc, "return _G.status").await, Some(404));
    assert_eq!(lua_bool(&rpc, "return _G.ok").await, Some(false));
    assert_eq!(lua_bool(&rpc, "return _G.rejected").await, Some(false));
}

#[tokio::test]
async fn post_sends_a_json_body() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // A non-string body is JSON-encoded and echoed back verbatim by /echo.
    exec_lua(
        &rpc,
        &format!(
            "_G.body = nil\n\
             nx.http.fetch('{base}/echo', {{ method = 'POST', body = {{ hi = 'there' }} }})\n\
               :next(function(r) _G.body = r:text() end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.body").await, "fetch never settled");
    assert_eq!(
        exec_lua(&rpc, "return _G.body").await.as_str(),
        Some(r#"{"hi":"there"}"#)
    );
}

#[tokio::test]
async fn fetch_rejects_on_a_transport_failure() {
    let (rpc, _incoming) = start().await;
    // Nothing is listening on this port → a connect failure → the promise REJECTS with
    // a { message } table (not a resolved response). Port 1 is privileged/unused.
    exec_lua(
        &rpc,
        "_G.err = nil\n\
         _G.resolved = false\n\
         nx.http.fetch('http://127.0.0.1:1/nope', { timeout = 1500 })\n\
           :next(function() _G.resolved = true end)\n\
           :catch(function(e) _G.err = e.message end)",
    )
    .await;
    assert!(await_truthy(&rpc, "_G.err").await, "fetch never rejected");
    assert_eq!(lua_bool(&rpc, "return _G.resolved").await, Some(false));
    assert_eq!(
        lua_bool(&rpc, "return type(_G.err) == 'string' and #_G.err > 0").await,
        Some(true)
    );
}

#[tokio::test]
async fn redirect_follow_is_the_default() {
    // Two accepts: the 302, then the followed GET /hello.
    let base = spawn_test_server(2);
    let (rpc, _incoming) = start().await;
    exec_lua(
        &rpc,
        &format!(
            "_G.res = nil\n\
             nx.http.fetch('{base}/redirect'):next(function(r) _G.res = r end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.res").await, "fetch never settled");
    // Followed the redirect through to the 200 body.
    assert_eq!(lua_u64(&rpc, "return _G.res.status").await, Some(200));
    assert_eq!(
        exec_lua(&rpc, "return _G.res:text()").await.as_str(),
        Some("hello world")
    );
}

#[tokio::test]
async fn redirect_manual_returns_the_3xx() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // `redirect = "manual"` resolves with the 302 itself (ok=false), not the target.
    exec_lua(
        &rpc,
        &format!(
            "_G.res = nil\n\
             nx.http.fetch('{base}/redirect', {{ redirect = 'manual' }})\n\
               :next(function(r) _G.res = r end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.res").await, "fetch never settled");
    assert_eq!(lua_u64(&rpc, "return _G.res.status").await, Some(302));
    assert_eq!(lua_bool(&rpc, "return _G.res.ok").await, Some(false));
    assert_eq!(
        exec_lua(&rpc, "return _G.res.headers['location']")
            .await
            .as_str(),
        Some("/hello")
    );
}

#[tokio::test]
async fn redirect_error_rejects() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // `redirect = "error"` rejects the promise on a redirect.
    exec_lua(
        &rpc,
        &format!(
            "_G.err = nil\n\
             _G.resolved = false\n\
             nx.http.fetch('{base}/redirect', {{ redirect = 'error' }})\n\
               :next(function() _G.resolved = true end)\n\
               :catch(function(e) _G.err = e.message end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.err").await, "fetch never rejected");
    assert_eq!(lua_bool(&rpc, "return _G.resolved").await, Some(false));
    assert_eq!(
        lua_bool(&rpc, "return type(_G.err) == 'string'").await,
        Some(true)
    );
}

#[tokio::test]
async fn query_params_are_appended_and_encoded() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // `opts.query` builds + encodes the query string (via the `form_urlencoded` crate) and
    // appends it to the URL. /target echoes the request-target so we can read it back.
    exec_lua(
        &rpc,
        &format!(
            "_G.tgt = nil\n\
             nx.http.fetch('{base}/target', {{ query = {{ q = 'hello world', n = 2 }} }})\n\
               :next(function(r) _G.tgt = r:text() end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.tgt").await, "fetch never settled");
    let target = exec_lua(&rpc, "return _G.tgt").await;
    let target = target.as_str().unwrap_or("");
    assert!(
        target.starts_with("/target?"),
        "query should append with `?`: {target}"
    );
    // A space encodes as `+` (form-urlencoded) — proof the lib did the encoding, not a
    // raw concat. Map order isn't guaranteed, so assert both pairs are present.
    assert!(target.contains("q=hello+world"), "target={target}");
    assert!(target.contains("n=2"), "target={target}");
}

#[tokio::test]
async fn form_body_is_urlencoded() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // `opts.form` sends an application/x-www-form-urlencoded body (echoed back by /echo).
    exec_lua(
        &rpc,
        &format!(
            "_G.body = nil\n\
             nx.http.fetch('{base}/echo', {{ method = 'POST', form = {{ a = 'x y', b = 'z' }} }})\n\
               :next(function(r) _G.body = r:text() end)"
        ),
    )
    .await;
    assert!(await_truthy(&rpc, "_G.body").await, "fetch never settled");
    let body = exec_lua(&rpc, "return _G.body").await;
    let body = body.as_str().unwrap_or("");
    assert!(body.contains("a=x+y"), "body={body}");
    assert!(body.contains("b=z"), "body={body}");
}

#[tokio::test]
async fn encode_query_helpers_are_public() {
    let (rpc, _incoming) = start().await;
    // The building blocks are public and lib-backed: encode_uri_component matches
    // encodeURIComponent (space -> %20), encode_query is form-urlencoded (space -> +).
    assert_eq!(
        exec_lua(&rpc, "return nx.http.encode_uri_component('a b/c')")
            .await
            .as_str(),
        Some("a%20b%2Fc")
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return nx.http.encode_query({ { 'q', 'a b' }, { 'q', 'c' } })"
        )
        .await
        .as_str(),
        Some("q=a+b&q=c")
    );
    assert_eq!(
        exec_lua(
            &rpc,
            "return nx.http.build_url('http://h/p?x=1', { y = 2 })"
        )
        .await
        .as_str(),
        Some("http://h/p?x=1&y=2")
    );
}

#[tokio::test]
async fn fetch_local_works_like_fetch() {
    let base = spawn_test_server(1);
    let (rpc, _incoming) = start().await;
    // In a bare session `nx.http.fetch_local` is exactly `nx.http.fetch` — it resolves the
    // same. (Its point is a daemon session, where it forces the local network; see
    // daemon_http.rs.)
    exec_lua(
        &rpc,
        &format!(
            "_G.res = nil\n\
             nx.http.fetch_local('{base}/hello'):next(function(r) _G.res = r:text() end)"
        ),
    )
    .await;
    assert!(
        await_truthy(&rpc, "_G.res").await,
        "fetch_local never settled"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.res").await.as_str(),
        Some("hello world")
    );
}
