//! The case operators — `gu` / `gU` / `g~` and their visual `u` / `U` / `~`
//! spellings — driven through the keyboard the way vim's are.
//!
//! Before these existed, `g` armed its prefix, the following `u` matched nothing,
//! and the key fell through to plain **undo**: `guu` silently rewound the buffer.

use crate::support::*;

#[tokio::test]
async fn guu_lowercases_the_line_and_g_uu_uppercases_it() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iAbC dEf<Esc>");
    feed(&rpc, "gg0guu");
    assert_eq!(lines(&rpc).await, vec!["abc def"]);
    feed(&rpc, "gUU");
    assert_eq!(lines(&rpc).await, vec!["ABC DEF"]);
    // `g~~` toggles every character of the line.
    feed(&rpc, "g~~");
    assert_eq!(lines(&rpc).await, vec!["abc def"]);
}

#[tokio::test]
async fn the_case_operators_double_through_the_g_prefix_too() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iAbC dEf<Esc>");
    // vim spells the doubling both ways: `guu` and `gugu`.
    feed(&rpc, "gg0gugu");
    assert_eq!(lines(&rpc).await, vec!["abc def"]);
    feed(&rpc, "gUgU");
    assert_eq!(lines(&rpc).await, vec!["ABC DEF"]);
}

#[tokio::test]
async fn a_case_operator_takes_a_motion_charwise() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc def ghi<Esc>");
    // `gUw` upper-cases exactly the first word, not the whole line.
    feed(&rpc, "gg0gUw");
    assert_eq!(lines(&rpc).await, vec!["ABC def ghi"]);
    // …and a text object takes the word under the cursor.
    feed(&rpc, "wgUiw");
    assert_eq!(lines(&rpc).await, vec!["ABC DEF ghi"]);
    // The cursor settles on the start of the range vim's `op_tilde` leaves it at.
    assert_eq!(cursor(&rpc).await.1, 4);
}

#[tokio::test]
async fn a_count_before_the_operator_spans_that_many_lines() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione<CR>two<CR>three<Esc>");
    feed(&rpc, "gg2gUU");
    assert_eq!(lines(&rpc).await, vec!["ONE", "TWO", "three"]);
}

#[tokio::test]
async fn visual_u_and_upper_u_recase_the_selection_instead_of_undoing() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iAbC dEf<Esc>");
    // The whole point: `u` from a selection is the lowercase *operator*, never a
    // rewind of the insert that made the line.
    feed(&rpc, "gg0Vu");
    assert_eq!(lines(&rpc).await, vec!["abc def"]);
    feed(&rpc, "VU");
    assert_eq!(lines(&rpc).await, vec!["ABC DEF"]);
    feed(&rpc, "V~");
    assert_eq!(lines(&rpc).await, vec!["abc def"]);
}

#[tokio::test]
async fn a_charwise_visual_recases_only_the_selection() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iabc def<Esc>");
    // `vll` selects `abc`; `U` upper-cases just that.
    feed(&rpc, "gg0vllU");
    assert_eq!(lines(&rpc).await, vec!["ABC def"]);
}

#[tokio::test]
async fn a_case_operator_is_one_undo_step() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iAbC dEf<Esc>");
    feed(&rpc, "gg0gUU");
    assert_eq!(lines(&rpc).await, vec!["ABC DEF"]);
    feed(&rpc, "u");
    assert_eq!(lines(&rpc).await, vec!["AbC dEf"]);
}

#[tokio::test]
async fn a_case_operator_dot_repeats() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ione two<CR>three four<Esc>");
    feed(&rpc, "gg0gUw");
    assert_eq!(lines(&rpc).await, vec!["ONE two", "three four"]);
    feed(&rpc, "j0.");
    assert_eq!(lines(&rpc).await, vec!["ONE two", "THREE four"]);
}

#[tokio::test]
async fn a_width_changing_case_fold_is_replaced_wholesale() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "istraße<Esc>");
    // `ß` upper-cases to two characters; the span is rewritten, not patched.
    feed(&rpc, "gg0gUU");
    assert_eq!(lines(&rpc).await, vec!["STRASSE"]);
}

#[tokio::test]
async fn a_case_operator_refuses_on_a_nomodifiable_buffer() {
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "iAbC<Esc>");
    feed(&rpc, ":set nomodifiable<CR>");
    feed(&rpc, "gg0gUU");
    assert_eq!(lines(&rpc).await, vec!["AbC"]);
}

#[tokio::test]
async fn a_case_operator_brackets_the_recased_span_with_the_change_marks() {
    // vim's `op_tilde` leaves `'[` / `']` around what it recased, so `` `[ `` jumps
    // to the change and `` v`] `` reselects it. The span is replaced wholesale, so
    // marks recorded *before* the rewrite are collapsed onto the edit by the remove
    // and dragged past its end by the insert: the visual spelling reported both
    // marks one past the last recased character, and the operator spelling set
    // neither. Both now record after the write, against the new length.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ialpha beta gamma<Esc>");
    feed(&rpc, "0wgUe");
    assert_eq!(lines(&rpc).await, vec!["alpha BETA gamma"]);
    feed(&rpc, "0`[");
    assert_eq!(cursor(&rpc).await, (1, 6), "`[ is the first recased char");
    feed(&rpc, "0`]");
    assert_eq!(cursor(&rpc).await, (1, 9), "`] is the last recased char");

    // The visual spelling of the same operator agrees.
    feed(&rpc, "0wveu");
    assert_eq!(lines(&rpc).await, vec!["alpha beta gamma"]);
    feed(&rpc, "0`[");
    assert_eq!(cursor(&rpc).await, (1, 6));
    feed(&rpc, "0`]");
    assert_eq!(cursor(&rpc).await, (1, 9));
}

#[tokio::test]
async fn a_width_changing_case_fold_brackets_its_grown_span() {
    // `ß` → `SS` makes the recased text longer than the source, so `']` must follow
    // the text that was actually written, not the range that was read.
    let (rpc, _incoming) = start(None).await;
    feed(&rpc, "ix straße y<Esc>");
    feed(&rpc, "0wgUe");
    assert_eq!(lines(&rpc).await, vec!["x STRASSE y"]);
    feed(&rpc, "0`]");
    assert_eq!(cursor(&rpc).await, (1, 8), "`] is the last char of STRASSE");
}
