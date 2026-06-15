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
