//! Async msgpack-RPC, framed exactly like neovim's RPC channel.
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
    Request { id: u64, method: String, params: Vec<Value> },
    Notification { method: String, params: Vec<Value> },
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

    tokio::spawn(writer_task(writer, out_rx));
    tokio::spawn(reader_task(reader, in_tx, pending.clone()));

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
        if writer.flush().await.is_err() {
            break;
        }
    }
}

async fn reader_task<R>(
    mut reader: R,
    in_tx: mpsc::UnboundedSender<Incoming>,
    pending: Pending,
) where
    R: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut chunk = [0u8; 8192];
    loop {
        // Drain every complete message currently buffered.
        loop {
            let parsed = {
                let mut cur = Cursor::new(&buf[..]);
                match rmpv::decode::read_value(&mut cur) {
                    Ok(v) => Some((v, cur.position() as usize)),
                    Err(_) => None, // truncated (need more) or empty
                }
            };
            match parsed {
                Some((val, n)) => {
                    buf.drain(..n);
                    if dispatch(val, &in_tx, &pending).is_err() {
                        return;
                    }
                }
                None => break,
            }
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
    let arr = match val {
        Value::Array(a) => a,
        _ => return Ok(()), // ignore malformed frames
    };
    match arr.first().and_then(Value::as_u64) {
        Some(0) => {
            let id = arr.get(1).and_then(Value::as_u64).unwrap_or(0);
            let method = arr.get(2).and_then(Value::as_str).unwrap_or("").to_string();
            let params = params_of(arr.get(3));
            in_tx
                .send(Incoming::Request { id, method, params })
                .map_err(|_| ())?;
        }
        Some(1) => {
            let id = arr.get(1).and_then(Value::as_u64).unwrap_or(0);
            let err = arr.get(2).cloned().unwrap_or(Value::Nil);
            let res = arr.get(3).cloned().unwrap_or(Value::Nil);
            if let Some(tx) = pending.lock().unwrap().remove(&id) {
                let _ = tx.send(if err.is_nil() { Ok(res) } else { Err(err) });
            }
        }
        Some(2) => {
            let method = arr.get(1).and_then(Value::as_str).unwrap_or("").to_string();
            let params = params_of(arr.get(2));
            in_tx
                .send(Incoming::Notification { method, params })
                .map_err(|_| ())?;
        }
        _ => {}
    }
    Ok(())
}

fn params_of(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(p)) => p.clone(),
        _ => Vec::new(),
    }
}

fn encode(val: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, val).expect("msgpack encoding cannot fail to a Vec");
    buf
}
