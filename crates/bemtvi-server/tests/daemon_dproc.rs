//! The daemon wire protocol for the duplex `btv.process` leg (`dproc_*`) and the
//! `btv.socket` leg (`sock_*`) — the remote transports a WASM/web edit-host uses to run
//! a DAP adapter on a daemon (the sibling of the LSP `lsp_*` leg).
//!
//! Driven DIRECTLY over an in-process `tokio::io::duplex` wire: one end runs the leg
//! (`serve_dproc_daemon_on` / `serve_sock_daemon_on`), the other sends the edit-host's
//! notifications and asserts on the daemon's replies. Faithful — a real child / a real
//! TCP connection runs on the "daemon" end; the bytes can only have crossed the wire.

use std::time::Duration;

use bemtvi_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Wire up a client RPC + the leg running over a duplex pair. `serve` is the leg fn.
fn wire_leg<F, Fut>(serve: F) -> (Rpc, UnboundedReceiver<Incoming>)
where
    F: FnOnce(Rpc, UnboundedReceiver<Incoming>) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let (client_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (dr, dw) = tokio::io::split(daemon_end);
    let (rpc_daemon, inc_daemon) = connect(dr, dw);
    tokio::spawn(serve(rpc_daemon, inc_daemon));

    let (cr, cw) = tokio::io::split(client_end);
    connect(cr, cw)
}

/// Collect inbound notifications until `pred` is satisfied or the budget runs out.
async fn drain_until<P>(
    inc: &mut UnboundedReceiver<Incoming>,
    mut pred: P,
) -> Vec<(String, Vec<Value>)>
where
    P: FnMut(&[(String, Vec<Value>)]) -> bool,
{
    let mut got: Vec<(String, Vec<Value>)> = Vec::new();
    for _ in 0..200 {
        if pred(&got) {
            break;
        }
        match tokio::time::timeout(Duration::from_millis(50), inc.recv()).await {
            Ok(Some(Incoming::Notification { method, params })) => got.push((method, params)),
            Ok(Some(_)) => {}
            _ => {}
        }
    }
    got
}

fn bytes_of(v: &Value) -> Vec<u8> {
    match v {
        Value::Binary(b) => b.clone(),
        Value::String(s) => s.as_bytes().to_vec(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn dproc_leg_round_trips_a_duplex_child_over_the_wire() {
    let (client, mut inc) = wire_leg(bemtvi_server::serve_dproc_daemon_on);

    // Open `cat` (echoes stdin → stdout) and feed it bytes over the wire.
    client.notify(
        "dproc_open",
        vec![
            Value::from(1u64),
            Value::Array(vec![Value::from("cat")]),
            Value::Nil,
            Value::Array(vec![]),
        ],
    );
    client.notify(
        "dproc_write",
        vec![Value::from(1u64), Value::Binary(b"ping\n".to_vec())],
    );

    // The daemon streams the echoed bytes back as dproc_out.
    let got = drain_until(&mut inc, |g| {
        g.iter().any(|(m, p)| {
            m == "dproc_out"
                && p.first().and_then(Value::as_u64) == Some(1)
                && bytes_of(&p[1]) == b"ping\n"
        })
    })
    .await;
    assert!(
        got.iter()
            .any(|(m, p)| m == "dproc_out" && bytes_of(&p[1]) == b"ping\n"),
        "the child echoed the stdin back over the wire; got {got:?}"
    );

    // Kill it; the daemon reports the exit.
    client.notify("dproc_kill", vec![Value::from(1u64)]);
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "dproc_exit")).await;
    assert!(
        got.iter()
            .any(|(m, p)| m == "dproc_exit" && p.first().and_then(Value::as_u64) == Some(1)),
        "the killed child reported dproc_exit; got {got:?}"
    );
}

#[tokio::test]
async fn sock_leg_round_trips_a_tcp_connection_over_the_wire() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // A local echo server for the daemon to dial.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        if let Ok((mut s, _)) = listener.accept().await {
            let mut buf = [0u8; 1024];
            loop {
                match s.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if s.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let (client, mut inc) = wire_leg(bemtvi_server::serve_sock_daemon_on);
    client.notify(
        "sock_connect",
        vec![
            Value::from(2u64),
            Value::from("127.0.0.1"),
            Value::from(port as u64),
        ],
    );
    // Connected, then write — the echo comes back as sock_data.
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "sock_connected")).await;
    assert!(
        got.iter()
            .any(|(m, p)| m == "sock_connected" && p.first().and_then(Value::as_u64) == Some(2)),
        "sock_connected fired; got {got:?}"
    );
    client.notify(
        "sock_write",
        vec![Value::from(2u64), Value::Binary(b"hi".to_vec())],
    );
    let got = drain_until(&mut inc, |g| {
        g.iter()
            .any(|(m, p)| m == "sock_data" && bytes_of(&p[1]) == b"hi")
    })
    .await;
    assert!(
        got.iter()
            .any(|(m, p)| m == "sock_data" && bytes_of(&p[1]) == b"hi"),
        "the echo server's bytes came back as sock_data; got {got:?}"
    );

    client.notify("sock_close", vec![Value::from(2u64)]);
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "sock_closed")).await;
    assert!(
        got.iter().any(|(m, _)| m == "sock_closed"),
        "sock_closed fired after close; got {got:?}"
    );
}

#[tokio::test]
async fn sock_leg_reports_connect_failure() {
    let (client, mut inc) = wire_leg(bemtvi_server::serve_sock_daemon_on);
    // Port 1 is unbound — connect refuses, surfacing as sock_closed with an error.
    client.notify(
        "sock_connect",
        vec![
            Value::from(3u64),
            Value::from("127.0.0.1"),
            Value::from(1u64),
        ],
    );
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "sock_closed")).await;
    let closed = got
        .iter()
        .find(|(m, _)| m == "sock_closed")
        .expect("sock_closed");
    assert!(
        matches!(closed.1.get(1), Some(Value::String(_))),
        "the connect failure carried an error string; got {got:?}"
    );
}
