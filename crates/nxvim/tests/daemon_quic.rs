//! Black-box test of the **native daemon transport** — the WebTransport/QUIC listener
//! (`nxvim --daemon --listen`) and the edit-host-side [`connect_quic`], the last leg of
//! the edit-host split (see `docs/plans/2026-06-09-edit-host-and-browser-lua.md` →
//! Phase 3q / Open Decision #2).
//!
//! This suite proves the QUIC *transport*: the listener runs in-process on its own
//! thread + runtime — a faithful stand-in for the separate daemon process (a different
//! runtime, reached only over a loopback QUIC socket) — and a real in-process edit-host
//! [`Server`](nxvim_server) drives it through `connect_quic`, which pins the daemon's
//! self-signed cert TOFU and presents the launch-minted bearer token. Because
//! `connect_quic` / `connect_daemon` are transport-agnostic, the editor-facing behavior
//! is identical to stdio — so the assertions that matter here are the two the stdio
//! suite *can't* make: that the wire actually crosses a QUIC connection, and that the
//! bearer token gates it (a bad token is rejected, the right one connects).

use std::net::SocketAddr;
use std::time::Duration;

use nxvim_server::{bind_quic_listener, connect_quic, mint_token, serve_quic, ListenerInfo};

/// Bind a QUIC daemon listener on an ephemeral loopback port and serve it on its own
/// thread + runtime — the stand-in for a separate `nxvim --daemon --listen` process. The
/// returned [`ListenerInfo`] carries the resolved address, the TOFU cert hash, and the
/// bearer token an edit-host needs to connect. The listener thread is detached and runs
/// until the test process exits.
fn spawn_quic_daemon() -> ListenerInfo {
    let (tx, rx) = std::sync::mpsc::channel::<ListenerInfo>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("daemon listener runtime");
        rt.block_on(async move {
            let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
            let (endpoint, info) =
                bind_quic_listener(addr, mint_token()).expect("bind QUIC listener");
            // Hand the connect credentials back before serving; the bind already resolved
            // the ephemeral `:0` to a concrete port.
            tx.send(info.clone()).expect("send listener info");
            // Serve forever; the test process exit tears this thread down.
            let _ = serve_quic(endpoint, info.token).await;
        });
    });
    rx.recv().expect("listener bound and reported its info")
}

/// The `https://HOST:PORT` dial URL for a bound listener.
fn dial_url(info: &ListenerInfo) -> String {
    format!("https://{}", info.addr)
}

/// The bearer token is the daemon's authorization gate (an unauthenticated daemon
/// listener is RCE by design). A connection presenting the *wrong* token is rejected
/// (403) and `connect_quic` fails; the *right* token connects. Proves the gate does real
/// work — not that any connection is accepted.
#[tokio::test]
async fn the_bearer_token_gates_the_connection() {
    let info = spawn_quic_daemon();
    let url = dial_url(&info);

    // Wrong token, correct cert hash → the daemon replies 403 and the connect fails.
    // Run on a blocking thread under a timeout so a regression that *hangs* (a dropped,
    // idle-timing-out session) fails loudly rather than stalling the suite.
    let bad_url = url.clone();
    let bad_cert = info.cert_hash.clone();
    let rejected =
        tokio::task::spawn_blocking(move || connect_quic(&bad_url, &bad_cert, &"0".repeat(64)));
    let rejected = tokio::time::timeout(Duration::from_secs(10), rejected)
        .await
        .expect("connect_quic with a bad token must return within 10s, not hang")
        .expect("join the connect task");
    assert!(
        rejected.is_err(),
        "a bad bearer token must be rejected, but connect_quic succeeded"
    );

    // The correct credentials connect.
    let accepted = connect_quic(&url, &info.cert_hash, &info.token);
    assert!(
        accepted.is_ok(),
        "the correct token must connect: {:?}",
        accepted.err()
    );
}
