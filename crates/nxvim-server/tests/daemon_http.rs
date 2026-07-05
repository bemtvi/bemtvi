//! The daemon wire protocol, `nx.http` half (the `http_op` leg).
//!
//! Proves async `nx.http.fetch` runs **on the daemon over a real wire** in a
//! native-daemon session: the editor is given a [`RemoteHttp`](nxvim_server::RemoteHttp)
//! as its HTTP seam (so the event-loop actor is `HttpBackend::Remote` — it has NO local
//! `ureq` and can ONLY send `http_op` requests), and a `serve_http_daemon` answers them
//! over an in-process `tokio::io::duplex` by running the real round-trip. The request is
//! encoded into one `http_op` request, run daemon-side, and the typed reply decoded back
//! — the same leg (and codec) the wasm edit-host uses, exercised natively here.
//!
//! Faithful, not a no-op: the actor holds no HTTP client, so a resolved response can only
//! have crossed the wire to the daemon, which fetched a throwaway loopback server.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::{RemoteHttp, ServerInit};
use nxvim_test_harness::{attach, exec_lua, spawn};
use tokio::sync::mpsc::UnboundedReceiver;

/// A one-shot loopback HTTP server that answers a single request with `200 OK` and
/// `body`, then exits. Returns its base URL.
fn spawn_one_shot(body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local_addr");
    thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            // Drain the request head (we don't branch on it).
            let mut chunk = [0u8; 1024];
            let mut buf = Vec::new();
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = write_all(&mut stream, head.as_bytes(), body.as_bytes());
        }
    });
    format!("http://{addr}")
}

fn write_all(stream: &mut TcpStream, head: &[u8], body: &[u8]) -> std::io::Result<()> {
    stream.write_all(head)?;
    stream.write_all(body)?;
    stream.flush()
}

/// Start a server whose `nx.http` seam is a [`RemoteHttp`] talking to a
/// `serve_http_daemon` over an in-process duplex. The actor is `HttpBackend::Remote` — no
/// local `ureq` — so every fetch must cross the wire. The notification receiver is
/// returned (dropping it would tear the client connection down and stop the server).
async fn spawn_with_daemon_http() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_http_daemon(daemon_reader, daemon_writer).await;
    });

    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let remote = RemoteHttp::connect(host_reader, host_writer);
    let init = ServerInit {
        http_jobs: Some(remote),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `return tostring(<expr>)` until it equals `want` (or the budget runs out).
async fn await_lua_eq(rpc: &Rpc, expr: &str, want: &str) -> bool {
    let code = format!("return tostring({expr})");
    for _ in 0..150 {
        if exec_lua(rpc, &code).await.as_str() == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// `nx.http.fetch` runs on the daemon over the wire: the actor has no local `ureq`, so a
/// resolved 200 with the server's body can only have come from the daemon's round-trip.
#[tokio::test]
async fn nx_http_fetch_runs_on_the_daemon_over_the_wire() {
    let base = spawn_one_shot("from-the-daemon");
    let (rpc, _incoming) = spawn_with_daemon_http().await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.__status = nil
               _G.__body = nil
               nx.http.fetch("{base}/thing"):next(
                 function(r) _G.__status = r.status; _G.__body = r:text() end,
                 function(e) _G.__body = "err:" .. tostring(e.message) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.__status", "200").await,
        "the fetch should resolve 200 over the daemon wire"
    );
    assert_eq!(
        exec_lua(&rpc, "return _G.__body").await.as_str(),
        Some("from-the-daemon")
    );
}

/// A fake daemon that answers every `http_op` with a loud `["err", "DAEMON-REACHED"]` — so a
/// request that crossed the wire is distinguishable (it rejects) from one that ran locally.
async fn serve_fake_http_daemon<R, W>(reader: R, writer: W)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (rpc, mut incoming) = nxvim_rpc::connect(reader, writer);
    while let Some(msg) = incoming.recv().await {
        if let Incoming::Request { id, method, .. } = msg {
            if method == "http_op" {
                let envelope = nxvim_lua::http_result_to_value(&Err(nxvim_lua::HttpError {
                    message: "DAEMON-REACHED".to_string(),
                }));
                rpc.respond(id, Ok(envelope));
            }
        }
    }
}

/// `nx.http.fetch_local` bypasses the daemon: in a daemon session where the daemon rejects
/// every `http_op`, a plain `fetch` rejects (it routed to the daemon) but `fetch_local`
/// resolves against a real loopback server (it ran on the client's local `ureq`).
#[tokio::test]
async fn nx_http_fetch_local_bypasses_the_daemon() {
    let base = spawn_one_shot("from-local");
    let (edit_host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (daemon_reader, daemon_writer) = tokio::io::split(daemon_end);
    tokio::spawn(serve_fake_http_daemon(daemon_reader, daemon_writer));
    let (host_reader, host_writer) = tokio::io::split(edit_host_end);
    let init = ServerInit {
        http_jobs: Some(RemoteHttp::connect(host_reader, host_writer)),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    exec_lua(
        &rpc,
        &format!(
            r#"_G.routed = nil
               _G.local_body = nil
               -- routed to the daemon → the fake rejects with DAEMON-REACHED
               nx.http.fetch("{base}/x"):next(
                 function(r) _G.routed = "ok:" .. r.status end,
                 function(e) _G.routed = "err:" .. tostring(e.message) end)
               -- forced local → runs the client's ureq against the real server
               nx.http.fetch_local("{base}/x"):next(
                 function(r) _G.local_body = r:text() end,
                 function(e) _G.local_body = "err:" .. tostring(e.message) end)
               return 1"#,
        ),
    )
    .await;

    assert!(
        await_lua_eq(&rpc, "_G.routed", "err:DAEMON-REACHED").await,
        "a plain fetch should route to the daemon (which rejects)"
    );
    assert!(
        await_lua_eq(&rpc, "_G.local_body", "from-local").await,
        "fetch_local should bypass the daemon and hit the real server"
    );
}
