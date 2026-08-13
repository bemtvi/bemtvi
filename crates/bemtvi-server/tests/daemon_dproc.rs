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

// ================================================== a bad port never dials a wrong one
//
// The port arrives as a wire integer and used to be cast with `as u16`, which
// TRUNCATES: `70000` silently becomes `4464`, so `btv.socket.connect{ port = 70000 }`
// dialed a completely different service — and, worse, one that might answer. It is
// refused loudly now, through the same `sock_closed` channel a refused connect uses
// (a connect that never began has no other error channel).

#[tokio::test]
async fn sock_leg_refuses_a_port_outside_the_dial_range() {
    let (client, mut inc) = wire_leg(bemtvi_server::serve_sock_daemon_on);
    // 70000 truncates to 4464 in 16 bits.
    client.notify(
        "sock_connect",
        vec![
            Value::from(9u64),
            Value::from("127.0.0.1"),
            Value::from(70_000u64),
        ],
    );
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "sock_closed")).await;
    let closed = got
        .iter()
        .find(|(m, _)| m == "sock_closed")
        .expect("an out-of-range port must be answered, not silently dialed");
    assert_eq!(
        closed.1.first().and_then(Value::as_u64),
        Some(9),
        "the refusal must be for the stream that asked; got {got:?}"
    );
    let msg = closed.1.get(1).and_then(Value::as_str).unwrap_or_default();
    assert!(
        msg.contains("70000") && msg.contains("range"),
        "the error must name the port and why it was refused, got {msg:?}"
    );
    assert!(
        !got.iter().any(|(m, _)| m == "sock_connected"),
        "nothing may be dialed for an out-of-range port; got {got:?}"
    );
}

#[tokio::test]
async fn sock_leg_still_accepts_the_top_of_the_range() {
    // The boundary the other way: 65535 is a legal port, so the guard must refuse
    // only what genuinely does not fit. Nothing listens there, so the observable is
    // a connect FAILURE rather than a range refusal.
    let (client, mut inc) = wire_leg(bemtvi_server::serve_sock_daemon_on);
    client.notify(
        "sock_connect",
        vec![
            Value::from(10u64),
            Value::from("127.0.0.1"),
            Value::from(65_535u64),
        ],
    );
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "sock_closed")).await;
    let closed = got
        .iter()
        .find(|(m, _)| m == "sock_closed")
        .expect("sock_closed");
    let msg = closed.1.get(1).and_then(Value::as_str).unwrap_or_default();
    assert!(
        !msg.contains("range"),
        "65535 fits in the dial range — it must be dialed, not refused: {msg:?}"
    );
}

// ============================================= a dropped link does not orphan children
//
// The daemon outlives its connections — that is its job. So a leg that spawned child
// processes must reap them when its edit-host goes away, or a long-running child runs
// forever and every reconnect stacks a fresh set on top of the last.

/// Whether `pid` is still a live process (Linux: the `/proc` entry exists and is not
/// a zombie). Used to observe the reap from outside.
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `(state)` is the field after the parenthesised comm; `Z` is a reaped-but-
        // unwaited zombie, which is not "still running".
        Ok(stat) => match stat.rsplit_once(") ") {
            Some((_, rest)) => !rest.starts_with('Z'),
            None => true,
        },
        Err(_) => false,
    }
}

/// The one-shot leg's half of the same property, observed through the pid it reports.
/// See the note on `a_dropped_link_leaves_no_duplex_child_running`: this pins the
/// PROPERTY, not one of the two mechanisms that enforce it.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_the_link_reaps_the_children_the_leg_spawned() {
    // The one-shot process leg, because it is the one that reports the child's pid
    // (`proc_spawned [id, pid]`) — which is how the reap is observed from outside.
    let (client, mut inc) = wire_leg(bemtvi_server::serve_proc_daemon_on);

    client.notify(
        "proc_spawn",
        vec![
            Value::from(77u64),
            Value::Array(vec![Value::from("sleep"), Value::from("300")]),
            Value::Nil,
            Value::Array(vec![]),
            Value::Binary(Vec::new()),
        ],
    );
    let got = drain_until(&mut inc, |g| g.iter().any(|(m, _)| m == "proc_spawned")).await;
    let pid = got
        .iter()
        .find(|(m, _)| m == "proc_spawned")
        .and_then(|(_, p)| p.get(1))
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("the leg must report the child's pid; got {got:?}"))
        as u32;
    assert!(
        pid_alive(pid),
        "the child is running before we drop the link"
    );

    drop(client);
    drop(inc);

    let mut reaped = false;
    for _ in 0..100 {
        if !pid_alive(pid) {
            reaped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        reaped,
        "the child (pid {pid}) outlived the connection that spawned it — every \
         reconnect would stack another"
    );
}

/// The same property for the DUPLEX leg, which reports no pid: observe the child by
/// what it would DO if it lived. A reaped child never reaches its `touch`.
///
/// This is the shape that pins the guarantee independently of which mechanism
/// enforces it. The leg has two — the child's event sink closing when the connection
/// ends, and the explicit abort of every handle it kept — so removing just one still
/// leaves the property true. That is deliberate: what must never regress is the
/// PROPERTY (a dropped link leaves nothing running), not a particular mechanism.
#[cfg(unix)]
#[tokio::test]
async fn a_dropped_link_leaves_no_duplex_child_running() {
    let dir = bemtvi_test_harness::temp_dir("dproc_orphan");
    let marker = dir.join("survived");
    let script = format!("sleep 3; touch {}", marker.to_string_lossy());

    let (client, inc) = wire_leg(bemtvi_server::serve_dproc_daemon_on);
    client.notify(
        "dproc_open",
        vec![
            Value::from(88u64),
            Value::Array(vec![
                Value::from("sh"),
                Value::from("-c"),
                Value::from(script.as_str()),
            ]),
            Value::Nil,
            Value::Array(vec![]),
        ],
    );
    // Let it actually start before the link goes away.
    tokio::time::sleep(Duration::from_millis(300)).await;
    drop(client);
    drop(inc);

    // Well past the child's own sleep: if it were still running it would have
    // touched the marker by now.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert!(
        !marker.exists(),
        "a child kept running after its connection was dropped — it reached its \
         side effect at {}",
        marker.display()
    );
}
