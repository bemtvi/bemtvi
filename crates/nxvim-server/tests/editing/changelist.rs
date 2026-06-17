use crate::support::*;

// ----- change list: `g;` / `g,` navigation, `:changes` ----------------------
//
// The change list records the position of each change in a buffer; `g;` walks to
// older changes, `g,` to newer. It is per-buffer and rides edits like the
// buffer-local marks. See `crates/nxvim-core/src/editor/changelist.rs`.
//
// These tests open a *file* (ten lines "1".."10") rather than typing the content,
// so the change list starts empty — typing the lines would itself fill it, as in
// vim.

/// Open a ten-line file; the change list starts empty.
async fn ten_line_file() -> (Rpc, UnboundedReceiver<Incoming>) {
    let path = write_temp("changelist", "txt", "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n");
    start(Some(path)).await
}

#[tokio::test]
async fn g_semicolon_and_comma_walk_the_change_list() {
    let (rpc, _incoming) = ten_line_file().await;
    feed(&rpc, "3GA!<Esc>"); // change on line 3
    feed(&rpc, "7GA?<Esc>"); // change on line 7, cursor ends here
    assert_eq!(cursor(&rpc).await.0, 7);

    // `g;` from the present skips the change we're sitting on (line 7) and lands on
    // the older change (line 3).
    feed(&rpc, "g;");
    assert_eq!(cursor(&rpc).await.0, 3);
    // Already at the oldest: `g;` is a no-op (vim's E662).
    feed(&rpc, "g;");
    assert_eq!(cursor(&rpc).await.0, 3);
    // `g,` walks forward to the newer change.
    feed(&rpc, "g,");
    assert_eq!(cursor(&rpc).await.0, 7);
    // Already at the newest: `g,` is a no-op (vim's E663).
    feed(&rpc, "g,");
    assert_eq!(cursor(&rpc).await.0, 7);
}

#[tokio::test]
async fn changes_on_one_line_coalesce_into_a_single_entry() {
    let (rpc, _incoming) = ten_line_file().await;
    // A whole typed word is many keystroke-edits, all on line 3 — one entry.
    feed(&rpc, "3GA hello<Esc>");
    feed(&rpc, "7GA x<Esc>");
    // `:changes` opens a read-only scratch listing (the focused bottom window).
    feed(&rpc, ":changes<CR>");
    let shown = lines(&rpc).await;
    // header + two coalesced entries + the trailing `>` present marker.
    assert_eq!(
        shown.len(),
        4,
        "expected exactly two change entries, got: {shown:?}"
    );
    assert_eq!(
        shown.first().map(String::as_str),
        Some(" change line  col text")
    );
}

#[tokio::test]
async fn the_change_list_survives_undo() {
    let (rpc, _incoming) = ten_line_file().await;
    feed(&rpc, "3GA!<Esc>"); // change on line 3
    feed(&rpc, "7GA?<Esc>"); // change on line 7
                             // Undo the line-7 change. The change list is restored from the undo
                             // snapshot (not cleared by the wholesale-replace), so line 3 remains.
    feed(&rpc, "u");
    feed(&rpc, "g;");
    assert_eq!(
        cursor(&rpc).await.0,
        3,
        "g; still reaches the surviving change after undo"
    );
}

#[tokio::test]
async fn a_change_entry_follows_lines_inserted_above_it() {
    let (rpc, _incoming) = ten_line_file().await;
    feed(&rpc, "7GA?<Esc>"); // change on line 7
                             // Insert three lines at the very top; the line-7 change entry must
                             // ride down to line 10 with its text.
    feed(&rpc, "ggOa<CR>b<CR>c<Esc>");
    feed(&rpc, ":changes<CR>");
    let shown = lines(&rpc).await;
    assert!(
        shown.iter().any(|l| l.contains(" 10 ") && l.contains("7?")),
        "the line-7 change should have shifted to line 10: {shown:?}"
    );
}

#[tokio::test]
async fn g_semicolon_on_an_unchanged_buffer_is_a_noop() {
    let (rpc, _incoming) = ten_line_file().await;
    feed(&rpc, "gg"); // a jump, not a change
    feed(&rpc, "g;"); // empty change list (vim's E664)
    assert_eq!(cursor(&rpc).await.0, 1);
}
