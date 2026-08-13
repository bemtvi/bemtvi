//! The **wasm leg's** LSP client, driven as the public state machine the edit-host
//! drives it as: commands in, wire ops and events out, server bytes fed back in.
//!
//! This leg exists because async-lsp does not: the browser edit-host has no runtime,
//! so `SyncLspClient` hand-builds the frames the native manager gets from lsp-types
//! and async-lsp's router. That is exactly where the two legs drift — and the tier-1
//! remote rule says a feature must behave identically whichever leg carries it, down
//! to the bytes, because a server READS the replies and changes what it sends next.
//!
//! Tier-1 and pure: no runtime, no process, no wire. The client's `take_wire_ops` is
//! the frame it would have written to the server's stdin, so the tests read the real
//! bytes.
//!
//! **This file compiles only with the `native` feature OFF** — the sync client itself
//! is `#[cfg(not(feature = "native"))]`, so it does not exist in the default build and
//! a plain `cargo test --workspace` does not run these. That is the same opt-in the
//! rest of the wasm leg uses (`cargo check --no-default-features`, the browser
//! `verify-*.mjs` scripts). Run them with:
//!
//! ```sh
//! cargo test -p bemtvi-lsp --no-default-features --test sync_client
//! ```
#![cfg(not(feature = "native"))]

use bemtvi_lsp::lsp_types::{Position, Url};
use bemtvi_lsp::{
    LspEvent, LspNotify, LspReply, LspRequest, ReqToken, ServerKey, ServerSpawn, SyncLspClient,
    WireOp,
};
use serde_json::Value;

/// A request token; the tests never route a reply by it, only observe the refusal.
fn token() -> ReqToken {
    ReqToken {
        kind: 0,
        generation: 1,
        cb_id: 9,
    }
}

fn key() -> ServerKey {
    ServerKey {
        name: "mock".into(),
        root: Some(std::path::PathBuf::from("/tmp/p")),
    }
}

fn spawn() -> ServerSpawn {
    ServerSpawn {
        program: "mock".into(),
        args: Vec::new(),
        cwd: Some(std::path::PathBuf::from("/tmp/p")),
        init_options: None,
        settings: None,
        capabilities: None,
        env: Vec::new(),
    }
}

/// A client with one server past its handshake, ready to send.
fn ready() -> SyncLspClient {
    let mut c = SyncLspClient::new();
    c.ensure_server(key(), spawn());
    // Answer the `initialize` the client just sent, so the phase flips to Ready and
    // buffered commands flush.
    let id = wire_id(&mut c);
    feed(
        &mut c,
        id,
        &json_frame(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1,
            "result": { "capabilities": {} }
        })),
    );
    let _ = c.take_wire_ops();
    c
}

/// The wire id the client assigned its (only) server.
fn wire_id(c: &mut SyncLspClient) -> u64 {
    c.take_wire_ops()
        .into_iter()
        .find_map(|op| match op {
            WireOp::Spawn { id, .. } => Some(id),
            _ => None,
        })
        .expect("the client spawns its server first")
}

/// `Content-Length`-framed JSON, as a server writes it.
fn json_frame(v: &Value) -> Vec<u8> {
    let body = serde_json::to_vec(v).unwrap();
    let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    out.extend_from_slice(&body);
    out
}

fn feed(c: &mut SyncLspClient, id: u64, bytes: &[u8]) {
    c.feed_stdout(id, bytes);
}

/// Every JSON body the client wrote to the server's stdin since the last take.
fn sent(c: &mut SyncLspClient) -> Vec<Value> {
    c.take_wire_ops()
        .into_iter()
        .filter_map(|op| match op {
            WireOp::Stdin { bytes, .. } => Some(bytes),
            _ => None,
        })
        .flat_map(|bytes| {
            // One `Stdin` op may carry several framed messages.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.split("Content-Length: ")
                .filter_map(|part| {
                    let body = part.split_once("\r\n\r\n")?.1;
                    serde_json::from_str::<Value>(body).ok()
                })
                .collect::<Vec<_>>()
        })
        .collect()
}

/// The first frame whose `method` is `m`.
fn sent_method(c: &mut SyncLspClient, m: &str) -> Option<Value> {
    sent(c).into_iter().find(|v| v["method"] == m)
}

// ============================================== didSave carries the native leg's shape
//
// lsp-types SKIPS an absent `text`; the hand-built params sent `"text": null`, which is
// a different frame for the same event.

#[test]
fn did_save_without_text_omits_the_field_rather_than_sending_null() {
    let mut c = ready();
    c.notify(
        key(),
        LspNotify::DidSave {
            uri: Url::parse("file:///a.rs").unwrap(),
            text: None,
        },
    );
    let frame = sent_method(&mut c, "textDocument/didSave").expect("didSave was sent");
    let params = frame["params"].as_object().expect("params object");
    assert!(params.contains_key("textDocument"), "identifier is sent");
    assert!(
        !params.contains_key("text"),
        "an absent text must be OMITTED, not sent as null: {frame}"
    );
}

#[test]
fn did_save_with_text_still_carries_it() {
    // The control: the field must appear when the server asked for it.
    let mut c = ready();
    c.notify(
        key(),
        LspNotify::DidSave {
            uri: Url::parse("file:///a.rs").unwrap(),
            text: Some("hello".into()),
        },
    );
    let frame = sent_method(&mut c, "textDocument/didSave").expect("didSave was sent");
    assert_eq!(frame["params"]["text"], Value::String("hello".into()));
}

// ======================================= an unmodelled server request gets the same error
//
// async-lsp's router answers a request it has no handler for with
// `{"code": -32601, "message": "No such method: X", "data": null}`. This leg used to
// ack EVERY unmodelled request with a null RESULT instead — which a server reads as
// "the client did this", the opposite conclusion. `window/workDoneProgress/create` is
// the case that made it matter: gopls sends no `$/progress` at all for a token the
// client refused, so the two legs disagreed about whether progress works.

#[test]
fn an_unmodelled_server_request_is_answered_with_method_not_found() {
    let mut c = ready();
    let id = c
        .take_wire_ops()
        .into_iter()
        .find_map(|op| match op {
            WireOp::Spawn { id, .. } => Some(id),
            _ => None,
        })
        .unwrap_or(1);
    feed(
        &mut c,
        id,
        &json_frame(&serde_json::json!({
            "jsonrpc": "2.0", "id": 42, "method": "window/showMessageRequest",
            "params": { "type": 1, "message": "hi" }
        })),
    );
    let reply = sent(&mut c)
        .into_iter()
        .find(|v| v["id"] == 42)
        .expect("an unmodelled request must still be answered");
    assert!(
        reply.get("result").is_none(),
        "a null RESULT tells the server we did it — it must be an error: {reply}"
    );
    let err = &reply["error"];
    assert_eq!(err["code"], -32601, "METHOD_NOT_FOUND: {reply}");
    assert_eq!(
        err["message"],
        Value::String("No such method: window/showMessageRequest".into()),
        "byte-identical to async-lsp's message, colon and all: {reply}"
    );
    assert!(
        err.as_object().is_some_and(|o| o.contains_key("data")),
        "async-lsp serializes `data` unconditionally: {reply}"
    );
}

#[test]
fn work_done_progress_create_is_acked_so_the_server_reports_progress() {
    // The one request that must NOT get the method-not-found above: a server reads
    // the refusal as "this client cannot do progress" and goes silent.
    let mut c = ready();
    let id = 1;
    feed(
        &mut c,
        id,
        &json_frame(&serde_json::json!({
            "jsonrpc": "2.0", "id": 7, "method": "window/workDoneProgress/create",
            "params": { "token": "tok-1" }
        })),
    );
    let reply = sent(&mut c)
        .into_iter()
        .find(|v| v["id"] == 7)
        .expect("the create must be answered");
    assert!(
        reply.get("error").is_none(),
        "refusing this makes the server send no $/progress at all: {reply}"
    );
    assert_eq!(reply["result"], Value::Null, "acked with a null result");
}

// ============================================ both legs accept the same raw methods
//
// The native leg dispatches a raw `client:request` through a compile-time table and
// REFUSES anything outside it. This leg cannot build that table, so an unsupported
// method used to be buffered and answered with a degraded "nothing found" — a plugin
// would see an empty result on the web and a loud error natively.

#[test]
fn a_raw_request_outside_the_shared_whitelist_is_refused_like_the_native_leg() {
    let mut c = ready();
    c.request(
        key(),
        token(),
        LspRequest::Raw {
            method: "totally/madeUp".into(),
            params: serde_json::json!({}),
        },
    );
    let refusal = c.take_events().into_iter().find_map(|e| match e {
        LspEvent::Reply {
            reply: LspReply::Raw(Err(msg)),
            ..
        } => Some(msg),
        _ => None,
    });
    let msg = refusal.expect("an unsupported raw method must be refused, not buffered");
    assert!(
        msg.contains("totally/madeUp") && msg.contains("unsupported"),
        "the refusal must name the method: {msg}"
    );
    assert!(
        sent(&mut c).iter().all(|v| v["method"] != "totally/madeUp"),
        "and nothing may reach the server"
    );
}

#[test]
fn a_whitelisted_raw_request_still_reaches_the_server() {
    // The control: the pre-flight must only refuse what the native table refuses.
    let mut c = ready();
    c.request(
        key(),
        token(),
        LspRequest::Raw {
            method: "workspace/executeCommand".into(),
            params: serde_json::json!({ "command": "x" }),
        },
    );
    assert!(
        sent_method(&mut c, "workspace/executeCommand").is_some(),
        "a whitelisted method must go out on the wire"
    );
}

#[test]
fn a_raw_notification_outside_the_whitelist_is_dropped_like_the_native_leg() {
    let mut c = ready();
    c.notify(
        key(),
        LspNotify::Raw {
            method: "totally/madeUp".into(),
            params: serde_json::json!({}),
        },
    );
    assert!(
        sent(&mut c).iter().all(|v| v["method"] != "totally/madeUp"),
        "an unsupported notification is dropped, as the native leg drops it"
    );
}

// ======================================= a handshake that never completes is bounded
//
// Commands issued before `initialize` returns are buffered — the sync analogue of the
// native per-server channel filling until the serve loop starts. A server that spawns
// but never speaks leaves that buffer growing for the session: the editor keeps
// issuing document-sync notifications and language requests, and every one is kept.
// Past the cap the server is killed as wedged (the analogue of the native leg's
// `INIT_GRACE`) and everything buffered is settled, so no `ReqToken` is stranded.

/// A client with a server that has NOT answered `initialize`, and its wire id.
fn wedged() -> (SyncLspClient, u64) {
    let mut c = SyncLspClient::new();
    c.ensure_server(key(), spawn());
    let id = wire_id(&mut c);
    (c, id)
}

#[test]
fn a_handshake_that_never_completes_does_not_buffer_without_bound() {
    let (mut c, _id) = wedged();
    // Well past the cap. Every one of these would otherwise be retained forever.
    for i in 0..5000u64 {
        c.request(
            key(),
            ReqToken {
                kind: 0,
                generation: i,
                cb_id: 0,
            },
            LspRequest::Hover {
                uri: Url::parse("file:///a.rs").unwrap(),
                position: Position::new(0, 0),
            },
        );
    }
    let killed = c
        .take_wire_ops()
        .into_iter()
        .any(|op| matches!(op, WireOp::Kill { .. }));
    assert!(
        killed,
        "a server that never finishes its handshake must be killed, not buffered \
         for the session"
    );
}

#[test]
fn killing_a_wedged_server_settles_everything_it_was_holding() {
    let (mut c, _id) = wedged();
    // Issue until the client gives up, counting what it ACCEPTED. Requests made
    // after the kill are a different question — the server is gone from the map by
    // then, and the editor has been told so — so the claim here is precise: every
    // request the client took while the server existed must come back.
    let mut issued = 0usize;
    let mut events = Vec::new();
    let mut killed = false;
    for i in 0..20_000u64 {
        c.request(
            key(),
            ReqToken {
                kind: 0,
                generation: i,
                cb_id: 0,
            },
            LspRequest::Hover {
                uri: Url::parse("file:///a.rs").unwrap(),
                position: Position::new(0, 0),
            },
        );
        issued += 1;
        events.extend(c.take_events());
        if c.take_wire_ops()
            .into_iter()
            .any(|op| matches!(op, WireOp::Kill { .. }))
        {
            killed = true;
            break;
        }
    }
    assert!(killed, "the wedged server must eventually be dropped");

    assert!(
        events
            .iter()
            .any(|e| matches!(e, LspEvent::ServerExited { message, .. }
                              if message.contains("wedged"))),
        "the editor must be told the server was dropped, not left believing it is \
         still coming up"
    );
    let replies = events
        .iter()
        .filter(|e| matches!(e, LspEvent::Reply { .. }))
        .count();
    assert_eq!(
        replies, issued,
        "every accepted request must settle exactly once — {replies}/{issued} did, \
         so the rest stranded their deferred callbacks"
    );
}

#[test]
fn a_handshake_that_completes_normally_flushes_what_it_buffered() {
    // The control: the cap must only fire on a WEDGED server. A slow-but-live one
    // buffers, completes, and sends what it held — killing it would break every
    // request issued during a normal (slow) startup.
    let mut c = SyncLspClient::new();
    c.ensure_server(key(), spawn());
    let _ = c.take_wire_ops();
    c.notify(
        key(),
        LspNotify::DidOpen {
            uri: Url::parse("file:///a.rs").unwrap(),
            language_id: "rust".into(),
            version: 1,
            text: "fn main() {}".into(),
        },
    );
    assert!(
        sent_method(&mut c, "textDocument/didOpen").is_none(),
        "nothing is sent before the handshake completes"
    );

    feed(
        &mut c,
        1,
        &json_frame(&serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "result": { "capabilities": {} }
        })),
    );
    assert!(
        sent_method(&mut c, "textDocument/didOpen").is_some(),
        "the buffered notification must flush once the server is ready"
    );
}
