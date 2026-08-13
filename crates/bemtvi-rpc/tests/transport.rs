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

// ============================================================ resumable framing
//
// The reader used to re-walk the whole buffered prefix on every read, so a large
// frame arriving in many small reads cost O(bytes x reads) marker walking — a
// 64 MiB frame in 8 KiB reads re-walks ~256 GiB. The walk is now resumable: it
// rewinds only to the start of the one value whose bytes have not all arrived, so
// each read re-parses at most that value's header.

/// One `[2, "ping", [<n small ints>]]` notification frame.
fn wide_notification(n: usize) -> Vec<u8> {
    let mut frame = Vec::new();
    rmpv::encode::write_value(
        &mut frame,
        &Value::Array(vec![
            Value::from(2u64),
            Value::from("ping"),
            Value::Array((0..n).map(|i| Value::from(i as u64)).collect()),
        ]),
    )
    .unwrap();
    frame
}

#[tokio::test]
async fn a_large_frame_split_into_many_reads_still_decodes() {
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig();
    // Wide, not deep: the value budget is what the resumed walk carries across
    // reads, so a frame with many values is the one that exercises it.
    let frame = wide_notification(200_000);

    // Chunks small enough that the frame spans hundreds of reads.
    for chunk in frame.chunks(4096) {
        peer.write_all(chunk).await.unwrap();
    }
    let msg = tokio::time::timeout(Duration::from_secs(20), incoming.recv())
        .await
        .expect("the reassembled frame must be dispatched");
    match msg {
        Some(Incoming::Notification { method, params }) => {
            assert_eq!(method, "ping");
            // `params` is the frame's third element, already unwrapped into its
            // elements — so its length is the array's.
            assert_eq!(
                params.len(),
                200_000,
                "every element survived the split reads"
            );
            assert_eq!(
                params.last().and_then(Value::as_u64),
                Some(199_999),
                "and the tail is intact, not truncated at a chunk boundary"
            );
        }
        other => panic!("expected the ping notification, got {other:?}"),
    }
}

#[tokio::test]
async fn one_big_frame_costs_about_the_same_as_the_same_bytes_in_whole_frames() {
    // The perf guard for the resumable walk, shaped so a regression cannot hide.
    //
    // Comparing a coarse against a fine delivery of the SAME frame does not work:
    // a non-resumable walk re-walks the buffered prefix in both (even a "coarse"
    // write arrives in several reads), so both arms slow down together and the
    // ratio between them barely moves.
    //
    // The baseline that cannot re-walk is the same total bytes delivered as many
    // WHOLE frames — each completes in the read that carries it, so there is no
    // buffered prefix to re-scan. Against that, one big frame trickled in small
    // chunks is O(bytes) when the walk resumes and O(bytes x reads) when it does
    // not. The budget is a ratio, so it does not encode this machine's speed.
    async fn deliver(frames: &[Vec<u8>], chunk: usize, expect: usize) -> Duration {
        let (mut peer, _rpc, _peer_reader, mut incoming) = rig();
        let start = std::time::Instant::now();
        for f in frames {
            for c in f.chunks(chunk) {
                peer.write_all(c).await.unwrap();
            }
        }
        for _ in 0..expect {
            tokio::time::timeout(Duration::from_secs(60), incoming.recv())
                .await
                .expect("every frame dispatched")
                .expect("a message");
        }
        start.elapsed()
    }

    // ~1.4 MB either way: 40 whole frames, or one frame of the same total size.
    let small: Vec<Vec<u8>> = (0..40).map(|_| wide_notification(9_000)).collect();
    let total: usize = small.iter().map(Vec::len).sum();
    let big = vec![wide_notification(360_000)];
    assert!(
        big[0].len() * 2 > total && total * 2 > big[0].len(),
        "the two deliveries must carry comparable bytes ({} vs {total})",
        big[0].len()
    );

    let whole = deliver(&small, usize::MAX, small.len()).await;
    // 2 KiB chunks: the big frame spans hundreds of reads.
    let trickled = deliver(&big, 2048, 1).await;

    let budget = whole.max(Duration::from_millis(20)) * 6;
    assert!(
        trickled < budget,
        "one big frame in small reads cost {trickled:?} against {whole:?} for the \
         same bytes in whole frames (budget {budget:?}) — the frame scan is being \
         restarted from the frame head on every read"
    );
}

#[tokio::test]
async fn a_frame_nested_past_the_depth_budget_is_rejected() {
    // An over-deep frame is refused, whichever layer refuses it. NOTE the limit of
    // what a black-box test can claim here: the scan's own depth pre-check has no
    // observable of its own — rmpv is depth-capped too, so a frame that slips past
    // the scan is torn down a moment later by the decode error instead. What the
    // pre-check buys is refusing it *before* buffering toward it, which the wire
    // cannot show. The per-path CHARGE MODEL is the part with real consequences,
    // and `a_wide_but_shallow_frame_is_not_mistaken_for_a_deep_one` is its guard.
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig();
    // 200 nested single-element arrays (`0x91` = fixarray of 1): 400 depth units,
    // past the 128 budget. `0xc0` is the nil at the bottom.
    let mut frame = vec![0x91u8; 200];
    frame.push(0xc0);
    peer.write_all(&frame).await.unwrap();

    let closed = tokio::time::timeout(Duration::from_secs(5), incoming.recv())
        .await
        .expect("the reader must tear down, not hang on the over-deep frame");
    assert!(
        closed.is_none(),
        "an over-budget frame closes the connection, got {closed:?}"
    );
}

#[tokio::test]
async fn a_wide_but_shallow_frame_is_not_mistaken_for_a_deep_one() {
    // The counterpart: the budget is per path, so siblings share their parent's
    // allowance. A flat frame of any width must pass — a frame-cumulative charge
    // would reject ordinary traffic.
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig();
    peer.write_all(&wide_notification(50_000)).await.unwrap();
    let msg = tokio::time::timeout(Duration::from_secs(10), incoming.recv())
        .await
        .expect("a wide frame is legitimate")
        .expect("a message");
    match msg {
        Incoming::Notification { method, .. } => assert_eq!(method, "ping"),
        other => panic!("expected the ping notification, got {other:?}"),
    }
}

// ====================================================== the sender's frame budget
//
// The reader rejects any frame past MAX_FRAME by tearing the WHOLE connection
// down — taking every unrelated in-flight request with it. So the sender refuses
// an over-large frame per call instead of shipping it: an `Err` for `request`, an
// error reply for `respond`, a dropped frame plus a stderr line for `notify`.

/// A `Value` that encodes to more than `MAX_FRAME` (64 MiB) bytes.
fn oversized_value() -> Value {
    Value::Binary(vec![0u8; 65 * 1024 * 1024])
}

#[tokio::test]
async fn an_oversized_request_errors_instead_of_killing_the_link() {
    let (_peer, rpc, _peer_reader, mut incoming) = rig();
    // Bounded: a regression SHIPS the frame, and the writer then blocks forever
    // filling a pipe nobody drains — so the refusal must be observed on a clock,
    // or the failure mode is a hung suite rather than a red test.
    let err = tokio::time::timeout(
        Duration::from_secs(10),
        rpc.request("huge", vec![oversized_value()]),
    )
    .await
    .expect("the refusal must be immediate — an over-large frame must never be sent")
    .expect_err("an over-large request must be refused");
    assert!(
        err.to_string().contains("too large"),
        "the refusal must name the real cause, got {err}"
    );
    // The link is still up: a normal notification still reaches the peer's reader
    // (and nothing has closed `incoming`).
    assert!(
        tokio::time::timeout(Duration::from_millis(200), incoming.recv())
            .await
            .is_err(),
        "the connection must stay open after a refused frame"
    );
}

#[tokio::test]
async fn an_oversized_notification_is_dropped_and_the_link_survives() {
    let (_peer, rpc, mut peer_reader, mut incoming) = rig();
    rpc.notify("huge", vec![oversized_value()]);
    // …then a small one, which must arrive: the drop is per-frame, not a teardown.
    rpc.notify("small", vec![Value::from(1u64)]);

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), peer_reader.read(&mut buf))
        .await
        .expect("the small notification must reach the peer")
        .expect("read");
    let v = rmpv::decode::read_value(&mut &buf[..n]).expect("a frame");
    let arr = v.as_array().expect("array");
    assert_eq!(
        arr.get(1).and_then(Value::as_str),
        Some("small"),
        "the oversized frame must never reach the wire; the next one must"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), incoming.recv())
            .await
            .is_err(),
        "the connection must stay open"
    );
}

#[tokio::test]
async fn an_oversized_response_answers_with_an_error_not_a_teardown() {
    let (_peer, rpc, mut peer_reader, _incoming) = rig();
    // Answer request id 7 with a payload no frame can carry.
    rpc.respond(7, Ok(oversized_value()));

    let mut buf = vec![0u8; 8192];
    let n = tokio::time::timeout(Duration::from_secs(5), peer_reader.read(&mut buf))
        .await
        .expect("a reply must still be sent")
        .expect("read");
    let v = rmpv::decode::read_value(&mut &buf[..n]).expect("a frame");
    let arr = v.as_array().expect("array");
    assert_eq!(arr.first().and_then(Value::as_u64), Some(1), "a response");
    assert_eq!(arr.get(1).and_then(Value::as_u64), Some(7), "for id 7");
    let err = arr.get(2).and_then(Value::as_str).unwrap_or_default();
    assert!(
        err.contains("exceeds"),
        "the requester must get a normal RPC error naming the cause, got {err:?}"
    );
    assert_eq!(arr.get(3), Some(&Value::Nil), "and no result");
}

// ============================================ a cancelled request is not mis-answered
//
// A `request().await` dropped before its response arrives (a caller-side timeout)
// used to leave its oneshot in the pending map until the response came or the
// link tore down. The entry is now removed when the future drops — and, whatever
// the bookkeeping, a late reply for a cancelled id must never be handed to a
// LATER request.

#[tokio::test]
async fn a_late_reply_for_a_cancelled_request_is_not_delivered_to_the_next_one() {
    let (mut peer, rpc, mut peer_reader, _incoming) = rig();

    // Issue a request and abandon it before any reply.
    let cancelled = rpc.clone();
    let handle = tokio::spawn(async move { cancelled.request("first", vec![]).await });
    // Read its frame off the wire to learn the id the client assigned.
    let mut buf = vec![0u8; 4096];
    let n = peer_reader.read(&mut buf).await.expect("the request frame");
    let sent = rmpv::decode::read_value(&mut &buf[..n]).expect("a frame");
    let first_id = sent
        .as_array()
        .and_then(|a| a.get(1)?.as_u64())
        .expect("id");
    handle.abort();
    let _ = handle.await;

    // Now a second request, which must get its OWN answer.
    let rpc2 = rpc.clone();
    let second = tokio::spawn(async move { rpc2.request("second", vec![]).await });
    let n = peer_reader.read(&mut buf).await.expect("the second frame");
    let sent = rmpv::decode::read_value(&mut &buf[..n]).expect("a frame");
    let second_id = sent
        .as_array()
        .and_then(|a| a.get(1)?.as_u64())
        .expect("id");
    assert_ne!(first_id, second_id, "ids are not reused");

    // Answer the CANCELLED id first, then the live one.
    for (id, payload) in [(first_id, "stale"), (second_id, "fresh")] {
        let mut frame = Vec::new();
        rmpv::encode::write_value(
            &mut frame,
            &Value::Array(vec![
                Value::from(1u64),
                Value::from(id),
                Value::Nil,
                Value::from(payload),
            ]),
        )
        .unwrap();
        peer.write_all(&frame).await.unwrap();
    }

    let got = tokio::time::timeout(Duration::from_secs(5), second)
        .await
        .expect("the live request must resolve")
        .expect("join")
        .expect("ok");
    assert_eq!(
        got.as_str(),
        Some("fresh"),
        "the live request must get its own reply, never the cancelled one's"
    );
}

// ---------------------------------------------------------------------------
// Inbound backpressure, and the reason it is opt-in
// ---------------------------------------------------------------------------
//
// `connect`'s inbound queue is unbounded: the reader decodes as fast as the peer
// sends and never blocks. That is what a request/response leg needs — the reader
// is also the sole deliverer of a pending `request`'s response, so a reader
// parked on a full queue strands the very reply its caller is awaiting.
//
// It is also, on a notification-only leg, a way for the peer to choose how much
// memory we spend: a remote terminal child spraying `term_data` accumulates until
// something dies, and is never told to slow down. `connect_bounded` is that leg's
// answer — the reader `await`s the push, stops draining the wire, and the peer is
// backpressured end to end.
//
// Both halves of that trade-off are pinned here, because picking the wrong one is
// silent in each direction: an unbounded flood shows up as memory, and a bounded
// request/response leg shows up as a hang.

/// Cap used by the bounded rigs below — small, so the flood reaches it quickly.
const TEST_CAP: usize = 64;

/// A rig over a deliberately *small* duplex, so the pipe's own buffer cannot
/// stand in for the queue: with a megabyte of slack the peer could park thousands
/// of frames in the socket and never notice the gate behind it.
fn rig_bounded() -> (
    tokio::io::DuplexStream,
    bemtvi_rpc::Rpc,
    tokio::io::DuplexStream,
    tokio::sync::mpsc::Receiver<Incoming>,
) {
    let (peer, us_reader) = tokio::io::duplex(512);
    let (us_writer, peer_reader) = tokio::io::duplex(1 << 16);
    let (rpc, rx) = bemtvi_rpc::connect_bounded(us_reader, us_writer, TEST_CAP);
    (peer, rpc, peer_reader, rx)
}

/// The same, unbounded.
fn rig_unbounded_small() -> (
    tokio::io::DuplexStream,
    bemtvi_rpc::Rpc,
    tokio::io::DuplexStream,
    tokio::sync::mpsc::UnboundedReceiver<Incoming>,
) {
    let (peer, us_reader) = tokio::io::duplex(512);
    let (us_writer, peer_reader) = tokio::io::duplex(1 << 16);
    let (rpc, rx) = connect(us_reader, us_writer);
    (peer, rpc, peer_reader, rx)
}

/// One `[2, "ping", []]` notification frame.
fn ping_frame() -> Vec<u8> {
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
    frame
}

/// Write frames until one blocks (backpressure reached) or `cap` are through.
/// Returns how many were accepted.
async fn flood(peer: &mut tokio::io::DuplexStream, cap: usize) -> usize {
    let frame = ping_frame();
    let mut sent = 0;
    while sent < cap {
        match tokio::time::timeout(Duration::from_millis(250), peer.write_all(&frame)).await {
            Ok(Ok(())) => sent += 1,
            // Blocked: the reader has stopped taking bytes off the socket.
            _ => break,
        }
    }
    sent
}

/// The frame budget a flood is measured against — an order of magnitude past the
/// queue's depth, so "did not stop" is unambiguous.
const FLOOD_CAP: usize = TEST_CAP * 16;

/// A peer flooding a bounded leg is throttled: once the consumer's queue is full
/// the reader stops draining the socket, so the peer's writes block. The point is
/// not the exact number — it is that a bound exists, and that it sits near the
/// queue's depth rather than near the peer's appetite.
#[tokio::test]
async fn a_flood_on_a_bounded_leg_backpressures_the_peer() {
    let (mut peer, _rpc, _peer_reader, _incoming) = rig_bounded();
    let sent = flood(&mut peer, FLOOD_CAP).await;
    assert!(
        sent < FLOOD_CAP,
        "the peer pushed {sent} frames with nothing draining them — an unbounded \
         inbound queue is exactly the memory the flood gate exists to deny"
    );
    // The gate should engage near the queue's depth, not thousands of frames
    // later. The slack covers the socket buffer and the frame in the reader's hand.
    assert!(
        sent <= TEST_CAP + 200,
        "backpressure engaged only after {sent} frames; the queue holds {TEST_CAP} — \
         something else is buffering without limit"
    );
}

/// The contrast, on the same rig: an *unbounded* leg never pushes back. This is
/// what every request/response leg still is, deliberately — and what makes the
/// bounded variant a decision rather than a default.
#[tokio::test]
async fn an_unbounded_leg_does_not_push_back() {
    let (mut peer, _rpc, _peer_reader, _incoming) = rig_unbounded_small();
    let sent = flood(&mut peer, FLOOD_CAP).await;
    assert_eq!(
        sent, FLOOD_CAP,
        "an unbounded queue should have absorbed every frame — if this stops early \
         the test's own flood is what broke, not the bound"
    );
}

/// Backpressure is a pause, not a break: once the consumer drains, the leg keeps
/// delivering, in order, with nothing lost. (A gate that dropped frames — or
/// killed the connection — would be a worse bug than the one it replaced.)
#[tokio::test]
async fn a_throttled_leg_resumes_once_the_consumer_drains() {
    let (mut peer, _rpc, _peer_reader, mut incoming) = rig_bounded();
    let sent = flood(&mut peer, FLOOD_CAP).await;
    assert!(sent < FLOOD_CAP, "expected the flood to be throttled");

    let mut drained = 0;
    for _ in 0..FLOOD_CAP {
        // The reader needs turns to refill the queue as we empty it.
        tokio::task::yield_now().await;
        while incoming.try_recv().is_ok() {
            drained += 1;
        }
        if drained >= sent {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        drained, sent,
        "every frame the peer got through must still be delivered — a bounded \
         queue may delay a message, never drop one"
    );

    // And the connection is still live: more frames flow.
    let more = flood(&mut peer, 8).await;
    assert_eq!(
        more, 8,
        "the leg must keep accepting once the consumer catches up"
    );
}

/// **Why the bound is opt-in.** The reader is the only thing that delivers a
/// pending `request`'s response, so on a leg where the queue can fill, a flood of
/// notifications parks the reader and the reply that is already on the wire is
/// never decoded — the caller waits forever. An unbounded leg cannot do that,
/// which is exactly why every request/response leg keeps one.
///
/// Here: a request goes out, then the peer floods far past any bounded cap
/// *without* the consumer draining, and only then answers. On the unbounded leg
/// the answer still arrives. Run the same shape over a bounded leg and this hangs
/// — which is the hazard, and the reason `connect_bounded` is documented as
/// notification-only.
#[tokio::test]
async fn a_response_still_arrives_behind_an_undrained_flood_on_an_unbounded_leg() {
    let (mut peer, rpc, mut peer_reader, _incoming) = rig_unbounded_small();

    let pending = tokio::spawn(async move { rpc.request("slow", vec![]).await });

    // Read the request off the wire so the peer knows its id.
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_secs(5), peer_reader.read(&mut buf))
        .await
        .expect("the request should reach the peer")
        .expect("read");
    let req: Value = rmpv::decode::read_value(&mut &buf[..n]).expect("decode");
    let id = req.as_array().expect("array")[1].as_u64().expect("id");

    // Flood well past any bounded cap, with nothing draining the inbound queue.
    let sent = flood(&mut peer, TEST_CAP * 8).await;
    assert_eq!(
        sent,
        TEST_CAP * 8,
        "the unbounded leg must swallow the flood — that is the property under test"
    );

    // Now answer. The reply sits *behind* every one of those notifications.
    let mut reply = Vec::new();
    rmpv::encode::write_value(
        &mut reply,
        &Value::Array(vec![
            Value::from(1u64),
            Value::from(id),
            Value::Nil,
            Value::from("answered"),
        ]),
    )
    .unwrap();
    peer.write_all(&reply).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("the response must be delivered from behind the flood")
        .expect("join")
        .expect("ok");
    assert_eq!(got.as_str(), Some("answered"));
}
