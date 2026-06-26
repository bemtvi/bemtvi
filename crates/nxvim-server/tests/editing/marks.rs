use crate::support::*;

// ----- marks (Phase 1: buffer-local `a`–`z`, `m` set, `` ` `` / `'` jump) -----

#[tokio::test]
async fn mark_set_and_jump_exact() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>");
    // Set mark `a` on line 2 at column 2 (the first `t` of "beta").
    feed(&rpc, "ggjll");
    assert_eq!(cursor(&rpc).await, (2, 2));
    feed(&rpc, "ma");
    // Move away, then `` `a `` returns to the exact (line, col).
    feed(&rpc, "gg");
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn mark_jump_line_lands_on_first_non_blank() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>o  beta<Esc>");
    // Mark `a` sits at end of the indented line 2; `'a` is linewise and lands on
    // the first non-blank (the `b` at column 2), not the exact mark column.
    feed(&rpc, "ma");
    feed(&rpc, "gg");
    feed(&rpc, "'a");
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn mark_jump_exact_is_an_exclusive_operator_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Mark `a` on the `w` (column 6), back to column 0, then `` d`a `` deletes
    // exclusively up to the mark: "hello " goes, "world" stays.
    feed(&rpc, "06l");
    assert_eq!(cursor(&rpc).await, (1, 6));
    feed(&rpc, "ma0");
    feed(&rpc, "d`a");
    assert_eq!(lines(&rpc).await, vec!["world"]);
}

#[tokio::test]
async fn mark_jump_line_is_a_linewise_operator_target() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>odelta<Esc>");
    // Mark `a` on line 3 ("gamma"); from line 1, `d'a` deletes lines 1–3
    // linewise, leaving only "delta".
    feed(&rpc, "ggjjma");
    feed(&rpc, "gg");
    feed(&rpc, "d'a");
    assert_eq!(lines(&rpc).await, vec!["delta"]);
}

#[tokio::test]
async fn jump_to_unset_mark_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Park the cursor on line 1, then jump to a mark that was never set.
    feed(&rpc, "gg");
    let map = latest_after(&rpc, &mut incoming, "`z").await;
    assert!(
        view_str(&map, "message").contains("E20"),
        "expected a loud 'mark not set' error, got: {:?}",
        view_str(&map, "message")
    );
    // The cursor did not move — no silent jump to a bogus position.
    assert_eq!(cursor(&rpc).await, (1, 0));
}

// ----- marks (Phase 2: marks track edits) -----------------------------------

#[tokio::test]
async fn mark_shifts_down_when_a_line_is_inserted_above() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>");
    // Mark `a` on line 3 ("gamma"), column 2.
    feed(&rpc, "0ll");
    assert_eq!(cursor(&rpc).await, (3, 2));
    feed(&rpc, "ma");
    // Open a brand-new line at the very top: every line below slides down one.
    feed(&rpc, "ggOzeta<Esc>");
    // The mark followed its text — `` `a `` lands on the now-line-4 "gamma" at the
    // same column, not the stale line 3.
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await, (4, 2));
}

#[tokio::test]
async fn mark_shifts_up_when_a_line_above_is_deleted() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>odelta<Esc>");
    // Mark `a` on line 4 ("delta"), column 2.
    feed(&rpc, "0ll");
    assert_eq!(cursor(&rpc).await, (4, 2));
    feed(&rpc, "ma");
    // Delete line 1: "delta" shifts up to line 3.
    feed(&rpc, "ggdd");
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await, (3, 2));
}

#[tokio::test]
async fn mark_is_dropped_when_its_line_is_deleted() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>");
    // Mark `a` on line 3 ("gamma").
    feed(&rpc, "0llma");
    // Delete the marked line itself: the mark has nowhere to live and is dropped.
    feed(&rpc, "dd");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
    // Cursor parks on the new last line; jumping to the dropped mark errors loudly
    // and leaves the cursor put — never a silent jump to a stale position.
    let map = latest_after(&rpc, &mut incoming, "`a").await;
    assert!(
        view_str(&map, "message").contains("E20"),
        "expected a loud 'mark not set' error after the line was deleted, got: {:?}",
        view_str(&map, "message")
    );
    assert_eq!(cursor(&rpc).await, (2, 0));
}

#[tokio::test]
async fn mark_column_shifts_when_text_is_inserted_earlier_in_its_line() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Mark `a` on the `w` of "world" (column 6).
    feed(&rpc, "06l");
    assert_eq!(cursor(&rpc).await, (1, 6));
    feed(&rpc, "ma");
    // Insert four characters at the start of the line: the mark slides right with
    // its text, from column 6 to column 10.
    feed(&rpc, "0iXYZ <Esc>");
    assert_eq!(lines(&rpc).await, vec!["XYZ hello world"]);
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await, (1, 10));
}

#[tokio::test]
async fn mark_rides_back_to_its_position_on_undo() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // Mark `a` on line 2 ("beta"), column 1.
    feed(&rpc, "0lma");
    // Insert a line above, shifting the mark to line 3, then undo it.
    feed(&rpc, "ggOinserted<Esc>");
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await, (3, 1));
    // Undo restores the pre-insert text *and* the mark that rode with that state.
    feed(&rpc, "u");
    feed(&rpc, "`a");
    assert_eq!(cursor(&rpc).await, (2, 1));
}

// ----- marks (Phase 3: global file marks A–Z, cross-buffer) -----------------

#[tokio::test]
async fn global_mark_jumps_back_to_its_buffer() {
    let (rpc, _incoming) = start(None).await;
    // Buffer 1 gets three lines; set the global mark `A` on line 2, column 4.
    feed(&rpc, "ialpha<Esc>o  beta<Esc>ogamma<Esc>");
    feed(&rpc, "ggj04l");
    assert_eq!(cursor(&rpc).await, (2, 4));
    feed(&rpc, "mA");
    // Switch to a fresh second buffer and put the cursor somewhere else.
    feed(&rpc, ":enew<CR>");
    feed(&rpc, "ione<Esc>otwo<Esc>");
    // `` `A `` crosses back to buffer 1 (its content reappears) at the exact spot.
    feed(&rpc, "`A");
    assert_eq!(lines(&rpc).await, vec!["alpha", "  beta", "gamma"]);
    assert_eq!(cursor(&rpc).await, (2, 4));
}

#[tokio::test]
async fn global_mark_line_jump_lands_on_first_non_blank() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>o  beta<Esc>ogamma<Esc>");
    // Mark `A` sits at column 4 of the indented line 2.
    feed(&rpc, "ggj04l");
    feed(&rpc, "mA");
    feed(&rpc, ":enew<CR>");
    feed(&rpc, "ione<Esc>");
    // `'A` is linewise: it crosses to buffer 1 and lands on the first non-blank
    // (the `b` at column 2), not the exact mark column.
    feed(&rpc, "'A");
    assert_eq!(lines(&rpc).await, vec!["alpha", "  beta", "gamma"]);
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn uppercase_mark_survives_a_buffer_switch_where_lowercase_does_not() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // On line 2: set both a buffer-local `a` and a global `A` at the same spot.
    feed(&rpc, "ggj0l");
    assert_eq!(cursor(&rpc).await, (2, 1));
    feed(&rpc, "ma"); // buffer-local mark `a`
    feed(&rpc, "mA"); // global mark `A`, same spot
                      // Move to a second buffer.
    feed(&rpc, ":enew<CR>");
    feed(&rpc, "ifresh<Esc>");
    // The lowercase mark is buffer-local: it does not exist in buffer 2, so the
    // jump errors loudly and the cursor stays put.
    let map = latest_after(&rpc, &mut incoming, "`a").await;
    assert!(
        view_str(&map, "message").contains("E20"),
        "lowercase mark must not leak across buffers, got: {:?}",
        view_str(&map, "message")
    );
    assert_eq!(lines(&rpc).await, vec!["fresh"]);
    // The uppercase global mark *does* cross back to buffer 1.
    feed(&rpc, "`A");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beta"]);
    assert_eq!(cursor(&rpc).await, (2, 1));
}

#[tokio::test]
async fn global_mark_into_a_closed_buffer_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    feed(&rpc, "ggjmA");
    // Open a second buffer, then delete the buffer the mark points into.
    feed(&rpc, ":enew<CR>");
    feed(&rpc, "ifresh<Esc>");
    feed(&rpc, ":bdelete! 1<CR>"); // `!` to discard buffer 1's unsaved edits
                                   // The mark now dangles at a closed buffer: jumping reports it loudly rather
                                   // than silently doing nothing or jumping into a phantom buffer.
    let map = latest_after(&rpc, &mut incoming, "`A").await;
    assert!(
        view_str(&map, "message").contains("E20"),
        "a global mark into a closed buffer must error loudly, got: {:?}",
        view_str(&map, "message")
    );
    assert_eq!(lines(&rpc).await, vec!["fresh"]);
}

// ----- marks (Phase 4: :marks display + automatic / special marks) ----------

#[tokio::test]
async fn marks_command_lists_set_marks() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>");
    // A buffer-local `a` and a global `B`.
    feed(&rpc, "ggj0lma");
    feed(&rpc, "mB");
    // `:marks` opens a read-only scratch listing (the focused bottom window).
    feed(&rpc, ":marks<CR>");
    let rows = lines(&rpc).await;
    // A header plus one row per set mark; the names `a` and `B` both appear.
    assert!(
        rows.iter()
            .any(|r| r.contains("mark") && r.contains("line")),
        "expected a header row, got: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.split_whitespace().next() == Some("a")),
        "the buffer-local mark `a` should be listed, got: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.split_whitespace().next() == Some("B")),
        "the global mark `B` should be listed, got: {rows:?}"
    );
}

#[tokio::test]
async fn previous_context_mark_returns_after_a_jump() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>odelta<Esc>");
    // Park on line 1, column 2, then jump to the last line with `G`.
    feed(&rpc, "gg0ll");
    assert_eq!(cursor(&rpc).await, (1, 2));
    feed(&rpc, "G");
    assert_eq!(cursor(&rpc).await, (4, 0));
    // `` `` `` returns to the exact pre-jump spot the jump stashed.
    feed(&rpc, "``");
    assert_eq!(cursor(&rpc).await, (1, 2));
    // `''` is the linewise form — first non-blank of that line (still column 0).
    feed(&rpc, "G");
    feed(&rpc, "''");
    assert_eq!(cursor(&rpc).await, (1, 0));
}

#[tokio::test]
async fn last_change_mark_after_an_edit() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>");
    // Replace a character on line 2, column 2 — the last change happens there.
    feed(&rpc, "ggj0llrX");
    assert_eq!(lines(&rpc).await, vec!["alpha", "beXa", "gamma"]);
    // Move far away, then `` `. `` returns to the just-changed position.
    feed(&rpc, "gg");
    feed(&rpc, "`.");
    assert_eq!(cursor(&rpc).await, (2, 2));
}

#[tokio::test]
async fn last_insert_mark_after_insert() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcdef<Esc>");
    // Insert "XY" at column 2; insert mode stops with the cursor past the "Y".
    feed(&rpc, "0lliXY<Esc>");
    assert_eq!(lines(&rpc).await, vec!["abXYcdef"]);
    // From elsewhere, `` `^ `` returns to where insert mode last stopped (the cell
    // after the inserted "XY", i.e. on the now-shifted "c" at column 4).
    feed(&rpc, "0");
    feed(&rpc, "`^");
    assert_eq!(cursor(&rpc).await, (1, 4));
}

#[tokio::test]
async fn visual_selection_sets_the_angle_marks() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha beta gamma<Esc>");
    // Select "beta" charwise: from column 6 to column 9, then leave visual mode.
    feed(&rpc, "0");
    feed(&rpc, "6l");
    feed(&rpc, "v3l<Esc>");
    assert_eq!(cursor(&rpc).await, (1, 9));
    // `` `< `` jumps to the selection start, `` `> `` to its last char.
    feed(&rpc, "0");
    feed(&rpc, "`<");
    assert_eq!(cursor(&rpc).await, (1, 6));
    feed(&rpc, "`>");
    assert_eq!(cursor(&rpc).await, (1, 9));
}

#[tokio::test]
async fn gv_reselects_the_last_charwise_selection() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha beta gamma<Esc>");
    // Select "beta" charwise (columns 6..=9), then leave visual mode.
    feed(&rpc, "06lv3l<Esc>");
    assert_eq!(cursor(&rpc).await, (1, 9));
    // Park the cursor elsewhere; `gv` re-enters charwise visual over "beta".
    feed(&rpc, "0");
    feed(&rpc, "gv");
    assert_eq!(mode(&rpc).await, "v");
    assert_eq!(cursor(&rpc).await, (1, 9));
    // Operating on the reselection deletes exactly "beta".
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["alpha  gamma"]);
}

#[tokio::test]
async fn gv_reselects_the_last_linewise_selection() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>obeta<Esc>ogamma<Esc>odelta<Esc>");
    // Select lines 2–3 linewise, then leave visual mode.
    feed(&rpc, "ggjVj<Esc>");
    // Park on line 1; `gv` re-enters *linewise* visual over the same two lines.
    feed(&rpc, "gg");
    feed(&rpc, "gv");
    assert_eq!(mode(&rpc).await, "V");
    // A linewise reselect deletes whole lines 2–3, leaving line 1 and line 4.
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["alpha", "delta"]);
}

/// The 1-based buffer line shown on each window row (`window0.numbers`), with
/// `~` filler rows dropped.
fn visible_lines(map: &[(Value, Value)]) -> Vec<u64> {
    window0_field(map, "numbers")
        .and_then(Value::as_array)
        .expect("numbers")
        .iter()
        .filter_map(Value::as_u64)
        .collect()
}

#[tokio::test]
async fn gv_scrolls_to_reveal_a_selection_left_above_the_viewport() {
    let (rpc, mut incoming) = start(None).await;
    // A buffer taller than the 24-row text area.
    let body = (1..=40)
        .map(|n| format!("L{n:02}"))
        .collect::<Vec<_>>()
        .join("<CR>");
    feed(&rpc, &format!("i{body}<Esc>"));
    // Select lines 3–5 linewise, then leave visual mode.
    feed(&rpc, "gg2jV2j<Esc>");
    // Scroll the selection off the top of the window.
    feed(&rpc, "G");
    // `gv` must scroll the small selection wholly back into view, its first line
    // at the very top — not pin its end (line 5) to the top with the body above
    // it scrolled off.
    let map = latest_after(&rpc, &mut incoming, "gv").await;
    let visible = visible_lines(&map);
    assert_eq!(visible.first().copied(), Some(3), "selection start at top");
    assert!(
        visible.contains(&5),
        "selection end visible, got {visible:?}"
    );
}

#[tokio::test]
async fn gv_brims_the_window_with_the_tail_of_an_oversized_selection() {
    let (rpc, mut incoming) = start(None).await;
    let body = (1..=40)
        .map(|n| format!("L{n:02}"))
        .collect::<Vec<_>>()
        .join("<CR>");
    feed(&rpc, &format!("i{body}<Esc>"));
    // Select lines 1–30 linewise — far taller than the 24-row window.
    feed(&rpc, "ggV29j<Esc>");
    feed(&rpc, "G");
    // The whole selection can't fit, so `gv` fills the window with its tail: the
    // cursor end (line 30) lands on the last row, line 1 stays off the top.
    let map = latest_after(&rpc, &mut incoming, "gv").await;
    let visible = visible_lines(&map);
    assert_eq!(visible.last().copied(), Some(30), "cursor end on last row");
    assert!(
        !visible.contains(&1),
        "selection start scrolled off, got {visible:?}"
    );
}

#[tokio::test]
async fn gv_without_a_prior_selection_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    let map = latest_after(&rpc, &mut incoming, "gv").await;
    assert!(
        view_str(&map, "message").contains("E20"),
        "expected a loud 'mark not set' error, got: {:?}",
        view_str(&map, "message")
    );
    // No selection was entered — still in Normal mode.
    assert_eq!(mode(&rpc).await, "n");
}

#[tokio::test]
async fn gv_remembers_the_shape_across_a_charwise_then_linewise_swap() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha beta gamma<Esc>");
    // A linewise selection over the (single) line, then a later charwise one:
    // `gv` must restore the *charwise* shape, not guess linewise from the marks.
    feed(&rpc, "V<Esc>");
    feed(&rpc, "06lv3l<Esc>");
    feed(&rpc, "0");
    feed(&rpc, "gv");
    assert_eq!(mode(&rpc).await, "v");
    feed(&rpc, "d");
    assert_eq!(lines(&rpc).await, vec!["alpha  gamma"]);
}

#[tokio::test]
async fn change_bracket_marks_around_a_yank() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>");
    // Yank "world" (columns 6..=10) with a charwise text-object yank.
    feed(&rpc, "06l");
    assert_eq!(cursor(&rpc).await, (1, 6));
    feed(&rpc, "yiw");
    // `` `[ `` lands on the first yanked char, `` `] `` on the last.
    feed(&rpc, "0");
    feed(&rpc, "`[");
    assert_eq!(cursor(&rpc).await, (1, 6));
    feed(&rpc, "`]");
    assert_eq!(cursor(&rpc).await, (1, 10));
}

#[tokio::test]
async fn setting_a_read_only_mark_errors_loudly() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ialpha<Esc>");
    // The automatic marks are read-only: `m.` is rejected loudly, not silently.
    let map = latest_after(&rpc, &mut incoming, "m.").await;
    assert!(
        view_str(&map, "message").contains("E191"),
        "setting a read-only mark must error loudly, got: {:?}",
        view_str(&map, "message")
    );
}
