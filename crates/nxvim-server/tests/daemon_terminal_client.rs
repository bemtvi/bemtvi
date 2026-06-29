//! The daemon wire protocol, terminal half — the **edit-host (client) side** of the remote
//! `:terminal` leg (`docs/plans/2026-06-28-native-remote-terminal.md`).
//!
//! The sibling `daemon_terminal.rs` proves the *daemon* end (the PTY host) in isolation; this
//! suite proves the missing *client* end: a real editor whose terminal seam is a
//! [`RemoteHostTerm`](nxvim_server::RemoteHostTerm) talking to a
//! [`serve_term_daemon_on`](nxvim_server::serve_term_daemon_on) over an in-process duplex. The
//! daemon runs a **real** `sh` PTY, so the bytes asserted on are an actual shell's output
//! crossing the wire — a stub could not produce a shell's `pwd` or echo interactive input.
//!
//! **Why this distinguishes remote from local.** The daemon spawns the PTY with the *daemon's*
//! working directory (seeded from [`ServerInit::remote_cwd`], a uniquely-named temp dir that is
//! not the test process's cwd). A correct remote `:terminal` opens there, so `pwd` prints that
//! unique path; the pre-fix behavior (spawning a PTY on the **local** machine in the test's own
//! cwd) prints something else entirely — the faithfulness argument the `daemon_chdir.rs` suite
//! makes for `:cd`/`getcwd`. PTY output is async (a real child + the wire), so each assertion
//! polls the terminal buffer with a bounded budget rather than sleeping a fixed amount.

use std::time::Duration;

use nxvim_rpc::{connect, Incoming, Rpc};
use nxvim_server::{RemoteHostTerm, ServerInit};
use nxvim_test_harness::{attach, buf_lines, feed, spawn, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// Start a real editor whose `:terminal` seam is a [`RemoteHostTerm`] connected to a
/// `serve_term_daemon_on` (the real PTY host) over an in-process duplex, with the daemon's
/// working directory seeded to `remote_cwd`. UI-attached. The returned `incoming` must be held
/// for the session's lifetime (dropping it closes the client RPC connection).
async fn spawn_remote_term(remote_cwd: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let (host_end, daemon_end) = tokio::io::duplex(1 << 16);

    // The daemon end: the real terminal engine (a real `sh` PTY) over the wire.
    let (dr, dw) = tokio::io::split(daemon_end);
    let (daemon_rpc, daemon_incoming) = connect(dr, dw);
    tokio::spawn(async move {
        let _ = nxvim_server::serve_term_daemon_on(daemon_rpc, daemon_incoming).await;
    });

    // The edit-host end: the client seam the server routes `:terminal` ops through.
    let (hr, hw) = tokio::io::split(host_end);
    let host_term = RemoteHostTerm::connect(hr, hw);

    let init = ServerInit {
        host_term: Some(host_term),
        remote_cwd: Some(remote_cwd.into()),
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// Poll the current buffer (`bufnr` 0) until its text contains `needle` or the budget runs
/// out. Matches against the lines **concatenated** (no separator) so a needle wrapped across
/// the terminal's fixed-width rows — a long path split at column 80 — is still found. Returns
/// the raw lines either way so a failure shows what *did* arrive.
async fn await_lines_contains(rpc: &Rpc, needle: &str) -> Vec<String> {
    for _ in 0..200 {
        let lines = buf_lines(rpc, 0).await;
        if lines.concat().contains(needle) {
            return lines;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    buf_lines(rpc, 0).await
}

/// A daemon session's `:terminal` opens its PTY on the **remote** (daemon) host, in the
/// daemon's working directory — not on the local machine. An interactive `sh` runs on the
/// daemon; typing `pwd` into it prints the seeded remote cwd, which streams back into the
/// terminal buffer. Local spawning (the pre-fix bug) would open the shell in the test
/// process's cwd, so its `pwd` could never print this unique path. (An interactive shell
/// stays alive, so the `pwd` output never races a child exit — the real-usage case.)
#[tokio::test]
async fn terminal_opens_on_the_daemon_in_its_cwd() {
    let dir = temp_dir("remote_term_cwd");
    let dir_str = dir.to_string_lossy().into_owned();
    // The unique basename is present whether or not the shell canonicalizes the path
    // (macOS `/var/folders` ↔ `/private/var/folders`), so it is the robust needle.
    let token = dir.file_name().unwrap().to_string_lossy().into_owned();

    let (rpc, _incoming) = spawn_remote_term(&dir_str).await;

    // Open an interactive shell on the daemon, then ask it where it is.
    feed(&rpc, ":terminal sh<CR>");
    feed(&rpc, "pwd<CR>");
    let lines = await_lines_contains(&rpc, &token).await;
    assert!(
        lines.concat().contains(&token),
        "a remote `:terminal` runs on the daemon in its cwd ({token:?}); typing `pwd` should \
         print that path, got: {lines:?}"
    );
}

/// Interactive input round-trips over the wire: in terminal mode, typed keys are forwarded as
/// bytes to the daemon's `cat`, whose echo streams back into the buffer. Proves the `term_write`
/// leg (not just the open) crosses the wire to the remote child.
#[tokio::test]
async fn terminal_input_round_trips_to_the_daemon_child() {
    let dir = temp_dir("remote_term_echo");
    let (rpc, _incoming) = spawn_remote_term(&dir.to_string_lossy()).await;

    // `cat` echoes its stdin back through the PTY; opening it leaves us in terminal mode, so
    // the typed word is forwarded as input bytes over the Term leg.
    feed(&rpc, ":terminal cat<CR>");
    feed(&rpc, "hello<CR>");
    let lines = await_lines_contains(&rpc, "hello").await;
    assert!(
        lines.concat().contains("hello"),
        "input typed in a remote terminal must reach the daemon's child and echo back, got: \
         {lines:?}"
    );
}
