//! Async msgpack-RPC transport. msgpack is used purely as a compact binary
//! framing — this is nxvim's own protocol, not a neovim-compatible channel.
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

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<std::result::Result<Value, Value>>>>>;

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
        let msg = Value::Array(vec![
            Value::from(2u64),
            Value::from(method),
            Value::Array(params),
        ]);
        let _ = self.out.send(encode(&msg));
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
        let msg = Value::Array(vec![
            Value::from(2u64),
            Value::from(method),
            Value::Array(params),
        ]);
        let _ = self.stream.send(encode(&msg)).await;
    }

    /// Send a request and await its response.
    pub async fn request(&self, method: &str, params: Vec<Value>) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = Value::Array(vec![
            Value::from(0u64),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        if self.out.send(encode(&msg)).is_err() {
            self.pending.lock().unwrap().remove(&id);
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
        let _ = self.out.send(encode(&msg));
    }
}

/// Wire up a connection over `reader`/`writer`.
pub fn connect<R, W>(reader: R, writer: W) -> (Rpc, mpsc::UnboundedReceiver<Incoming>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (stream_tx, stream_rx) = mpsc::channel::<Vec<u8>>(STREAM_CAP);
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    let mut writer_handle = tokio::spawn(writer_task(writer, out_rx, stream_rx));
    let mut reader_handle = tokio::spawn(reader_task(reader, in_tx, pending.clone()));

    // Couple the two halves and fail in-flight requests on teardown. When either
    // task ends — the reader on EOF/corrupt frame, the writer on a write error —
    // the connection is dead: abort the survivor (so a dropped reader can't leave
    // the writer parked on `out_rx`, and vice versa) and drain `pending`, so every
    // outstanding `request().await` resolves to an error instead of blocking
    // forever on a oneshot whose sender would otherwise linger in the map.
    let cleanup = pending.clone();
    tokio::spawn(async move {
        tokio::select! {
            _ = &mut writer_handle => reader_handle.abort(),
            _ = &mut reader_handle => writer_handle.abort(),
        }
        cleanup.lock().unwrap().clear();
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
    mut writer: W,
    mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut stream_rx: mpsc::Receiver<Vec<u8>>,
) where
    W: AsyncWrite + Unpin,
{
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

/// Maximum container nesting we will accept. rmpv's decoder is recursive, so a
/// peer sending deeply-nested arrays/maps can otherwise overflow the reader
/// thread's stack and *abort the process* — a far worse outcome than a rejected
/// message. This cap is well below where recursion threatens the stack and well
/// above any legitimate RPC payload; exceeding it tears the connection down as a
/// malformed frame. ([`scan_frame`] enforces it without recursing, before rmpv —
/// itself also depth-capped — ever sees the bytes.)
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
fn scan_frame(buf: &[u8]) -> Scan {
    let mut pos = 0usize;
    let mut seen: u64 = 0;
    // stack[i] = number of values still owed at nesting level i. One value is
    // owed at the (virtual) root: the frame itself.
    let mut stack: Vec<u64> = vec![1];

    while let Some(top) = stack.last_mut() {
        if *top == 0 {
            stack.pop();
            continue;
        }
        *top -= 1;

        seen += 1;
        if seen > MAX_VALUES {
            return Scan::TooLarge;
        }
        let Some(&marker) = buf.get(pos) else {
            return Scan::Incomplete;
        };
        pos += 1;

        // For a container, push its declared child count as a new level; reject
        // up front if it would blow the value budget or nest too deep. `2*len`
        // for maps (key + value) can't overflow a u64 (len ≤ u32::MAX).
        macro_rules! container {
            ($count:expr) => {{
                let count: u64 = $count;
                if seen.saturating_add(count) > MAX_VALUES || stack.len() >= MAX_DEPTH {
                    return Scan::TooLarge;
                }
                stack.push(count);
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
                if skip(buf, &mut pos, $n).is_none() {
                    return Scan::Incomplete;
                }
            };
        }
        macro_rules! lenbytes {
            ($n:expr) => {
                match read_be(buf, &mut pos, $n) {
                    Some(v) => v,
                    None => return Scan::Incomplete,
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
    Scan::Complete(pos)
}

async fn reader_task<R>(mut reader: R, in_tx: mpsc::UnboundedSender<Incoming>, pending: Pending)
where
    R: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
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
            // read (`Incomplete`) means the frame isn't fully buffered yet, so
            // wait for more bytes; a budget/structure violation (`TooLarge`)
            // means the leading bytes will never decode acceptably — tear the
            // connection down instead of re-scanning the same bad prefix forever.
            let n = match scan_frame(&buf[consumed..]) {
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
            if dispatch(val, &in_tx, &pending).is_err() {
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

fn dispatch(
    val: Value,
    in_tx: &mpsc::UnboundedSender<Incoming>,
    pending: &Pending,
) -> std::result::Result<(), ()> {
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
            in_tx
                .send(Incoming::Request { id, method, params })
                .map_err(|_| ())?;
        }
        Some(1) => {
            let id = arr.get(1).and_then(Value::as_u64).unwrap_or(0);
            let err = take(&mut arr, 2);
            let res = take(&mut arr, 3);
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(if err.is_nil() { Ok(res) } else { Err(err) });
            }
        }
        Some(2) => {
            let method = take_str(&mut arr, 1);
            let params = take_params(&mut arr, 2);
            in_tx
                .send(Incoming::Notification { method, params })
                .map_err(|_| ())?;
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
