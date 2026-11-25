//! `'fileformat'` (`'ff'`) — the line-ending convention a buffer was read with and is
//! written back with. The rope always holds `\n`; read detects the style and normalizes,
//! `:w` converts back. These assert the option plumbing AND the read/write round-trip.
//!
//! (Trailing-newline note, as in `encoding.rs`: bemtvi keeps a phantom trailing `\n`, so a
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
    // the value the statusline (btv.bo) reads agrees
    let ff = exec_lua(&rpc, "return btv.bo[0].fileformat").await;
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
    let ff = exec_lua(&rpc, "return btv.bo[0].fileformat").await;
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

// A `\r` that isn't part of a line break is a character of its line, as vim has it (it
// paints as `^M`). The rope only ever holds `\n` breaks, every `\r\n` or classic-Mac `\r`
// having been normalized on read, so a `\r` left in it is text: splitting the line there
// put every row after it one ahead of tree-sitter's, which counts rows by `\n` alone.
#[tokio::test]
async fn a_lone_carriage_return_is_part_of_its_line() {
    let path = temp_path("ff_lone_cr");
    let original: &[u8] = b"a\rb\nc\n";
    std::fs::write(&path, original).expect("write file");
    let (rpc, mut incoming) = open_file(&path).await;
    assert_eq!(lines(&rpc).await, vec!["a\rb", "c"]);
    let ff = exec_lua(&rpc, "return btv.bo[0].fileformat").await;
    assert_eq!(ff.as_str(), Some("unix"));
    // It paints as `^M`, inside its line.
    let map = redraw_after(&rpc, &mut incoming, "gg").await;
    assert_eq!(first_visible_line(&map), "a^Mb");
    // `j` from the first line reaches the second, `c`.
    feed(&rpc, "ggjx");
    assert_eq!(lines(&rpc).await, vec!["a\rb", ""]);
    feed(&rpc, "u");
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}

#[tokio::test]
async fn a_lone_carriage_return_in_a_dos_file_stays_in_its_line() {
    let path = temp_path("ff_dos_lone_cr");
    let original: &[u8] = b"one\r\nt\rwo\r\n";
    std::fs::write(&path, original).expect("write dos file");
    let (rpc, mut incoming) = open_file(&path).await;
    assert_eq!(lines(&rpc).await, vec!["one", "t\rwo"]);
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}

#[tokio::test]
async fn a_classic_mac_file_still_breaks_at_each_carriage_return() {
    let path = temp_path("ff_mac");
    let original: &[u8] = b"one\rtwo\r";
    std::fs::write(&path, original).expect("write mac file");
    let (rpc, mut incoming) = open_file(&path).await;
    assert_eq!(lines(&rpc).await, vec!["one", "two"]);
    let ff = exec_lua(&rpc, "return btv.bo[0].fileformat").await;
    assert_eq!(ff.as_str(), Some("mac"));
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}

// vim's detection: Dos only when every `\n` follows a `\r`, so a file mixing the two is
// Unix and its `\r`s stay in their lines. (The first line break used to decide, so a
// stray `\r` early in a Unix file made the whole file Mac: every `\r` became a break
// and `:w` wrote every `\n` back as a `\r`.)
#[tokio::test]
async fn a_file_mixing_line_breaks_is_unix_and_writes_back_unchanged() {
    let path = temp_path("ff_mixed");
    let original: &[u8] = b"one\r\ntwo\n";
    std::fs::write(&path, original).expect("write file");
    let (rpc, mut incoming) = open_file(&path).await;
    assert_eq!(lines(&rpc).await, vec!["one\r", "two"]);
    let ff = exec_lua(&rpc, "return btv.bo[0].fileformat").await;
    assert_eq!(ff.as_str(), Some("unix"));
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}

// A Unix file whose last byte is a lone `\r` ends without a line break (`'noeol'`, as
// vim reads it): that `\r` is a character of the last line, not a classic-Mac line end.
#[tokio::test]
async fn a_unix_file_ending_in_a_lone_carriage_return_has_no_final_newline() {
    let path = temp_path("ff_trailing_cr");
    let original: &[u8] = b"a\nb\r";
    std::fs::write(&path, original).expect("write file");
    let (rpc, mut incoming) = open_file(&path).await;
    assert_eq!(lines(&rpc).await, vec!["a", "b\r"]);
    let eol = exec_lua(&rpc, "return btv.bo[0].endofline").await;
    assert_eq!(eol.as_bool(), Some(false));
    redraw_after(&rpc, &mut incoming, ":set nofixeol<CR>").await;
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}
