//! Multi-cursor (Helix-style), placement-mode model.
//!
//! `<A-c>` enters MULTICURSOR *placement* mode and drops a cursor at the active
//! position. There, motions move only the active (primary) cursor — you navigate
//! (including `/`-search) and drop more cursors with `c` (or `{count}c{motion}`,
//! e.g. `10cj`). Leaving with `<Esc>` keeps the placed cursors and returns to
//! Normal, where motions and edits act on every cursor at once; a second `<Esc>`
//! collapses back to the primary.

use crate::support::*;

/// The focused window's secondary-cursor screen positions as `(row, col)`
/// pairs, read out of the redraw's per-window `cursors` array.
fn secondary_cursors(map: &[(Value, Value)]) -> Vec<(u64, u64)> {
    window0_field(map, "cursors")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| {
                    let pair = v.as_array()?;
                    Some((pair.first()?.as_u64()?, pair.get(1)?.as_u64()?))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn alt_c_enters_placement_mode_and_drops_a_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<Esc>gg");

    let map = redraw_after(&rpc, &mut incoming, "<A-c>").await;
    assert_eq!(field_str(&map, "mode_label"), "MULTICURSOR", "entered mode");
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "a cursor is dropped at the active position"
    );
}

#[tokio::test]
async fn motions_navigate_only_the_primary_while_placing() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");

    // Drop a cursor on line 1, then move down: the placed cursor stays put.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>j").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "the placed cursor did not follow the motion"
    );
    assert_eq!(cursor(&rpc).await, (2, 0), "the primary navigated down");
}

#[tokio::test]
async fn c_places_another_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");

    // Place on line 1 (entry), navigate down, place again with `c`.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>jc").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (1, 0)],
        "two cursors are placed (lines 1 and 2)"
    );
}

#[tokio::test]
async fn counted_c_motion_places_many() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<Esc>gg");

    // `{count}c{motion}` places `count` cursors starting at the current line: the
    // entry already dropped one on line 1, so `2cj` covers lines 1 and 2.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>2cj").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (1, 0)],
        "counted placement covers the current line plus count-1 steps"
    );
}

#[tokio::test]
async fn counted_c_motion_includes_the_starting_position() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ione two three<Esc>gg");

    // Sit the primary on the first word's `o` with no cursor there yet (enter
    // placement, which drops one, then `c` toggles it back off). `2cw` should then
    // place 2 cursors at the *current* word and the next — "one" (col 0) and "two"
    // (col 4) — not skip ahead to "two" and "three".
    feed(&rpc, "<A-c>c");
    let map = redraw_after(&rpc, &mut incoming, "2cw").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (0, 4)],
        "the starting word gets a cursor; the count is inclusive of it"
    );
}

#[tokio::test]
async fn c_at_an_occupied_cell_toggles_the_cursor_off() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<Esc>gg");

    // `<A-c>` drops a cursor at the cell; `c` on the same cell clears it.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>c").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![],
        "c on an occupied cell toggles that cursor off"
    );

    // A third `c` re-adds it — confirming the toggle, not a one-way clear.
    let map = redraw_after(&rpc, &mut incoming, "c").await;
    assert_eq!(secondary_cursors(&map), vec![(0, 0)], "c again re-drops it");
}

#[tokio::test]
async fn overlapping_cursors_merge_after_a_motion() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar baz<Esc>gg");

    // Place cursors at col 0 and col 4 on line 1, finish placement.
    feed(&rpc, "<A-c>wc<Esc>");
    // `0` sends both to the line start — they overlap and collapse to one.
    let map = redraw_after(&rpc, &mut incoming, "0").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![],
        "the two cursors converged and merged into the primary"
    );
}

#[tokio::test]
async fn esc_keeps_cursors_and_then_edits_apply_to_all() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");

    // Place on lines 1 and 2, leave placement, then `x` edits both.
    feed(&rpc, "<A-c>jc<Esc>x");
    assert_eq!(
        lines(&rpc).await,
        vec!["bc", "bc", "abc"],
        "after <Esc>, x deletes under every placed cursor"
    );
}

#[tokio::test]
async fn leaving_does_not_add_a_cursor_at_the_parked_primary() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ixxxx<CR>yyyy<CR>zzzz<CR>wwww<Esc>gg0");

    // The entry `<A-c>` drops a cursor on line 0; `jc`/`jc` place two more on lines
    // 1 and 2; then `j` *navigates* the primary down to line 3 without placing one
    // there. Leaving must not turn that parked spot into an edit cursor — the
    // primary snaps onto the nearest placed cursor (line 2) instead.
    feed(&rpc, "<A-c>jcjcj");
    let map = redraw_after(&rpc, &mut incoming, "<Esc>").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (1, 0)],
        "the placed set is lines 0/1/2 — the primary represents line 2"
    );
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "the primary snapped onto the line-2 cursor, not the parked line 3"
    );

    // `x` therefore edits only the three placed lines; the parked line 3 (`wwww`)
    // is left untouched — no phantom cursor was created there.
    feed(&rpc, "x");
    assert_eq!(
        lines(&rpc).await,
        vec!["xxx", "yyy", "zzz", "wwww"],
        "the navigated-to line keeps its first char — it was never a cursor"
    );
}

#[tokio::test]
async fn esc_after_insert_backsteps_every_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>foo<Esc>gg");

    // Place cursors on lines 1 and 2, insert "X" at each, then leave insert.
    // Like single-cursor vim, `<Esc>` must back *every* cursor off its
    // insert-stop column onto the last inserted cell — not just the primary's.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>jc<Esc>iX<Esc>").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["Xfoo", "Xfoo"],
        "X inserted at both"
    );
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "the secondary cursor backstepped onto the inserted X, like the primary"
    );
    assert_eq!(cursor(&rpc).await, (2, 0), "primary backstepped onto the X");
}

#[tokio::test]
async fn motions_move_every_cursor_after_placement() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>foo bar<Esc>gg");

    // Place on lines 1 and 2, leave placement, then `w` advances both.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>jc<Esc>w").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 4)],
        "the line-1 cursor advanced a word with the primary"
    );
    assert_eq!(cursor(&rpc).await, (2, 4), "primary advanced a word too");
}

#[tokio::test]
async fn search_navigates_then_places_at_a_match() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>baz foo<Esc>gg");

    // Enter placement (drops a cursor on the first foo at 1:0), `/foo` jumps to the
    // second foo, `c` places there, <Esc> finishes, `x` deletes both f's.
    feed(&rpc, "<A-c>/foo<CR>c<Esc>x");
    assert_eq!(
        lines(&rpc).await,
        vec!["oo bar", "baz oo"],
        "a cursor was placed at the search match and edited with the rest"
    );
}

#[tokio::test]
async fn search_in_normal_mode_clears_the_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>foo bar<CR>foo bar<Esc>gg");

    // Place cursors and finish placement (Normal mode, cursors active).
    feed(&rpc, "<A-c>jc<Esc>");
    // A search navigates away — abandoning the multi-cursor session.
    let map = redraw_after(&rpc, &mut incoming, "/bar<CR>").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![],
        "search in Normal mode cleared the placed cursors"
    );
}

#[tokio::test]
async fn n_in_normal_mode_clears_the_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>foo bar<CR>foo bar<Esc>gg");

    // Establish a last search (no cursors yet), return to the top, then place.
    feed(&rpc, "/bar<CR>gg<A-c>jc<Esc>");
    // `n` repeats the search — and likewise abandons the cursors.
    let map = redraw_after(&rpc, &mut incoming, "n").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![],
        "n in Normal mode cleared the placed cursors"
    );
}

#[tokio::test]
async fn second_esc_collapses_to_a_single_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");

    // Place on lines 1 and 2, <Esc> to Normal (cursors active), <Esc> again to
    // collapse, then `x` edits only the primary's line.
    feed(&rpc, "<A-c>jc<Esc><Esc>x");
    assert_eq!(
        lines(&rpc).await,
        vec!["abc", "bc", "abc"],
        "after the collapsing <Esc>, only the primary cursor remains"
    );
}

#[tokio::test]
async fn multi_cursor_edit_undoes_as_one_step() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>foo bar<Esc>gg");

    feed(&rpc, "<A-c>jc<Esc>dw");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar"], "dw at every cursor");
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar", "foo bar"],
        "one undo restores every cursor's edit"
    );
}

#[tokio::test]
async fn undo_keeps_the_placed_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<CR>foo bar<Esc>gg");

    // Place cursors on lines 1 and 2, finish, edit, then undo.
    feed(&rpc, "<A-c>jc<Esc>dw");
    assert_eq!(lines(&rpc).await, vec!["bar", "bar"]);

    // Undo restores the text — and must NOT clear the placed cursors (they are
    // live editor state, not document history).
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo bar", "foo bar"],
        "text restored"
    );
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "undo kept the placed cursor"
    );
}

#[tokio::test]
async fn placement_undo_removes_the_last_placed_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");

    // Entry drops a cursor on line 1; navigate down and `c` drops a second.
    feed(&rpc, "<A-c>jc");
    // `u` *in placement mode* undoes the cursor placement, not a text edit: the
    // last-placed cursor (line 2) is removed, leaving the line-1 one.
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        field_str(&map, "mode_label"),
        "MULTICURSOR",
        "still placing"
    );
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "undo removed the most recently placed cursor"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["abc", "abc", "abc"],
        "text untouched"
    );
}

#[tokio::test]
async fn placement_undo_treats_a_counted_drop_as_one_step() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<Esc>gg");

    // Entry drops one on line 1; `2cj` covers lines 1 and 2 (current + one step).
    feed(&rpc, "<A-c>2cj");
    // A single `u` undoes the whole `2cj` batch — "10cj undoes the 10 cursors
    // placed" — back to just the entry cursor, with the primary where it started.
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "the counted placement undid as a single step"
    );
    // `cursor()` is 1-based row, so the primary back at buffer-line 0 reads as row 1.
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "the primary returned to the start"
    );
}

#[tokio::test]
async fn placement_undo_then_redo_replaces_the_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<Esc>gg");

    feed(&rpc, "<A-c>2cj");
    feed(&rpc, "u");
    // `<C-r>` in placement mode redoes the undone placement.
    let map = redraw_after(&rpc, &mut incoming, "<C-r>").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (1, 0)],
        "redo re-placed the counted drop"
    );
}

#[tokio::test]
async fn placement_undo_can_remove_the_entry_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<Esc>gg");

    // `<A-c>` enters placement and drops one cursor; `u` undoes that drop, leaving
    // placement mode active with no cursors yet (still MULTICURSOR).
    feed(&rpc, "<A-c>");
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        field_str(&map, "mode_label"),
        "MULTICURSOR",
        "still placing"
    );
    assert_eq!(
        secondary_cursors(&map),
        vec![],
        "undo removed the entry cursor"
    );
}

#[tokio::test]
async fn placement_redo_is_discarded_by_a_new_placement() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");

    // Place two cursors (lines 1, 2), undo the second, then drop a fresh one on
    // line 3 — the discarded redo must not resurrect the line-2 cursor.
    feed(&rpc, "<A-c>jc");
    feed(&rpc, "u");
    feed(&rpc, "jc");
    let map = redraw_after(&rpc, &mut incoming, "<C-r>").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (2, 0)],
        "a new placement cleared the redo history"
    );
}

// ===== per-cursor visual mode ===============================================

#[tokio::test]
async fn visual_charwise_delete_applies_to_every_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello world<CR>hello world<Esc>gg");

    // A cursor on each line at col 0, finish placement (Normal, both active).
    feed(&rpc, "<A-c>jc<Esc>");
    // `v` opens visual at every cursor, `e` extends each to the word end, `d`
    // deletes the selection ("hello") under every cursor.
    feed(&rpc, "ved");
    assert_eq!(
        lines(&rpc).await,
        vec![" world", " world"],
        "v e d removed each cursor's word"
    );
}

#[tokio::test]
async fn visual_linewise_delete_applies_to_every_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<CR>ddd<Esc>gg");

    // A cursor on line 1 (entry) and line 3, finish placement.
    feed(&rpc, "<A-c>2jc<Esc>");
    // `V` opens linewise visual at both, `d` deletes each cursor's whole line.
    feed(&rpc, "Vd");
    assert_eq!(
        lines(&rpc).await,
        vec!["bbb", "ddd"],
        "V d removed each cursor's line"
    );
}

#[tokio::test]
async fn visual_change_applies_to_every_cursor() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<CR>hello<Esc>gg");

    feed(&rpc, "<A-c>jc<Esc>");
    // `v e c` deletes "hello" at every cursor and drops into Insert there; typing
    // then inserts at every cursor.
    feed(&rpc, "vecX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["X", "X"],
        "v e c then typing changed the selection at every cursor"
    );
}

#[tokio::test]
async fn visual_motion_extends_every_selection_render() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<CR>hello world<Esc>gg");

    feed(&rpc, "<A-c>jc<Esc>");
    // `v e`: extend the selection to the word end ("hello", screen cols [0,5)) at
    // both cursors. The primary (line 2) rides `selection`; the secondary (line 1)
    // rides `secondary_selection`.
    let map = redraw_after(&rpc, &mut incoming, "ve").await;
    let sel = view_selection(&map);
    assert_eq!(
        sel.get(1).copied().flatten(),
        Some((0, 5)),
        "the primary's selection covers its word (row 1)"
    );
    assert_eq!(
        sel.first().copied().flatten(),
        None,
        "the primary owns no selection on row 0"
    );
    let sec = view_secondary_selection(&map);
    assert_eq!(
        sec.first().cloned().unwrap_or_default(),
        vec![(0, 5)],
        "the secondary's selection covers its word on row 0"
    );
    assert!(
        sec.iter().skip(1).all(Vec::is_empty),
        "no other row carries a secondary selection"
    );
}

#[tokio::test]
async fn visual_esc_collapses_selections_but_keeps_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<CR>hello world<Esc>gg");

    feed(&rpc, "<A-c>jc<Esc>");
    // Open visual, extend, then `<Esc>` — the selections collapse but the cursors
    // survive into Normal (a `w` then moves every cursor).
    let map = redraw_after(&rpc, &mut incoming, "ve<Esc>w").await;
    assert_eq!(field_str(&map, "mode_label"), "NORMAL", "back to Normal");
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 6)],
        "the secondary cursor survived <Esc> and moved a word"
    );
    assert!(
        view_secondary_selection(&map).iter().all(Vec::is_empty),
        "no selection remains after <Esc>"
    );
}

#[tokio::test]
async fn undo_restores_cursors_to_their_pre_edit_position() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar<Esc>gg");

    // Primary at col 0, a secondary at col 4 ('b'); finish placement.
    feed(&rpc, "<A-c>wc0<Esc>");
    // `x` deletes under both — the col-0 deletion shifts the col-4 cursor left to
    // col 3.
    feed(&rpc, "x");
    assert_eq!(lines(&rpc).await, vec!["oo ar"]);

    // Undo restores the text *and* puts the cursor back at its pre-edit column 4,
    // not the shifted column 3.
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(lines(&rpc).await, vec!["foo bar"], "text restored");
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 4)],
        "the secondary returned to its pre-edit column"
    );
}

// ---- per-cursor insert-entry, open-line, yank/paste (the deferred keys) ----

#[tokio::test]
async fn capital_a_appends_at_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    // Cursors on lines 1 and 2; `A` must jump *each* to its own line end.
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "AX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fooX", "barX"],
        "A appended at every line's end, not just the primary's"
    );
}

#[tokio::test]
async fn a_inserts_after_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "aX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fXoo", "bXar"],
        "a entered insert one cell past every cursor"
    );
}

#[tokio::test]
async fn capital_i_inserts_at_first_non_blank_of_every_cursor() {
    let (rpc, _i) = start(None).await;
    // Two indented lines; `I` must land at each line's first non-blank.
    feed(&rpc, "i  foo<CR>    bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "IX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["  Xfoo", "    Xbar"],
        "I inserted at the first non-blank of every line"
    );
}

#[tokio::test]
async fn o_opens_a_line_below_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "oZ<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "Z", "bar", "Z"],
        "o opened and typed on a new line below every cursor"
    );
}

#[tokio::test]
async fn capital_o_opens_a_line_above_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "OZ<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["Z", "foo", "Z", "bar"],
        "O opened and typed on a new line above every cursor"
    );
}

#[tokio::test]
async fn yank_line_and_paste_is_per_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    // Each cursor yanks its own line; `p` pastes each cursor's own line below it —
    // not the last-yanked line under both.
    feed(&rpc, "yyp");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "foo", "bar", "bar"],
        "every cursor pasted the line it yanked"
    );
}

#[tokio::test]
async fn yank_word_and_paste_is_per_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    // `yiw` captures each cursor's word; `p` pastes each cursor's own word after it.
    feed(&rpc, "yiwp");
    assert_eq!(
        lines(&rpc).await,
        vec!["ffoooo", "bbarar"],
        "every cursor pasted the word it yanked"
    );
}

#[tokio::test]
async fn paste_broadcasts_when_yank_count_differs() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>baz<Esc>gg");
    // A single-cursor yank fills the unnamed register only (no per-cursor set).
    feed(&rpc, "yy");
    // Now place two cursors and paste: counts differ, so every cursor pastes the
    // one yanked line.
    feed(&rpc, "<A-c>jc<Esc>p");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "foo", "bar", "foo", "baz"],
        "with no matching per-cursor set, paste broadcasts the unnamed register"
    );
}

#[tokio::test]
async fn insert_enter_splits_at_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "i<CR><Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["", "foo", "", "bar"],
        "Enter split the line at every cursor"
    );
}

#[tokio::test]
async fn insert_backspace_deletes_at_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    // Move both cursors to col 1, then backspace deletes the first char of each.
    feed(&rpc, "li<BS><Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["oo", "ar"],
        "Backspace deleted before every cursor"
    );
}

#[tokio::test]
async fn multi_cursor_paste_undoes_as_one_step() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<Esc>gg");
    feed(&rpc, "<A-c>jc<Esc>");
    feed(&rpc, "yyp");
    assert_eq!(lines(&rpc).await, vec!["foo", "foo", "bar", "bar"]);
    feed(&rpc, "u");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "bar"],
        "a single undo reverts the whole multi-cursor paste"
    );
}

#[tokio::test]
async fn visual_o_swaps_ends_at_every_cursor() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello<CR>world<Esc>gg");
    // Cursors on lines 1 and 2; enter visual and extend each to col 2.
    feed(&rpc, "<A-c>jc<Esc>");
    // `o` swaps every cursor's ends, so the following `l` shrinks each selection
    // from the left (cols 1..2) before `d` deletes it.
    feed(&rpc, "vllold");
    assert_eq!(
        lines(&rpc).await,
        vec!["hlo", "wld"],
        "o swapped ends at every cursor; the delete shrank from the left of each"
    );
}

// ===== mode-specific keymaps =================================================
// A `vim.keymap.set('m', …)` map is scoped to the MULTICURSOR *placement* mode:
// it fires only while placing, normal `'n'` maps don't leak into placement, and
// the placement map doesn't leak into normal mode. The all-mode `''` :map still
// covers placement (it covers every normal-ish mode).

/// A keymap declared for the placement mode (`'m'`) fires while placing — and the
/// keys it consumes don't also reach the editor.
#[tokio::test]
async fn placement_keymap_fires_while_placing() {
    let dir = temp_dir("mc_keymap_m");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('m', 'x', function() print('PLACEMENT_MAP') end)\n",
    )
    .await;
    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // Enter placement, then `x` fires the `'m'` map (not the editor's delete-char).
    let map = redraw_after(&rpc, &mut incoming, "<A-c>x").await;
    assert_eq!(
        message(&map),
        "PLACEMENT_MAP",
        "the 'm' map fired while placing"
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["hello"],
        "x was consumed by the placement map, not the editor"
    );
}

/// Isolation, both directions: a placement (`'m'`) map is inert in normal mode,
/// and a normal (`'n'`) map is inert while placing — built-ins still pass through.
#[tokio::test]
async fn placement_keymap_is_isolated_from_normal() {
    let dir = temp_dir("mc_keymap_iso");
    let (rpc, _i) = start_with_config(
        &dir,
        "vim.keymap.set('m', 'd', 'x')\n\
         vim.keymap.set('n', 'q', 'x')\n",
    )
    .await;
    feed(&rpc, "ihello<Esc>0");
    assert_eq!(lines(&rpc).await, vec!["hello"]);

    // In normal mode the `'m'` map for `d` does NOT fire: `dl` deletes a char the
    // ordinary way (operator + motion), leaving "ello".
    feed(&rpc, "dl");
    assert_eq!(
        lines(&rpc).await,
        vec!["ello"],
        "the 'm' map for d is inert in normal mode"
    );

    // While placing, the `'n'` map for `q` does NOT fire (it would delete via `x`);
    // `q` is no-op grammar in placement, so the text is untouched.
    feed(&rpc, "<A-c>q<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["ello"],
        "the 'n' map for q is inert while placing"
    );
}

/// An all-mode `''` (`:map`) mapping covers placement mode too, like every other
/// normal-ish mode.
#[tokio::test]
async fn all_mode_map_covers_placement() {
    let dir = temp_dir("mc_keymap_all");
    let (rpc, mut incoming) = start_with_config(
        &dir,
        "vim.keymap.set('', 'g.', function() print('ALL_MODE_MAP') end)\n",
    )
    .await;
    feed(&rpc, "ihello<Esc>0");

    let map = redraw_after(&rpc, &mut incoming, "<A-c>g.").await;
    assert_eq!(
        message(&map),
        "ALL_MODE_MAP",
        "an '' all-mode map fires while placing"
    );
}
