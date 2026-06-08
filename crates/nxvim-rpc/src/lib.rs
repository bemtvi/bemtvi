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
use std::io::{self, Cursor};
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

/// A cloneable handle for sending requests, notifications and responses.
#[derive(Clone)]
pub struct Rpc {
    out: mpsc::UnboundedSender<Vec<u8>>,
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
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

    let mut writer_handle = tokio::spawn(writer_task(writer, out_rx));
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
        pending,
        next_id: Arc::new(AtomicU64::new(1)),
    };
    (rpc, in_rx)
}

async fn writer_task<W>(mut writer: W, mut out_rx: mpsc::UnboundedReceiver<Vec<u8>>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(bytes) = out_rx.recv().await {
        if writer.write_all(&bytes).await.is_err() {
            break;
        }
        // Coalesce the flush: write everything *already* queued before paying a
        // single flush, so a burst of frames (e.g. a redraw per keystroke under
        // fast typing) costs one flush syscall for the whole batch instead of
        // one per frame. `try_recv` only drains what is queued right now — it
        // never waits for new frames — so this adds no latency to the last frame
        // in a burst, and a lone frame behaves exactly as before (drain empties
        // immediately, then flush).
        let mut write_err = false;
        while let Ok(more) = out_rx.try_recv() {
            if writer.write_all(&more).await.is_err() {
                write_err = true;
                break;
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
/// preallocates arrays/maps, so a huge length prefix can't OOM us before its
/// data arrives — this guards the buffer that *holds* those bytes.)
const MAX_FRAME: usize = 64 * 1024 * 1024; // 64 MiB

/// Maximum container nesting we will decode. rmpv's decoder is recursive, so a
/// peer sending deeply-nested arrays/maps can otherwise overflow the reader
/// thread's stack and *abort the process* — a far worse outcome than a rejected
/// message. This cap is well below where recursion threatens the stack and well
/// above any legitimate RPC payload; exceeding it surfaces as a clean
/// `DepthLimitExceeded`, which we treat as a malformed frame below.
const MAX_DEPTH: usize = 128;

async fn reader_task<R>(mut reader: R, in_tx: mpsc::UnboundedSender<Incoming>, pending: Pending)
where
    R: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        // Drain every complete message currently buffered.
        loop {
            let parsed = {
                let mut cur = Cursor::new(&buf[..]);
                match rmpv::decode::read_value_with_max_depth(&mut cur, MAX_DEPTH) {
                    Ok(v) => Ok(Some((v, cur.position() as usize))),
                    // A short read means the frame isn't fully buffered yet, so
                    // wait for more bytes. Any other decode error means the
                    // stream is structurally corrupt (bad marker, depth blown)
                    // and the leading bytes will never decode — tear the
                    // connection down instead of re-reading the same bad prefix
                    // forever.
                    Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
                    Err(_) => Err(()),
                }
            };
            match parsed {
                Ok(Some((val, n))) => {
                    buf.drain(..n);
                    if dispatch(val, &in_tx, &pending).is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(()) => return, // malformed frame: drop the connection
            }
        }

        // A frame that grows past the cap without ever completing is garbage or
        // abusively large; refuse it rather than buffer without bound.
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
            let method = arr.get(2).and_then(Value::as_str).unwrap_or("").to_string();
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
            let method = arr.get(1).and_then(Value::as_str).unwrap_or("").to_string();
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
