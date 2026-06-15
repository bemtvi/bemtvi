//! The daemon wire protocol, terminal half (Phase 7 of
//! `docs/plans/2026-06-14-terminal-in-buffer.md`).
//!
//! Proves the **web `:terminal` transport leg** end to end at the wire boundary: the
//! browser's vt100 emulator never runs a PTY itself — it ships `term_open`/`term_write`/
//! `term_resize`/`term_kill` to the daemon, which owns the real
//! [`TerminalManager`](nxvim_server) PTY engine and streams the child's output back as
//! `term_data` pushes and its exit as `term_exit`. These tests stand in for the browser:
//! they drive [`serve_term_daemon_on`](nxvim_server::serve_term_daemon_on) directly over an
//! in-process `tokio::io::duplex` (the stand-in for the WebTransport bidi stream), send the
//! same notifications the worker would, and assert on the pushed bytes.
//!
//! Faithful, not a no-op: the child is a real `sh`/`cat` spawned on the daemon side, and the
//! bytes asserted on are its actual PTY output crossing the wire — a stub that echoed the
//! request back could not produce a shell's `hello` or echo interactive input. POSIX commands
//! keep it hermetic. PTY output is async, so each assertion polls the pushed notifications with
//! a bounded timeout rather than sleeping a fixed amount.

use std::time::Duration;

use nxvim_rpc::{connect, Incoming, Rpc};
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
        let _ = nxvim_server::serve_term_daemon_on(daemon_rpc, daemon_incoming).await;
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
async fn await_text(
    incoming: &mut UnboundedReceiver<Incoming>,
    buf: u64,
    needle: &str,
) -> (String, Option<Option<i32>>) {
    let mut text = String::new();
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(50), incoming.recv()).await {
            Ok(Some(Incoming::Notification { method, params })) => {
                if let Some(bytes) = term_data_for(buf, &method, &params) {
                    text.push_str(&String::from_utf8_lossy(&bytes));
                    if text.contains(needle) {
                        return (text, Some(None));
                    }
                } else if method == "term_exit"
                    && params.first().and_then(Value::as_u64) == Some(buf)
                {
                    let code = params.get(1).and_then(Value::as_i64).map(|c| c as i32);
                    if text.contains(needle) {
                        return (text, Some(code));
                    }
                    return (text, None); // exited without ever producing the needle
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break, // closed or this poll timed out with nothing
        }
    }
    (text, None)
}

/// Wait for `buf`'s `term_exit`, returning its code (or `None` on timeout).
async fn await_exit(incoming: &mut UnboundedReceiver<Incoming>, buf: u64) -> Option<i32> {
    for _ in 0..200 {
        match tokio::time::timeout(Duration::from_millis(50), incoming.recv()).await {
            Ok(Some(Incoming::Notification { method, params })) => {
                if method == "term_exit" && params.first().and_then(Value::as_u64) == Some(buf) {
                    return params.get(1).and_then(Value::as_i64).map(|c| c as i32);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    None
}

/// A child's stdout streams back over the wire as `term_data`, and its exit as `term_exit`
/// — the daemon ran a real `sh`, so the `hello` can only be that shell's PTY output.
#[tokio::test]
async fn terminal_output_streams_over_the_daemon_wire() {
    let (rpc, mut incoming) = spawn_term_daemon();
    term_open(&rpc, 1, &["sh", "-c", "printf 'hello\\n'"], 24, 80);

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
    term_open(&rpc, 7, &["cat"], 24, 80);

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
