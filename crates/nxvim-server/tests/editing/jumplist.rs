use crate::support::*;

// ----- jump list: `<C-o>` / `<C-i>` / `<Tab>` navigation, `:jumps` ----------
//
// A *jump* is the set of motions vim records in the jumplist (`gg`/`G`, `:line`,
// `/`/`?` search, mark jumps) — never an ordinary `h`/`j`/word motion. `<C-o>`
// walks back through the positions jumped *from*; `<C-i>` (== `<Tab>`) walks
// forward again. See `crates/nxvim-core/src/editor/jumps.rs`.

/// Insert ten single-token lines "1".."10"; the cursor lands on line 10.
async fn ten_lines(rpc: &Rpc) {
    feed(rpc, "i1<CR>2<CR>3<CR>4<CR>5<CR>6<CR>7<CR>8<CR>9<CR>10<Esc>");
    assert_eq!(
        cursor(rpc).await.0,
        10,
        "setup leaves the cursor on line 10"
    );
}

#[tokio::test]
async fn ctrl_o_returns_to_the_pre_jump_position() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    // `gg` is a jump: it records line 10 as the position jumped from.
    feed(&rpc, "gg");
    assert_eq!(cursor(&rpc).await.0, 1);
    // `<C-o>` walks back to where the jump started.
    feed(&rpc, "<C-o>");
    assert_eq!(cursor(&rpc).await.0, 10);
}

#[tokio::test]
async fn ctrl_i_walks_forward_again_after_ctrl_o() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // jump 10 -> 1
    feed(&rpc, "<C-o>"); // back to 10
    assert_eq!(cursor(&rpc).await.0, 10);
    // `<C-i>` returns forward to the line we `<C-o>`'d away from.
    feed(&rpc, "<C-i>");
    assert_eq!(cursor(&rpc).await.0, 1);
}

#[tokio::test]
async fn tab_is_ctrl_i_in_normal_mode() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg");
    feed(&rpc, "<C-o>");
    assert_eq!(cursor(&rpc).await.0, 10);
    // `<Tab>` is the same key as `<C-i>` in a terminal — also jumps forward.
    feed(&rpc, "<Tab>");
    assert_eq!(cursor(&rpc).await.0, 1);
}

#[tokio::test]
async fn multiple_jumps_walk_back_then_forward_in_order() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await; // cursor on line 10
    feed(&rpc, "5G"); // 10 -> 5  (records 10)
    feed(&rpc, "3G"); // 5 -> 3   (records 5)
    assert_eq!(cursor(&rpc).await.0, 3);

    feed(&rpc, "<C-o>"); // -> 5
    assert_eq!(cursor(&rpc).await.0, 5);
    feed(&rpc, "<C-o>"); // -> 10
    assert_eq!(cursor(&rpc).await.0, 10);

    feed(&rpc, "<C-i>"); // -> 5
    assert_eq!(cursor(&rpc).await.0, 5);
    feed(&rpc, "<C-i>"); // -> 3 (back to the most recent spot)
    assert_eq!(cursor(&rpc).await.0, 3);
}

#[tokio::test]
async fn a_search_records_a_jump() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // -> line 1 (records 10)
    feed(&rpc, "/7<CR>"); // -> line 7 (records 1)
    assert_eq!(cursor(&rpc).await.0, 7);
    feed(&rpc, "<C-o>"); // back to where the search started
    assert_eq!(cursor(&rpc).await.0, 1);
}

#[tokio::test]
async fn a_colon_line_address_records_a_jump() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // -> 1 (records 10)
    feed(&rpc, ":8<CR>"); // -> line 8 (records 1)
    assert_eq!(cursor(&rpc).await.0, 8);
    feed(&rpc, "<C-o>");
    assert_eq!(cursor(&rpc).await.0, 1);
}

#[tokio::test]
async fn ordinary_motions_do_not_record_jumps() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // the only jump (records 10)
                      // Plain `j` motions are *not* jumps; they must not push onto the list.
    feed(&rpc, "jjj"); // -> line 4 the slow way
    assert_eq!(cursor(&rpc).await.0, 4);
    // So `<C-o>` skips straight back to the pre-`gg` position, not line 1/2/3.
    feed(&rpc, "<C-o>");
    assert_eq!(cursor(&rpc).await.0, 10);
}

#[tokio::test]
async fn ctrl_o_on_an_empty_jumplist_is_a_noop() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // line 1, but make no jump after
    feed(&rpc, "jj"); // -> line 3 via non-jumps
    feed(&rpc, "<C-o>"); // one jump on the list -> line 1's source (10)
    assert_eq!(cursor(&rpc).await.0, 10);
    // The list is now exhausted backward; a further `<C-o>` cannot move past it.
    feed(&rpc, "<C-o>");
    assert_eq!(cursor(&rpc).await.0, 10);
}

#[tokio::test]
async fn a_count_jumps_several_entries_at_once() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await; // line 10
    feed(&rpc, "2G"); // records 10
    feed(&rpc, "4G"); // records 2
    feed(&rpc, "6G"); // records 4, now on line 6
    assert_eq!(cursor(&rpc).await.0, 6);
    // `3<C-o>` steps back three entries: 6 -> (4 -> 2 -> 10).
    feed(&rpc, "3<C-o>");
    assert_eq!(cursor(&rpc).await.0, 10);
}

#[tokio::test]
async fn jumps_command_lists_the_jumplist_in_a_panel() {
    let (rpc, mut incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // records 10
    feed(&rpc, "5G"); // records 1
    let map = latest_after(&rpc, &mut incoming, ":jumps<CR>").await;

    assert_eq!(panel_title(&map), "Jumps");
    let lines = panel_lines(&map);
    assert_eq!(
        lines.first().map(String::as_str),
        Some(" jump line  col file/text")
    );
    // Both jumped-from lines (10 and 1) appear, showing their line text.
    let joined = lines.join("\n");
    assert!(
        joined.contains(" 10 ") && joined.contains("10"),
        "jumps panel was: {lines:?}"
    );
    // The pointer sits at the present: a trailing `>` marks it.
    assert!(
        lines.iter().any(|l| l.trim() == ">"),
        "expected a trailing `>` marker, got: {lines:?}"
    );
}

#[tokio::test]
async fn ctrl_i_at_the_present_is_a_noop() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "gg"); // records 10, now on line 1, pointer at the end
                      // Nothing has been `<C-o>`'d, so there is nothing newer to go forward to.
    feed(&rpc, "<C-i>");
    assert_eq!(cursor(&rpc).await.0, 1);
}

#[tokio::test]
async fn backtick_mark_jump_records_a_jump() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "5G"); // records 10, on line 5
    feed(&rpc, "ma"); // mark a on line 5
    feed(&rpc, "gg"); // records 5, on line 1
                      // `` `a `` is a jump: it records the pre-jump line 1.
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await.0, 5);
    feed(&rpc, "<C-o>"); // returns to where `` `a `` was issued from
    assert_eq!(cursor(&rpc).await.0, 1);
}

#[tokio::test]
async fn g_backtick_mark_jump_does_not_record_a_jump() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await;
    feed(&rpc, "5G"); // records 10, on line 5
    feed(&rpc, "ma");
    feed(&rpc, "gg"); // records 5, on line 1
                      // `` g`a `` is the *quiet* jump — same landing as `` `a `` but it
                      // leaves the jumplist untouched, so line 1 is never recorded.
    feed(&rpc, "g`a");
    assert_eq!(cursor(&rpc).await.0, 5);
    // Hence `<C-o>` does not return to line 1 (contrast the test above); the
    // unchanged list walks back to line 5's own source.
    feed(&rpc, "<C-o>");
    assert_eq!(
        cursor(&rpc).await.0,
        5,
        "g` must not have recorded a jump back to line 1"
    );
}

#[tokio::test]
async fn a_jump_target_follows_lines_inserted_above_it() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await; // line 10
    feed(&rpc, "5G"); // records 10, on line 5
    feed(&rpc, "gg"); // records 5, on line 1
                      // Insert three lines above the top. The recorded jump targets
                      // (text "10" and "5") must ride down with their text.
    feed(&rpc, "Oa<CR>b<CR>c<Esc>");
    feed(&rpc, "<C-o>"); // back to the "5" target, now on line 8
    let (line, _) = cursor(&rpc).await;
    let all = lines(&rpc).await;
    assert_eq!(line, 8, "the jump target shifted down by the three inserts");
    assert_eq!(all[line - 1], "5", "and still points at its original text");
}

#[tokio::test]
async fn a_jump_target_follows_lines_deleted_above_it() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await; // line 10
    feed(&rpc, "8G"); // records 10, on line 8
    feed(&rpc, "gg"); // records 8, on line 1
                      // Delete the first two lines; the "8" target shifts up by two.
    feed(&rpc, "2dd");
    feed(&rpc, "<C-o>"); // back to the "8" target
    let (line, _) = cursor(&rpc).await;
    let all = lines(&rpc).await;
    assert_eq!(line, 6, "the jump target shifted up by the two deletes");
    assert_eq!(all[line - 1], "8");
}

#[tokio::test]
async fn deleting_a_jump_targets_line_drops_it_from_the_list() {
    let (rpc, _incoming) = start(None).await;
    ten_lines(&rpc).await; // line 10
    feed(&rpc, "8G"); // records 10, on line 8
    feed(&rpc, "gg"); // records 8, on line 1
                      // Walk down (non-jump) to line 8 and delete it: the "8" jump
                      // entry must drop, leaving only the "10" entry.
    feed(&rpc, "7jdd");
    feed(&rpc, "<C-o>"); // the only surviving target is "10"
    let (line, _) = cursor(&rpc).await;
    let all = lines(&rpc).await;
    assert_eq!(
        all[line - 1],
        "10",
        "the dropped entry's slot is gone; 10 remains"
    );
}
