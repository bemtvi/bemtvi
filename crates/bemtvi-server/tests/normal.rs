//! Behavior tests for `:[range]normal[!]` — running an ex-supplied key sequence
//! through the editor as if typed. Driven black-box over RPC: feed the command,
//! then assert on the resulting buffer lines / cursor. The argument is *literal*
//! (vim semantics): a `<CR>` in it is the four characters `<`,`C`,`R`,`>`, not
//! Enter; a real Enter / Esc can only be embedded via a Lua-built string (the
//! `:execute` analogue), exercised in `normal_inserts_then_returns_to_normal`.

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{cursor, exec_lua, feed, lines, start_attached, temp_dir};
use tokio::sync::mpsc::UnboundedReceiver;

/// Spawn a server on a temp file pre-filled with `text`, UI attached. The caller
/// keeps the returned `incoming` in scope — dropping it closes the RPC connection
/// (see the harness note on keeping `incoming` alive).
async fn start(text: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    let dir = temp_dir("normal_cmd");
    let file = dir.join("buf.txt");
    std::fs::write(&file, text).expect("write file");
    let init = ServerInit {
        file: Some(file.to_string_lossy().into_owned()),
        config_dir: Some(dir.clone()),
        runtimepath: vec![dir],
        ..Default::default()
    };
    start_attached(init, 80, 25).await
}

/// `:normal! {motion}` drives a built-in motion: `gg` jumps to the top.
#[tokio::test]
async fn normal_bang_runs_a_motion() {
    let (rpc, _incoming) = start("alpha\nbeta\ngamma\ndelta\n").await;
    feed(&rpc, "G"); // jump to the last line first
    assert_eq!(cursor(&rpc).await, (4, 0));
    feed(&rpc, ":normal! gg<CR>");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

/// `:normal!` runs an editing command: `dd` deletes the current line.
#[tokio::test]
async fn normal_bang_deletes_a_line() {
    let (rpc, _incoming) = start("alpha\nbeta\ngamma\n").await;
    feed(&rpc, "j"); // onto "beta"
    feed(&rpc, ":normal! dd<CR>");
    assert_eq!(lines(&rpc).await, vec!["alpha", "gamma"]);
}

/// The argument is literal: a typed `<CR>` (fed as `<lt>CR>` so the command line
/// receives the four characters `<`,`C`,`R`,`>`) is appended verbatim — it does
/// NOT split the line the way a real Enter key would.
#[tokio::test]
async fn normal_arg_is_literal_not_keynotation() {
    let (rpc, _incoming) = start("x\n").await;
    feed(&rpc, ":normal! A<lt>CR><CR>");
    assert_eq!(lines(&rpc).await, vec!["x<CR>"]);
}

/// A real Esc embedded via a Lua-built string (the `:execute` analogue) finishes
/// the insert and returns to Normal mode: the following `x` deletes rather than
/// inserts, proving `\x1b` mapped to the Esc key.
#[tokio::test]
async fn normal_inserts_then_returns_to_normal() {
    let (rpc, _incoming) = start("world\n").await;
    // `i` inserts "hello " before "world", `\27` (ESC) leaves insert mode.
    exec_lua(&rpc, "vim.cmd('normal! ihello \\27')").await;
    assert_eq!(lines(&rpc).await, vec!["hello world"]);
    // Back in Normal mode: `x` deletes the char under the cursor (the space).
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["helloworld"]);
}

/// A range runs the keys once per line: `:%normal! x` deletes the first character
/// of every line.
#[tokio::test]
async fn normal_with_range_runs_per_line() {
    let (rpc, _incoming) = start("one\ntwo\nthree\n").await;
    feed(&rpc, ":%normal! x<CR>");
    assert_eq!(lines(&rpc).await, vec!["ne", "wo", "hree"]);
}

/// A ranged `:normal` is a single undo step: one `u` reverts every line.
#[tokio::test]
async fn normal_range_is_one_undo_step() {
    let (rpc, _incoming) = start("one\ntwo\nthree\n").await;
    feed(&rpc, ":%normal! x<CR>");
    assert_eq!(lines(&rpc).await, vec!["ne", "wo", "hree"]);
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["one", "two", "three"]);
}

/// A *trailing* keystroke that happens to be whitespace is still a keystroke: a
/// literal tab (`\9`) is `<Tab>` == `<C-i>`, which walks forward in the jumplist.
/// Regression: the ex line was whitespace-trimmed before `:normal`'s argument was
/// parsed, so the tab was silently dropped and the command became a no-op.
#[tokio::test]
async fn normal_bang_trailing_tab_jumps_forward() {
    let (rpc, _incoming) = start("1\n2\n3\n4\n5\n").await;
    feed(&rpc, "G"); // jump 1 -> 5, recording line 1 as the position jumped from
    assert_eq!(cursor(&rpc).await.0, 5);
    feed(&rpc, "<C-o>"); // back to line 1
    assert_eq!(cursor(&rpc).await.0, 1);
    // `\9` is a literal tab — `<C-i>`, so this jumps forward to line 5 again.
    exec_lua(&rpc, "vim.cmd('normal! \\9')").await;
    assert_eq!(cursor(&rpc).await.0, 5);
}

/// A trailing space is likewise significant: `<Space>` is `l` in Normal mode, so
/// `0 ` lands on column 1 rather than column 0.
#[tokio::test]
async fn normal_bang_trailing_space_moves_right() {
    let (rpc, _incoming) = start("abcdef\n").await;
    exec_lua(&rpc, "vim.cmd('normal! 0 ')").await;
    assert_eq!(cursor(&rpc).await, (1, 1));
}

/// `:normal` is recognized via its abbreviations (`:norm`) too.
#[tokio::test]
async fn normal_abbreviation_norm() {
    let (rpc, _incoming) = start("alpha\nbeta\n").await;
    feed(&rpc, "G");
    feed(&rpc, ":norm gg<CR>");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

/// An incomplete command at the end of `:normal`'s keys is aborted as if `<Esc>`
/// were typed (vim): an Insert begun by the keys ends there. With a range that
/// happens per line, so `:%normal Ax` appends to every line instead of the second
/// line receiving the literal keys `Ax`; and the editor is back in Normal mode
/// afterwards, so a following `u` undoes rather than being typed.
#[tokio::test]
async fn normal_aborts_an_unfinished_insert_per_line_and_at_the_end() {
    let (rpc, _incoming) = start("a\nb\n").await;
    feed(&rpc, ":%normal Ax<CR>");
    assert_eq!(lines(&rpc).await, vec!["ax", "bx"]);
    assert_eq!(bemtvi_test_harness::mode(&rpc).await, "n");
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["a", "b"]);
    feed(&rpc, ":normal ihi<CR>");
    assert_eq!(bemtvi_test_harness::mode(&rpc).await, "n");
    // An operator left pending is dropped, not carried into the next keystroke.
    feed(&rpc, ":normal d<CR>");
    feed(&rpc, "j");
    assert_eq!(lines(&rpc).await, vec!["hia", "b"]);
}

/// `:normal` with no keys is vim's `E471: Argument required`, not a silent no-op.
#[tokio::test]
async fn normal_without_keys_is_e471() {
    let (rpc, mut incoming) = start("x\n").await;
    feed(&rpc, ":normal<CR>");
    rpc.request("nvim_get_mode", vec![]).await.expect("barrier");
    let frame =
        bemtvi_test_harness::drain_to_latest_redraw(&mut incoming, |_| true).expect("a redraw");
    let msg = bemtvi_test_harness::message(&frame);
    assert!(msg.contains("E471"), "got {msg:?}");
}
