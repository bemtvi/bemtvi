//! Black-box tests for the native `nx.buf.set_text` API (alias `nvim_buf_set_text`):
//! the precise sub-line counterpart of `set_lines`. Like `set_lines` it queues an edit
//! applied after the chunk, so a write in one `exec_lua` is read back in the next; the
//! suite covers the splice cases (in-line replace / point insert / delete / multi-line /
//! cross-line span), coordinate clamping, undo, the `modified` flag, and the fail-loud
//! guards (nomodifiable, non-string / newline-bearing line). Driven over RPC.

use nxvim_rpc::Rpc;
use nxvim_test_harness::{exec_lua, start_with_file as open};
use rmpv::Value;

/// The current buffer's lines joined with `|`.
async fn lines(rpc: &Rpc) -> String {
    match exec_lua(rpc, "return table.concat(nx.buf.lines(0, 0, -1), '|')").await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected a string, got {other:?}"),
    }
}

/// Queue a `set_text(0, …)` edit (applied after this chunk), then read the result back
/// on the next round-trip — proving the edit lands between chunks.
async fn set_and_read(rpc: &Rpc, args: &str) -> String {
    exec_lua(rpc, &format!("nx.buf.set_text(0, {args})")).await;
    lines(rpc).await
}

#[tokio::test]
async fn replaces_within_a_line() {
    let (rpc, _inc) = open("hello world\n").await;
    // Bytes [0,5) of line 0 ("hello") → "HELLO", in place.
    assert_eq!(
        set_and_read(&rpc, r#"0, 0, 0, 5, { "HELLO" }"#).await,
        "HELLO world"
    );
}

#[tokio::test]
async fn inserts_at_a_point() {
    let (rpc, _inc) = open("ac\n").await;
    // An empty range (start == end) at col 1 is a pure insertion.
    assert_eq!(set_and_read(&rpc, r#"0, 1, 0, 1, { "b" }"#).await, "abc");
}

#[tokio::test]
async fn deletes_a_sub_line_range() {
    let (rpc, _inc) = open("abcd\n").await;
    // Empty replacement over [1,3) ("bc") deletes it.
    assert_eq!(set_and_read(&rpc, r#"0, 1, 0, 3, {}"#).await, "ad");
}

#[tokio::test]
async fn replacement_can_span_multiple_lines() {
    let (rpc, _inc) = open("ab\n").await;
    // Splice two lines at col 1 → the line splits (no trailing newline is added).
    assert_eq!(
        set_and_read(&rpc, r#"0, 1, 0, 1, { "X", "Y" }"#).await,
        "aX|Yb"
    );
}

#[tokio::test]
async fn spans_across_lines() {
    let (rpc, _inc) = open("foo\nbar\n").await;
    // From (0,1) to (1,2) removes "oo\nba" and splices "X" → "fXr" on one line.
    assert_eq!(set_and_read(&rpc, r#"0, 1, 1, 2, { "X" }"#).await, "fXr");
}

#[tokio::test]
async fn out_of_range_column_clamps_to_line_end() {
    let (rpc, _inc) = open("hi\n").await;
    // A column past the line end clamps to it (neovim's tolerance), so [0,2) is replaced.
    assert_eq!(set_and_read(&rpc, r#"0, 0, 0, 99, { "X" }"#).await, "X");
}

#[tokio::test]
async fn the_edit_is_undoable_as_one_group() {
    let (rpc, _inc) = open("hello world\n").await;
    assert_eq!(
        set_and_read(&rpc, r#"0, 0, 0, 5, { "HELLO" }"#).await,
        "HELLO world"
    );
    exec_lua(&rpc, "nx.cmd('undo')").await;
    assert_eq!(lines(&rpc).await, "hello world");
}

#[tokio::test]
async fn the_edit_marks_the_buffer_modified() {
    let (rpc, _inc) = open("ab\n").await;
    exec_lua(&rpc, r#"nx.buf.set_text(0, 0, 0, 0, 0, { "z" })"#).await;
    assert_eq!(
        exec_lua(&rpc, "return vim.bo[0].modified").await.as_bool(),
        Some(true)
    );
}

#[tokio::test]
async fn the_promise_resolves_after_the_edit_is_visible() {
    let (rpc, _inc) = open("hello\n").await;
    exec_lua(
        &rpc,
        r#"nx.async(function()
             nx.await(nx.buf.set_text(0, 0, 0, 0, 5, { "HELLO" }))
             _G.__seen = table.concat(nx.buf.lines(0, 0, -1), "|")
           end)()"#,
    )
    .await;
    let mut seen = String::new();
    for _ in 0..20 {
        if let Value::String(s) = exec_lua(&rpc, "return _G.__seen or ''").await {
            seen = s.into_str().unwrap_or_default();
            if !seen.is_empty() {
                break;
            }
        }
    }
    assert_eq!(seen, "HELLO");
}

#[tokio::test]
async fn a_nomodifiable_buffer_fails_loud() {
    let (rpc, _inc) = open("ab\n").await;
    exec_lua(&rpc, "vim.bo[0].modifiable = false").await;
    let ok = exec_lua(
        &rpc,
        r#"return (pcall(nx.buf.set_text, 0, 0, 0, 0, 1, { "X" }))"#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
    assert_eq!(lines(&rpc).await, "ab");
}

#[tokio::test]
async fn a_non_string_replacement_line_fails_loud() {
    let (rpc, _inc) = open("a\n").await;
    let ok = exec_lua(
        &rpc,
        "return (pcall(nx.buf.set_text, 0, 0, 0, 0, 0, { 42 }))",
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
}

#[tokio::test]
async fn a_newline_bearing_line_fails_loud() {
    let (rpc, _inc) = open("a\n").await;
    let ok = exec_lua(
        &rpc,
        r#"return (pcall(nx.buf.set_text, 0, 0, 0, 0, 0, { "x\ny" }))"#,
    )
    .await;
    assert_eq!(ok.as_bool(), Some(false));
}
