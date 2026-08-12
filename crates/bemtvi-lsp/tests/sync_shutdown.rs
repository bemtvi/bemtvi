//! Black-box wire-contract test for the wasm-side [`SyncLspClient`] teardown.
//! The client is compiled only with `native` off (a documented decision), so
//! run this suite with `cargo test -p bemtvi-lsp --no-default-features` — under
//! the default features it compiles to an empty binary.
//!
//! `shutdown()` must perform the documented graceful stop — the `shutdown`
//! *request* then the `exit` notification, then the kill — mirroring the native
//! serve loop (`socket.shutdown(()) → exit → kill`). The client once sent only
//! `exit`, so a spec-conforming server was killed without ever seeing the
//! request its comment claimed it got.
#![cfg(not(feature = "native"))]

use std::path::PathBuf;

use bemtvi_lsp::{ServerKey, ServerSpawn, SyncLspClient, WireOp};

/// The JSON-RPC bodies of the `Stdin` wire ops, in order (framing stripped by
/// splitting at the header terminator).
fn stdin_bodies(ops: &[WireOp]) -> Vec<String> {
    ops.iter()
        .filter_map(|op| match op {
            WireOp::Stdin { bytes, .. } => {
                let s = String::from_utf8_lossy(bytes);
                s.split_once("\r\n\r\n").map(|(_, body)| body.to_string())
            }
            _ => None,
        })
        .collect()
}

#[test]
fn shutdown_sends_the_shutdown_request_before_exit_and_kill() {
    let mut client = SyncLspClient::new();
    let key = ServerKey {
        name: "mock".into(),
        root: Some(PathBuf::from("/tmp")),
    };
    client.ensure_server(
        key.clone(),
        ServerSpawn {
            program: "mock-ls".into(),
            ..ServerSpawn::default()
        },
    );
    let _ = client.take_wire_ops(); // Spawn + initialize — not under test.

    client.shutdown(key);
    let ops = client.take_wire_ops();

    let bodies = stdin_bodies(&ops);
    let shutdown_at = bodies.iter().position(|b| {
        b.contains("\"method\":\"shutdown\"") || b.contains("\"method\": \"shutdown\"")
    });
    let exit_at = bodies
        .iter()
        .position(|b| b.contains("\"method\":\"exit\"") || b.contains("\"method\": \"exit\""));
    assert!(
        shutdown_at.is_some(),
        "the shutdown request must be sent (got stdin bodies: {bodies:?})"
    );
    assert!(exit_at.is_some(), "the exit notification must be sent");
    assert!(
        shutdown_at < exit_at,
        "shutdown precedes exit (shutdown at {shutdown_at:?}, exit at {exit_at:?})"
    );
    assert!(
        matches!(ops.last(), Some(WireOp::Kill { .. })),
        "the kill closes the sequence, got {:?}",
        ops.last()
    );
}
