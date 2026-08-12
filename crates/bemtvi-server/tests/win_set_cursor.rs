//! Black-box tests for `btv.win.set_cursor` as the snippet-engine **jump** primitive
//! (P5): a pure-Lua engine that owns its tabstop session moves the caret between
//! tabstops by calling `btv.win.set_cursor` from its own (insert-mode) jump key. These
//! tests exercise exactly that shape — an insert-mode Lua keymap that repositions the
//! caret, after which further typing lands at the new spot — plus the end-of-line
//! tabstop case (a caret one past the last char, legal in insert mode).
//!
//! `btv.win.set_cursor(win, line, col)` is 1-based line / 0-based byte col (the
//! sanctioned public counterpart of the intentionally-absent `nvim_win_set_cursor`).

use bemtvi_rpc::{Incoming, Rpc};
use bemtvi_server::ServerInit;
use bemtvi_test_harness::{attach, exec_lua, feed, lines, spawn, temp_dir};
use rmpv::Value;
use tokio::sync::mpsc::UnboundedReceiver;

async fn start(dir: &std::path::Path, init_lua: &str) -> (Rpc, UnboundedReceiver<Incoming>) {
    std::fs::write(dir.join("init.lua"), init_lua).expect("write init.lua");
    let init = ServerInit {
        config_dir: Some(dir.to_path_buf()),
        runtimepath: vec![dir.to_path_buf()],
        ..Default::default()
    };
    let (rpc, incoming) = spawn(init);
    attach(&rpc, 80, 24).await;
    (rpc, incoming)
}

/// `"row,col"` of the current cursor (0-based), read from the mirror.
async fn cursor(rpc: &Rpc) -> String {
    match exec_lua(
        rpc,
        "local c = btv._cur_cursor or {}; return string.format('%d,%d', (c.row or 1) - 1, c.col or 0)",
    )
    .await
    {
        Value::String(s) => s.into_str().unwrap_or_default(),
        other => panic!("expected a string, got {other:?}"),
    }
}

#[tokio::test]
async fn an_insert_mode_map_jumps_the_caret_and_typing_lands_there() {
    let dir = temp_dir("win_set_cursor_jump");
    // <Tab> in insert mode is remapped to move the caret to (row 1, col 2) — the shape
    // a snippet engine's "jump to next tabstop" takes.
    let (rpc, _inc) = start(
        &dir,
        "btv.keymap.set('i', '<Tab>', function() btv.win.set_cursor(0, 2, 2) end)",
    )
    .await;
    feed(&rpc, "ihello<CR>world<Esc>"); // two lines
    feed(&rpc, "gg0i"); // back to (0,0), insert mode
    feed(&rpc, "X"); // "Xhello"
    feed(&rpc, "<Tab>"); // jump to (row1, col2)
    feed(&rpc, "Y"); // insert at (1,2): "woYrld"
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["Xhello", "woYrld"]);
}

#[tokio::test]
async fn the_jump_moves_the_reported_cursor() {
    let dir = temp_dir("win_set_cursor_pos");
    let (rpc, _inc) = start(
        &dir,
        "btv.keymap.set('i', '<Tab>', function() btv.win.set_cursor(0, 2, 3) end)",
    )
    .await;
    feed(&rpc, "ihello<CR>world<Esc>");
    feed(&rpc, "gg0i");
    feed(&rpc, "<Tab>");
    // 0-based (row 1, col 3).
    assert_eq!(cursor(&rpc).await, "1,3");
}

#[tokio::test]
async fn a_jump_to_end_of_line_allows_insert_past_the_last_char() {
    let dir = temp_dir("win_set_cursor_eol");
    // Jump to column 5 == len("world"), one past the last char — legal only in insert
    // mode, and exactly where a `$0` / trailing tabstop at line end sits.
    let (rpc, _inc) = start(
        &dir,
        "btv.keymap.set('i', '<Tab>', function() btv.win.set_cursor(0, 2, 5) end)",
    )
    .await;
    feed(&rpc, "ihello<CR>world<Esc>");
    feed(&rpc, "gg0i");
    feed(&rpc, "<Tab>");
    feed(&rpc, "!"); // appends at EOL
    feed(&rpc, "<Esc>");
    assert_eq!(lines(&rpc).await, vec!["hello", "world!"]);
}
