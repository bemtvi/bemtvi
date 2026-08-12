//! The daemon **dial handshake**: the seams a dialer hands back must already be
//! routed onto the live connection.
//!
//! `connect_daemon` returns a [`DaemonClient`](bemtvi_server::DaemonClient) whose seams
//! issue their requests through swappable link cells. The very first thing every caller
//! does with that client is the config-resolve round trip (`RemoteConfig::resolve`) —
//! so if the cells are still empty when the client is handed back, that round trip fails
//! loud with `daemon disconnected` before the wire is ever touched, and the session dies
//! at startup with `could not resolve the session config from the daemon: daemon
//! disconnected`. Faithful, not a no-op: the daemon is a real
//! [`run_daemon_io`](bemtvi_server::run_daemon_io) over an in-process duplex serving a
//! real temp file, so a seam op that reaches it returns that file's bytes.

use bemtvi_server::{connect_daemon, FsRead, HostFsAsync};
use bemtvi_test_harness::write_temp;

/// Stand up a real daemon over an in-process duplex and dial it, exactly as the binary's
/// stdio/ssh path does.
fn dial() -> bemtvi_server::DaemonClient {
    let (host_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (d_reader, d_writer) = tokio::io::split(daemon_end);
    tokio::spawn(async move {
        let _ = bemtvi_server::run_daemon_io(d_reader, d_writer).await;
    });
    let (h_reader, h_writer) = tokio::io::split(host_end);
    connect_daemon(h_reader, h_writer)
}

/// Regression: the seams are live the *instant* `connect_daemon` returns.
///
/// The link thread publishes the dialed connection into the swappable cells; if it did so
/// only after handing the client back, the caller's first seam op races that publish and
/// fails loud with `daemon disconnected` — the client is returned before the link thread
/// is next scheduled, so under load (a saturated `cargo test --workspace`) the op runs
/// first and finds an empty cell. The dial must publish *before* the handoff.
///
/// The race is between two OS threads (`connect_daemon` runs the link on its own thread
/// with its own runtime), so one attempt would only catch it by luck: the window is a few
/// instructions wide. Dialing repeatedly on a *multi-threaded* runtime — true parallelism,
/// which the product's own current-thread runtimes never give the window — and requiring
/// **every** attempt to reach the daemon is what makes this a real guard: pre-fix it fails
/// within the first couple dozen dials even on an idle machine; post-fix the publish
/// happens-before the handoff, so the cell can never be empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seams_are_live_the_instant_connect_daemon_returns() {
    let path = write_temp("daemon_dial_race", "txt", "hello over the wire\n");

    for attempt in 0..400 {
        let client = dial();
        // The first seam op after the dial — no `.await` between the handoff above and
        // this call, exactly like the config-resolve round trip a real session issues.
        match client.host_fs.read(path.clone()).await {
            Ok(FsRead::File(bytes, _)) => assert_eq!(
                String::from_utf8_lossy(&bytes),
                "hello over the wire\n",
                "dial {attempt}: the first seam op reads the daemon's file"
            ),
            Ok(_) => {
                panic!("dial {attempt}: expected the daemon's file bytes, got a dir/new marker")
            }
            Err(e) => panic!(
                "dial {attempt}: the first seam op after connect_daemon failed instead of \
                 reaching the live daemon: {e}"
            ),
        }
    }
}
