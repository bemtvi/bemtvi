//! Black-box tests for the native `nx.buf.set_lines` API (alias `nvim_buf_set_lines`):
//! the editor's one buffer-text mutation. It queues an edit applied after the chunk, so
//! a write in one `exec_lua` is read back in the next; the suite covers the splice cases
//! (replace / whole-buffer / delete / append), undo, the `modified` flag, and the
//! fail-loud guards (nomodifiable, non-string line). Driven over RPC like the siblings.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, start_attached, write_temp};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn open(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("buf_set_lines", "txt", content);
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    start_attached(init, 80, 24).await
}

/// The current buffer's lines joined with `|`.
async fn lines(rpc: &Rpc) -> String {
    match exec_lua(rpc, "return table.concat(nx.buf.lines(0, 0, -1), '|')").await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Queue a `set_lines(0, …)` edit (applied after this chunk), then read the result back
/// on the next round-trip — proving the edit lands between chunks.
async fn set_and_read(rpc: &Rpc, args: &str) -> String {
    exec_lua(rpc, &format!("nx.buf.set_lines(0, {args})")).await;
    lines(rpc).await
}

#[tokio::test]
async fn replaces_a_middle_line() {
    let (rpc, _inc) = open("a\nb\nc\n").await;
    assert_eq!(set_and_read(&rpc, r#"1, 2, false, { "B" }"#).await, "a|B|c");
}

#[tokio::test]
async fn replaces_a_run_with_a_different_count() {
    let (rpc, _inc) = open("a\nb\nc\nd\n").await;
    // Lines [1,3) ("b","c") → three lines: the buffer grows.
    assert_eq!(
        set_and_read(&rpc, r#"1, 3, false, { "X", "Y", "Z" }"#).await,
        "a|X|Y|Z|d"
    );
}

#[tokio::test]
async fn whole_buffer_replace_with_negative_end() {
    let (rpc, _inc) = open("a\nb\nc\n").await;
    // (0, -1) spans the whole buffer; `-1` is one past the last line.
    assert_eq!(
        set_and_read(&rpc, r#"0, -1, false, { "only" }"#).await,
        "only"
    );
}

#[tokio::test]
async fn empty_replacement_deletes_the_range() {
    let (rpc, _inc) = open("a\nb\nc\n").await;
    assert_eq!(set_and_read(&rpc, r#"1, 2, false, {}"#).await, "a|c");
}

#[tokio::test]
async fn append_at_the_end() {
    let (rpc, _inc) = open("a\nb\n").await;
    // (-1, -1) is the empty range at EOF → an append.
    assert_eq!(
        set_and_read(&rpc, r#"-1, -1, false, { "c" }"#).await,
        "a|b|c"
    );
}

#[tokio::test]
async fn the_edit_is_undoable_as_one_group() {
    let (rpc, _inc) = open("a\nb\nc\n").await;
    assert_eq!(set_and_read(&rpc, r#"1, 2, false, { "B" }"#).await, "a|B|c");
    exec_lua(&rpc, "nx.cmd('undo')").await;
    assert_eq!(lines(&rpc).await, "a|b|c");
}

#[tokio::test]
async fn the_edit_marks_the_buffer_modified() {
    let (rpc, _inc) = open("a\nb\n").await;
    exec_lua(&rpc, r#"nx.buf.set_lines(0, 0, 1, false, { "A" })"#).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.bo[0].modified").await.as_bool(),
        Some(true)
    );
}

#[tokio::test]
async fn the_promise_resolves_after_the_edit_is_visible() {
    let (rpc, _inc) = open("a\nb\nc\n").await;
    // Await the promise, then read inside the SAME async body: the resolution point is
    // after the edit has landed, so the read sees the new content. Stash it in a global
    // the next round-trip returns.
    exec_lua(
        &rpc,
        r#"nx.async(function()
             nx.await(nx.buf.set_lines(0, 1, 2, false, { "B" }))
             _G.__seen = table.concat(nx.buf.lines(0, 0, -1), "|")
           end)()"#,
    )
    .await;
    // Poll the stashed value across a few ticks (the await settles on the next tick).
    let mut seen = String::new();
    for _ in 0..20 {
        if let Value::String(s) = exec_lua(&rpc, "return _G.__seen or ''").await {
            seen = s.into_str().unwrap_or_default();
            if !seen.is_empty() {
                break;
            }
        }
    }
    assert_eq!(seen, "a|B|c");
}

#[tokio::test]
async fn a_nomodifiable_buffer_fails_loud() {
    let (rpc, _inc) = open("a\nb\n").await;
    exec_lua(&rpc, "vim.bo[0].modifiable = false").await;
    let ok = exec_lua(
        &rpc,
        r#"return (pcall(nx.buf.set_lines, 0, 0, 1, false, { "X" }))"#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
    // …and the buffer is untouched.
    assert_eq!(lines(&rpc).await, "a|b");
}

#[tokio::test]
async fn a_non_string_replacement_line_fails_loud() {
    let (rpc, _inc) = open("a\n").await;
    let ok = exec_lua(
        &rpc,
        "return (pcall(nx.buf.set_lines, 0, 0, 1, false, { 42 }))",
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
}

#[tokio::test]
async fn a_newline_bearing_line_fails_loud() {
    let (rpc, _inc) = open("a\n").await;
    let ok = exec_lua(
        &rpc,
        r#"return (pcall(nx.buf.set_lines, 0, 0, 1, false, { "x\ny" }))"#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
}
