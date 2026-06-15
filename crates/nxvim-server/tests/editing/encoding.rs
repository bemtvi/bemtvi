//! `'fileencoding'` / `'fileencodings'` / `'bomb'` options (Phase 1 of the
//! multi-encoding work, docs/plans/2026-06-14-encoding-and-invalid-utf8.md).
//!
//! This phase wires the *options* — `:set` and `vim.bo`/`vim.o` accept the
//! values, validate them (fail loud on garbage), and read them back. The
//! convert-on-read/write seam they drive is a later phase; here we only assert
//! the option plumbing.

use crate::support::*;

#[tokio::test]
async fn fileencoding_query_defaults_to_utf8() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set fileencoding?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=utf-8")
    );
}

#[tokio::test]
async fn fileencoding_accepts_and_echoes_latin1() {
    // `latin1` resolves to windows-1252 (browser-style) but reads back under its
    // vim spelling, so a round-trip through `:set fenc=` is stable.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, ":set fenc=latin1<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=latin1")
    );
}

#[tokio::test]
async fn fileencoding_rejects_unknown_value() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set fenc=no-such-charset<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E474"),
        "expected E474 invalid-argument, got {msg:?}"
    );
}

#[tokio::test]
async fn fileencoding_is_buffer_local() {
    // Like `regexsyntax`/`tabstop`, `:set fenc` sets a per-buffer value: one
    // buffer can be latin1 while a fresh one keeps the utf-8 default.
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "isome text<Esc>"); // make buffer 1 non-throwaway
    feed(&rpc, ":set fenc=latin1<CR>"); // buffer 1 -> latin1
    feed(&rpc, ":enew<CR>"); // buffer 2 -> default utf-8
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=utf-8"),
        "a fresh buffer carries the utf-8 default"
    );
    feed(&rpc, ":bp<CR>"); // back to buffer 1
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=latin1"),
        "buffer 1 kept its own latin1 value"
    );
}

#[tokio::test]
async fn fileencoding_marks_buffer_modified() {
    // Changing the on-disk encoding implies the next write re-encodes, so the
    // buffer differs from disk — vim marks it modified.
    let (rpc, _i) = start(None).await;
    assert_eq!(
        lua_bool(&rpc, "return vim.bo.modified").await,
        Some(false),
        "a fresh buffer starts unmodified"
    );
    feed(&rpc, ":set fenc=latin1<CR>");
    assert_eq!(
        lua_bool(&rpc, "return vim.bo.modified").await,
        Some(true),
        "setting fileencoding marks the buffer modified"
    );
}

#[tokio::test]
async fn fileencoding_settable_via_vim_bo() {
    let (rpc, _i) = start(None).await;
    exec_lua(&rpc, "vim.bo.fileencoding = 'latin1'").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.fileencoding").await.as_str(),
        Some("latin1"),
        "vim.bo.fileencoding write-through reads back through the mirror"
    );
}

#[tokio::test]
async fn fileencodings_query_defaults_to_the_detection_list() {
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set fileencodings?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencodings=ucs-bom,utf-8,latin1")
    );
}

#[tokio::test]
async fn fileencodings_rejects_an_unknown_entry() {
    // The `ucs-bom` BOM-sniff pseudo-entry is fine, but a bogus encoding label
    // anywhere in the list fails the whole `:set` loud.
    let (rpc, mut incoming) = start(None).await;
    let map = redraw_after(&rpc, &mut incoming, ":set fencs=ucs-bom,bogus<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        msg.contains("E474"),
        "expected E474 invalid-argument, got {msg:?}"
    );
}

#[tokio::test]
async fn fileencodings_settable_via_vim_o() {
    let (rpc, _i) = start(None).await;
    exec_lua(&rpc, "vim.o.fileencodings = 'utf-8,latin1'").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.o.fileencodings").await.as_str(),
        Some("utf-8,latin1"),
        "vim.o.fileencodings is a global string read back through the mirror"
    );
}

#[tokio::test]
async fn bomb_toggles_and_is_buffer_local() {
    let (rpc, _i) = start(None).await;
    assert_eq!(
        lua_bool(&rpc, "return vim.bo.bomb").await,
        Some(false),
        "no BOM by default"
    );
    feed(&rpc, ":set bomb<CR>");
    assert_eq!(lua_bool(&rpc, "return vim.bo.bomb").await, Some(true));
    feed(&rpc, ":set nobomb<CR>");
    assert_eq!(lua_bool(&rpc, "return vim.bo.bomb").await, Some(false));
}

// ===== Phases 2–3: the read/write transcode seam =============================
//
// Each test writes a temp file with raw bytes, opens it through the server's
// normal (synchronous, native) startup read, and asserts on what the buffer shows
// and — after `:w` — on the exact bytes back on disk. nxvim keeps the rope UTF-8
// and `'fileencoding'` governs the on-disk form; these prove the round-trip.
//
// Note on trailing newlines: nxvim always maintains a trailing newline in the rope
// (the phantom final line), so a byte-identical round-trip requires the original
// file to already end in one — every fixture here does.

/// Open `path` through a real server and return the connected client. (A thin alias
/// for `start(Some(path))` that documents intent at these call sites.)
async fn open_file(path: &std::path::Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    start(Some(path.to_string_lossy().into_owned())).await
}

#[tokio::test]
async fn invalid_utf8_opens_and_round_trips_byte_identical() {
    // A file with bytes that aren't valid UTF-8 no longer refuses to open: it falls
    // through `'fileencodings'` to the latin1 (windows-1252) terminal fallback, which
    // is a total, bijective single-byte codec — so it opens *and* `:w` reproduces the
    // original bytes exactly, with no lossy `from_utf8_lossy` corruption.
    let path = temp_path("enc_invalid");
    let original: &[u8] = b"hello \xff\xfe world\n";
    std::fs::write(&path, original).expect("write invalid-utf8 file");
    let (rpc, mut incoming) = open_file(&path).await;

    assert_eq!(
        lines(&rpc).await,
        vec!["hello ÿþ world"],
        "0xff/0xfe decode to ÿ/þ via the latin1 fallback"
    );
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=latin1"),
        "an undecodable-as-utf8 file lands on the latin1 fallback"
    );

    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        original,
        "writing back an unedited resilience buffer must be byte-identical"
    );
}

#[tokio::test]
async fn latin1_decodes_and_round_trips() {
    // A real latin1 file: 0xe9 is é. It opens showing é, carries fileencoding=latin1,
    // and `:w` reproduces the 0xe9 byte (not the two-byte utf-8 é).
    let path = temp_path("enc_latin1");
    std::fs::write(&path, b"caf\xe9\n").expect("write latin1 file");
    let (rpc, mut incoming) = open_file(&path).await;

    assert_eq!(lines(&rpc).await, vec!["café"]);
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=latin1")
    );

    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        b"caf\xe9\n",
        "a latin1 buffer re-encodes é back to the single byte 0xe9"
    );
}

#[tokio::test]
async fn utf16le_bom_decodes_sets_bomb_and_reemits() {
    // A UTF-16LE file with a BOM: it decodes, fileencoding=utf-16le, bomb is set, and
    // `:w` re-emits the BOM and writes the text back as UTF-16LE (incl. the 0a 00
    // newline) — exactly the original bytes.
    let path = temp_path("enc_utf16");
    let mut original = vec![0xFF, 0xFE]; // UTF-16LE BOM
    for unit in "Hi\n".encode_utf16() {
        original.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(&path, &original).expect("write utf-16le file");
    let (rpc, mut incoming) = open_file(&path).await;

    assert_eq!(lines(&rpc).await, vec!["Hi"]);
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=utf-16le")
    );
    assert_eq!(
        lua_bool(&rpc, "return vim.bo.bomb").await,
        Some(true),
        "a BOM'd file carries bomb=true"
    );

    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        original,
        "writing back re-emits the BOM and the UTF-16LE encoding exactly"
    );
}

#[tokio::test]
async fn utf8_bom_decodes_sets_bomb_and_reemits() {
    // A UTF-8 file with a BOM: fileencoding=utf-8, bomb=true, and `:w` keeps the BOM.
    let path = temp_path("enc_utf8bom");
    let mut original = vec![0xEF, 0xBB, 0xBF];
    original.extend_from_slice("héllo\n".as_bytes());
    std::fs::write(&path, &original).expect("write utf-8 BOM file");
    let (rpc, mut incoming) = open_file(&path).await;

    assert_eq!(
        lines(&rpc).await,
        vec!["héllo"],
        "the BOM is stripped from the text"
    );
    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=utf-8")
    );
    assert_eq!(lua_bool(&rpc, "return vim.bo.bomb").await, Some(true));

    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}

#[tokio::test]
async fn set_fenc_utf8_converts_a_latin1_buffer_on_write() {
    // Opening a latin1 file then `:set fenc=utf-8` re-encodes it to utf-8 on `:w`:
    // the single byte 0xe9 becomes the two-byte utf-8 é (c3 a9).
    let path = temp_path("enc_convert");
    std::fs::write(&path, b"caf\xe9\n").expect("write latin1 file");
    let (rpc, mut incoming) = open_file(&path).await;

    feed(&rpc, ":set fenc=utf-8<CR>");
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        "café\n".as_bytes(),
        "the buffer is rewritten as utf-8"
    );
}

#[tokio::test]
async fn write_fails_loud_on_unrepresentable_char() {
    // A char the target encoding can't represent aborts the write loud (E513) and
    // leaves the file untouched — never a silently NCR-mangled save. (Note: € is
    // representable in windows-1252 as 0x80; a CJK char like 中 genuinely isn't.)
    let path = temp_path("enc_faillou");
    let original: &[u8] = b"caf\xe9\n";
    std::fs::write(&path, original).expect("write latin1 file");
    let (rpc, mut incoming) = open_file(&path).await;

    feed(&rpc, "o中<Esc>"); // append a line with an unrepresentable-in-latin1 char
    let map = redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E513"), "expected a loud E513, got {msg:?}");

    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        original,
        "a failed encode must leave the file untouched"
    );
}

#[tokio::test]
async fn valid_utf8_with_spua_scalar_round_trips_unchanged() {
    // A valid utf-8 file that genuinely contains a Supplementary-PUA-A scalar
    // (U+F0041) decodes as utf-8 and writes back byte-identical — no spurious escape
    // or re-mapping corrupts it. (Guards the bijection the original PUA scheme worried
    // about; with the windows-1252 fallback the scalar is never touched.)
    let path = temp_path("enc_spua");
    let original = "x\u{F0041}y\n".as_bytes().to_vec();
    std::fs::write(&path, &original).expect("write spua file");
    let (rpc, mut incoming) = open_file(&path).await;

    let map = redraw_after(&rpc, &mut incoming, ":set fenc?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fileencoding=utf-8"),
        "a valid utf-8 file is detected as utf-8, not the fallback"
    );
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(std::fs::read(&path).expect("re-read"), original);
}

/// The first window's projected display rows — the `^X` / `<xx>`-substituted wire
/// text the client paints, *not* the raw buffer content `nvim_buf_get_lines`
/// returns.
fn window_display_lines(map: &[(Value, Value)]) -> Vec<String> {
    let windows = map_get(map, "windows")
        .and_then(Value::as_array)
        .expect("a windows array");
    let Value::Map(w0) = &windows[0] else {
        panic!("window 0 is not a map")
    };
    map_get(w0, "lines")
        .and_then(Value::as_array)
        .expect("a lines array")
        .iter()
        .map(|l| l.as_str().unwrap_or("").to_string())
        .collect()
}

/// The highlight group names on display row `row` of the first window.
fn window_hl_groups(map: &[(Value, Value)], row: usize) -> Vec<String> {
    let windows = map_get(map, "windows")
        .and_then(Value::as_array)
        .expect("a windows array");
    let Value::Map(w0) = &windows[0] else {
        panic!("window 0 is not a map")
    };
    map_get(w0, "highlights")
        .and_then(Value::as_array)
        .and_then(|rows| rows.get(row))
        .and_then(Value::as_array)
        .map(|spans| {
            spans
                .iter()
                .filter_map(|s| s.as_array()?.get(2)?.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn unprintable_control_bytes_render_as_caret_and_hex_tokens() {
    // A file with an embedded C0 control (0x01) and a C1 control (0x81, an
    // undefined windows-1252 high byte that the latin1 fallback passes through as
    // U+0081) decodes resiliently — but those bytes would paint as a font tofu box.
    // The display projection substitutes them vim-style: C0/DEL as `^X` caret
    // notation (2 cells), C1 as `<xx>` hex (4 cells). The buffer still holds the
    // raw scalars (so `:w` round-trips), only the rendered row changes.
    let path = temp_path("enc_control");
    let original: &[u8] = b"a\x01b\x81c\n";
    std::fs::write(&path, original).expect("write control-byte file");
    let (rpc, mut incoming) = open_file(&path).await;

    // The rope keeps the decoded scalars — content reads are unaffected.
    assert_eq!(
        lines(&rpc).await,
        vec!["a\u{1}b\u{81}c"],
        "the buffer holds the raw control scalars, not their display form"
    );

    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        window_display_lines(&map).first().map(String::as_str),
        Some("a^Ab<81>c"),
        "0x01 → ^A caret, 0x81 → <81> hex in the painted row"
    );

    // Each token is overlaid with `SpecialKey` so it reads as non-text (one span
    // for `^A`, one for `<81>`).
    let groups = window_hl_groups(&map, 0);
    assert_eq!(
        groups.iter().filter(|g| *g == "SpecialKey").count(),
        2,
        "both substituted tokens carry a SpecialKey highlight: {groups:?}"
    );

    // Round-trip is still byte-identical (display is purely cosmetic).
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        original,
        "the display substitution must not touch the on-disk bytes"
    );
}
