//! Dot-repeat (`.`) — replay the last buffer-changing command.
//!
//! Phase 1: `.` re-feeds the recorded raw key stream of the last change through
//! the input path, so the count, register, operator, motion, and any inserted
//! text all re-parse from the keys. These black-box tests feed vim notation and
//! assert on buffer contents / cursor, exactly as a client observes them.

use crate::support::*;

#[tokio::test]
async fn dot_repeats_x() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcdef<Esc>0");
    feed(&rpc, "x"); // delete 'a'
    feed(&rpc, "."); // delete 'b'
    assert_eq!(lines(&rpc).await, vec!["cdef"]);
}

#[tokio::test]
async fn dot_repeats_x_with_recorded_count() {
    // `3x` records its count; `.` with no new count replays the whole `3x`.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcdefgh<Esc>0");
    feed(&rpc, "3x"); // delete abc
    feed(&rpc, "."); // delete def
    assert_eq!(lines(&rpc).await, vec!["gh"]);
}

#[tokio::test]
async fn dot_repeats_dw() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione two three<Esc>0");
    feed(&rpc, "dw"); // remove "one "
    feed(&rpc, "."); // remove "two "
    assert_eq!(lines(&rpc).await, vec!["three"]);
}

#[tokio::test]
async fn dot_repeats_dd() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<CR>four<Esc>gg");
    feed(&rpc, "dd"); // remove "one"
    feed(&rpc, "."); // remove "two"
    assert_eq!(lines(&rpc).await, vec!["three", "four"]);
}

#[tokio::test]
async fn dot_repeats_change_with_inserted_text() {
    // `ciwfoo<Esc>` then `w.` changes the next word's text object to "foo" —
    // the inserted "foo" replays from the recorded insert-session keys.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione two<Esc>0");
    feed(&rpc, "ciwfoo<Esc>");
    feed(&rpc, "w."); // jump to "two", change it too
    assert_eq!(lines(&rpc).await, vec!["foo foo"]);
}

#[tokio::test]
async fn dot_repeats_append_on_a_new_line() {
    // `A;<Esc>` then `j.` appends `;` to the next line (insert text replays).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<Esc>gg");
    feed(&rpc, "A;<Esc>");
    feed(&rpc, "j.");
    assert_eq!(lines(&rpc).await, vec!["one;", "two;"]);
}

#[tokio::test]
async fn dot_repeats_replace_char() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcd<Esc>0");
    feed(&rpc, "rx"); // a -> x; `r` leaves the cursor on the replaced char
    feed(&rpc, "ll."); // skip 'b', land on 'c', c -> x
    assert_eq!(lines(&rpc).await, vec!["xbxd"]);
}

#[tokio::test]
async fn dot_repeats_toggle_case() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iab<Esc>0");
    feed(&rpc, "~"); // a -> A (cursor advances to b)
    feed(&rpc, "."); // b -> B
    assert_eq!(lines(&rpc).await, vec!["AB"]);
}

#[tokio::test]
async fn dot_repeats_paste() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo<Esc>0");
    feed(&rpc, "yl"); // yank 'f' into the unnamed register
    feed(&rpc, "p"); // paste after cursor -> "ffoo"
    feed(&rpc, "."); // paste again -> "fffoo"
    assert_eq!(lines(&rpc).await, vec!["fffoo"]);
}

#[tokio::test]
async fn pure_motion_does_not_change_what_dot_repeats() {
    // A `w` between the change and `.` must not become the repeated command.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione two three<Esc>0");
    feed(&rpc, "dw"); // remove "one "
    feed(&rpc, "w"); // pure motion: jumps over "two " to "three"... but
                     // after `dw` the buffer is "two three" with cursor on "two"; `w` -> "three".
    feed(&rpc, "."); // still repeats `dw`, removing the word under the cursor
    assert_eq!(lines(&rpc).await, vec!["two "]);
}

#[tokio::test]
async fn undo_is_not_repeated_by_dot() {
    // After `xu`, `.` repeats the `x` (the last *change*), not the undo.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcd<Esc>0");
    feed(&rpc, "x"); // "bcd"
    feed(&rpc, "u"); // back to "abcd"
    feed(&rpc, "."); // repeat the x, not the undo -> "bcd"
    assert_eq!(lines(&rpc).await, vec!["bcd"]);
}

#[tokio::test]
async fn ex_delete_is_not_repeated_by_dot() {
    // `:d<CR>` deletes a line but transits command mode, so `.` does not capture
    // it. `.` must keep replaying the change that preceded it (the `x`), not the
    // line delete — otherwise `.` here would wipe the whole "world" line.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ihello<CR>world<Esc>gg");
    feed(&rpc, "x"); // delete 'h' -> "ello"
    feed(&rpc, ":d<CR>"); // delete the line "ello" -> buffer is "world"
    assert_eq!(lines(&rpc).await, vec!["world"]);
    feed(&rpc, "."); // repeats the `x`, not the `:d` -> "orld"
    assert_eq!(lines(&rpc).await, vec!["orld"]);
}

#[tokio::test]
async fn dot_with_no_prior_change_is_a_noop() {
    // Only motions have run — nothing buffer-changing — so `.` does nothing.
    // (An insert *would* be recorded, so this deliberately avoids one.)
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "lll0"); // pure motions on the empty buffer
    feed(&rpc, "."); // nothing recorded yet
    assert_eq!(lines(&rpc).await, vec![""]);
}

#[tokio::test]
async fn repeated_dot_keeps_replaying_the_original_change() {
    // `.` must never become the new last change: a second/third `.` replays the
    // original `x`, not a degenerate "repeat the repeat".
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcdef<Esc>0");
    feed(&rpc, "x"); // delete 'a' -> "bcdef"
    feed(&rpc, "..."); // delete b, c, d
    assert_eq!(lines(&rpc).await, vec!["ef"]);
}

// ===== Phase 2 — `[count].` count override ===================================

#[tokio::test]
async fn count_on_dot_overrides_recorded_count_for_words() {
    // `dw` then `3.` runs `3dw` — the new count replaces the (absent) recorded one.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione two three four five<Esc>0");
    feed(&rpc, "dw"); // remove "one " -> "two three four five"
    feed(&rpc, "3."); // 3dw: remove "two three four " -> "five"
    assert_eq!(lines(&rpc).await, vec!["five"]);
}

#[tokio::test]
async fn count_on_dot_replaces_recorded_count() {
    // `2dd` then `3.` deletes three lines (the new count *replaces* the recorded 2).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<CR>f<Esc>gg");
    feed(&rpc, "2dd"); // delete a,b -> c,d,e,f
    feed(&rpc, "3."); // 3dd: delete c,d,e -> f
    assert_eq!(lines(&rpc).await, vec!["f"]);
}

#[tokio::test]
async fn dot_without_count_reuses_recorded_count() {
    // `2dd` then plain `.` repeats with the recorded count (two lines), not one.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<Esc>gg");
    feed(&rpc, "2dd"); // delete a,b -> c,d,e
    feed(&rpc, "."); // 2dd again: delete c,d -> e
    assert_eq!(lines(&rpc).await, vec!["e"]);
}

#[tokio::test]
async fn count_on_dot_overrides_x() {
    // `2x` then `3.` deletes three chars (override), not two.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcdefgh<Esc>0");
    feed(&rpc, "2x"); // delete a,b -> "cdefgh"
    feed(&rpc, "3."); // 3x: delete c,d,e -> "fgh"
    assert_eq!(lines(&rpc).await, vec!["fgh"]);
}

#[tokio::test]
async fn leading_zero_before_dot_is_a_motion_not_a_count() {
    // `0` is the column-zero motion, not a count, so `0.` moves to column 0 and
    // then repeats the recorded change with its own count (here, single `x`).
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcde<Esc>0");
    feed(&rpc, "x"); // delete 'a' -> "bcde", cursor col 0
    feed(&rpc, "$"); // jump to end ('e')
    feed(&rpc, "0."); // `0` -> col 0 ('b'); `.` repeats `x` -> delete 'b' -> "cde"
    assert_eq!(lines(&rpc).await, vec!["cde"]);
}

// ===== Phase 3 — size-faithful visual-mode dot-repeat ========================

#[tokio::test]
async fn dot_repeats_a_linewise_visual_delete() {
    // `Vjd` deletes two lines; `.` deletes the next two — the same line count.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<CR>d<CR>e<CR>f<Esc>gg");
    feed(&rpc, "Vjd"); // delete a, b -> c,d,e,f
    feed(&rpc, "."); // delete c, d -> e,f
    assert_eq!(lines(&rpc).await, vec!["e", "f"]);
}

#[tokio::test]
async fn dot_repeats_a_charwise_visual_delete_by_size() {
    // `vll` selects three chars; `.` deletes three more, regardless of words.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabcdefgh<Esc>0");
    feed(&rpc, "vlld"); // delete "abc" -> "defgh", cursor on 'd'
    feed(&rpc, "."); // delete "def" -> "gh"
    assert_eq!(lines(&rpc).await, vec!["gh"]);
}

#[tokio::test]
async fn visual_dot_reselects_the_same_size_not_the_same_motion() {
    // The faithful behavior: `viwd` records the *size* (3 chars), so repeating it
    // over a longer word deletes only three chars — not the whole next word as a
    // naive `viw` key-replay would. This is the distinguishing test.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ifoo barbarbar<Esc>0");
    feed(&rpc, "viwd"); // delete "foo" (3 chars) -> " barbarbar"
    feed(&rpc, "w."); // 3-char delete at "barbarbar" -> " barbar", NOT " "
    assert_eq!(lines(&rpc).await, vec![" barbar"]);
}

#[tokio::test]
async fn dot_repeats_a_charwise_visual_change() {
    // `vcZ<Esc>` changes one char to "Z"; `.` changes the next char the same way.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc<Esc>0");
    feed(&rpc, "vcZ<Esc>"); // 'a' -> "Z" => "Zbc", cursor on 'Z'
    feed(&rpc, "l."); // move to 'b', change it -> "ZZc"
    assert_eq!(lines(&rpc).await, vec!["ZZc"]);
}

#[tokio::test]
async fn dot_repeats_a_linewise_visual_change() {
    // `Vjc` changes two lines into typed text; `.` does the same to the next two.
    // (Operates clear of the final line, which has a separate linewise-change-at-
    // EOF quirk unrelated to dot-repeat.)
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<CR>four<CR>five<Esc>gg");
    feed(&rpc, "Vjchi<Esc>"); // change one,two -> "hi" => hi,three,four,five
    feed(&rpc, "j."); // change three,four -> "hi" => hi,hi,five
    assert_eq!(lines(&rpc).await, vec!["hi", "hi", "five"]);
}
