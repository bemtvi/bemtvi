//! `'scrolloff'` — the vertical scroll margin. The viewport keeps at least
//! `scrolloff` text rows above and below the cursor, scrolling early so the cursor
//! never rests within the margin (except against the buffer's own top/bottom, where
//! there is no content to justify the margin). The vertical analogue of
//! `sidescrolloff`; off (`0`) by default, so these assert against the historical
//! edge-reaching behavior as the control.
//!
//! The attached UI is 80×25 → a 24-row text viewport (`write_n_lines` buffers are
//! 100 lines of `line1`..`line100`, so `first_visible_line` names the top line and
//! `cursor_row` is the cursor's 0-based screen row within the body).

use crate::support::*;

#[tokio::test]
async fn scrolloff_zero_lets_the_cursor_reach_the_bottom_edge() {
    // Control: with the default `scrolloff=0`, jumping down lands the cursor on the
    // very bottom text row (row 23 of a 24-row viewport), no margin below it.
    let path = write_n_lines("so0", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    let map = redraw_after(&rpc, &mut incoming, "50G").await;
    assert_eq!(cursor(&rpc).await, (50, 0), "cursor on line 50");
    assert_eq!(
        view_u64(&map, "cursor_row"),
        23,
        "scrolloff=0: cursor reaches the bottom row"
    );
    assert_eq!(
        first_visible_line(&map),
        "line27",
        "scrolloff=0: line 50 pinned to the bottom row → top is line 27"
    );
}

#[tokio::test]
async fn scrolloff_keeps_a_bottom_margin_when_jumping_down() {
    // With `scrolloff=5`, the cursor lands no lower than 5 rows from the bottom, so
    // the viewport scrolls further up-buffer than `scrolloff=0` would.
    let path = write_n_lines("sodown", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    command(&rpc, "set scrolloff=5").await;
    let map = redraw_after(&rpc, &mut incoming, "50G").await;

    assert_eq!(cursor(&rpc).await, (50, 0), "cursor still on line 50");
    assert_eq!(
        view_u64(&map, "cursor_row"),
        18,
        "cursor sits 5 rows in from the bottom (row 23 - 5)"
    );
    assert_eq!(
        first_visible_line(&map),
        "line32",
        "the viewport scrolled so line 50 keeps a 5-row margin below"
    );
}

#[tokio::test]
async fn scrolloff_keeps_a_top_margin_when_moving_up() {
    // Moving back up toward the top of the viewport pulls `top` up-buffer so the
    // cursor never rests within 5 rows of the top edge.
    let path = write_n_lines("soup", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    command(&rpc, "set scrolloff=5").await;
    // Land on line 50 (top becomes line 32, cursor 5 rows off the bottom)…
    let _ = redraw_after(&rpc, &mut incoming, "50G").await;
    // …then step up 15 lines to line 35, which would be only 3 rows below the top —
    // inside the margin — so the viewport scrolls up to restore the 5-row gap.
    let map = redraw_after(&rpc, &mut incoming, "15k").await;

    assert_eq!(cursor(&rpc).await, (35, 0), "cursor on line 35");
    assert_eq!(
        view_u64(&map, "cursor_row"),
        5,
        "cursor sits 5 rows in from the top"
    );
    assert_eq!(
        first_visible_line(&map),
        "line30",
        "top scrolled up to line 30"
    );
}

#[tokio::test]
async fn scrolloff_does_not_open_blank_rows_past_end_of_file() {
    // The bottom margin is honored only while real content sits below the cursor.
    // Jumping to the last line pins it to the bottom edge (cursor in the margin) —
    // the viewport must NOT scroll further to manufacture a 5-row gap of `~` fillers.
    let path = write_n_lines("soeof", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    command(&rpc, "set scrolloff=5").await;
    let map = redraw_after(&rpc, &mut incoming, "G").await;

    assert_eq!(cursor(&rpc).await, (100, 0), "cursor on the last line");
    assert_eq!(
        view_u64(&map, "cursor_row"),
        23,
        "the last line reaches the bottom row — no blank rows opened below it"
    );
    assert_eq!(
        first_visible_line(&map),
        "line77",
        "top is line 77 (last line at the bottom), not scrolled past it"
    );
}

#[tokio::test]
async fn ctrl_e_respects_scrolloff() {
    // `<C-e>` scrolls the viewport down one line. With `scrolloff=5`, a cursor that
    // the scroll would push within the top margin is carried down to the margin
    // boundary instead of pinned to the top edge — and the following viewport clamp
    // (which enforces the same margin) leaves the scroll intact.
    let path = write_n_lines("soce", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    command(&rpc, "set scrolloff=5").await;
    // Sit on line 6 with the buffer top still on screen (top = line 1, cursor row 5).
    let _ = redraw_after(&rpc, &mut incoming, "5j").await;
    // `<C-e>` scrolls the view to line 2 on top; line 6 would then be only 4 rows
    // down, inside the margin, so the cursor rides down to line 7 (5 rows in).
    let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;

    assert_eq!(
        first_visible_line(&map),
        "line2",
        "<C-e> scrolled down one line"
    );
    assert_eq!(
        cursor(&rpc).await,
        (7, 0),
        "cursor carried to the 5-row margin"
    );
    assert_eq!(
        view_u64(&map, "cursor_row"),
        5,
        "cursor rests exactly on the margin boundary",
    );
}

#[tokio::test]
async fn ctrl_e_stops_at_the_scrolloff_margin_past_the_last_line() {
    // Scrolling toward the end of the buffer carries the cursor down with the view
    // until it is pinned to the last line — and then *stops*, with the last line
    // still `scrolloff` rows below the top edge. It must neither walk the last line
    // all the way to the top row (the margin is the point) nor keep accepting the
    // keypress only for the viewport clamp to drag `top` back a margin's worth every
    // time (a bounce that never advances).
    let path = write_n_lines("soeofscroll", 100);
    let (rpc, mut incoming) = start(Some(path)).await;

    command(&rpc, "set scrolloff=5").await;
    // `G` pins line 100 to the bottom row → top is line 77 (24-row viewport).
    let map = redraw_after(&rpc, &mut incoming, "G").await;
    assert_eq!(first_visible_line(&map), "line77");

    // Each `<C-e>` moves the top down one line, up to line 95 on top — where line 100
    // sits exactly 5 rows down, on the margin boundary.
    for (n, expected) in (78..=95).map(|l| (l - 77, format!("line{l}"))) {
        let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;
        assert_eq!(
            first_visible_line(&map),
            expected,
            "<C-e> #{n} advanced the viewport"
        );
    }
    assert_eq!(
        cursor(&rpc).await,
        (100, 0),
        "cursor rode down to the last line"
    );
    // Further presses are no-ops — the view rests there rather than bouncing.
    for n in 1..=3 {
        let map = redraw_after(&rpc, &mut incoming, "<C-e>").await;
        assert_eq!(
            first_visible_line(&map),
            "line95",
            "<C-e> #{n} past the limit left the viewport alone"
        );
        assert_eq!(
            view_u64(&map, "cursor_row"),
            5,
            "the last line rests on the margin boundary, 5 rows in from the top"
        );
    }
}

#[tokio::test]
async fn a_count_on_ctrl_e_scrolls_that_many_lines() {
    let (rpc, _incoming) = start(None).await;
    let text: Vec<String> = (1..=30).map(|n| format!("line {n}")).collect();
    let text: Vec<&str> = text.iter().map(String::as_str).collect();
    edit_probe(&rpc, &text, (1, 0), ":set scrolloff=0<CR>5<C-e>").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.win.saveview().topline").await,
        Value::from(6)
    );
    assert_eq!(cursor(&rpc).await, (6, 0));
    // Past the end, vim scrolls as far as it can: the last line at the top.
    edit_probe(&rpc, &["a", "b", "c"], (1, 0), "5<C-e>").await;
    assert_eq!(cursor(&rpc).await, (3, 0));
    feed(&rpc, "2<C-y>");
    assert_eq!(
        exec_lua(&rpc, "return btv.win.saveview().topline").await,
        Value::from(1)
    );
}

#[tokio::test]
async fn ctrl_y_scrolls_back_up_through_wrapped_lines_under_scrolloff() {
    // Regression: with `wrap` and a `'scrolloff'` margin, scrolling back up got stuck.
    // `<C-y>` parked the cursor `scrolloff` *buffer lines* from the bottom, but each
    // line here wraps to three rows, so that line sat far below the window and the
    // per-redraw `ensure_visible` dragged `top` straight back down — every press a
    // bounce to the same spot. The cursor must be parked by screen rows.
    let body: String = (1..=60)
        .map(|i| format!("{i}{}\n", "x".repeat(200)))
        .collect();
    let path = write_temp("so_wrap_up", "txt", &body);
    let (rpc, _incoming) = start(Some(path)).await;
    feed(
        &rpc,
        ":set nonumber<CR>:set norelativenumber<CR>:set wrap<CR>:set scrolloff=8<CR>",
    );
    feed(&rpc, "30G");
    let topline = || async { exec_lua(&rpc, "return btv.win.saveview().topline").await };
    let start = topline().await.as_u64().unwrap();
    assert!(start > 10, "30G scrolled the view down (top {start})");
    for n in 1..=5u64 {
        feed(&rpc, "<C-y>");
        assert_eq!(
            topline().await.as_u64().unwrap(),
            start - n,
            "<C-y> #{n} scrolled the view up one line"
        );
    }
}
