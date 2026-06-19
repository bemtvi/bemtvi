//! Behavior tests for `:[range]normal[!]` — running an ex-supplied key sequence
//! through the editor as if typed. Driven black-box over RPC: feed the command,
//! then assert on the resulting buffer lines / cursor. The argument is *literal*
//! (vim semantics): a `<CR>` in it is the four characters `<`,`C`,`R`,`>`, not
//! Enter; a real Enter / Esc can only be embedded via a Lua-built string (the
//! `:execute` analogue), exercised in `normal_inserts_then_returns_to_normal`.

use nxvim_rpc::{Incoming, Rpc};
use nxvim_server::ServerInit;
use nxvim_test_harness::{cursor, exec_lua, feed, lines, start_attached, temp_dir};
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

/// `:normal` is recognized via its abbreviations (`:norm`) too.
#[tokio::test]
async fn normal_abbreviation_norm() {
    let (rpc, _incoming) = start("alpha\nbeta\n").await;
    feed(&rpc, "G");
    feed(&rpc, ":norm gg<CR>");
    assert_eq!(cursor(&rpc).await, (1, 0));
}
