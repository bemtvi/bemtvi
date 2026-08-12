//! `'endofline'` (`'eol'`) / `'fixendofline'` (`'fixeol'`) — whether the file's last
//! line is terminated, and whether a write terminates it.
//!
//! bemtvi's rope always ends in a `\n` (the phantom that stands for vim's implicit
//! newline after every line), so this pair is the only place the fact "the file had no
//! final newline" lives. `'endofline'` records it on read; `'fixendofline'` (on by
//! default, as in vim) decides whether `:w` supplies the missing one. Together they
//! make `Buffer::document_text()` — the bytes the buffer *represents* — well-defined.
//!
//! Reference behavior probed against `nvim --headless -u NONE` (see
//! `docs/plans/2026-07-26-endofline.md` for the table these mirror).

use crate::support::*;

async fn open_file(path: &std::path::Path) -> (Rpc, UnboundedReceiver<Incoming>) {
    start(Some(path.to_string_lossy().into_owned())).await
}

/// The first window's whole status line as one string (the `status` segment array
/// flattened) — the same accessor `statusline.rs` uses.
fn status_text(map: &[(Value, Value)]) -> String {
    field(map, "status")
        .and_then(Value::as_array)
        .expect("a status segment array")
        .iter()
        .filter_map(|seg| {
            let Value::Map(m) = seg else { return None };
            m.iter()
                .find(|(k, _)| k.as_str() == Some("text"))
                .and_then(|(_, v)| v.as_str())
        })
        .collect()
}

/// Every window's status line, flattened — so a check can cover a panel/listing
/// window as well as the document one that opened under it.
fn all_status_text(map: &[(Value, Value)]) -> String {
    field(map, "windows")
        .and_then(Value::as_array)
        .expect("a window array")
        .iter()
        .filter_map(|w| {
            let Value::Map(m) = w else { return None };
            m.iter()
                .find(|(k, _)| k.as_str() == Some("status"))
                .and_then(|(_, v)| v.as_array())
        })
        .flatten()
        .filter_map(|seg| {
            let Value::Map(m) = seg else { return None };
            m.iter()
                .find(|(k, _)| k.as_str() == Some("text"))
                .and_then(|(_, v)| v.as_str())
        })
        .collect()
}

/// Open `bytes` as a file, run `keys`, and return what ended up on disk.
async fn round_trip(tag: &str, bytes: &[u8], keys: &str) -> Vec<u8> {
    let path = temp_path(tag);
    std::fs::write(&path, bytes).expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    redraw_after(&rpc, &mut incoming, keys).await;
    std::fs::read(&path).expect("re-read")
}

#[tokio::test]
async fn read_detects_whether_the_last_line_is_terminated() {
    // The flag is set from the bytes: a terminated file reads `eol` on, an
    // unterminated one off. An *empty* file has no final newline either, and reads off
    // — which is what keeps its document empty (see the 0-byte test below).
    for (tag, bytes, expected) in [
        ("eol_read_yes", &b"a\nb\n"[..], true),
        ("eol_read_no", &b"a\nb"[..], false),
        ("eol_read_just_nl", &b"\n"[..], true),
        ("eol_read_empty", &b""[..], false),
    ] {
        let path = temp_path(tag);
        std::fs::write(&path, bytes).expect("write fixture");
        let (rpc, _incoming) = open_file(&path).await;
        let eol = exec_lua(&rpc, "return btv.bo[0].endofline").await;
        assert_eq!(
            eol.as_bool(),
            Some(expected),
            "{tag}: 'endofline' for {bytes:?}"
        );
        // `'fixendofline'` defaults on, matching vim, so the default write behavior is
        // unchanged by any of this.
        let fixeol = exec_lua(&rpc, "return btv.bo[0].fixendofline").await;
        assert_eq!(
            fixeol.as_bool(),
            Some(true),
            "{tag}: 'fixendofline' default"
        );
    }
}

#[tokio::test]
async fn a_file_without_a_final_newline_gains_one_by_default() {
    // vim's `'fixeol'` is on out of the box and bemtvi matches it: the missing newline
    // is supplied on write. This is the *current* bemtvi behavior too — the point of the
    // option is that it is now a choice rather than the only possibility.
    assert_eq!(
        round_trip("eol_fix_default", b"a\nb", ":w<CR>").await,
        b"a\nb\n",
        "the default write terminates the last line"
    );
}

#[tokio::test]
async fn nofixendofline_round_trips_a_file_with_no_final_newline() {
    // The whole point: opt out and the file survives a save byte for byte.
    assert_eq!(
        round_trip("eol_nofix", b"a\nb", ":set nofixeol<CR>:w<CR>").await,
        b"a\nb",
        "'nofixendofline' leaves the last line unterminated"
    );
}

#[tokio::test]
async fn nofixendofline_still_writes_the_newline_a_file_already_had() {
    // `'fixendofline'` only *supplies* a missing terminator; it never removes one. A
    // normal file is untouched by turning it off.
    assert_eq!(
        round_trip("eol_nofix_had_one", b"a\nb\n", ":set nofixeol<CR>:w<CR>").await,
        b"a\nb\n",
        "a terminated file is unaffected by 'nofixendofline'"
    );
}

#[tokio::test]
async fn set_noeol_drops_the_terminator_under_nofixendofline() {
    // Both halves are settable: `:set noeol` says "this document does not end with a
    // newline" and `nofixeol` says "don't add one back", so together they strip a
    // trailing newline from a file that had one. (Under the default `fixeol` the write
    // would put it straight back — that's the pair working as documented.)
    assert_eq!(
        round_trip("eol_set_noeol", b"a\nb\n", ":set noeol nofixeol<CR>:w<CR>").await,
        b"a\nb",
        ":set noeol nofixeol strips the trailing newline"
    );
    assert_eq!(
        round_trip("eol_set_noeol_fix", b"a\nb\n", ":set noeol<CR>:w<CR>").await,
        b"a\nb\n",
        ":set noeol alone is undone by the default 'fixendofline'"
    );
}

#[tokio::test]
async fn an_empty_file_stays_empty_across_a_write() {
    // vim writes 0 bytes for a 0-byte file (its `ML_EMPTY` case) — bemtvi used to write
    // one, because `to_save_bytes` returned the whole rope and the rope is never empty.
    // The document of a 0-byte file is empty, and `'fixendofline'` never terminates an
    // empty document.
    assert_eq!(
        round_trip("eol_empty", b"", ":w<CR>").await,
        b"",
        "a 0-byte file survives a write at 0 bytes"
    );
}

#[tokio::test]
async fn a_file_holding_one_newline_is_not_an_empty_document() {
    // The case that needs `'endofline'` to be defined off the *bytes*: `"\n"` and `""`
    // are the same rope and the same single empty line, and only the flag tells them
    // apart. Writing must not collapse this file to 0 bytes.
    assert_eq!(
        round_trip("eol_one_nl", b"\n", ":w<CR>").await,
        b"\n",
        "a file holding exactly one newline round-trips at one byte"
    );
}

#[tokio::test]
async fn a_buffer_emptied_by_editing_keeps_the_terminator_its_file_had() {
    // The third documented divergence (see `docs/plans/2026-07-26-endofline.md`). vim
    // writes 0 bytes here — but *not* because `'eol'` changed: probed, `&eol` is still
    // `1` after `ggdG`, and the 0-byte write comes from `ML_EMPTY`, the second hidden
    // bit bemtvi deliberately does without. That bit is also why vim can undo back to a
    // terminated file: with one honest flag, clearing it on `ggdG` would make
    // `ggdG` + `u` + `:w` under `'nofixendofline'` *drop* a terminator the file has —
    // strictly worse than writing the one byte the flag still honestly describes.
    assert_eq!(
        round_trip("eol_emptied_by_editing", b"a\n", "ggdG:w<CR>").await,
        b"\n",
        "the flag still says the document ends with a newline, so the write does"
    );
    // A no-eol file emptied the same way *does* reach 0 bytes — its flag was already
    // off — which is vim's answer too.
    assert_eq!(
        round_trip("eol_emptied_noeol", b"a", "ggdG:w<CR>").await,
        b"",
        "an unterminated file emptied by editing writes nothing"
    );
    // And the undo the divergence buys: the terminator survives a round trip through
    // an emptied buffer even with the fixer off.
    assert_eq!(
        round_trip("eol_emptied_undone", b"a\n", ":set nofixeol<CR>ggdGu:w<CR>").await,
        b"a\n",
        "undoing back to the file's content writes the file's own bytes"
    );
}

#[tokio::test]
async fn typing_into_an_empty_file_terminates_the_line_it_created() {
    // Once the document has content, `'fixendofline'` applies to it as usual.
    assert_eq!(
        round_trip("eol_empty_typed", b"", "ix<Esc>:w<CR>").await,
        b"x\n",
        "content typed into an empty file is written terminated"
    );
    assert_eq!(
        round_trip(
            "eol_empty_typed_nofix",
            b"",
            ":set nofixeol<CR>ix<Esc>:w<CR>"
        )
        .await,
        b"x",
        "…and unterminated when 'fixendofline' is off"
    );
}

#[tokio::test]
async fn editing_a_no_eol_file_leaves_only_its_last_line_unterminated() {
    // `'endofline'` describes the document's *end*, not any particular line: appending
    // a line to a no-eol file leaves the new last line unterminated and every earlier
    // line terminated. (vim behaves identically — probed.)
    assert_eq!(
        round_trip("eol_append", b"a\nb", ":set nofixeol<CR>Goc<Esc>:w<CR>").await,
        b"a\nb\nc",
        "the appended line becomes the unterminated one"
    );
}

#[tokio::test]
async fn a_dos_file_without_a_final_break_round_trips() {
    // The `'fileformat'` conversion runs over the *document*, so a no-eol dos file
    // keeps CRLF between its lines and nothing after the last one.
    assert_eq!(
        round_trip("eol_dos", b"a\r\nb", ":set nofixeol<CR>:w<CR>").await,
        b"a\r\nb",
        "a no-eol dos file round-trips"
    );
    // …and with `'fixendofline'` on, the supplied terminator is a CRLF too.
    assert_eq!(
        round_trip("eol_dos_fix", b"a\r\nb", ":w<CR>").await,
        b"a\r\nb\r\n",
        "the supplied terminator honors 'fileformat'"
    );
}

#[tokio::test]
async fn a_latin1_file_without_a_final_break_round_trips() {
    // The encoding seam sits after the document is assembled, so no-eol composes with
    // a non-UTF-8 `'fileencoding'` (0xE9 is `é` in latin1).
    assert_eq!(
        round_trip("eol_latin1", b"caf\xe9", ":set nofixeol<CR>:w<CR>").await,
        b"caf\xe9",
        "a no-eol latin1 file round-trips"
    );
}

#[tokio::test]
async fn a_write_that_supplied_the_newline_updates_endofline() {
    // bemtvi keeps the flag accurate across a write (vim leaves `&eol` stale at 0 here).
    // It matters: the LSP sync path keys off `'endofline'`, so a stale `false` would pin
    // an ordinary buffer to full-text sync forever.
    let path = temp_path("eol_after_write");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].endofline").await.as_bool(),
        Some(false),
        "reads as no-eol"
    );
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].endofline").await.as_bool(),
        Some(true),
        "'fixendofline' supplied the newline, so the document now ends with one"
    );
    // And under `nofixeol` the flag stays off, because the file really still has no
    // trailing newline.
    let path = temp_path("eol_after_write_nofix");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    feed(&rpc, ":set nofixeol<CR>");
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].endofline").await.as_bool(),
        Some(false),
        "a preserved no-eol file keeps the flag off"
    );
}

#[tokio::test]
async fn the_options_are_settable_from_lua_and_queryable() {
    let path = temp_path("eol_lua");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;

    // `:set eol?` / `:set fixeol?` give a real readout under both the full names and
    // the vim abbreviations.
    let map = redraw_after(&rpc, &mut incoming, ":set eol?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("noendofline")
    );
    let map = redraw_after(&rpc, &mut incoming, ":set fixendofline?<CR>").await;
    assert_eq!(
        field(&map, "message").and_then(Value::as_str),
        Some("fixendofline")
    );

    // A Lua write reaches the live buffer and the mirror reads it back.
    exec_lua(&rpc, "btv.bo[0].fixendofline = false").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].fixendofline")
            .await
            .as_bool(),
        Some(false),
        "btv.bo write reaches the core"
    );
    redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    assert_eq!(
        std::fs::read(&path).expect("re-read"),
        b"a\nb",
        "the Lua-set 'nofixendofline' governs the write"
    );
}

#[tokio::test]
async fn the_written_message_reports_an_unterminated_write() {
    // vim's `"f" [noeol] 2L, 3B written`: the tag says the file it just left on disk
    // has no final line break, which under `'nofixendofline'` is the whole point.
    let path = temp_path("eol_msg_nofix");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    feed(&rpc, ":set nofixeol<CR>");
    let map = redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = field(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        msg.contains("[noeol]") && msg.contains("2L, 3B written"),
        "the write echo tags the unterminated file, got {msg:?}"
    );

    // The default `'fixendofline'` supplied the terminator, so the *same* file writes
    // untagged — the tag describes disk, not how the buffer was read.
    let path = temp_path("eol_msg_fix");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    let map = redraw_after(&rpc, &mut incoming, ":w<CR>").await;
    let msg = field(&map, "message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    assert!(
        !msg.contains("[noeol]") && msg.contains("2L, 4B written"),
        "a terminated write is untagged, got {msg:?}"
    );
}

#[tokio::test]
async fn the_status_line_flags_an_unterminated_file() {
    // The only visible cue that saving may change the file's last byte.
    let path = temp_path("eol_status");
    std::fs::write(&path, b"a\nb").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let status = status_text(&map);
    assert!(
        status.contains("[noeol]"),
        "the default status line flags a no-eol file, got {status:?}"
    );

    let path = temp_path("eol_status_normal");
    std::fs::write(&path, b"a\nb\n").expect("write fixture");
    let (rpc, mut incoming) = open_file(&path).await;
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    let status = status_text(&map);
    assert!(
        !status.contains("[noeol]"),
        "an ordinary file carries no flag, got {status:?}"
    );
}

#[tokio::test]
async fn the_noeol_flag_stays_off_buffers_it_would_say_nothing_about() {
    // `'endofline'` is honestly off for *every* document that doesn't end with a
    // newline — which includes each of these — but the status-line marker means "the
    // file on disk has an unterminated last line", and none of them has one. Flagging
    // them would put `[noeol]` on a stock `bemtvi` with no file at all.

    // 1. `[No Name]`: an empty document. Nothing to terminate.
    let (rpc, mut incoming) = start(None).await;
    let status = status_text(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert!(
        !status.contains("[noeol]"),
        "an empty buffer is not an unterminated file, got {status:?}"
    );

    // 2. A brand-new file (the path doesn't exist yet): also an empty document — vim
    //    shows `[New]` for this, never `[noeol]`.
    let path = temp_path("eol_status_new_file");
    let (rpc, mut incoming) = open_file(&path).await;
    let status = status_text(&redraw_after(&rpc, &mut incoming, "<Esc>").await);
    assert!(
        !status.contains("[noeol]"),
        "a new file is not an unterminated file, got {status:?}"
    );
    // …and typing into it *does* earn the flag, so the gate is the empty document and
    // not the missing file.
    let status = status_text(&redraw_after(&rpc, &mut incoming, "ix<Esc>").await);
    assert!(
        status.contains("[noeol]"),
        "content typed into it is unterminated until saved, got {status:?}"
    );

    // 3. A scratch surface: `[Messages]` is a `nofile` panel, never written to disk.
    //    Its document is non-empty, so only the buftype gate keeps the marker off it.
    let (rpc, mut incoming) = start(None).await;
    let status = all_status_text(&redraw_after(&rpc, &mut incoming, ":messages<CR>").await);
    assert!(
        !status.contains("[noeol]"),
        "editor chrome is never an unterminated file, got {status:?}"
    );
}
