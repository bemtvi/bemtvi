//! The remote-mode wire layer (`RemoteClient`): synchronous msgpack-RPC framing, the rich
//! `view_json` serialization, and the outgoing RPC encoders.
//!
//! These run on the **host** target (linking the rlib), driving `RemoteClient` exactly as
//! `web/index.html` does in remote mode — feed raw bytes in, read the JSON view back out,
//! and decode the bytes the encoder methods return. No browser, no server: a hand-built
//! `redraw` frame stands in for the wire.

use nxvim_web::RemoteClient;
use rmpv::Value;
use serde_json::Value as Json;

/// `RemoteClient::new` installs a wasm-bindgen panic hook that aborts on a non-wasm host
/// (it forwards to an imported JS `console.error`). Drop it back to the default so a failed
/// assertion unwinds and reports normally — same dance as the other web host tests.
fn client(w: usize, h: usize) -> RemoteClient {
    let c = RemoteClient::new(w, h);
    let _ = std::panic::take_hook();
    c
}

fn s(v: &str) -> Value {
    Value::from(v)
}

/// A `{ key => value }` msgpack map from string keys.
fn map(entries: Vec<(&str, Value)>) -> Value {
    Value::Map(entries.into_iter().map(|(k, v)| (s(k), v)).collect())
}

/// Encode a `redraw` notification `[2, "redraw", [map]]` for the given window `lines`,
/// with a one-entry style palette (`fg = #ff8800`), a keyword highlight on row 0, and a
/// styled status segment — enough to exercise every server-only field path.
fn redraw_frame(lines: &[&str]) -> Vec<u8> {
    let styles = Value::Array(vec![map(vec![("fg", Value::from(0xff_8800u64))])]);
    let line_vals = Value::Array(lines.iter().map(|l| s(l)).collect());
    // highlights: per row, an array of [start, end, group, style_id] spans. Row 0 only.
    let mut hl_rows: Vec<Value> = lines.iter().map(|_| Value::Array(vec![])).collect();
    if !hl_rows.is_empty() {
        hl_rows[0] = Value::Array(vec![Value::Array(vec![
            Value::from(0u64),
            Value::from(5u64),
            s("keyword"),
            Value::from(0u64),
        ])]);
    }
    let window = map(vec![
        ("lines", line_vals),
        ("highlights", Value::Array(hl_rows)),
        (
            "status",
            Value::Array(vec![map(vec![
                ("text", s("x.rs")),
                ("style", Value::from(0u64)),
            ])]),
        ),
    ]);
    let redraw_map = map(vec![
        ("mode_label", s("NORMAL")),
        ("styles", styles),
        ("windows", Value::Array(vec![window])),
    ]);
    let frame = Value::Array(vec![
        Value::from(2u64),
        s("redraw"),
        Value::Array(vec![redraw_map]),
    ]);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &frame).unwrap();
    buf
}

/// Parse the client's `view_json()` into a `serde_json::Value` for field assertions.
fn view(c: &mut RemoteClient) -> Json {
    serde_json::from_str(&c.view_json()).unwrap()
}

#[test]
fn decodes_redraw_into_rich_view_json() {
    let mut c = client(80, 24);
    c.feed(&redraw_frame(&["hello world"]));
    assert!(c.dirty(), "a redraw landed");
    assert!(!c.closed());

    let v = view(&mut c);
    assert_eq!(v["mode"], "NORMAL");
    // Style palette resolved to CSS hex.
    assert_eq!(v["styles"][0]["fg"], "#ff8800");
    let win = &v["windows"][0];
    assert_eq!(win["lines"][0], "hello world");
    // The keyword highlight span survives as [start, end, group, style_id].
    assert_eq!(
        win["highlights"][0][0],
        serde_json::json!([0, 5, "keyword", 0])
    );
    // The status segment is segment-based (text + resolved style), not synthesized.
    assert_eq!(win["status"][0]["text"], "x.rs");
    assert_eq!(win["status"][0]["style"]["fg"], "#ff8800");

    // view_json() clears the dirty flag.
    assert!(!c.dirty(), "view_json consumed the redraw");
}

#[test]
fn reassembles_a_frame_split_across_two_feeds() {
    let mut c = client(80, 24);
    let frame = redraw_frame(&["split me"]);
    let mid = frame.len() / 2;

    c.feed(&frame[..mid]);
    assert!(!c.dirty(), "a partial frame must not produce a redraw");
    assert!(!c.closed());

    c.feed(&frame[mid..]);
    assert!(c.dirty(), "the completed frame produces a redraw");
    assert_eq!(view(&mut c)["windows"][0]["lines"][0], "split me");
}

#[test]
fn processes_multiple_frames_in_one_feed() {
    let mut c = client(80, 24);
    let mut both = redraw_frame(&["first"]);
    both.extend_from_slice(&redraw_frame(&["second"]));

    c.feed(&both);
    assert!(c.dirty());
    // The last frame wins (full frames replace the view); both were drained.
    assert_eq!(view(&mut c)["windows"][0]["lines"][0], "second");
}

#[test]
fn an_over_deep_frame_closes_the_stream() {
    let mut c = client(80, 24);
    // 200 nested 1-element arrays (`0x91` = fixarray len 1), then a nil. This blows the
    // decoder's depth limit (kept equal to nxvim-rpc's MAX_DEPTH = 128) — a decode error
    // that is *not* a short read, so the leading bytes will never decode and the stream is
    // torn down rather than re-read forever. Guards both the corrupt-frame path and the
    // MAX_DEPTH parity with the server's reader.
    let mut bytes = vec![0x91u8; 200];
    bytes.push(0xc0); // nil, the innermost element
    c.feed(&bytes);
    assert!(
        c.closed(),
        "a structurally corrupt (over-deep) frame closes the stream"
    );
}

#[test]
fn encodes_outgoing_rpc_frames() {
    let mut c = client(120, 40);

    // attach() is a request [0, id, "nvim_ui_attach", [w, h, {}]].
    let attach = decode(&c.attach());
    let a = attach.as_array().unwrap();
    assert_eq!(a[0].as_u64(), Some(0), "request type");
    assert_eq!(a[2].as_str(), Some("nvim_ui_attach"));
    let params = a[3].as_array().unwrap();
    assert_eq!(params[0].as_u64(), Some(120));
    assert_eq!(params[1].as_u64(), Some(40));

    // input("x") is a notification [2, "nvim_input", ["x"]].
    let input = decode(&c.input("x"));
    let i = input.as_array().unwrap();
    assert_eq!(i[0].as_u64(), Some(2), "notification type");
    assert_eq!(i[1].as_str(), Some("nvim_input"));
    assert_eq!(i[2].as_array().unwrap()[0].as_str(), Some("x"));

    // input_mouse forwards [button, action, modifier, grid=0, row, col].
    let mouse = decode(&c.input_mouse("left", "press", "", 3, 5));
    let m = mouse.as_array().unwrap();
    assert_eq!(m[1].as_str(), Some("nvim_input_mouse"));
    let mp = m[2].as_array().unwrap();
    assert_eq!(mp[0].as_str(), Some("left"));
    assert_eq!(mp[1].as_str(), Some("press"));
    assert_eq!(mp[3].as_u64(), Some(0), "single-grid");
    assert_eq!(mp[4].as_u64(), Some(3));
    assert_eq!(mp[5].as_u64(), Some(5));

    // key() goes through the shared notation encoder (ctrl-w → "<C-w>").
    let ctrl_w = decode(&c.key(true, false, false, "w"));
    assert_eq!(
        ctrl_w.as_array().unwrap()[2].as_array().unwrap()[0].as_str(),
        Some("<C-w>")
    );

    // An unrecognized key name encodes to nothing.
    assert!(c.key(false, false, false, "").is_empty());
}

#[test]
fn owes_a_response_to_a_server_request() {
    let mut c = client(80, 24);
    // A server→client request [0, 7, "some_method", []].
    let req = Value::Array(vec![
        Value::from(0u64),
        Value::from(7u64),
        s("some_method"),
        Value::Array(vec![]),
    ]);
    let mut buf = Vec::new();
    rmpv::encode::write_value(&mut buf, &req).unwrap();
    c.feed(&buf);

    let resp = c.take_response().expect("a response is owed");
    let r = decode(&resp);
    let ra = r.as_array().unwrap();
    assert_eq!(ra[0].as_u64(), Some(1), "response type");
    assert_eq!(ra[1].as_u64(), Some(7), "echoes the request id");
    assert!(ra[2].is_nil(), "no error");
    assert!(
        c.take_response().is_none(),
        "the owed response is taken once"
    );
}

fn decode(bytes: &[u8]) -> Value {
    rmpv::decode::read_value(&mut &bytes[..]).unwrap()
}
