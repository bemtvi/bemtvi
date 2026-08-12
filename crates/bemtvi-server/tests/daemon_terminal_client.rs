//! The daemon wire protocol, terminal half — the **edit-host (client) side** of the remote
//! `:terminal` leg (`docs/plans/2026-06-28-native-remote-terminal.md`).
//!
//! The sibling `daemon_terminal.rs` proves the *daemon* end (the PTY host) in isolation; this
//! suite proves the missing *client* end: a real editor whose terminal seam is a
//! [`RemoteHostTerm`](bemtvi_server::RemoteHostTerm) talking to a
//! [`serve_term_daemon_on`](bemtvi_server::serve_term_daemon_on) over an in-process duplex. The
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

use bemtvi_rpc::{connect, Incoming, Rpc};
use bemtvi_server::{RemoteHostTerm, ServerInit};
use bemtvi_test_harness::{attach, await_lines_where, buf_lines, feed, spawn, temp_dir};
use rmpv::Value;
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
        let _ = bemtvi_server::serve_term_daemon_on(daemon_rpc, daemon_incoming).await;
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

/// Poll the current buffer until its text contains `needle`. Matches against the lines
/// **concatenated** (no separator) so a needle wrapped across the terminal's fixed-width
/// rows — a long path split at column 80 — is still found.
async fn await_lines_contains(rpc: &Rpc, needle: &str) -> Vec<String> {
    await_lines_where(rpc, |lines| lines.concat().contains(needle)).await
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

/// Parse `"<rows> <cols>"` (an `stty size` line) into its rows, ignoring command echoes /
/// prompts. Requires exactly two integer fields so `sh-3.2$ stty size` (three+ fields) and
/// the bare `stty size` echo (non-numeric) are skipped.
fn parse_stty_rows(line: &str) -> Option<u32> {
    match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        [r, c] => Some((r.parse::<u32>().ok()?, c.parse::<u32>().ok()?).0),
        _ => None,
    }
}

/// Repeatedly run `stty size` in the current (interactive) remote terminal until the PTY's
/// row count reaches `want_min`, returning the last value seen. Re-running each poll is
/// deliberate: a UI resize forwards to the daemon PTY asynchronously (`term_resize` over the
/// wire → SIGWINCH), and each `feed` drives a redraw whose `sync_terminal_sizes` forwards it —
/// so a fresh `stty` eventually observes the grown size once it lands. The *last* two-integer
/// line in the buffer is the newest `stty` output.
async fn poll_stty_rows(rpc: &Rpc, want_min: u32) -> u32 {
    let mut last = 0;
    for _ in 0..120 {
        feed(rpc, "stty size<CR>");
        tokio::time::sleep(Duration::from_millis(30)).await;
        if let Some(r) = buf_lines(rpc, 0)
            .await
            .iter()
            .rev()
            .find_map(|l| parse_stty_rows(l))
        {
            last = r;
            if r >= want_min {
                return r;
            }
        }
    }
    last
}

/// A remote terminal's PTY tracks the editor window size, so a full-screen pager fills the
/// screen instead of showing only a few lines. The child opens at the editor's rows/cols
/// (`term_open`), and a later UI resize is forwarded over the wire (`term_resize`) — so the
/// child's own `stty size` reflects the *current* size, not a stale tiny one. This guards the
/// resize leg: if it weren't forwarded, a terminal opened in a briefly-small window (or before
/// layout settles, as a GUI can) would stay tiny — the "shows only a few lines" report.
#[tokio::test]
async fn remote_terminal_pty_tracks_window_resizes() {
    let dir = temp_dir("remote_term_size");
    let (rpc, _incoming) = spawn_remote_term(&dir.to_string_lossy()).await;

    // Open one interactive shell; the initial 24-row attach yields a ~23-row PTY (a row goes
    // to the status line).
    feed(&rpc, ":terminal sh<CR>");
    let initial = poll_stty_rows(&rpc, 1).await;
    assert!(
        (20..=24).contains(&initial),
        "the remote PTY should open at the attached size (~23 rows for a 24-row UI), got {initial}"
    );

    // Grow the UI to 50 rows; the new size must cross the wire to the daemon PTY.
    rpc.request(
        "btv_ui_try_resize",
        vec![Value::from(120u64), Value::from(50u64), Value::Map(vec![])],
    )
    .await
    .expect("btv_ui_try_resize");

    let resized = poll_stty_rows(&rpc, 45).await;
    assert!(
        resized >= 45,
        "a UI resize must forward to the remote PTY (≈49 rows for a 50-row UI), got {resized} — \
         a terminal stuck near the initial {initial} rows is the 'shows only a few lines' bug"
    );
}

// The remote terminal child's `TERM`/`COLORTERM` (so ncurses pagers work and color is
// enabled even though the daemon has no usable `TERM` of its own) is set by the shared
// `open_pty` — the *daemon's* copy of it — not carried over the wire (`term_open` ships only
// argv/cwd/rows/cols). It is therefore covered rigorously by
// `terminal::terminal_advertises_its_emulator_term_over_a_bogus_ambient_one`, which forces a
// bogus ambient `TERM` (an end-to-end daemon test can't, since the daemon shares the test
// process's inherited environment).
