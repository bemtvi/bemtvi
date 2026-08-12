//! Black-box transport tests: drive the public [`connect`] API over an
//! in-memory duplex pipe, feed raw bytes as a peer would, and assert on
//! observable behavior (does the connection stay up, tear down, or hang?).
//!
//! These exercise the reader's framing robustness — the part that can't be
//! reached through the editor RPC surface, since the server never emits
//! malformed frames.

use std::time::Duration;

use bemtvi_rpc::{connect, Incoming};
use rmpv::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Wire up an `Rpc` over a duplex pipe and hand back the half a peer writes
/// into (`peer`) plus the stream of incoming messages. The returned `Rpc` and
/// the peer's read half are kept alive by the caller so the connection only
/// ends when the reader itself decides to.
fn rig() -> (
    tokio::io::DuplexStream,
    bemtvi_rpc::Rpc,
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

/// A frame whose array length prefix claims a colossal element count must be
/// rejected the instant the header is seen — not buffered toward the frame-size
/// cap while rmpv tries to materialize billions of `Value`s (a tiny crafted
/// message would otherwise amplify into gigabytes of heap and quadratic CPU).
/// Before the allocation-budget scan, the inner array reads as an
/// `UnexpectedEof` (its elements never arrive), so the reader waits forever and
/// this hangs; after, the oversized declared length tears the connection down,
/// closing the incoming channel promptly.
#[tokio::test]
async fn oversized_length_prefix_is_rejected_promptly() {
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig();

    // [2, "x", <array32 claiming 0xFFFFFFFF elements>] — 9 bytes, no payload.
    let bomb = [
        0x93, // fixarray, 3 elements
        0x02, // 2 (notification tag)
        0xa1, 0x78, // fixstr "x"
        0xdd, 0xff, 0xff, 0xff, 0xff, // array32, len = 4_294_967_295
    ];
    peer.write_all(&bomb).await.unwrap();

    // `peer` stays open, so the only way the stream ends is the reader tearing
    // it down on the abusive length — an EOF can't mask the result here.
    let outcome = tokio::time::timeout(Duration::from_secs(2), incoming.recv()).await;
    assert!(
        matches!(outcome, Ok(None)),
        "expected teardown on an oversized length prefix, got {outcome:?}"
    );
}

/// An in-flight `request().await` must resolve to an error when the connection
/// drops, not hang forever. Before the fix the reader exiting on EOF left the
/// request's `oneshot::Sender` sitting in the `pending` map (kept alive by the
/// `Rpc` handle the caller still holds), so the awaiter blocked indefinitely;
/// the timeout elapses and the test fails. After the fix the teardown drains
/// `pending`, dropping the sender so the receiver resolves to `Err`.
#[tokio::test]
async fn in_flight_request_fails_when_the_connection_drops() {
    let (peer, rpc, peer_reader, _incoming) = rig();

    // Fire a request the peer will never answer.
    let handle = tokio::spawn(async move { rpc.request("never_answered", vec![]).await });

    // Let it register in `pending` and flush, then drop both peer halves so the
    // reader hits EOF (and the writer's next write fails).
    tokio::time::sleep(Duration::from_millis(50)).await;
    drop(peer);
    drop(peer_reader);

    let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
    match outcome {
        Ok(Ok(Err(_))) => {} // resolved to an error — correct
        Ok(Ok(Ok(v))) => panic!("request unexpectedly succeeded: {v:?}"),
        Ok(Err(join)) => panic!("request task panicked: {join:?}"),
        Err(_) => panic!("request hung after the connection dropped instead of erroring"),
    }
}

/// A `request()` racing connection teardown must resolve to an error, never
/// hang. The teardown task drains `pending` exactly once; a request that
/// registers its oneshot *after* that drain — while the aborted writer task is
/// still mid-poll on another worker, so the outbound channel still accepts the
/// frame — parks forever on a sender nothing will ever complete or drop. The
/// window only opens on a multi-thread runtime with the writer busy (an idle
/// writer is dropped synchronously inside `abort()`, closing the channel before
/// the drain), so this floods the writer with notifications while hammering
/// requests across many teardowns and requires every request to resolve. Before
/// the fix some iteration's request lands in the drain-to-writer-death window
/// and times out; after the fix the closed marker under the `pending` lock
/// fails it fast in every interleaving.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_racing_teardown_errors_instead_of_hanging() {
    for iter in 0..400u32 {
        let (peer, rpc, mut peer_reader, _incoming) = rig();

        // Drain the peer side so the writer's `write_all`s stay Ready — a
        // writer parked on a full pipe is idle, and an idle writer is aborted
        // synchronously, which closes the window this test exists to hit.
        let drain = tokio::spawn(async move {
            let mut sink = [0u8; 1 << 16];
            while peer_reader.read(&mut sink).await.unwrap_or(0) > 0 {}
        });

        // Keep the writer task busy: an unbounded stream of notifications keeps
        // its coalescing drain loop spinning inside a single long poll, so the
        // teardown's `abort()` lands mid-poll and defers the writer's death.
        let flood_rpc = rpc.clone();
        let flood = tokio::spawn(async move {
            let payload = vec![Value::from("x".repeat(512))];
            loop {
                for _ in 0..16 {
                    flood_rpc.notify("flood", payload.clone());
                }
                tokio::task::yield_now().await;
            }
        });

        // Hammer requests through the teardown. The peer never answers, so
        // every request must resolve to an error — either drained at teardown
        // or refused on the dead channel. A timeout is the hang.
        let req_rpc = rpc.clone();
        let requester = tokio::spawn(async move {
            let mut errors = 0u32;
            while errors < 8 {
                match tokio::time::timeout(Duration::from_secs(2), req_rpc.request("r", vec![]))
                    .await
                {
                    Ok(Err(_)) => errors += 1, // resolved — correct
                    Ok(Ok(v)) => panic!("request unexpectedly succeeded: {v:?}"),
                    Err(_) => return true, // hung in the teardown window
                }
            }
            false
        });

        // Let the flood and requester spin up, then kill the peer->us pipe so
        // the reader EOFs and teardown races the busy writer.
        tokio::time::sleep(Duration::from_micros(200)).await;
        drop(peer);

        let hung = requester.await.expect("requester task panicked");
        flood.abort();
        drain.abort();
        assert!(!hung, "iteration {iter}: request racing teardown hung");
    }
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

/// A burst of outbound notifications must all reach the peer, intact and in
/// order. The writer task coalesces a queued burst into one drained batch and a
/// single flush; this guards that batching against dropping or reordering
/// frames (the editing suites exercise it incidentally via redraw bursts, but
/// nothing else asserts writer-side delivery directly).
#[tokio::test]
async fn writer_delivers_a_burst_of_notifications_in_order() {
    let (_peer, rpc, mut peer_reader, _incoming) = rig();

    // Fire the burst synchronously: every frame is queued before the writer
    // task wakes, so it drains them as one coalesced batch.
    const N: u64 = 64;
    for i in 0..N {
        rpc.notify("tick", vec![Value::from(i)]);
    }

    // Read the written bytes and decode complete frames until we have all N (or
    // time out). `buf` only ever holds the not-yet-decoded tail between reads.
    let mut buf: Vec<u8> = Vec::new();
    let mut decoded: Vec<Value> = Vec::new();
    let mut chunk = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while (decoded.len() as u64) < N {
        let n = tokio::time::timeout_at(deadline, peer_reader.read(&mut chunk))
            .await
            .expect("writer should deliver the burst before the deadline")
            .expect("peer read");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut cursor = std::io::Cursor::new(&buf);
        // Decode whole frames; a partial trailing frame leaves `read_value`
        // mid-stream, so drain only up to the last *complete* frame and keep the
        // remainder for the next read.
        let mut complete = 0u64;
        while let Ok(v) = rmpv::decode::read_value(&mut cursor) {
            decoded.push(v);
            complete = cursor.position();
        }
        buf.drain(..complete as usize);
    }

    assert_eq!(
        decoded.len() as u64,
        N,
        "every frame in the burst was delivered"
    );
    for (i, v) in decoded.iter().enumerate() {
        // Each frame is the notification [2, "tick", [i]] — index preserved.
        let arr = v.as_array().expect("array frame");
        assert_eq!(arr[0].as_u64(), Some(2), "notification tag");
        assert_eq!(arr[1].as_str(), Some("tick"));
        assert_eq!(
            arr[2].as_array().expect("params")[0].as_u64(),
            Some(i as u64)
        );
    }
}
