//! Multi-cursor (Helix-style), placement-mode model.
//!
//! `<A-c>` enters MULTICURSOR *placement* mode and drops a cursor at the active
//! position. There, motions move only the active (primary) cursor — you navigate
//! (including `/`-search) and drop more cursors with `c` (or `{count}c{motion}`,
//! e.g. `10cj`). Leaving with `<Esc>` keeps the placed cursors and returns to
//! Normal, where motions and edits act on every cursor at once; a second `<Esc>`
//! collapses back to the primary.

use crate::support::*;

/// The focused window's secondary-cursor positions as `(row, col)` pairs, read
/// out of the redraw's per-window `cursors` array.
///
/// CONVENTION: the row is a **0-based screen row** (the redraw's own coordinate),
/// *not* a 1-based buffer line. This is deliberately **off by one from
/// [`cursor`]**, which relays `nvim_win_get_cursor`'s 1-based line. So a cursor on
/// the buffer's first line reads as row `0` here but line `1` from `cursor`; a
/// secondary sharing the primary's line is `secondary_cursors` row `N` vs
/// `cursor` line `N + 1`. Don't cross-compare the two raw. We keep the raw redraw
/// value rather than normalizing, so the helper stays a faithful read of what
/// clients actually render.
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

/// Like [`secondary_cursors`] but reads the **focused** window's `cursors` array
/// rather than `windows[0]` — needed once a split exists, since the focused
/// window need not be the first in the redraw's `windows` list.
fn focused_secondary_cursors(map: &[(Value, Value)]) -> Vec<(u64, u64)> {
    let windows = match map_get(map, "windows") {
        Some(Value::Array(a)) => a,
        _ => return Vec::new(),
    };
    let focused = windows.iter().find_map(|w| match w {
        Value::Map(m) if map_get(m, "focused").and_then(Value::as_bool) == Some(true) => Some(m),
        _ => None,
    });
    let Some(m) = focused else { return Vec::new() };
    map_get(m, "cursors")
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

    // `{count}c{motion}` treats the count as the motion *distance*: `2cj` drops a
    // cursor on the current line and at each of the two lines `j` visits — relative
    // lines 0, 1, 2 — so the bottom lands where `2j` would (relative line 2), not 1.
    let map = redraw_after(&rpc, &mut incoming, "<A-c>2cj").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (1, 0), (2, 0)],
        "counted placement covers the current line plus count motion steps"
    );
}

#[tokio::test]
async fn counted_c_motion_includes_the_starting_position() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ione two three<Esc>gg");

    // Sit the primary on the first word's `o` with no cursor there yet (enter
    // placement, which drops one, then `c` toggles it back off). The count is the
    // motion distance, so `2cw` covers the current word and the two `w` visits —
    // "one" (col 0), "two" (col 4), and "three" (col 8).
    feed(&rpc, "<A-c>c");
    let map = redraw_after(&rpc, &mut incoming, "2cw").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (0, 4), (0, 8)],
        "the starting word gets a cursor, then `count` motion steps"
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

    // Entry drops one on line 1; `2cj` covers lines 1, 2, and 3 (current + two steps).
    feed(&rpc, "<A-c>2cj");
    // A single `u` undoes the whole `2cj` batch — "3cj undoes the cursors placed" —
    // back to just the entry cursor, with the primary where it started.
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
        vec![(0, 0), (1, 0), (2, 0)],
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
async fn single_cursor_undo_after_multicursor_does_not_resurrect_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo<CR>bar<CR>baz<Esc>gg");

    // Place a cursor on lines 1 and 2, finish placement, then a multi-cursor `x`.
    feed(&rpc, "<A-c>jc<Esc>x");
    assert_eq!(lines(&rpc).await, vec!["oo", "ar", "baz"]);

    // Undo the multi-cursor edit (restoring the placed cursors), then collapse
    // back to a single cursor and make an ordinary single-cursor edit lower down.
    feed(&rpc, "u");
    feed(&rpc, "<Esc>");
    feed(&rpc, "Gx"); // last line, single cursor
    assert_eq!(lines(&rpc).await, vec!["foo", "bar", "az"]);

    // Undoing *this* single-cursor edit must leave the cursor at the change and
    // must NOT resurrect the old multi-cursor set baked into the branch point.
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["foo", "bar", "baz"],
        "text restored"
    );
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "cursor stayed at the change (1-based line 3), not the old multi-cursor position"
    );
    assert_eq!(
        secondary_cursors(&map),
        Vec::<(u64, u64)>::new(),
        "no phantom multi-cursors were resurrected"
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

// ===== mouse: click toggles a cursor while placing ==========================

/// In placement mode a left-click drops a cursor at the clicked cell, and a
/// second click on that same cell removes it — the mouse form of the toggling
/// `c` command. The primary navigates to the click for free.
#[tokio::test]
async fn mouse_click_toggles_cursor_while_placing() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");
    command(&rpc, "set nonumber norelativenumber").await;
    // Enter placement: a cursor at (0,0). With nonumber the global screen cell
    // equals the buffer (line, col).
    feed(&rpc, "<A-c>");
    // Click a bare cell on line 2 (row 1, col 1) → adds a cursor there.
    feed_mouse(&rpc, "left", "press", 1, 1);
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0), (1, 1)],
        "the click added a cursor at the clicked cell"
    );
    assert_eq!(
        cursor(&rpc).await,
        (2, 1),
        "the primary navigated to the click"
    );
    // Click the same cell again → toggles that cursor off.
    feed_mouse(&rpc, "left", "press", 1, 1);
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "the second click on the same cell removed the cursor"
    );
}

/// Cursors dropped by mouse-click behave exactly like keyboard-placed ones: after
/// `<Esc>` finishes placement, an edit applies at every clicked position.
#[tokio::test]
async fn mouse_placed_cursors_edit_together() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<A-c>"); // cursor at (0,0)
    feed_mouse(&rpc, "left", "press", 1, 0); // add at line 2, col 0
    feed_mouse(&rpc, "left", "press", 2, 0); // add at line 3, col 0
    let _ = lines(&rpc).await; // barrier
    feed(&rpc, "<Esc>x"); // finish placement, then delete under every cursor
    assert_eq!(
        lines(&rpc).await,
        vec!["bc", "bc", "bc"],
        "the edit ran at all three clicked cursors"
    );
}

/// `u` in placement mode steps back a mouse-placed cursor, just like an undo of a
/// keyboard `c` drop — the click records placement undo.
#[tokio::test]
async fn mouse_placement_is_undoable() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg");
    command(&rpc, "set nonumber norelativenumber").await;
    feed(&rpc, "<A-c>"); // cursor at (0,0)
    feed_mouse(&rpc, "left", "press", 1, 1); // add at (1,1)
    let map = redraw_after(&rpc, &mut incoming, "").await;
    assert_eq!(secondary_cursors(&map), vec![(0, 0), (1, 1)]);
    // Undo the placement: the clicked cursor is removed, the first stays.
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        secondary_cursors(&map),
        vec![(0, 0)],
        "u stepped back the mouse placement"
    );
}

// ----- per-window cursors ----------------------------------------------------

/// Secondary cursors belong to the focused window, like the primary cursor does:
/// drop a multi-cursor set in one split and the *other* split (even sharing the
/// buffer) shows none of its own.
#[tokio::test]
async fn secondary_cursors_are_per_window() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg");

    // Split first (both windows share the buffer, focus on the new one), then
    // place two secondary cursors in the focused window.
    feed(&rpc, "<C-w>v");
    let map = redraw_after(&rpc, &mut incoming, "<A-c>jcjc<Esc>").await;
    assert_eq!(
        focused_secondary_cursors(&map),
        vec![(0, 0), (1, 0)],
        "two secondary cursors placed in this window",
    );

    // Switch to the other split: its cursor set is its own — empty — not the
    // first window's leaked multi-cursors.
    let map = redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    assert_eq!(
        focused_secondary_cursors(&map),
        Vec::<(u64, u64)>::new(),
        "the other split has no secondary cursors of its own",
    );
}

/// Each split keeps an independent cursor set across back-and-forth focus: a set
/// placed in one window survives switching away and back, while the other window
/// carries its own different set.
#[tokio::test]
async fn each_window_keeps_its_own_secondary_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg");

    // Window A (the new split): one secondary cursor on line 1.
    feed(&rpc, "<C-w>v");
    let map = redraw_after(&rpc, &mut incoming, "<A-c>jc<Esc>").await;
    assert_eq!(focused_secondary_cursors(&map), vec![(0, 0)], "A's set");

    // Window B: two secondary cursors, on lines 1 and 2.
    let map = redraw_after(&rpc, &mut incoming, "<C-w>wgg<A-c>jcjc<Esc>").await;
    assert_eq!(
        focused_secondary_cursors(&map),
        vec![(0, 0), (1, 0)],
        "B's own (different) set",
    );

    // Back to A: its single cursor is intact, not overwritten by B's.
    let map = redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    assert_eq!(
        focused_secondary_cursors(&map),
        vec![(0, 0)],
        "A still holds exactly its own set after the round trip",
    );
}

/// The per-buffer undo tree is shared between two windows onto the same buffer,
/// but a multi-cursor set is window-local: undoing *another* window's
/// multi-cursor edit reverts the text without importing that window's cursors
/// into the current one.
#[tokio::test]
async fn undo_does_not_import_another_windows_cursors() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg");

    // Window A (the new split): place cursors on lines 1 and 2, then make a
    // multi-cursor edit (`x` deletes the first char at every cursor).
    feed(&rpc, "<C-w>v");
    feed(&rpc, "<A-c>jcjc<Esc>x");

    // Window B shares the buffer (and so the undo tree), but starts cursor-free.
    let map = redraw_after(&rpc, &mut incoming, "<C-w>w").await;
    assert_eq!(
        focused_secondary_cursors(&map),
        Vec::<(u64, u64)>::new(),
        "B has no secondary cursors of its own",
    );

    // B undoes A's edit: the text reverts, but A's secondary cursors must not be
    // resurrected in B.
    let map = redraw_after(&rpc, &mut incoming, "u").await;
    assert_eq!(
        focused_secondary_cursors(&map),
        Vec::<(u64, u64)>::new(),
        "undoing another window's multi-cursor edit imports no cursors",
    );
}
