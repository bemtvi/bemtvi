//! Black-box transport tests: drive the public [`connect`] API over an
//! in-memory duplex pipe, feed raw bytes as a peer would, and assert on
//! observable behavior (does the connection stay up, tear down, or hang?).
//!
//! These exercise the reader's framing robustness — the part that can't be
//! reached through the editor RPC surface, since the server never emits
//! malformed frames.

use std::time::Duration;

use nxvim_rpc::{connect, Incoming};
use rmpv::Value;
use tokio::io::AsyncWriteExt;

/// Wire up an `Rpc` over a duplex pipe and hand back the half a peer writes
/// into (`peer`) plus the stream of incoming messages. The returned `Rpc` and
/// the peer's read half are kept alive by the caller so the connection only
/// ends when the reader itself decides to.
fn rig() -> (
    tokio::io::DuplexStream,
    nxvim_rpc::Rpc,
    tokio::io::DuplexStream,
    tokio::sync::mpsc::UnboundedReceiver<Incoming>,
) {
    // peer -> us: bytes written to `peer` are read by the reader task.
    let (peer, us_reader) = tokio::io::duplex(1 << 20);
    // us -> peer: the writer task drains into `peer_reader` (unused here, but
    // held open so writes never fail).
    let (us_writer, peer_reader) = tokio::io::duplex(1 << 20);
    let (rpc, incoming) = connect(us_reader, us_writer);
    (peer, rpc, peer_reader, incoming)
}

/// A structurally-malformed frame (deeply nested arrays past rmpv's depth
/// limit) must tear the connection down, not be mistaken for a truncated read
/// and re-read forever. Before the fix this test hangs (the reader spins
/// waiting for more bytes that complete a frame that never can), so the timeout
/// elapses and the assert fails. After the fix the reader drops the connection,
/// closing the incoming channel promptly.
#[tokio::test]
async fn malformed_frame_closes_connection_instead_of_hanging() {
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig();

    // 0x91 = fixarray of length 1. A run of them nests past the reader's
    // decode-depth cap, yielding a DepthLimitExceeded decode error — a
    // structural error, distinct from a short/truncated read. (The cap also
    // stops this from overflowing the reader's stack and aborting the process,
    // which is what an uncapped recursive decode would do here.)
    let garbage = vec![0x91u8; 512];
    peer.write_all(&garbage).await.unwrap();

    // `peer` stays in scope (open), so the stream can only end by the reader
    // tearing it down — an EOF would close it regardless of the fix and mask
    // the bug.
    let outcome = tokio::time::timeout(Duration::from_secs(2), incoming.recv()).await;
    assert!(
        matches!(outcome, Ok(None)),
        "expected the connection to be torn down on malformed input, got {outcome:?}"
    );
}

/// A frame split across two reads (a genuinely truncated prefix, then the rest)
/// must still be reassembled and dispatched — i.e. the malformed-frame teardown
/// must not also kill legitimately-incomplete reads. Passes before and after
/// the fix; guards against the fix over-reacting to short reads.
#[tokio::test]
async fn split_frame_is_reassembled_and_dispatched() {
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig();

    // Notification: [2, "ping", []].
    let mut frame = Vec::new();
    rmpv::encode::write_value(
        &mut frame,
        &Value::Array(vec![
            Value::from(2u64),
            Value::from("ping"),
            Value::Array(vec![]),
        ]),
    )
    .unwrap();

    let mid = frame.len() / 2;
    peer.write_all(&frame[..mid]).await.unwrap();
    // Let the reader observe the truncated prefix and (correctly) wait.
    tokio::time::sleep(Duration::from_millis(50)).await;
    peer.write_all(&frame[mid..]).await.unwrap();

    let msg = tokio::time::timeout(Duration::from_secs(2), incoming.recv())
        .await
        .expect("notification should arrive once the frame is complete");
    match msg {
        Some(Incoming::Notification { method, .. }) => assert_eq!(method, "ping"),
        other => panic!("expected ping notification, got {other:?}"),
    }
}
