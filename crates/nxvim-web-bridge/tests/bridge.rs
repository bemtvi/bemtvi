//! Tests for the bridge:
//!
//! - the embedded-asset HTTP surface (static files + the `/config.json` mode probe),
//!   driven over real TCP with a blocking HTTP client;
//! - the per-connection relay byte pump, driven against a real stub editor child over
//!   real OS pipes — the same `relay_connection` the Socket.IO layer uses, with an
//!   in-memory channel + collector standing in for the live socket (just as the editor
//!   crates test over an in-process duplex instead of real stdio). The Socket.IO wire
//!   itself is covered end to end by the Phase-5 Playwright test with the browser's
//!   `socket.io-client`.
//!
//! The stub stands in for `nxvim --server`; see `src/bin/stub_server.rs`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use nxvim_web_bridge::{app, relay_connection, ServerSpec};
use rmpv::Value;
use tokio::sync::{mpsc as tokio_mpsc, Notify};

/// Path to the compiled stub editor child.
fn stub_spec() -> ServerSpec {
    ServerSpec {
        // The stub ignores argv, so no `--server` flag is needed.
        program: PathBuf::from(env!("CARGO_BIN_EXE_stub_server")),
        args: vec![],
    }
}

/// Spawn the bridge on an ephemeral port relaying to the stub child; return its address.
/// The server thread runs for the test's lifetime (process exit reaps it).
fn start_bridge() -> SocketAddr {
    let (addr_tx, addr_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind ephemeral port");
            addr_tx
                .send(listener.local_addr().expect("local_addr"))
                .expect("send addr");
            axum::serve(listener, app(stub_spec()))
                .await
                .expect("serve");
        });
    });
    addr_rx.recv().expect("bridge bound")
}

/// `[0, 1, "nvim_ui_attach", [80, 24, {}]]` — a representative first client frame.
fn attach_frame() -> Vec<u8> {
    let frame = Value::Array(vec![
        Value::from(0),
        Value::from(1),
        Value::from("nvim_ui_attach"),
        Value::Array(vec![Value::from(80), Value::from(24), Value::Map(vec![])]),
    ]);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &frame).expect("encode attach");
    buf
}

#[test]
fn serves_config_json_in_remote_mode() {
    let addr = start_bridge();
    let body = ureq::get(&format!("http://{addr}/config.json"))
        .call()
        .expect("GET /config.json")
        .into_string()
        .expect("body");
    assert!(
        body.contains("\"mode\":\"remote\""),
        "expected remote mode, got: {body}"
    );
}

#[test]
fn serves_embedded_index_html() {
    let addr = start_bridge();
    let resp = ureq::get(&format!("http://{addr}/")).call().expect("GET /");
    assert_eq!(
        resp.content_type(),
        "text/html",
        "index.html should be served as HTML"
    );
    let body = resp.into_string().expect("body");
    assert!(
        body.to_lowercase().contains("nxvim"),
        "index.html should mention nxvim"
    );
}

#[test]
fn unknown_asset_is_404() {
    let addr = start_bridge();
    let err = ureq::get(&format!("http://{addr}/does-not-exist.js"))
        .call()
        .expect_err("missing asset should 404");
    match err {
        ureq::Error::Status(code, _) => assert_eq!(code, 404),
        other => panic!("expected 404, got {other:?}"),
    }
}

/// The relay spawns the editor child, forwards a client frame into its stdin, and pumps
/// the child's reply back out — reassembled across the chunk boundary the stub injects.
///
/// Multi-thread runtime: the test thread blocks on `recv_timeout` to collect chunks, so
/// the relay must run on a separate worker (a current-thread runtime would starve it).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn relay_forwards_input_and_reassembles_reply() {
    let (inbound_tx, inbound_rx) = tokio_mpsc::unbounded_channel::<Bytes>();
    // The relay's `emit` collects outbound chunks here (the live socket's stand-in).
    let (chunk_tx, chunk_rx) = mpsc::channel::<Vec<u8>>();
    let shutdown = Arc::new(Notify::new());

    let relay_shutdown = shutdown.clone();
    let relay = tokio::spawn(async move {
        let emit = move |chunk: &[u8]| chunk_tx.send(chunk.to_vec()).is_ok();
        relay_connection(&stub_spec(), inbound_rx, emit, relay_shutdown).await
    });

    // Send the attach frame; the stub replies with a canned `redraw`, split across two
    // flushes so it arrives as multiple chunks for the pump to forward separately.
    inbound_tx
        .send(Bytes::from(attach_frame()))
        .expect("send attach");

    // Accumulate forwarded chunks until they reassemble into one complete msgpack frame.
    let mut acc: Vec<u8> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    let decoded = loop {
        match chunk_rx.recv_timeout(Duration::from_millis(500)) {
            Ok(chunk) => {
                acc.extend_from_slice(&chunk);
                let mut cursor = acc.as_slice();
                if let Ok(value) = rmpv::decode::read_value(&mut cursor) {
                    assert!(
                        cursor.is_empty(),
                        "reassembled a frame plus {} trailing bytes",
                        cursor.len()
                    );
                    break value;
                }
                // Partial frame so far — keep collecting.
            }
            Err(_) if Instant::now() >= deadline => {
                panic!(
                    "no complete frame within timeout; {} bytes so far",
                    acc.len()
                )
            }
            Err(_) => {}
        }
    };

    // The reassembled frame is the redraw notification `[2, "redraw", [...]]`.
    let arr = decoded.as_array().expect("frame is an array");
    assert_eq!(arr[0].as_u64(), Some(2), "notification type");
    assert_eq!(arr[1].as_str(), Some("redraw"), "redraw method");

    // Tearing down via the shutdown signal kills the child and returns the relay.
    shutdown.notify_one();
    relay
        .await
        .expect("relay task joins")
        .expect("relay returns Ok");
}
