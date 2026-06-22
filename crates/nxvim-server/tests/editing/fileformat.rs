//! `'fileformat'` (`'ff'`) — the line-ending convention a buffer was read with and is
//! written back with. The rope always holds `\n`; read detects the style and normalizes,
//! `:w` converts back. These assert the option plumbing AND the read/write round-trip.
//!
//! (Trailing-newline note, as in `encoding.rs`: nxvim keeps a phantom trailing `\n`, so a
//! byte-identical round-trip needs the fixture to already end in a line break.)

use crate::support::*;

async fn open_file(path: &std::path::Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    start(Some(path.to_string_lossy().into_owned())).await
}

#[tokio::test]
async fn fileformat_query_defaults_to_unix() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set fileformat?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileformat=unix")
    );
}

#[tokio::test]
async fn fileformat_rejects_unknown_value() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set ff=banana<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E474"), "expected E474, got {msg:?}");
}

#[tokio::test]
async fn dos_file_opens_clean_and_round_trips() {
    // A CRLF file opens with clean lines (no stray \r), reports fileformat=dos, and `:w`
    // reproduces the \r\n bytes — the rope normalized to \n, the write converted back.
    let path = temp_path("ff_dos");
    let original: &[u8] = b"one\r\ntwo\r\n";
    std::fs::write(&path, original).expect("write dos file");
    let (rpc, mut incoming) = open_file(&path).await;

    assert_eq!(lines(&rpc).await, vec!["one", "two"], "lines carry no \\r");
    let map = redraw_after(&rpc, &mut incoming, ":set ff?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileformat=dos")
    );
    // the value the statusline (nx.bo) reads agrees
    let ff = exec_lua(&rpc, "return nx.bo[0].fileformat").await;
    assert_eq!(ff.as_str(), Some("dos"));

    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        original,
        "an unedited dos buffer writes back byte-identical"
    );
}

#[tokio::test]
async fn unix_file_reports_unix() {
    let path = temp_path("ff_unix");
    std::fs::write(&path, b"one\ntwo\n").expect("write unix file");
    let (rpc, _incoming) = open_file(&path).await;
    let ff = exec_lua(&rpc, "return nx.bo[0].fileformat").await;
    assert_eq!(ff.as_str(), Some("unix"));
}

#[tokio::test]
async fn set_ff_unix_converts_dos_on_write() {
    // Opening a dos file then `:set ff=unix` makes the next write emit \n endings.
    let path = temp_path("ff_convert");
    std::fs::write(&path, b"a\r\nb\r\n").expect("write dos file");
    let (rpc, mut incoming) = open_file(&path).await;

    feed(&rpc, ":set ff=unix<CR>");
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        b"a\nb\n",
        ":set ff=unix converts the line endings on write"
    );
}
