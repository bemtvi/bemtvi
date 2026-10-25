//! The daemon wire protocol, terminal half (Phase 7 of
//! `docs/plans/2026-06-14-terminal-in-buffer.md`).
//!
//! Proves the **web `:terminal` transport leg** end to end at the wire boundary: the
//! browser's vt100 emulator never runs a PTY itself — it ships `term_open`/`term_write`/
//! `term_resize`/`term_kill` to the daemon, which owns the real
//! [`TerminalManager`](bemtvi_server) PTY engine and streams the child's output back as
//! `term_data` pushes and its exit as `term_exit`. These tests stand in for the browser:
//! they drive [`serve_term_daemon_on`](bemtvi_server::serve_term_daemon_on) directly over an
//! in-process `tokio::io::duplex` (the stand-in for the WebTransport bidi stream), send the
//! same notifications the worker would, and assert on the pushed bytes.
//!
//! Faithful, not a no-op: the child is a real `sh`/`cat` spawned on the daemon side, and the
//! bytes asserted on are its actual PTY output crossing the wire — a stub that echoed the
//! request back could not produce a shell's `hello` or echo interactive input. POSIX commands
//! keep it hermetic (on Windows, which has neither, the always-present `cmd` stands in). PTY
//! output is async, so each assertion polls the pushed notifications with
//! a bounded timeout rather than sleeping a fixed amount.

use std::time::Duration;

use bemtvi_rpc::{connect, Incoming, Rpc};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

/// Stand up a `serve_term_daemon_on` over an in-process duplex and return the *client*
/// side (the role the browser worker plays): an [`Rpc`] to send `term_*` notifications on
/// and its inbound stream to read `term_data`/`term_exit` pushes from. The daemon end runs
/// the real terminal engine on the test runtime.
fn spawn_term_daemon() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (client_end, daemon_end) = tokio::io::duplex(1 << 16);

    let (dr, dw) = tokio::io::split(daemon_end);
    let (daemon_rpc, daemon_incoming) = connect(dr, dw);
    tokio::spawn(async move {
        let _ = bemtvi_server::serve_term_daemon_on(daemon_rpc, daemon_incoming).await;
    });

    let (cr, cw) = tokio::io::split(client_end);
    connect(cr, cw)
}

/// Open a terminal for `buf` running `argv`, sized `rows`×`cols` (no cwd).
fn term_open(rpc: &Rpc, buf: u64, argv: &[&str], rows: u16, cols: u16) {
    rpc.notify(
        "term_open",
        vec![
            Value::from(buf),
            Value::Array(argv.iter().map(|s| Value::from(*s)).collect()),
            Value::Nil,
            Value::from(rows),
            Value::from(cols),
        ],
    );
}

/// The bytes of a `term_data` push, if this notification is one for `buf`.
fn term_data_for(buf: u64, method: &str, params: &[Value]) -> Option<Vec<u8>> {
    if method != "term_data" || params.first().and_then(Value::as_u64) != Some(buf) {
        return None;
    }
    match params.get(1) {
        Some(Value::Binary(b)) => Some(b.clone()),
        Some(Value::String(s)) => Some(s.as_bytes().to_vec()),
        _ => Some(Vec::new()),
    }
}

/// Collect `term_data` for `buf` until its text contains `needle` (returns `Some(exit?)`,
/// the exit code if a `term_exit` already arrived) or the budget runs out (returns `None`).
/// The accumulated text is returned either way so a failure can show what *did* arrive.
///
/// A poll that times out with nothing is the *normal* case while the daemon is still
/// spawning its child — it means keep waiting, not give up. Treating it as the end of the
/// stream collapsed the whole 200 × 50ms budget into the first 50ms, so the test failed
/// (instantly, having never waited) whenever a loaded `cargo test --workspace` made the
/// daemon slower than one poll. Only a *closed* channel ends the wait.
///
/// **`term_exit` is not the end of the stream either.** The daemon deliberately splits the
/// two: `term_data` rides the *backpressured* stream channel (so a slow consumer throttles
/// the child instead of growing an unbounded backlog) while `term_exit` rides the control
/// channel, "so it is delivered promptly even behind a backed-up data stream"
/// (`daemon.rs`, `serve_term_daemon_on`). A short-lived child under load therefore
/// reports its exit *before* its trailing output — by design. Giving up there failed
/// ~20% of loaded runs with an empty `text`, having discarded a `hello` that was still on
/// the wire. Record the code and keep draining; the needle still decides.
async fn await_text(
    incoming: &mut UnboundedReceiver<Incoming>,
    buf: u64,
    needle: &str,
) -> (String, Option<Option<i32>>) {
    let mut text = String::new();
    let mut exited: Option<Option<i32>> = None;
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(50), incoming.recv()).await {
            Ok(Some(Incoming::Notification { method, params })) => {
                if let Some(bytes) = term_data_for(buf, &method, &params) {
                    text.push_str(&String::from_utf8_lossy(&bytes));
                    if text.contains(needle) {
                        return (text, Some(exited.flatten()));
                    }
                } else if method == "term_exit"
                    && params.first().and_then(Value::as_u64) == Some(buf)
                {
                    let code = params.get(1).and_then(Value::as_i64).map(|c| c as i32);
                    if text.contains(needle) {
                        return (text, Some(code));
                    }
                    // Trailing output may still be in flight behind it — hold the code
                    // and keep draining rather than reporting "exited without output".
                    exited = Some(code);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,  // the connection closed — nothing more is coming
            Err(_) => continue, // this poll saw nothing; spend the rest of the budget
        }
    }
    (text, None)
}

/// Wait for `buf`'s `term_exit`, returning its code (or `None` on timeout). An empty poll
/// spends one slice of the budget rather than ending the wait — see [`await_text`].
async fn await_exit(incoming: &mut UnboundedReceiver<Incoming>, buf: u64) -> Option<i32> {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(50), incoming.recv()).await {
            Ok(Some(Incoming::Notification { method, params })) => {
                if method == "term_exit" && params.first().and_then(Value::as_u64) == Some(buf) {
                    return params.get(1).and_then(Value::as_i64).map(|c| c as i32);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    None
}

/// A child's stdout streams back over the wire as `term_data`, and its exit as `term_exit`
/// — the daemon ran a real `sh`, so the `hello` can only be that shell's PTY output.
#[tokio::test]
async fn terminal_output_streams_over_the_daemon_wire() {
    let (rpc, mut incoming) = spawn_term_daemon();
    // Windows has no `sh`; `cmd /c echo` is its always-present one-shot printer.
    let argv: &[&str] = if cfg!(windows) {
        &["cmd", "/c", "echo hello"]
    } else {
        &["sh", "-c", "printf 'hello\\n'"]
    };
    term_open(&rpc, 1, argv, 24, 80);

    let (text, exit) = await_text(&mut incoming, 1, "hello").await;
    assert!(
        exit.is_some(),
        "the daemon must stream the child's output back as term_data; got: {text:?}"
    );

    // The child runs to completion, so its exit crosses the wire too (code 0 for `printf`).
    let code = match exit {
        Some(Some(code)) => Some(code),
        _ => await_exit(&mut incoming, 1).await,
    };
    assert_eq!(
        code,
        Some(0),
        "a clean `printf` exit must arrive as term_exit code 0"
    );
}

/// Interactive input round-trips: writing to the PTY reaches the child, whose echo streams
/// back. A `term_kill` then ends the otherwise-immortal `cat`, proving kill crosses the wire.
#[tokio::test]
async fn terminal_echoes_interactive_input_and_honors_kill() {
    let (rpc, mut incoming) = spawn_term_daemon();
    // Windows has no `cat`; `cmd`'s `type con` echoes console input line by line the same way.
    let argv: &[&str] = if cfg!(windows) {
        &["cmd", "/c", "type con"]
    } else {
        &["cat"]
    };
    term_open(&rpc, 7, argv, 24, 80);

    // `cat` echoes its input back through the PTY; the bytes we write must come back out.
    rpc.notify(
        "term_write",
        vec![Value::from(7u64), Value::Binary(b"world\n".to_vec())],
    );
    let (text, seen) = await_text(&mut incoming, 7, "world").await;
    assert!(
        seen.is_some(),
        "input written over the wire must reach the child and echo back; got: {text:?}"
    );

    // `cat` never exits on its own; the kill must terminate it (the resulting exit arrives).
    rpc.notify("term_kill", vec![Value::from(7u64)]);
    assert!(
        await_exit(&mut incoming, 7).await.is_some(),
        "term_kill must terminate the child and surface its exit"
    );
}

/// Stand up a `serve_term_daemon_on` whose inbound stream the test feeds directly, so the
/// test can end the leg (drop the sender — what a dropped daemon link does to its demuxed
/// `incoming`) and observe what happens to the children it spawned. Returns the sender
/// that drives the leg, and the client side of the reply wire to read `term_data` from.
#[cfg(target_os = "linux")]
fn spawn_term_daemon_fed() -> (
    tokio::sync::mpsc::UnboundedSender<Incoming>,
    UnboundedReceiver<Incoming>,
    Rpc,
) {
    let (client_end, daemon_end) = tokio::io::duplex(1 << 16);
    let (dr, dw) = tokio::io::split(daemon_end);
    let (daemon_rpc, _ignored_inbound) = connect(dr, dw);
    let (feed_tx, feed_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let _keep = _ignored_inbound;
        let _ = bemtvi_server::serve_term_daemon_on(daemon_rpc, feed_rx).await;
    });
    let (cr, cw) = tokio::io::split(client_end);
    let (client_rpc, client_incoming) = connect(cr, cw);
    (feed_tx, client_incoming, client_rpc)
}

#[cfg(target_os = "linux")]
fn fed_open(feed: &tokio::sync::mpsc::UnboundedSender<Incoming>, buf: u64, argv: &[&str]) {
    feed.send(Incoming::Notification {
        method: "term_open".into(),
        params: vec![
            Value::from(buf),
            Value::Array(argv.iter().map(|s| Value::from(*s)).collect()),
            Value::Nil,
            Value::from(24u64),
            Value::from(80u64),
        ],
    })
    .unwrap();
}

/// A shell that reports its pid and then parks forever, so only an explicit kill (or a
/// hangup) can end it.
#[cfg(target_os = "linux")]
const PARKED: &[&str] = &["sh", "-c", "echo PID=$$; exec sleep 300"];

/// Read the `PID=<n>` line a [`PARKED`] child printed.
#[cfg(target_os = "linux")]
async fn parked_pid(incoming: &mut UnboundedReceiver<Incoming>, buf: u64) -> u32 {
    let (text, seen) = await_text(incoming, buf, "\n").await;
    assert!(seen.is_some(), "the parked child never reported: {text:?}");
    let digits: String = text
        .split("PID=")
        .nth(1)
        .unwrap_or("")
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("no pid in {text:?}"))
}

/// Whether `pid` is still a live (non-zombie) process.
#[cfg(target_os = "linux")]
fn alive(pid: u32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_some_and(|state| state != "Z" && state != "X"),
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
async fn await_dead(pid: u32) -> bool {
    for _ in 0..250 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// When the edit-host's link drops, the terminal leg ends — and it must take its PTY
/// children with it. The daemon outlives its connections, so a shell left behind keeps
/// running (and its reader thread stays parked on the PTY) for every disconnect.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_dropped_link_reaps_its_terminal_children() {
    let (feed, mut incoming, _client) = spawn_term_daemon_fed();
    fed_open(&feed, 3, PARKED);
    let pid = parked_pid(&mut incoming, 3).await;
    assert!(alive(pid), "the parked child should be running");

    drop(feed); // the link is gone
    assert!(
        await_dead(pid).await,
        "the terminal child {pid} outlived the connection that spawned it"
    );
}

/// Re-opening a terminal on a buffer id that already has one replaces the session — the
/// previous child must be ended, not leaked behind the new one.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn reopening_a_terminal_id_ends_the_previous_child() {
    let (feed, mut incoming, _client) = spawn_term_daemon_fed();
    fed_open(&feed, 4, PARKED);
    let first = parked_pid(&mut incoming, 4).await;
    fed_open(&feed, 4, PARKED);
    assert!(
        await_dead(first).await,
        "the replaced terminal child {first} was leaked"
    );
    drop(feed);
}
