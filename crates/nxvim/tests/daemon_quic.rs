//! Black-box test of the **native daemon transport** — the WebTransport/QUIC listener
//! (`nxvim --daemon --listen`) and the edit-host-side [`connect_quic`], the last leg of
//! the edit-host split (see `docs/plans/2026-06-09-edit-host-and-browser-lua.md` →
//! Phase 3q / Open Decision #2).
//!
//! The stdio suite ([`daemon_stdio`]) already proved the six-leg multiplexer over one
//! ordered stream against the real `--daemon` binary; this suite proves the *transport*
//! the stdio stand-in was standing in for. The listener runs in-process on its own
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

use nxvim_rpc::Incoming;
use nxvim_server::{
    bind_quic_listener, connect_quic, mint_token, serve_quic, ListenerInfo, ServerInit,
};
use nxvim_test_harness::{attach, buf_lines, exec_lua, feed, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

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

/// Connect a real in-process edit-host [`Server`](nxvim_server) to `info`'s listener over
/// QUIC, its five host seams all riding that one connection, opening `file` off-tick over
/// the wire at startup. Returns the editor's RPC handle *and* its notification receiver —
/// the caller must keep the receiver alive, or dropping it tears the editor connection
/// down (the editor then sees EOF and quits).
async fn spawn_edit_host_quic(
    info: &ListenerInfo,
    file: &str,
) -> (nxvim_rpc::Rpc, UnboundedReceiver<Incoming>) {
    // `connect_quic` blocks until the QUIC handshake + WebTransport session + bidi stream
    // are up; the listener is on its own thread, so this returns in milliseconds.
    let client = connect_quic(&dial_url(info), &info.cert_hash, &info.token).expect("connect_quic");
    let init = ServerInit {
        file: Some(file.to_string()),
        host_fs_async: Some(Box::new(client.host_fs)),
        host_proc: Some(Box::new(client.host_proc)),
        blocking_system: Some(Box::new(client.blocking_system)),
        lsp_transport: Some(Box::new(client.lsp_transport)),
        lua_fs: Some(Box::new(client.lua_fs)),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    // `attach` returning proves startup did not block on the (deferred, off-tick) fetch.
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll `nvim_buf_get_lines` until it matches `want` or the budget runs out — the initial
/// open and the watch-driven autoreload both land off-tick.
async fn await_lines(rpc: &nxvim_rpc::Rpc, want: &[&str]) -> Vec<String> {
    for _ in 0..150 {
        let lines = buf_lines(rpc, 0).await;
        if lines == want {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    buf_lines(rpc, 0).await
}

/// A real edit-host `Server` drives a QUIC daemon, exercising four wire classes through
/// the running editor **over a real QUIC connection**: the off-tick fs read (startup
/// open) and write (`:w`), the blocking `sys_run` bridge (`vim.system():wait()`), the
/// watch push (external-change autoreload), and `luafs` (`vim.uv.fs_stat`). Each carries
/// bytes a stub couldn't invent, proving the QUIC transport carries the full multiplexer —
/// not merely that a connection was established.
#[tokio::test]
async fn edit_host_drives_a_daemon_over_quic() {
    let dir = temp_dir("daemon-quic");
    let path = dir.join("doc.txt");
    let path_str = path.to_str().unwrap().to_string();
    std::fs::write(&path, "line one\nline two\n").expect("seed file");

    let info = spawn_quic_daemon();
    // Keep `_incoming` alive for the whole test — dropping it tears the editor's RPC
    // connection down (the editor would then see EOF and quit).
    let (rpc, _incoming) = spawn_edit_host_quic(&info, &path_str).await;

    // --- fs read leg: the startup buffer was fetched off-tick over the QUIC wire. With a
    // daemon fs seam present the editor never reads local disk for the open.
    let lines = await_lines(&rpc, &["line one", "line two"]).await;
    assert_eq!(
        lines,
        &["line one", "line two"],
        "startup open over the QUIC wire"
    );

    // --- luafs leg: `vim.uv.fs_stat` / `filereadable` resolve against the daemon's fs.
    // The file is 18 bytes ("line one\nline two\n").
    let size = exec_lua(
        &rpc,
        &format!(r#"return vim.uv.fs_stat("{path_str}").size"#),
    )
    .await;
    assert_eq!(
        size.as_u64(),
        Some(18),
        "vim.uv.fs_stat over luafs: {size:?}"
    );
    let readable = exec_lua(
        &rpc,
        &format!(r#"return vim.fn.filereadable("{path_str}")"#),
    )
    .await;
    assert_eq!(
        readable.as_u64(),
        Some(1),
        "vim.fn.filereadable over luafs: {readable:?}"
    );

    // --- sys_run leg: the *blocking* `vim.system(...):wait()` parks the editor thread on
    // the reply while the shared link thread drives the QUIC wire and the daemon runs the
    // child — the exact bridge that would deadlock if the connection were driven by the
    // (parked) editor thread.
    let sys = exec_lua(
        &rpc,
        r#"return vim.system({ "printf", "sys-leg-ok" }):wait().stdout"#,
    )
    .await;
    assert_eq!(
        sys.as_str(),
        Some("sys-leg-ok"),
        "vim.system:wait over sys_run: {sys:?}"
    );

    // --- fs write leg: edit the buffer and `:w`. The save is off-tick — `modified` clears
    // only when the daemon acks the `fs_write` — so poll until it clears, then confirm the
    // *edited* bytes actually landed on the daemon's disk.
    feed(&rpc, "ggO");
    feed(&rpc, "line zero");
    feed(&rpc, "<Esc>");
    feed(&rpc, ":w<CR>");
    let mut cleared = false;
    for _ in 0..150 {
        if exec_lua(&rpc, "return vim.bo.modified").await.as_bool() == Some(false) {
            cleared = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cleared,
        ":w should clear `modified` once the daemon acks the write"
    );
    let on_disk = std::fs::read_to_string(&path).expect("read back the saved file");
    assert_eq!(
        on_disk, "line zero\nline one\nline two\n",
        "the edited bytes crossed the QUIC wire and the daemon wrote them"
    );

    // --- watch leg: change the file externally; only the daemon watches the remote file,
    // so it must detect the drift and push `fs_changed`, which the edit-host turns into an
    // off-tick autoreload (the buffer is clean after the save).
    std::fs::write(&path, "reloaded\nfrom\nwatch\n").expect("external write");
    let reloaded = await_lines(&rpc, &["reloaded", "from", "watch"]).await;
    assert_eq!(
        reloaded,
        &["reloaded", "from", "watch"],
        "external change autoreloaded via the daemon's watch push over the QUIC wire"
    );
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
