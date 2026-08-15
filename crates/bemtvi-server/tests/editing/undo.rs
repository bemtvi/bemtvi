use crate::support::*;

// ----- `:undolist` ----------------------------------------------------------
//
// vim's `:undol[ist]` lists **the leafs in the tree of changes** — the tip of each
// branch, not every state — with its seq (`number`), its depth from the root
// (`changes`), its age (`when`) and its write number (`saved`). bemtvi's undo timeline
// is monotonic-since-start rather than wall-clock, so `when` stays relative at every
// age instead of switching to a clock time the way vim does past 100 seconds.

/// The listing panel's rows, minus the header. Dismisses the panel afterwards — it
/// is a *focus-locked* overlay, so a caller that kept typing would drive the listing
/// instead of the file.
async fn undolist_rows(rpc: &Rpc) -> Vec<String> {
    feed_sync(rpc, ":undolist<CR>").await;
    let shown = lines(rpc).await;
    assert_eq!(
        shown.first().map(String::as_str),
        Some("number changes  when               saved"),
        "listing was: {shown:?}"
    );
    feed_sync(rpc, "q").await;
    assert!(!panel_is_open(rpc).await, "`q` dismisses the listing");
    shown[1..].to_vec()
}

/// The `number` and `changes` columns of each row, parsed.
fn seq_and_changes(rows: &[String]) -> Vec<(u64, u64)> {
    rows.iter()
        .map(|r| {
            let mut f = r.split_whitespace();
            let seq = f.next().expect("number column").parse().expect("seq");
            let changes = f.next().expect("changes column").parse().expect("changes");
            (seq, changes)
        })
        .collect()
}

#[tokio::test]
async fn undolist_on_an_untouched_buffer_says_nothing_to_undo() {
    let (rpc, mut incoming) = start_with_file("alpha\nbravo\n").await;

    assert_eq!(
        message_after(&rpc, &mut incoming, ":undolist<CR>").await,
        "Nothing to undo"
    );
    assert!(
        !panel_is_open(&rpc).await,
        "no history means no listing panel"
    );
}

#[tokio::test]
async fn undolist_lists_one_leaf_for_a_linear_history() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\ncharlie\n").await;

    // Three separate changes on a straight line: only the newest state is a tip.
    for keys in ["x", "jx", "jx"] {
        feed_sync(&rpc, keys).await;
    }

    let rows = undolist_rows(&rpc).await;
    assert_eq!(
        seq_and_changes(&rows),
        vec![(3, 3)],
        "a linear history has exactly one leaf, three changes deep: {rows:?}"
    );
}

#[tokio::test]
async fn undolist_lists_a_leaf_per_branch() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\ncharlie\n").await;

    // Two changes, undo one, then edit again — the undone future is not discarded,
    // it becomes a sibling branch, so the tree now has two tips.
    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "u").await;
    feed_sync(&rpc, "jx").await;

    let rows = undolist_rows(&rpc).await;
    assert_eq!(
        seq_and_changes(&rows),
        vec![(2, 2), (3, 2)],
        "both branch tips list, each two changes from the root: {rows:?}"
    );
}

#[tokio::test]
async fn undolist_shows_the_pending_edit_as_the_state_it_will_become() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;

    // A single change is still *uncommitted* (the tree commits lazily, at the next
    // change-group boundary). It is a real reachable state, so it lists — under the
    // seq `:undo {N}` will accept for it.
    feed_sync(&rpc, "x").await;

    let rows = undolist_rows(&rpc).await;
    assert_eq!(
        seq_and_changes(&rows),
        vec![(1, 1)],
        "the pending state lists as seq 1: {rows:?}"
    );
}

#[tokio::test]
async fn undolist_saved_column_carries_the_write_number() {
    let path = write_temp("undolist_saved", "txt", "alpha\nbravo\n");
    let (rpc, _incoming) = start(Some(path)).await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, ":w<CR>").await;

    let rows = undolist_rows(&rpc).await;
    assert_eq!(seq_and_changes(&rows), vec![(1, 1)]);
    assert!(
        rows[0].trim_end().ends_with("  1"),
        "the written state carries save number 1: {rows:?}"
    );

    // A further change is a *different* state, so it carries no save number.
    feed_sync(&rpc, "jx").await;
    let rows = undolist_rows(&rpc).await;
    assert_eq!(seq_and_changes(&rows), vec![(2, 2)]);
    assert!(
        !rows[0].trim_end().ends_with("  1"),
        "an unwritten state has a blank saved column: {rows:?}"
    );
}

#[tokio::test]
async fn undolist_when_column_reads_as_an_age() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;

    let rows = undolist_rows(&rpc).await;
    assert!(
        rows[0].contains(" ago"),
        "the `when` column is a relative age (the undo clock is monotonic): {rows:?}"
    );
}

#[tokio::test]
async fn undolist_is_read_only_and_leaves_the_history_alone() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;
    let before = seq_and_changes(&undolist_rows(&rpc).await);

    // Opening the listing must not commit the pending edit early — re-reading it
    // gives the same tree, and closing it leaves the buffer's own undo intact.
    let after = seq_and_changes(&undolist_rows(&rpc).await);
    assert_eq!(before, after, "the listing is a read-only projection");

    feed_sync(&rpc, "u").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["alpha".to_string(), "bravo".to_string()],
        "the edit still undoes after the listing was opened twice"
    );
}

// ----- `g-` / `g+` and `:earlier` / `:later` --------------------------------
//
// These walk the undo *states in the order they were made*, across branches — not the
// tree the way `u` / `<C-r>` do. On a linear history the two coincide; once a branch
// exists they deliberately diverge, which is the point of the pair.

/// A buffer whose history forks: `x` on line 1 (state 1), undone, then `x` on line 2
/// (state 2). State 1 is on an abandoned branch that no `u` can reach.
async fn forked_history() -> (Rpc, UnboundedReceiver<Incoming>) {
    let (rpc, incoming) = start_with_file("alpha\nbravo\ncharlie\n").await;
    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "u").await;
    feed_sync(&rpc, "jx").await;
    (rpc, incoming)
}

fn v(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|s| s.to_string()).collect()
}

#[tokio::test]
async fn g_minus_and_g_plus_walk_a_linear_history() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\ncharlie\n").await;

    feed_sync(&rpc, "x").await; // state 1
    feed_sync(&rpc, "jx").await; // state 2
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));

    feed_sync(&rpc, "g-").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo", "charlie"]));
    feed_sync(&rpc, "g-").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo", "charlie"]));

    feed_sync(&rpc, "g+").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo", "charlie"]));
    feed_sync(&rpc, "g+").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));
}

#[tokio::test]
async fn g_minus_reaches_an_abandoned_branch_that_undo_cannot() {
    let (rpc, _incoming) = forked_history().await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "ravo", "charlie"]));

    // `g-` steps to the previous state *in time* — the first edit, which lives on the
    // branch the undo walked away from.
    feed_sync(&rpc, "g-").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["lpha", "bravo", "charlie"]),
        "`g-` crosses to the abandoned branch"
    );
}

#[tokio::test]
async fn undo_still_walks_the_tree_where_time_travel_crosses_branches() {
    let (rpc, _incoming) = forked_history().await;

    // Same starting point as the test above; `u` walks to the tree *parent* instead,
    // which is the original text. The two commands must not be aliases.
    feed_sync(&rpc, "u").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha", "bravo", "charlie"]),
        "`u` walks to the parent, never sideways into a branch"
    );
}

#[tokio::test]
async fn time_travel_takes_a_count() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\ncharlie\n").await;

    for keys in ["x", "jx", "jx"] {
        feed_sync(&rpc, keys).await;
    }
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "harlie"]));

    feed_sync(&rpc, "3g-").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo", "charlie"]));
    feed_sync(&rpc, "2g+").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));
}

#[tokio::test]
async fn earlier_and_later_are_the_ex_form() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\ncharlie\n").await;

    for keys in ["x", "jx", "jx"] {
        feed_sync(&rpc, keys).await;
    }

    feed_sync(&rpc, ":earlier 3<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo", "charlie"]));
    feed_sync(&rpc, ":later 2<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));

    // A bare command means one state, and the abbreviations resolve.
    feed_sync(&rpc, ":ea<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo", "charlie"]));
    feed_sync(&rpc, ":lat<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));
}

#[tokio::test]
async fn a_large_count_travels_as_far_as_it_can() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "jx").await;

    // vim clamps rather than refusing: 99 states back is the oldest state.
    feed_sync(&rpc, ":earlier 99<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo"]));
    feed_sync(&rpc, ":later 99<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo"]));
}

#[tokio::test]
async fn time_travel_reports_both_boundaries() {
    let (rpc, mut incoming) = start_with_file("alpha\nbravo\n").await;

    assert_eq!(
        message_after(&rpc, &mut incoming, "g-").await,
        "Already at oldest change"
    );

    feed_sync(&rpc, "x").await;
    assert_eq!(
        message_after(&rpc, &mut incoming, "g+").await,
        "Already at newest change"
    );
}

#[tokio::test]
async fn earlier_rejects_a_non_count_argument() {
    let (rpc, mut incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;
    assert_eq!(
        message_after(&rpc, &mut incoming, ":earlier zz<CR>").await,
        "E475: Invalid argument: zz",
        "a bad argument fails loud, never silently doing nothing"
    );
    assert_eq!(
        lines(&rpc).await,
        v(&["lpha", "bravo"]),
        "and the buffer is untouched"
    );
}

#[tokio::test]
async fn time_travel_is_not_the_dot_repeat_target() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "g-").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo"]));

    // `.` must replay the `x`, not the travel — travelling is navigation, like `u`.
    feed_sync(&rpc, ".").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo"]));
}

// ----- `:earlier`/`:later` time and file-write units -------------------------

#[tokio::test]
async fn earlier_and_later_travel_by_time() {
    // Place each state at a known second on the editor's monotonic timeline.
    let (rpc, clock, _incoming) = start_mono_clocked("alpha\nbravo\ncharlie\n").await;

    clock.set_secs(100);
    feed_sync(&rpc, "x").await; // state 1 @ 100
    clock.set_secs(150);
    feed_sync(&rpc, "jx").await; // state 2 @ 150 (commits state 1)
    clock.set_secs(200);
    feed_sync(&rpc, "jx").await; // state 3 @ 200
    clock.set_secs(210);

    // From state 3 (@200), 30 seconds earlier is 170 — the newest state at or before
    // it is state 2 (@150). Measured from the *current state's* time, as vim does.
    feed_sync(&rpc, ":earlier 30s<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));

    // Another 30s back from 150 is 120 → state 1 (@100).
    feed_sync(&rpc, ":earlier 30s<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo", "charlie"]));

    // Forward 60s from 100 is 160 → the oldest state at or after it is state 3 (@200).
    feed_sync(&rpc, ":later 60s<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "harlie"]));
}

#[tokio::test]
async fn earlier_accepts_minutes_hours_and_days() {
    let (rpc, clock, _incoming) = start_mono_clocked("alpha\nbravo\ncharlie\n").await;

    clock.set_secs(0);
    feed_sync(&rpc, "x").await; // state 1 @ 0
    clock.set_secs(3540); // 59 minutes
    feed_sync(&rpc, "jx").await; // state 2 @ 3540
    clock.set_secs(7200); // 2 hours
    feed_sync(&rpc, "jx").await; // state 3 @ 7200
    clock.set_secs(7300);

    // "What did the buffer look like an hour before the current state?" — 7200-3600
    // is 3600, and the newest state at or before that is state 2 (@3540).
    feed_sync(&rpc, ":earlier 1h<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "charlie"]));

    // 59 minutes before state 2 (@3540) is 0 — state 1, not the root, since a tie on
    // the second resolves to the newest state stamped with it.
    feed_sync(&rpc, ":earlier 59m<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo", "charlie"]));

    // A day forward from state 1 reaches past everything: the newest state.
    feed_sync(&rpc, ":later 1d<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo", "harlie"]));

    // And a day back reaches past the beginning: the original text.
    feed_sync(&rpc, ":earlier 1d<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo", "charlie"]));
}

#[tokio::test]
async fn earlier_1f_returns_to_what_is_on_disk() {
    let path = write_temp("earlier_f", "txt", "alpha\nbravo\ncharlie\n");
    let (rpc, _incoming) = start(Some(path)).await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, ":w<CR>").await; // write 1 == state 1
    feed_sync(&rpc, "jx").await; // an unwritten change on top

    // The current state is not itself a write, so the first step back is the write.
    feed_sync(&rpc, ":earlier 1f<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["lpha", "bravo", "charlie"]),
        "`:earlier 1f` from a dirty buffer returns to the written state"
    );

    // From the write itself, another step back goes before it — the original text.
    feed_sync(&rpc, ":earlier 1f<CR>").await;
    assert_eq!(lines(&rpc).await, v(&["alpha", "bravo", "charlie"]));
}

#[tokio::test]
async fn later_1f_steps_forward_through_writes() {
    let path = write_temp("later_f", "txt", "alpha\nbravo\ncharlie\n");
    let (rpc, _incoming) = start(Some(path)).await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, ":w<CR>").await; // write 1
    feed_sync(&rpc, "jx").await;
    feed_sync(&rpc, ":w<CR>").await; // write 2
    feed_sync(&rpc, "jx").await; // unwritten

    feed_sync(&rpc, ":earlier 2f<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["lpha", "bravo", "charlie"]),
        "two writes back from an unwritten state is write 1"
    );
    feed_sync(&rpc, ":later 1f<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["lpha", "ravo", "charlie"]),
        "and one write forward is write 2"
    );
}

#[tokio::test]
async fn earlier_f_past_the_first_write_reaches_the_original_text() {
    let path = write_temp("earlier_f_end", "txt", "alpha\nbravo\n");
    let (rpc, _incoming) = start(Some(path)).await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, ":w<CR>").await;
    feed_sync(&rpc, "jx").await;

    feed_sync(&rpc, ":earlier 99f<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha", "bravo"]),
        "travelling past the first write lands on the original text, not an error"
    );
}

#[tokio::test]
async fn earlier_by_write_never_travels_forward() {
    // A save number is stamped onto whichever state is current when `:w` runs, so it
    // does not increase with seq: writing, undoing and writing again leaves write 1 on
    // a *later* state than write 2. Seeking "one write back" by number alone would
    // then walk straight forward into the abandoned branch — `:earlier` must never
    // move the buffer to a newer state.
    let path = write_temp("earlier_f_dir", "txt", "alpha\n");
    let (rpc, _incoming) = start(Some(path)).await;

    feed_sync(&rpc, "ia<Esc>").await; // state 1: "aalpha"
    feed_sync(&rpc, "ib<Esc>").await; // state 2: "baalpha"
    feed_sync(&rpc, ":w<CR>").await; // write 1 == state 2
    feed_sync(&rpc, "u").await; // back to state 1
    feed_sync(&rpc, ":w<CR>").await; // write 2 == state 1

    feed_sync(&rpc, ":earlier 1f<CR>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha"]),
        "no write lies behind state 1, so `:earlier 1f` travels as far back as it \
         goes — never forward onto write 1's abandoned branch"
    );
}

#[tokio::test]
async fn later_by_write_never_travels_backward() {
    // The mirror of `earlier_by_write_never_travels_forward`: from a branch with no
    // write behind it, the only write in the tree is *older*, and seeking it by number
    // alone would rewind the buffer under a `:later`.
    let path = write_temp("later_f_dir", "txt", "alpha\n");
    let (rpc, _incoming) = start(Some(path)).await;

    feed_sync(&rpc, "ia<Esc>").await; // state 1
    feed_sync(&rpc, "ib<Esc>").await; // state 2: "baalpha"
    feed_sync(&rpc, ":w<CR>").await; // write 1 == state 2
    feed_sync(&rpc, ":undo 0<CR>").await; // back to the original text
    feed_sync(&rpc, "ic<Esc>").await; // state 3: "calpha", a branch off the root
    feed_sync(&rpc, "id<Esc>").await; // state 4: "dcalpha"
    feed_sync(&rpc, "u").await; // land on state 3

    feed_sync(&rpc, ":later 1f<CR>").await;
    let after = lines(&rpc).await;
    assert_ne!(
        after,
        v(&["baalpha"]),
        "`:later 1f` must not rewind onto write 1, which is an older state"
    );
    assert_eq!(
        after,
        v(&["dcalpha"]),
        "no write lies ahead of state 3, so it travels as far forward as it goes"
    );
}

#[tokio::test]
async fn an_absurd_travel_count_saturates_instead_of_wrapping() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "jx").await;

    // A count near the integer limits must travel as far as it goes — never wrap into
    // a negative that walks the other way, and never overflow. `4611686018427387904d`
    // is the one that overflows a naive `secs * 86400`.
    for arg in [
        "18446744073709551615",
        "9223372036854775807d",
        "4611686018427387904d",
        "9999999999999999999f",
    ] {
        feed_sync(&rpc, ":later 1000<CR>").await; // back to the newest state
        feed_sync(&rpc, &format!(":earlier {arg}<CR>")).await;
        assert_eq!(
            lines(&rpc).await,
            v(&["alpha", "bravo"]),
            "`:earlier {arg}` travels all the way back"
        );
    }
}

#[tokio::test]
async fn earlier_rejects_a_count_too_large_to_be_a_number() {
    let (rpc, mut incoming) = start_with_file("alpha\nbravo\n").await;

    // Past `u64` it is not a count at all — loud, like any other bad argument.
    assert_eq!(
        message_after(&rpc, &mut incoming, ":earlier 99999999999999999999<CR>").await,
        "E475: Invalid argument: 99999999999999999999"
    );
}

#[tokio::test]
async fn earlier_rejects_an_unknown_unit() {
    let (rpc, mut incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, "x").await;
    for arg in ["3y", "s", "1sx"] {
        assert_eq!(
            message_after(&rpc, &mut incoming, &format!(":earlier {arg}<CR>")).await,
            format!("E475: Invalid argument: {arg}"),
            "`{arg}` is not a travel spec"
        );
    }
    assert_eq!(lines(&rpc).await, v(&["lpha", "bravo"]));
}

// ----- `'undolevels'` -------------------------------------------------------
//
// Each undo state holds a full snapshot, so an unbounded history grows for as long as
// the session lives. `'undolevels'` bounds it: the oldest states are pruned, taking
// any branches that forked below them with them (vim's `u_freeheader`/`u_freebranch`).

/// How many states the current buffer's history holds, and which seq it is on.
async fn tree_size(rpc: &Rpc) -> (u64, u64) {
    let got = exec_lua(
        rpc,
        "local t = btv.undotree.get(0) \
         local n = 0 \
         local function walk(es) for _, e in ipairs(es) do n = n + 1 walk(e.alt or {}) end end \
         walk(t.entries) \
         return { n, t.seq_cur }",
    )
    .await;
    let a = got.as_array().expect("a pair");
    (
        a[0].as_u64().expect("count"),
        a[1].as_u64().expect("seq_cur"),
    )
}

#[tokio::test]
async fn undolevels_bounds_the_history() {
    let (rpc, _incoming) = start_with_file("alpha\n").await;

    feed_sync(&rpc, ":set undolevels=3<CR>").await;
    for _ in 0..12 {
        feed_sync(&rpc, "ax<Esc>").await;
    }
    // Commit the last pending edit so every change is a real state.
    feed_sync(&rpc, "ax<Esc>").await;

    let (states, _) = tree_size(&rpc).await;
    assert!(
        states <= 4,
        "13 changes under `undolevels=3` must prune to the newest few, kept {states}"
    );
}

#[tokio::test]
async fn the_default_history_is_bounded_at_vims_thousand() {
    let (rpc, _incoming) = start_with_file("alpha\n").await;

    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].undolevels").await.as_i64(),
        Some(1000),
        "`'undolevels'` defaults to vim's 1000"
    );
}

#[tokio::test]
async fn a_pruned_history_still_undoes_correctly() {
    let (rpc, _incoming) = start_with_file("abcdefgh\n").await;

    feed_sync(&rpc, ":set undolevels=2<CR>").await;
    for _ in 0..5 {
        feed_sync(&rpc, "x").await; // five separate one-character deletions
    }
    feed_sync(&rpc, "ix<Esc>").await; // commit the fifth
    assert_eq!(lines(&rpc).await, v(&["xfgh"]));

    // The two most recent states are still reachable, in order.
    feed_sync(&rpc, "u").await;
    assert_eq!(lines(&rpc).await, v(&["fgh"]));
    feed_sync(&rpc, "u").await;
    assert_eq!(lines(&rpc).await, v(&["efgh"]));

    // And the pruned floor reports the boundary rather than jumping somewhere wrong.
    feed_sync(&rpc, "u").await;
    feed_sync(&rpc, "u").await;
    let bottom = lines(&rpc).await;
    feed_sync(&rpc, "u").await;
    assert_eq!(
        lines(&rpc).await,
        bottom,
        "undo stops at the pruned floor instead of rewinding past it"
    );
    assert!(
        !bottom[0].is_empty() && bottom[0] != "abcdefgh",
        "the original text was pruned away, so it is not reachable: {bottom:?}"
    );
}

#[tokio::test]
async fn undolevels_minus_one_records_no_undo() {
    let (rpc, mut incoming) = start_with_file("alpha\nbravo\n").await;

    feed_sync(&rpc, ":set undolevels=-1<CR>").await;
    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "jx").await;
    assert_eq!(lines(&rpc).await, v(&["lpha", "ravo"]));

    // Nothing was recorded, so `u` reports the boundary — and, critically, must not
    // rewind the text to some stale snapshot.
    assert_eq!(
        message_after(&rpc, &mut incoming, "u").await,
        "Already at oldest change"
    );
    assert_eq!(
        lines(&rpc).await,
        v(&["lpha", "ravo"]),
        "`undolevels=-1` records no undo, but never loses the live text"
    );
}

#[tokio::test]
async fn undolevels_prunes_the_branches_below_a_dropped_state() {
    let (rpc, mut incoming) = start_with_file("abcdefgh\n").await;

    feed_sync(&rpc, ":set undolevels=2<CR>").await;
    // Fork: state 1, state 2, undo, then state 3 as state 1's second child. State 2 is
    // now an abandoned branch hanging off state 1.
    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, "u").await;
    feed_sync(&rpc, "ix<Esc>").await; // state 3
                                      // One more change forces state 1 out of the tree — and the abandoned branch that
                                      // forked below it can no longer be reached, so it goes too.
    feed_sync(&rpc, "x").await; // state 4
    feed_sync(&rpc, "ix<Esc>").await; // commit state 4

    assert!(
        message_after(&rpc, &mut incoming, ":undo 2<CR>")
            .await
            .contains("E830"),
        "the abandoned branch went with the state it forked from"
    );
    // ...while the live spine's own states are still addressable.
    assert!(
        !message_after(&rpc, &mut incoming, ":undo 4<CR>")
            .await
            .contains("E830"),
        "the surviving states stay reachable by seq"
    );
}

#[tokio::test]
async fn undolevels_is_settable_per_buffer_and_globally() {
    let (rpc, _incoming) = start_with_file("alpha\n").await;

    // `:setlocal` touches only this buffer; `:setglobal` seeds the ones opened after.
    feed_sync(&rpc, ":setlocal undolevels=5<CR>").await;
    feed_sync(&rpc, ":setglobal undolevels=7<CR>").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].undolevels").await.as_i64(),
        Some(5)
    );

    feed_sync(&rpc, ":enew<CR>").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].undolevels").await.as_i64(),
        Some(7),
        "a new buffer is born from the global tier"
    );
}

#[tokio::test]
async fn undolevels_is_settable_from_lua() {
    let (rpc, _incoming) = start_with_file("alpha\n").await;

    // `vim.bo` / `btv.bo` reach the *core*, not just the mirror the write echoes into
    // for read-after-write — so the read-back has to be a separate round trip, after
    // the server has pushed the core's own value back.
    exec_lua(&rpc, "vim.bo[0].undolevels = 3").await;
    assert_eq!(
        exec_lua(&rpc, "return vim.bo[0].undolevels").await.as_i64(),
        Some(3),
        "a `vim.bo` write reaches the core rather than being silently dropped"
    );

    // And it is the history that moves, not only the readout: `-1` records nothing,
    // so `u` has nowhere to go.
    exec_lua(&rpc, "btv.bo[0].undolevels = -1").await;
    assert_eq!(
        exec_lua(&rpc, "return btv.bo[0].undolevels").await.as_i64(),
        Some(-1)
    );
    feed_sync(&rpc, "ix<Esc>").await;
    feed_sync(&rpc, "iy<Esc>").await;
    feed_sync(&rpc, "u").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["yxalpha"]),
        "`btv.bo.undolevels = -1` bounds the real history: nothing was recorded to undo"
    );
}

/// vim's `undotree().save_cur` is `b_u_save_nr_cur` — "the file write nr after which we
/// are now", the base `:earlier 1f` counts back from — not "a write stamped on exactly
/// this state". Editing on top of a write keeps it; undoing back past one drops it.
///
/// bemtvi's `:earlier {N}f` already resolves it that way (an ancestor walk, so a write
/// on an abandoned branch is not behind us); the projection reported only the current
/// node's own `save`, so the number a visualizer read disagreed with the number the
/// command it drives actually counts from.
#[tokio::test]
async fn undotree_save_cur_is_the_write_the_state_descends_from() {
    let path = write_temp("save_cur", "txt", "alpha\nbravo\n");
    let (rpc, _incoming) = start(Some(path)).await;

    async fn save_cur(rpc: &Rpc) -> i64 {
        exec_lua(rpc, "return btv.undotree.get(0).save_cur")
            .await
            .as_i64()
            .expect("save_cur is a number")
    }

    feed_sync(&rpc, "x").await;
    feed_sync(&rpc, ":w<CR>").await;
    assert_eq!(save_cur(&rpc).await, 1, "the written state is write 1");

    // An uncommitted edit on top of the write still descends from it…
    feed_sync(&rpc, "jx").await;
    assert_eq!(
        save_cur(&rpc).await,
        1,
        "a pending edit is still after write 1"
    );
    // …and so does the next one, once the first has committed.
    feed_sync(&rpc, "kx").await;
    assert_eq!(save_cur(&rpc).await, 1);

    // Undoing back down the spine keeps reporting it, right up to the write itself.
    feed_sync(&rpc, "u").await;
    assert_eq!(save_cur(&rpc).await, 1);
    feed_sync(&rpc, "u").await;
    assert_eq!(save_cur(&rpc).await, 1, "back on the written state itself");

    // Only stepping past the write drops it to "before any write".
    feed_sync(&rpc, "u").await;
    assert_eq!(
        save_cur(&rpc).await,
        0,
        "the original text is before write 1"
    );
}

// ----- `g-` / `g+` are Normal-mode only -------------------------------------
//
// vim guards the pair with `checkclearopq`: with an operator armed or a selection up
// it clears the pending command and beeps rather than travelling. Rewinding there
// would move the text out from under the very thing the next key operates on.

#[tokio::test]
async fn time_travel_is_refused_under_a_selection() {
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;
    feed_sync(&rpc, "ix<Esc>").await;
    feed_sync(&rpc, "iy<Esc>").await;
    assert_eq!(lines(&rpc).await, v(&["yxalpha", "bravo"]));

    feed_sync(&rpc, "vl").await;
    feed_sync(&rpc, "g-").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["yxalpha", "bravo"]),
        "`g-` under a selection is a dead-end key, not a rewind of the selected text"
    );
    feed_sync(&rpc, "g+").await;
    assert_eq!(lines(&rpc).await, v(&["yxalpha", "bravo"]), "`g+` likewise");

    // The selection itself survives — vim clears the pending *command*, not Visual —
    // so the operator that follows still applies to what was selected.
    assert_eq!(mode(&rpc).await, "v", "still in Visual mode");
    feed_sync(&rpc, "d").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha", "bravo"]),
        "the two selected characters go: the selection was never dropped"
    );
}

#[tokio::test]
async fn time_travel_is_refused_with_an_operator_pending() {
    let (rpc, _incoming) = start_with_file("alpha bravo\n").await;
    feed_sync(&rpc, "ix<Esc>").await;
    feed_sync(&rpc, "iy<Esc>").await;
    assert_eq!(lines(&rpc).await, v(&["yxalpha bravo"]));

    feed_sync(&rpc, "dg-").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["yxalpha bravo"]),
        "`g-` under a pending operator neither travels nor deletes"
    );

    // …and the operator went with it, so the next motion only moves the cursor.
    feed_sync(&rpc, "w").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["yxalpha bravo"]),
        "the pending `d` was cleared, so `w` is a plain motion"
    );
    assert_eq!(cursor(&rpc).await, (1, 8), "`w` moved to the next word");
}

#[tokio::test]
async fn a_refused_time_travel_stops_a_macro() {
    // The refusal is vim's `clearopbeep`, and bemtvi's beep is what ends a macro
    // playback — so a recorded `g-`-under-a-selection stops the run rather than
    // silently falling through to the rest of the register.
    let (rpc, _incoming) = start_with_file("alpha\n").await;
    feed_sync(&rpc, "ix<Esc>").await;

    // Recording executes live: the `g-` is refused, then `<Esc>` and `x` still run,
    // so one character goes here.
    feed_sync(&rpc, "<F2>avg-<Esc>x<F2>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha"]),
        "the recording pass deleted the `x`"
    );

    feed_sync(&rpc, "<F3>a").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha"]),
        "replaying stops at the refused `g-`, so its trailing `x` never runs"
    );
    assert_eq!(
        mode(&rpc).await,
        "v",
        "the playback died inside the selection the macro had just made"
    );
}

#[tokio::test]
async fn undo_and_redo_are_refused_under_a_selection() {
    // The same guard, for the rest of the family. vim reaches it two ways: `<C-r>` is
    // `checkclearopq`-guarded outright (`nv_redo_or_register`), and `u` is redirected
    // to the `gu` lowercase *operator* (`nv_undo`), so neither ever rewinds from a
    // selection. bemtvi has no case operator to redirect to, so `u` is a loud dead end
    // — which is the half that matters: a silent rewind leaves the selection anchored
    // at offsets belonging to a state that no longer exists.
    let (rpc, _incoming) = start_with_file("alpha\nbravo\n").await;
    feed_sync(&rpc, "ix<Esc>").await;
    feed_sync(&rpc, "iy<Esc>").await;
    assert_eq!(lines(&rpc).await, v(&["yxalpha", "bravo"]));

    feed_sync(&rpc, "vl").await;
    feed_sync(&rpc, "u").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["yxalpha", "bravo"]),
        "`u` under a selection does not rewind"
    );
    feed_sync(&rpc, "<C-r>").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["yxalpha", "bravo"]),
        "`<C-r>` under a selection does not redo"
    );

    assert_eq!(mode(&rpc).await, "v", "still in Visual mode");
    feed_sync(&rpc, "d").await;
    assert_eq!(
        lines(&rpc).await,
        v(&["alpha", "bravo"]),
        "the selection survived both refusals"
    );

    // And Normal mode is untouched: `u` still walks the tree.
    feed_sync(&rpc, "u").await;
    assert_eq!(lines(&rpc).await, v(&["yxalpha", "bravo"]));
}
