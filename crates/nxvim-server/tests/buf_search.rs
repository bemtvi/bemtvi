//! Black-box tests for the native `nx.buf.search` API: plain / pcre / vim lookups
//! over the buffer mirror, start position, direction, captures. Driven over RPC like
//! the other suites; the buffer is seeded from a temp file so its mirror lines are
//! the haystack.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{exec_lua, start_attached, write_temp};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

const CONTENT: &str = "alpha beta\nFoo = 42\nfoo bar foo\n<<<<<<< HEAD\nthe end\n";

async fn open(content: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("buf_search", "txt", content);
    let init = ServerInit {
        file: Some(path),
        ..Default::default()
    };
    start_attached(init, 80, 24).await
}

/// Run `nx.buf.search(0, <args>)` and format the result as
/// "line:col:end_col:text:cap1,cap2" (or "nil"), so one string assertion covers the
/// whole match.
async fn search(rpc: &Rpc, args: &str) -> String {
    let code = format!(
        "local m = nx.buf.search(0, {args})\n\
         if not m then return 'nil' end\n\
         return m.line..':'..m.col..':'..m.end_col..':'..m.text..':'..table.concat(m.captures, ',')"
    );
    match exec_lua(rpc, &code).await {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected a string result, got {other:?}"),
    }
}

#[tokio::test]
async fn plain_forward_is_case_sensitive_by_default() {
    let (rpc, _inc) = open(CONTENT).await;
    // "Foo" (line 2) is skipped; the first lowercase "foo" is line 3 col 0.
    assert_eq!(
        search(&rpc, r#""foo", { plain = true }"#).await,
        "3:0:3:foo:"
    );
}

#[tokio::test]
async fn plain_ignorecase_matches_the_capitalized_hit() {
    let (rpc, _inc) = open(CONTENT).await;
    assert_eq!(
        search(&rpc, r#""foo", { plain = true, ignorecase = true }"#).await,
        "2:0:3:Foo:"
    );
}

#[tokio::test]
async fn from_position_skips_past_the_start_column() {
    let (rpc, _inc) = open(CONTENT).await;
    // Line 3 is "foo bar foo"; starting at col 1 skips the col-0 hit → the col-8 one.
    assert_eq!(
        search(
            &rpc,
            r#""foo", { plain = true, from = { line = 3, col = 1 } }"#
        )
        .await,
        "3:8:11:foo:"
    );
}

#[tokio::test]
async fn pcre_from_position_sees_matches_overlapping_an_earlier_one() {
    let (rpc, _inc) = open("xaaaa\n").await;
    // Non-overlapping matches from col 0 are 1..3 and 3..5; the scan must restart at
    // `from` instead, so from col 2 the match is 2..4 — not 3..5.
    assert_eq!(
        search(&rpc, r#""aa", { from = { line = 1, col = 2 } }"#).await,
        "1:2:4:aa:"
    );
}

#[tokio::test]
async fn backward_finds_the_last_match_before_the_start() {
    let (rpc, _inc) = open(CONTENT).await;
    assert_eq!(
        search(
            &rpc,
            r#""foo", { plain = true, backward = true, from = { line = 5, col = 99 } }"#
        )
        .await,
        "3:8:11:foo:"
    );
}

#[tokio::test]
async fn no_match_returns_nil() {
    let (rpc, _inc) = open(CONTENT).await;
    assert_eq!(search(&rpc, r#""zzz", { plain = true }"#).await, "nil");
}

#[tokio::test]
async fn pcre_anchors_per_line_and_returns_captures() {
    let (rpc, _inc) = open(CONTENT).await;
    // `^foo` anchors to each line's start, so capital "Foo" (line 2) is skipped.
    assert_eq!(search(&rpc, r#""^foo""#).await, "3:0:3:foo:");
    // Captures: `(\w+) = (\d+)` over "Foo = 42".
    assert_eq!(
        search(&rpc, r#""(\\w+) = (\\d+)""#).await,
        "2:0:8:Foo = 42:Foo,42"
    );
}

#[tokio::test]
async fn vim_engine_matches_with_captures() {
    let (rpc, _inc) = open(CONTENT).await;
    // vim "magic" groups `\(\)` over "Foo = 42".
    assert_eq!(
        search(&rpc, r#""\\(\\w\\+\\) = \\(\\d\\+\\)", { engine = "vim" }"#).await,
        "2:0:8:Foo = 42:Foo,42"
    );
}

#[tokio::test]
async fn vim_engine_finds_an_anchored_conflict_marker() {
    let (rpc, _inc) = open(CONTENT).await;
    // The use case behind the API: jump to a conflict start marker cheaply.
    assert_eq!(
        search(&rpc, r#""^<<<<<<<", { engine = "vim" }"#).await,
        "4:0:7:<<<<<<<:"
    );
}

#[tokio::test]
async fn an_invalid_pattern_fails_loud() {
    let (rpc, _inc) = open(CONTENT).await;
    // An unbalanced group is a compile error, surfaced as a Lua error (not nil).
    let code = "local ok, err = pcall(nx.buf.search, 0, '(unterminated')\n\
                return ok";
    assert_eq!(exec_lua(&rpc, code).await.as_bool(), Some(false));
}
