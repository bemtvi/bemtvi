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

use nxvim_server::{
    bind_quic_listener, connect_quic, mint_token, serve_quic, ListenerInfo, ServerInit,
};
use nxvim_test_harness::{attach, buf_lines, exec_lua, feed, spawn, temp_dir};

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

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out — the off-tick
/// fetch (initial open) lands a moment after attach.
async fn await_lines(rpc: &nxvim_rpc::Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..100 {
        if buf_lines(rpc, 0).await == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// The legs split across **separate** QUIC streams (Control / Proc / Lsp) all carry real
/// traffic, not just the connection setup the token-gate test proves. Drives a full
/// edit-host `Server` whose seams come from `connect_quic`, against a real `serve_quic`
/// daemon backed by the `Std*` impls — so a file read/write crosses the **Control** stream
/// and a spawned process crosses the **Proc** stream, each over its own QUIC bidi.
///
/// Faithful, not a no-op: the daemon's `StdHostFs` reads/writes a real temp file and its
/// `StdHostProc` runs a real `sh`, and the asserted bytes can only have come back across
/// the wire. (This proves the multi-stream transport *carries* each leg; head-of-line
/// isolation between the streams is QUIC's per-stream flow control, architectural — a
/// deterministic latency assertion would be inherently timing-flaky, so it's omitted.)
#[tokio::test]
async fn native_legs_round_trip_over_separate_quic_streams() {
    let info = spawn_quic_daemon();
    let url = dial_url(&info);

    // A real temp file the daemon's `StdHostFs` serves; its bytes cross the QUIC wire.
    let dir = temp_dir("quic_multistream");
    let file = dir.join("note.txt");
    std::fs::write(&file, "alpha\nbeta\n").expect("seed the remote file");

    let client = connect_quic(&url, &info.cert_hash, &info.token).expect("connect over QUIC");
    let init = ServerInit {
        file: Some(file.to_string_lossy().into_owned()),
        host_proc: Some(Box::new(client.host_proc)),
        host_fs_async: Some(Box::new(client.host_fs)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        fs_jobs: Some(client.fs_jobs),
        ..Default::default()
    };
    let (rpc, _incoming) = spawn(init);
    attach(&rpc, 80, 24).await;

    // Control stream — `fs_read`: the file's bytes loaded over the wire into the buffer.
    assert_eq!(
        await_lines(&rpc, &["alpha", "beta"]).await,
        vec!["alpha", "beta"],
        "the startup file's bytes must arrive over the Control QUIC stream"
    );

    // Proc stream — a child runs on the *separate* Proc stream and its stdout returns.
    exec_lua(
        &rpc,
        "_G.res = nil\n\
         nx.run({ cmd = 'sh', args = { '-c', 'echo hello' } }):next(function(r) _G.res = r end)",
    )
    .await;
    let mut proc_stdout = None;
    for _ in 0..100 {
        let out = exec_lua(&rpc, "return _G.res and _G.res.stdout").await;
        if let Some(s) = out.as_str() {
            proc_stdout = Some(s.to_string());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        proc_stdout.as_deref(),
        Some("hello\n"),
        "the process's stdout must return over the Proc QUIC stream"
    );

    // Control stream — `fs_write`: append a line and `:w`; the daemon's real file updates.
    feed(&rpc, "Gonew line<Esc>");
    feed(&rpc, ":w<CR>");
    let mut on_disk = String::new();
    for _ in 0..100 {
        on_disk = std::fs::read_to_string(&file).unwrap_or_default();
        if on_disk.contains("new line") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        on_disk, "alpha\nbeta\nnew line\n",
        "the save must push the edited bytes back over the Control QUIC stream to disk"
    );
}
