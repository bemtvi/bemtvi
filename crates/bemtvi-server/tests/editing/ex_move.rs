use crate::support::*;

// ----- :move / :copy -----

#[tokio::test]
async fn ex_move_sends_a_line_below_the_destination() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<CR>ddd<Esc>gg");
    feed(&rpc, ":1m3<CR>"); // line 1 lands after line 3
    assert_eq!(lines(&rpc).await, vec!["bbb", "ccc", "aaa", "ddd"]);
}

#[tokio::test]
async fn ex_move_to_zero_puts_the_range_at_the_top() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>");
    feed(&rpc, ":3m0<CR>"); // address 0 = before the first line
    assert_eq!(lines(&rpc).await, vec!["ccc", "aaa", "bbb"]);
}

#[tokio::test]
async fn ex_move_moves_a_multi_line_range_and_lands_the_cursor_on_it() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<CR>ddd<Esc>");
    feed(&rpc, ":1,2m$<CR>");
    assert_eq!(lines(&rpc).await, vec!["ccc", "ddd", "aaa", "bbb"]);
    // Cursor lands on the last moved line (vim).
    assert_eq!(cursor(&rpc).await, (4, 0));
}

#[tokio::test]
async fn ex_move_up_with_a_relative_address() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>"); // cursor on line 3
    feed(&rpc, ":m-2<CR>"); // current line moves above the one before it
    assert_eq!(lines(&rpc).await, vec!["aaa", "ccc", "bbb"]);
}

#[tokio::test]
async fn ex_move_into_its_own_range_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":1,3m2<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["aaa", "bbb", "ccc"]); // unchanged
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E134"), "expected E134, got {msg:?}");
}

#[tokio::test]
async fn ex_move_missing_destination_fails_loud() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<Esc>");
    let map = redraw_after(&rpc, &mut incoming, ":1m<CR>").await;
    assert_eq!(lines(&rpc).await, vec!["aaa", "bbb"]);
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(msg.contains("E14"), "expected E14, got {msg:?}");
}

#[tokio::test]
async fn ex_move_is_undoable_as_one_change() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>");
    feed(&rpc, ":1m$<CR>");
    assert_eq!(lines(&rpc).await, vec!["bbb", "ccc", "aaa"]);
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["aaa", "bbb", "ccc"]);
}

#[tokio::test]
async fn ex_copy_duplicates_the_range_after_the_destination() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>");
    feed(&rpc, ":1t$<CR>");
    assert_eq!(lines(&rpc).await, vec!["aaa", "bbb", "ccc", "aaa"]);
}

// ----- the visual-mode "move the selection down/up" keymap -----

/// The classic `vim.keymap.set("v", "J", ":m '>+1<cr>gv=gv")`: the mapping's
/// leading `:` prefills `'<,'>`, `:m '>+1` drops the selection one line lower,
/// and `gv` reselects it so the map can repeat.
#[tokio::test]
async fn visual_j_keymap_moves_the_selection_down() {
    let (rpc, _i) = start(None).await;
    exec_lua(
        &rpc,
        r#"vim.keymap.set("v", "J", ":m '>+1<cr>gv=gv")
           vim.keymap.set("v", "K", ":m '<-2<cr>gv=gv")"#,
    )
    .await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<CR>ddd<Esc>gg");
    feed(&rpc, "VjJ"); // select lines 1-2, move down one
    assert_eq!(lines(&rpc).await, vec!["ccc", "aaa", "bbb", "ddd"]);
    // The selection survived, so the map repeats.
    feed(&rpc, "J");
    assert_eq!(lines(&rpc).await, vec!["ccc", "ddd", "aaa", "bbb"]);
    feed(&rpc, "K");
    assert_eq!(lines(&rpc).await, vec!["ccc", "aaa", "bbb", "ddd"]);
}

/// Moving a range to just above itself or just below its last line is a no-op
/// in vim, not *E134*: the reversing idiom `:g/^/m0` runs `:1m0` on its first
/// line, and `:m.` names the range's own line. The buffer is untouched and not
/// modified, and the cursor rests on the range's last line.
#[tokio::test]
async fn ex_move_onto_its_own_boundary_is_a_silent_no_op() {
    let file = write_temp("ex_move_noop", "txt", "aaa\nbbb\nccc\n");
    let (rpc, mut incoming) = start(Some(file)).await;
    let map = redraw_after(&rpc, &mut incoming, ":1m0<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(!msg.contains("E134"), "`:1m0` is a no-op, got {msg:?}");
    let map = redraw_after(&rpc, &mut incoming, ":1,2m2<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(!msg.contains("E134"), "`:1,2m2` is a no-op, got {msg:?}");
    assert_eq!(cursor(&rpc).await, (2, 0));
    assert_eq!(lines(&rpc).await, vec!["aaa", "bbb", "ccc"]);
    assert_eq!(
        exec_lua(&rpc, "return vim.bo.modified").await.as_bool(),
        Some(false)
    );
    let map = redraw_after(&rpc, &mut incoming, ":g/^/m0<CR>").await;
    let msg = field(&map, "message").and_then(Value::as_str).unwrap_or("");
    assert!(
        !msg.contains("E134"),
        "`:g/^/m0` reverses silently, got {msg:?}"
    );
    assert_eq!(lines(&rpc).await, vec!["ccc", "bbb", "aaa"]);
}
