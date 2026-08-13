//! Async msgpack-RPC transport. msgpack is used purely as a compact binary
//! framing — this is bemtvi's own protocol, not a neovim-compatible channel.
//!
//! Messages are msgpack arrays:
//! * Request:      `[0, msgid, method, params]`
//! * Response:     `[1, msgid, error, result]`
//! * Notification: `[2, method, params]`
//!
//! [`connect`] drives any [`AsyncRead`]/[`AsyncWrite`] pair (an in-process
//! duplex for the embedded server, a socket/stdio for external clients) and
//! returns an [`Rpc`] handle plus a stream of [`Incoming`] messages. The handle
//! is cheap to clone and `Send + Sync`, so the reader/writer run as independent
//! tasks and never block the consumer.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rmpv::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};

/// A message arriving from the peer that the consumer must act on.
#[derive(Debug)]
pub enum Incoming {
    Request {
        id: u64,
        method: String,
        params: Vec<Value>,
    },
    Notification {
        method: String,
        params: Vec<Value>,
    },
}

type PendingMap = HashMap<u64, oneshot::Sender<std::result::Result<Value, Value>>>;

/// In-flight requests awaiting a response, keyed by msgid. `None` once the
/// connection has been torn down: the cleanup task in [`connect`] `take`s the
/// map exactly once (failing every outstanding request), so a [`Rpc::request`]
/// racing that teardown must be refused at registration — its frame can still
/// be accepted by the outbound channel (the aborted writer may outlive the
/// drain by a poll), but nothing would ever resolve or drop its oneshot.
type Pending = Arc<Mutex<Option<PendingMap>>>;

/// Removes a request's pending-map entry when the caller's future is dropped
/// before the response arrives (cancellation). Without this guard, a cancelled
/// `request().await` (e.g. a caller-side timeout) whose response never comes
/// would leave its oneshot sender in the map until the response arrived or the
/// connection tore down — unbounded growth under repeated cancels on a
/// long-lived connection whose peer never answers. The removal is a no-op on
/// every completed path: [`dispatch`] removes the entry *before* resolving the
/// await, and teardown `take`s the whole map.
struct PendingGuard {
    pending: Pending,
    id: u64,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if let Some(map) = self.pending.lock().unwrap().as_mut() {
            map.remove(&self.id);
        }
    }
}

/// Bounded capacity of the [`Rpc::notify_stream`] channel — how many bulk frames
/// (e.g. terminal PTY chunks) may sit queued for the writer before a producer
/// `await`s. Small on purpose: it is the *backpressure window*, the amount of
/// high-volume data allowed in flight ahead of the consumer. Too large defeats the
/// point (a flood buffers without bound, so a `^C` keeps draining for seconds); too
/// small thrashes. ~4 PTY chunks ≈ 32 KiB is enough to keep the wire busy while
/// keeping the post-cancel drain sub-second.
const STREAM_CAP: usize = 4;

/// A cloneable handle for sending requests, notifications and responses.
#[derive(Clone)]
pub struct Rpc {
    out: mpsc::UnboundedSender<Vec<u8>>,
    /// Backpressured channel for bulk one-way streaming ([`Rpc::notify_stream`]).
    /// Bounded, so a producer faster than the writer is throttled rather than
    /// queuing without limit. Drained by the writer task at lower priority than
    /// `out` (control frames go first).
    stream: mpsc::Sender<Vec<u8>>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl Rpc {
    /// Fire-and-forget notification.
    pub fn notify(&self, method: &str, params: Vec<Value>) {
        let bytes = notification(method, params);
        if bytes.len() > MAX_FRAME {
            eprintln!(
                "bemtvi-rpc: dropping notification '{method}': encoded frame is {} bytes, \
                 over the {MAX_FRAME}-byte limit",
                bytes.len()
            );
            return;
        }
        let _ = self.out.send(bytes);
    }

    /// Backpressured notification for **bulk, one-way streaming** data — terminal
    /// PTY output, specifically. Unlike [`notify`](Self::notify) (which queues
    /// without bound and returns at once), this `await`s when the writer is behind,
    /// so the producer is throttled to the consumer's drain rate instead of letting
    /// a flood pile up unbounded. That backpressure is what lets a `^C` actually
    /// stop a runaway command: only [`STREAM_CAP`] frames are ever in flight, so the
    /// queued-but-undrained backlog stays tiny. Resolves (dropping the frame) if the
    /// connection has closed. Use only for high-volume data that can tolerate pacing;
    /// control/latency-sensitive frames must use [`notify`](Self::notify).
    pub async fn notify_stream(&self, method: &str, params: Vec<Value>) {
        let bytes = notification(method, params);
        if bytes.len() > MAX_FRAME {
            eprintln!(
                "bemtvi-rpc: dropping stream notification '{method}': encoded frame is {} bytes, \
                 over the {MAX_FRAME}-byte limit",
                bytes.len()
            );
            return;
        }
        let _ = self.stream.send(bytes).await;
    }

    /// Send a request and await its response.
    pub async fn request(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        // Register under the lock only while the connection is live (`Some`).
        // The teardown drain flips it to `None` under this same lock, so a
        // sender can never be inserted *after* the drain — the interleaving
        // that would leave this call parked forever on an unreachable oneshot.
        {
            let mut pending = self.pending.lock().unwrap();
            match pending.as_mut() {
                Some(map) => map.insert(id, tx),
                None => return Err(anyhow!("rpc connection closed")),
            };
        }
        // Hold the entry's cleanup guard for the rest of the await: if this
        // future is dropped before the response is dispatched, the guard's
        // `Drop` removes the sender from the map (see [`PendingGuard`]).
        let _cleanup = PendingGuard {
            pending: self.pending.clone(),
            id,
        };
        let msg = Value::Array(vec![
            Value::from(0u64),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        let Some(bytes) = encode_checked(&msg) else {
            // Never sent, so the pending entry must not survive — the guard's
            // `Drop` (bound above) removes it. Name the real cause instead of
            // falling through to the generic connection-closed error.
            return Err(anyhow!(
                "rpc request frame too large: '{method}' encodes to more than the \
                 {MAX_FRAME}-byte limit"
            ));
        };
        if self.out.send(bytes).is_err() {
            if let Some(map) = self.pending.lock().unwrap().as_mut() {
                map.remove(&id);
            }
            return Err(anyhow!("rpc connection closed"));
        }
        match rx.await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(anyhow!("rpc error: {e}")),
            Err(_) => Err(anyhow!("rpc connection closed")),
        }
    }

    /// Respond to a previously received [`Incoming::Request`].
    pub fn respond(&self, id: u64, result: std::result::Result<Value, Value>) {
        let (err, res) = match result {
            Ok(v) => (Value::Nil, v),
            Err(e) => (e, Value::Nil),
        };
        let msg = Value::Array(vec![Value::from(1u64), Value::from(id), err, res]);
        let Some(bytes) = encode_checked(&msg) else {
            // Refuse with a small error reply rather than ship a frame the peer's
            // reader rejects by tearing the whole link down (taking unrelated
            // in-flight requests with it). The requester sees a normal RPC error.
            let refuse = Value::Array(vec![
                Value::from(1u64),
                Value::from(id),
                Value::String(
                    format!("response frame exceeds the {MAX_FRAME}-byte rpc limit").into(),
                ),
                Value::Nil,
            ]);
            let _ = self.out.send(encode(&refuse));
            return;
        };
        let _ = self.out.send(bytes);
    }
}

/// The decoded-inbound sink a [`connect`]/[`connect_bounded`] reader drains the wire into:
/// unbounded (drain fire-and-forget — the default, never blocks the reader) or bounded
/// (the reader `await`s the push, so a consumer that can't keep up stops the wire —
/// [`connect_bounded`]).
enum InSink {
    Unbounded(mpsc::UnboundedSender<Incoming>),
    Bounded(mpsc::Sender<Incoming>),
}

impl InSink {
    /// Push one decoded frame. `false` when the consumer dropped its receiver — tear the
    /// connection down (the caller of the bounded variant sees a `false` from a full
    /// channel only once the receiver is dropped; a full bounded channel *awaits* instead).
    async fn push(&self, v: Incoming) -> bool {
        match self {
            InSink::Unbounded(tx) => tx.send(v).is_ok(),
            InSink::Bounded(tx) => tx.send(v).await.is_ok(),
        }
    }
}

/// Wire up a connection over `reader`/`writer`, draining inbound frames into an
/// unbounded queue (the reader never blocks — nothing on the wire can ever stall the
/// caller, and a response for a pending `request` is always delivered). See
/// [`connect_bounded`] for the backpressuring variant.
pub fn connect<R, W>(reader: R, writer: W) -> (Rpc, mpsc::UnboundedReceiver<Incoming>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (stream_tx, stream_rx) = mpsc::channel::<Vec<u8>>(STREAM_CAP);
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));

    let mut writer_handle = tokio::spawn(writer_task(writer, out_rx, stream_rx));
    let mut reader_handle = tokio::spawn(reader_task(
        reader,
        InSink::Unbounded(in_tx),
        pending.clone(),
    ));

    // Couple the two halves and fail in-flight requests on teardown. When either
    // task ends — the reader on EOF/corrupt frame, the writer on a write error —
    // the connection is dead: abort the survivor (so a dropped reader can't leave
    // the writer parked on `out_rx`, and vice versa) and `take` the pending map,
    // which both fails every outstanding `request().await` (their senders drop)
    // and marks the connection closed (`None`), so a request racing this drain
    // is refused at registration instead of parking forever on a oneshot the
    // (already-taken) map can no longer hand to anyone.
    let cleanup = pending.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = &mut writer_handle => reader_handle.abort(),
            _ = &mut reader_handle => writer_handle.abort(),
        }
        cleanup.lock().unwrap().take();
    });

    let rpc = Rpc {
        out: out_tx,
        stream: stream_tx,
        pending,
        next_id: Arc::new(AtomicU64::new(1)),
    };
    (rpc, in_rx)
}

/// Wire up a connection whose **inbound** queue is bounded at `in_cap` frames: the
/// reader `await`s the push, so when the consumer can't keep up the reader stops
/// draining the wire, the transport's write side fills, and the peer's own producer is
/// backpressured end-to-end. Without this, a peer that floods notifications (a remote
/// terminal child's output) accumulates unboundedly in the consumer's queue — memory
/// grows until the peer is killed, and the peer is never told to slow down.
///
/// **Notification-only legs only.** The reader is the sole deliverer of a pending
/// `request`'s response (via the pending map), so a leg that `await`s responses while
/// the queue is full deadlocks — the response frame sits unread behind the full queue.
/// The Term leg qualifies (the edit-host drives it with notifications and the daemon
/// answers only with `term_data`/`term_exit` pushes); a request/response leg must keep
/// using [`connect`], whose unbounded queue never strands a response.
pub fn connect_bounded<R, W>(reader: R, writer: W, in_cap: usize) -> (Rpc, mpsc::Receiver<Incoming>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (stream_tx, stream_rx) = mpsc::channel::<Vec<u8>>(STREAM_CAP);
    let (in_tx, in_rx) = mpsc::channel::<Incoming>(in_cap);
    let pending: Pending = Arc::new(Mutex::new(Some(HashMap::new())));

    let mut writer_handle = tokio::spawn(writer_task(writer, out_rx, stream_rx));
    let mut reader_handle =
        tokio::spawn(reader_task(reader, InSink::Bounded(in_tx), pending.clone()));

    // Couple the two halves and fail in-flight requests on teardown (see `connect`).
    let cleanup = pending.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = &mut writer_handle => reader_handle.abort(),
            _ = &mut reader_handle => writer_handle.abort(),
        }
        cleanup.lock().unwrap().take();
    });

    let rpc = Rpc {
        out: out_tx,
        stream: stream_tx,
        pending,
        next_id: Arc::new(AtomicU64::new(1)),
    };
    (rpc, in_rx)
}

async fn writer_task<W>(
    writer: W,
    mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut stream_rx: mpsc::Receiver<Vec<u8>>,
) where
    W: AsyncWrite + Unpin,
{
    // Buffer the sink: callers hand `connect` raw transports (TCP, QUIC
    // streams, stdio, the in-process duplex), where each `write_all` is its own
    // syscall and `flush` on e.g. `TcpStream` is a no-op — so unbuffered, the
    // batch coalescing below still pays one syscall *per frame*. Buffered, a
    // batch's frames coalesce in memory and the single `flush` at the end
    // performs the actual write (frames larger than the buffer write through).
    // Every loop iteration flushes before parking for more input, so no bytes
    // ever sit unflushed while the connection idles.
    let mut writer = tokio::io::BufWriter::new(writer);
    loop {
        // Wait for the next frame on either channel; `biased` so the unbounded
        // control channel (`out`) is always preferred over the bounded streaming
        // channel (`stream`), keeping control/latency-sensitive frames ahead of a
        // bulk terminal flood. `else` fires only once *both* channels are closed
        // (all `Rpc` handles dropped) → the connection is going away.
        let bytes = tokio::select! {
            biased;
            Some(b) = out_rx.recv() => b,
            Some(b) = stream_rx.recv() => b,
            else => break,
        };
        if writer.write_all(&bytes).await.is_err() {
            break;
        }
        // Coalesce the flush: write everything *already* queued before paying a
        // single flush, so a burst of frames (e.g. a redraw per keystroke under
        // fast typing) costs one flush syscall for the whole batch instead of one
        // per frame. `try_recv` only drains what is queued right now — it never
        // waits — so this adds no latency to the last frame in a burst. Control
        // first, then stream, preserving the same priority within a batch.
        let mut write_err = false;
        while let Ok(more) = out_rx.try_recv() {
            if writer.write_all(&more).await.is_err() {
                write_err = true;
                break;
            }
        }
        while !write_err {
            match stream_rx.try_recv() {
                Ok(more) => {
                    if writer.write_all(&more).await.is_err() {
                        write_err = true;
                    }
                }
                Err(_) => break,
            }
        }
        if write_err || writer.flush().await.is_err() {
            break;
        }
    }
}

/// Largest single not-yet-complete frame we will buffer before concluding the
/// peer is sending garbage (or an abusively large message) and tearing the
/// connection down. Bounds memory against a peer that streams bytes which never
/// finish a value. (rmpv itself caps str/bin preallocation at 64 KiB and never
/// preallocates arrays/maps, so a huge length prefix can't OOM us *before* its
/// data arrives — this guards the buffer that *holds* those bytes.)
const MAX_FRAME: usize = 64 * 1024 * 1024; // 64 MiB

/// Maximum depth budget we will accept, charged **exactly as rmpv's decoder
/// spends it** — 1 unit per scalar, 2 per container level (the value's entry
/// plus `read_array_data`/`read_map_data`), 2 per bin payload, 3 per str/ext
/// payload (`read_str_data`/`read_ext_body` recurse into `read_bin_data`) —
/// applied **per descending path**: a container hands its children `budget - 2`
/// and siblings share their parent's budget, exactly as rmpv passes `depth`
/// down by value, so a flat frame of any width passes while a path nested past
/// `MAX_DEPTH / 2` containers fails. The identical charge is what makes
/// [`scan_frame`]'s rejection threshold exactly the decoder's: a frame that
/// passes the scan is one `read_value_with_max_depth(MAX_DEPTH)` decodes, and
/// an over-budget frame is rejected before rmpv ever sees the bytes — not
/// first buffered, then refused by a decode error that tears the connection
/// down anyway. rmpv's decoder is recursive, so a peer sending deeply-nested
/// arrays/maps could otherwise overflow the reader thread's stack and *abort
/// the process* — a far worse outcome than a rejected message. This cap is
/// well below where recursion threatens the stack and well above any
/// legitimate RPC payload; exceeding it tears the connection down as a
/// malformed frame.
const MAX_DEPTH: usize = 128;

/// Maximum number of msgpack *values* (scalars + container slots) one frame may
/// declare. The wire-byte cap ([`MAX_FRAME`]) alone does **not** bound the
/// decoded structure: an array/map length prefix is attacker-controlled and can
/// claim billions of elements in a handful of bytes, which rmpv would then
/// materialize as a `Vec<Value>` — up to ~40 bytes of heap per 1 wire byte, a
/// ~40× amplification. This cap is checked against the *declared* length the
/// instant a container header is read ([`scan_frame`]), so such a frame is
/// rejected immediately instead of buffering toward [`MAX_FRAME`] and decoding a
/// multi-gigabyte structure first. Set to [`MAX_FRAME`]: a frame that genuinely
/// fits within the byte cap can hold at most that many values (each costs ≥1
/// wire byte), so this never rejects a frame the byte cap would have accepted —
/// it only short-circuits length prefixes that could never legitimately fit.
const MAX_VALUES: u64 = MAX_FRAME as u64;

/// Outcome of [`scan_frame`]: walking the buffered bytes for one complete
/// top-level msgpack value, enforcing allocation budgets on declared lengths.
enum Scan {
    /// A complete value occupies the first `usize` bytes of the slice.
    Complete(usize),
    /// The buffered bytes are a (valid) prefix of a value — wait for more.
    Incomplete,
    /// Declared sizes exceed [`MAX_VALUES`]/[`MAX_DEPTH`]/[`MAX_FRAME`], or a
    /// declared str/bin/ext length could never fit — reject the peer.
    TooLarge,
}

/// Resumable state of a frame scan, carried by the reader across reads. A scan
/// that returns [`Scan::Incomplete`] rewinds to the start of the one value
/// whose marker was claimed but whose bytes have not fully arrived, and stores
/// the walk here; the next call resumes from there. Without this, a large
/// multi-value frame arriving in many small reads would be re-walked from the
/// start on every read — quadratic in the frame size (each read re-scans the
/// whole buffered prefix, e.g. a 64 MiB frame in 8 KiB reads re-walks ~256 GiB
/// of markers). With it, each read re-parses at most the header of one
/// incomplete value, so a frame is walked once per byte overall.
struct FrameScan {
    /// Offset of the next byte to parse, relative to the frame's start.
    pos: usize,
    /// Values accounted so far (scalars + container slots); the [`MAX_VALUES`]
    /// budget carries across reads.
    seen: u64,
    /// Depth budget at each nesting level — `budget[d]` is the budget rmpv's
    /// `read_value_inner` would receive for a value owed at level `d` (see
    /// [`MAX_DEPTH`]). Per descending path, not frame-cumulative: a container
    /// hands its children `budget[d] - 2` (pushed with its header), and
    /// siblings at one level all run on the same value — mirroring how rmpv
    /// passes `depth` down by value. Carries across reads like `seen`; the
    /// walk only *checks* it (never spends it), so an incomplete value's
    /// rewind needs no budget undo.
    budget: [u64; MAX_DEPTH],
    /// `stack[i]` = values still owed at nesting level `i`, with `depth` levels
    /// in use. One value is owed at the (virtual) root: the frame itself. Fixed
    /// size — [`MAX_DEPTH`] bounds it — so a frame costs no heap allocation
    /// (the reader decodes many small frames per burst; this runs per frame).
    stack: [u64; MAX_DEPTH],
    depth: usize,
}

impl FrameScan {
    fn new() -> FrameScan {
        let mut s = FrameScan {
            pos: 0,
            seen: 0,
            budget: [MAX_DEPTH as u64; MAX_DEPTH],
            stack: [0; MAX_DEPTH],
            depth: 1,
        };
        s.stack[0] = 1;
        s
    }
}

/// Big-endian read of `n` length bytes at `*pos`, advancing `pos`. `None` (→
/// treat as [`Scan::Incomplete`]) when fewer than `n` bytes are buffered.
fn read_be(buf: &[u8], pos: &mut usize, n: usize) -> Option<u64> {
    let end = pos.checked_add(n)?;
    let bytes = buf.get(*pos..end)?;
    *pos = end;
    Some(bytes.iter().fold(0u64, |acc, &b| (acc << 8) | b as u64))
}

/// Advance `pos` past `n` payload bytes. `None` (→ [`Scan::Incomplete`]) when
/// the payload isn't fully buffered yet. `n` is always ≤ [`MAX_FRAME`] at the
/// call sites, so the `as usize` can't truncate (the reader runs only on
/// 64-bit native targets, where `usize` is 64-bit regardless).
fn skip(buf: &[u8], pos: &mut usize, n: u64) -> Option<()> {
    let end = (*pos as u64).checked_add(n)?;
    if end > buf.len() as u64 {
        return None;
    }
    *pos = end as usize;
    Some(())
}

/// Walk one msgpack value at the front of `buf` **without allocating or
/// recursing**, enforcing the [`MAX_VALUES`]/[`MAX_DEPTH`]/[`MAX_FRAME`] budgets
/// against the *declared* length fields before rmpv ever acts on them. This is
/// the guard that turns an attacker-controlled length prefix (which rmpv would
/// otherwise honor by building a structure dwarfing the wire bytes) into a clean
/// rejection. Nesting is tracked with an explicit stack of "values still owed at
/// this level", so the scan itself can't overflow the stack on a deeply nested
/// frame, and a container's element count is bounded the moment its header is
/// seen — not after its (possibly never-arriving) elements are buffered.
///
/// `state` carries the walk across calls: on [`Scan::Incomplete`] the walk is
/// stored (rewound to the start of the one value whose bytes haven't fully
/// arrived), and the next call — `buf` being the same frame-relative slice —
/// resumes from there. A complete frame resets the state, so the next call
/// starts a fresh walk. (See [`FrameScan`] for why: resuming keeps large frames
/// O(n) instead of re-walking the buffered prefix on every read.)
fn scan_frame(buf: &[u8], state: &mut Option<FrameScan>) -> Scan {
    let mut st = match state.take() {
        Some(st) => st,
        None => FrameScan::new(),
    };

    while st.depth > 0 {
        let top = &mut st.stack[st.depth - 1];
        if *top == 0 {
            st.depth -= 1;
            continue;
        }
        *top -= 1;

        st.seen += 1;
        if st.seen > MAX_VALUES {
            return Scan::TooLarge;
        }
        let value_start = st.pos;
        let Some(&marker) = buf.get(st.pos) else {
            // The marker byte isn't buffered, so this value's depth budget was
            // never consulted (it is consulted only once the marker is read,
            // below) — the plain rewind is therefore correct.
            st.stack[st.depth - 1] += 1; // owed again by its parent
            st.seen -= 1; // and uncounted
            st.pos = value_start;
            *state = Some(st);
            return Scan::Incomplete;
        };
        st.pos += 1;

        // Check the depth budget exactly as rmpv's decoder will (see
        // `MAX_DEPTH` for the per-value charges): this value, owed at level
        // `st.depth - 1`, needs `budget[depth - 1] >= charge` — the same
        // check `read_value_inner`'s decrements make. Checked before the
        // payload is walked, so an over-budget frame is rejected without
        // buffering toward it. A container's children inherit `budget - 2`
        // (pushed in `container!`); nothing here mutates the budget, so the
        // `incomplete!` rewind below needs no undo.
        let charge = match marker {
            // container levels: entry + read_array_data / read_map_data
            0x90..=0x9f | 0xdc | 0xdd | 0x80..=0x8f | 0xde | 0xdf => 2,
            // bin payloads: entry + read_bin_data
            0xc4..=0xc6 => 2,
            // str / ext payloads: entry + read_str_data/read_ext_body
            // + read_bin_data
            0xa0..=0xbf | 0xd9..=0xdb | 0xd4..=0xd8 | 0xc7..=0xc9 => 3,
            _ => 1, // scalars: the value's entry alone
        };
        if st.budget[st.depth - 1] < charge {
            return Scan::TooLarge;
        }

        // A value whose marker was claimed (and counted) but whose bytes were
        // not all buffered: undo the claim so the resumed walk re-parses it
        // from its marker once the missing bytes arrive. The re-parse costs
        // only the value's header (a payload skip is O(1)), so each read
        // re-walks at most one incomplete value — never the buffered prefix.
        // The depth budget needs no undo here: it lives per level and the walk
        // only *checks* it (a container's children budget is pushed with its
        // header, which the resume re-walks from).
        macro_rules! incomplete {
            ($value_start:expr) => {{
                st.stack[st.depth - 1] += 1; // owed again by its parent
                st.seen -= 1; // and uncounted
                st.pos = $value_start;
                *state = Some(st);
                return Scan::Incomplete;
            }};
        }

        // For a container, push its declared child count as a new level; reject
        // up front if it would blow the value budget. `2*len` for maps (key +
        // value) can't overflow a u64 (len ≤ u32::MAX). (The depth check above
        // guarantees `budget[depth - 1] >= 2` here — a container always
        // charges 2 — so the push below is within `stack`'s [`MAX_DEPTH`]
        // size: a path of containers is capped at `MAX_DEPTH / 2` levels by
        // that budget.)
        macro_rules! container {
            ($count:expr) => {{
                let count: u64 = $count;
                if st.seen.saturating_add(count) > MAX_VALUES {
                    return Scan::TooLarge;
                }
                st.budget[st.depth] = st.budget[st.depth - 1] - 2;
                st.stack[st.depth] = count;
                st.depth += 1;
            }};
        }
        // A str/bin/ext payload that can't fit in the frame buffer can never
        // complete — reject rather than buffer toward MAX_FRAME forever.
        macro_rules! payload {
            ($len:expr) => {{
                let len: u64 = $len;
                if len > MAX_FRAME as u64 {
                    return Scan::TooLarge;
                }
                len
            }};
        }
        macro_rules! adv {
            ($n:expr) => {
                if skip(buf, &mut st.pos, $n).is_none() {
                    incomplete!(value_start);
                }
            };
        }
        macro_rules! lenbytes {
            ($n:expr) => {
                match read_be(buf, &mut st.pos, $n) {
                    Some(v) => v,
                    None => incomplete!(value_start),
                }
            };
        }

        match marker {
            // fixint (pos/neg), nil, reserved (rmpv decodes 0xc1 as Nil), bools.
            0x00..=0x7f | 0xe0..=0xff | 0xc0..=0xc3 => {}
            0xcc | 0xd0 => adv!(1),                      // u8 / i8
            0xcd | 0xd1 => adv!(2),                      // u16 / i16
            0xce | 0xd2 | 0xca => adv!(4),               // u32 / i32 / f32
            0xcf | 0xd3 | 0xcb => adv!(8),               // u64 / i64 / f64
            0xa0..=0xbf => adv!((marker & 0x1f) as u64), // fixstr
            // str/bin: read the length field (ends its `pos` borrow), then skip.
            0xd9 => {
                let l = payload!(lenbytes!(1));
                adv!(l)
            } // str8
            0xda => {
                let l = payload!(lenbytes!(2));
                adv!(l)
            } // str16
            0xdb => {
                let l = payload!(lenbytes!(4));
                adv!(l)
            } // str32
            0xc4 => {
                let l = payload!(lenbytes!(1));
                adv!(l)
            } // bin8
            0xc5 => {
                let l = payload!(lenbytes!(2));
                adv!(l)
            } // bin16
            0xc6 => {
                let l = payload!(lenbytes!(4));
                adv!(l)
            } // bin32
            0x90..=0x9f => container!((marker & 0x0f) as u64), // fixarray
            0xdc => container!(lenbytes!(2)),                  // array16
            0xdd => container!(lenbytes!(4)),                  // array32
            0x80..=0x8f => container!((marker & 0x0f) as u64 * 2), // fixmap
            0xde => container!(lenbytes!(2) * 2),              // map16
            0xdf => container!(lenbytes!(4) * 2),              // map32
            // fixext: 1 type byte + N data bytes.
            0xd4 => adv!(2),
            0xd5 => adv!(3),
            0xd6 => adv!(5),
            0xd7 => adv!(9),
            0xd8 => adv!(17),
            // ext8/16/32: <len-field> then 1 type byte + len data bytes.
            0xc7 => {
                let l = payload!(lenbytes!(1));
                adv!(1 + l)
            }
            0xc8 => {
                let l = payload!(lenbytes!(2));
                adv!(1 + l)
            }
            0xc9 => {
                let l = payload!(lenbytes!(4));
                adv!(1 + l)
            }
        }
    }
    *state = None;
    Scan::Complete(st.pos)
}

async fn reader_task<R>(mut reader: R, in_tx: InSink, pending: Pending)
where
    R: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    // The in-progress frame scan, resumed across reads (see [`FrameScan`]).
    let mut scan: Option<FrameScan> = None;
    loop {
        // Drain every complete message currently buffered. Track a `consumed`
        // offset and decode from `buf[consumed..]` rather than draining after
        // each frame: when a single read delivers many frames (a coalesced
        // redraw burst or PTY flood), draining per-frame would memmove the whole
        // remaining buffer left on every iteration — O(frames × bytes), i.e.
        // quadratic. Instead we advance the offset and drain the consumed prefix
        // exactly once below.
        let mut consumed = 0usize;
        loop {
            // Bound the frame's *declared* sizes before allocating: `scan_frame`
            // walks the buffered bytes without recursing or allocating and tells
            // us whether a complete, within-budget value is present. Only then do
            // we hand that exact, bounded slice to rmpv to materialize. A short
            // read (`Incomplete`) means the frame isn't fully buffered yet — the
            // walk is stored in `scan` and resumed once more bytes arrive — so
            // wait for more; a budget/structure violation (`TooLarge`) means the
            // leading bytes will never decode acceptably — tear the connection
            // down instead of re-scanning the same bad prefix forever.
            let n = match scan_frame(&buf[consumed..], &mut scan) {
                Scan::Complete(n) => n,
                Scan::Incomplete => break,
                Scan::TooLarge => return,
            };
            let mut cur = Cursor::new(&buf[consumed..consumed + n]);
            let val = match rmpv::decode::read_value_with_max_depth(&mut cur, MAX_DEPTH) {
                Ok(v) => v,
                // `scan_frame` already vetted completeness, budget and depth, so
                // rmpv should not fail here; if it nonetheless does, treat the
                // frame as malformed and drop the connection rather than spin.
                Err(_) => return,
            };
            consumed += n;
            if dispatch(val, &in_tx, &pending).await.is_err() {
                return;
            }
        }
        // Discard the fully-decoded prefix in one shift.
        if consumed > 0 {
            buf.drain(..consumed);
        }

        // A frame that grows past the cap without ever completing is garbage or
        // abusively large; refuse it rather than buffer without bound. `buf` now
        // holds only the incomplete tail, so this bounds a single pending frame.
        if buf.len() > MAX_FRAME {
            return;
        }

        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break, // peer closed
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn dispatch(val: Value, in_tx: &InSink, pending: &Pending) -> std::result::Result<(), ()> {
    let mut arr = match val {
        Value::Array(a) => a,
        _ => return Ok(()), // ignore malformed frames
    };
    // `arr` is owned, so move the payload-bearing fields (params / result) out
    // instead of deep-cloning them — these can be large (e.g. a `set_lines` /
    // `get_lines` array) and this runs on every inbound frame. The id (Copy) and
    // the short method name are still read by reference.
    match arr.first().and_then(Value::as_u64) {
        Some(0) => {
            let id = arr.get(1).and_then(Value::as_u64).unwrap_or(0);
            let method = take_str(&mut arr, 2);
            let params = take_params(&mut arr, 3);
            if !in_tx.push(Incoming::Request { id, method, params }).await {
                return Err(());
            }
        }
        Some(1) => {
            let id = arr.get(1).and_then(Value::as_u64).unwrap_or(0);
            let err = take(&mut arr, 2);
            let res = take(&mut arr, 3);
            if let Some(tx) = pending.lock().unwrap().as_mut().and_then(|m| m.remove(&id)) {
                let _ = tx.send(if err.is_nil() { Ok(res) } else { Err(err) });
            }
        }
        Some(2) => {
            let method = take_str(&mut arr, 1);
            let params = take_params(&mut arr, 2);
            if !in_tx.push(Incoming::Notification { method, params }).await {
                return Err(());
            }
        }
        _ => {}
    }
    Ok(())
}

/// Move element `idx` out of `arr`, leaving `Value::Nil` in its place; `Nil`
/// when the index is out of bounds. Lets `dispatch` take ownership of a frame's
/// payload fields without cloning them.
fn take(arr: &mut [Value], idx: usize) -> Value {
    arr.get_mut(idx)
        .map(|v| std::mem::replace(v, Value::Nil))
        .unwrap_or(Value::Nil)
}

/// Move the params array out of `arr[idx]` (empty when absent or not an array).
fn take_params(arr: &mut [Value], idx: usize) -> Vec<Value> {
    match take(arr, idx) {
        Value::Array(p) => p,
        _ => Vec::new(),
    }
}

/// Move the method-name string out of `arr[idx]` (empty when absent or not a
/// string). Takes ownership of the decoder's already-allocated `String` rather
/// than re-allocating a copy via `as_str().to_string()` — this runs on every
/// inbound frame, where the method name is the only string field.
fn take_str(arr: &mut [Value], idx: usize) -> String {
    match take(arr, idx) {
        Value::String(s) => s.into_str().unwrap_or_default(),
        _ => String::new(),
    }
}

/// Initial capacity for an encoded frame's buffer. A starting size large enough
/// to hold the common small frame (a notification or a getter reply) in one
/// allocation, and to cut the number of reallocations a larger redraw frame
/// pays as it grows — versus starting from `Vec::new()`'s capacity 0 and
/// reallocating from the first byte on every message.
const ENCODE_BUF_HINT: usize = 1024;

fn encode(val: &Value) -> Vec<u8> {
    let mut buf = Vec::with_capacity(ENCODE_BUF_HINT);
    rmpv::encode::write_value(&mut buf, val).expect("msgpack encoding cannot fail to a Vec");
    buf
}

/// Encode `val` as one RPC frame, refusing (`None`) any frame the peer's reader
/// would reject: the reader's budget check ([`reader_task`], `buf.len() >
/// MAX_FRAME`) tears the **whole connection** down on an oversized frame, taking
/// unrelated in-flight requests with it. The per-method refusal is loud — an
/// error reply for [`respond`](Rpc::respond), an `Err` for
/// [`request`](Rpc::request), a stderr line for the notifications — so a sender
/// that produced an over-large payload hears about it instead of silently
/// killing the link.
///
/// Residual gap: the byte cap is the only dimension checked here. The reader
/// additionally rejects on [`MAX_VALUES`] and [`MAX_DEPTH`] (`scan_frame`), and
/// a byte-passing frame can still trip the depth one (a path nested past
/// `MAX_DEPTH / 2` containers) — the value cap cannot, since a frame's values
/// cost it at least one wire byte each. A sender that builds such a frame still
/// tears the link; mirroring the reader's per-descending-path depth charge here
/// would duplicate rmpv's budget arithmetic and drift with it, so this is
/// documented rather than encoded. Deep nesting only arises from a hostile or
/// pathological local sender, unlike an oversized payload which any bug can
/// produce.
fn encode_checked(val: &Value) -> Option<Vec<u8>> {
    let buf = encode(val);
    (buf.len() <= MAX_FRAME).then_some(buf)
}

/// Build and encode a notification frame `[2, method, params]`. Shared by
/// [`Rpc::notify`] and [`Rpc::notify_stream`], which differ only in the channel
/// the encoded bytes are handed to.
fn notification(method: &str, params: Vec<Value>) -> Vec<u8> {
    encode(&Value::Array(vec![
        Value::from(2u64),
        Value::from(method),
        Value::Array(params),
    ]))
}
