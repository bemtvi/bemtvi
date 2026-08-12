//! Helix editing model — Phase 4b: multiple selections.
//!
//! `C` / `Alt-C` grow a multi-selection by copying the primary onto the next /
//! previous line; `(` / `)` rotate which selection is primary; and a verb then
//! runs over *every* selection at once (the multi-selection edit foundation).
//! Tests assert on the resulting buffer text and the primary cursor.

use crate::support::*;

/// `C` copies the selection onto the next line, then a verb edits both selections.
#[tokio::test]
async fn copy_down_then_delete_edits_every_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg0:helix<CR>");
    // `C` adds a selection on line 2 (both are 1-wide at col 0); `d` deletes the
    // char under every selection.
    feed(&rpc, "Cd");
    assert_eq!(
        lines(&rpc).await,
        vec!["bc", "bc", "abc"],
        "the first char of lines 1 and 2 was deleted, line 3 untouched",
    );
}

/// `C` makes the copy the new primary (so a repeat walks downward); `)` rotates
/// the primary back — observable through the reported cursor.
#[tokio::test]
async fn copy_makes_new_primary_and_rotate_moves_it() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg0:helix<CR>");

    feed(&rpc, "C");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "the copy on line 2 is now primary"
    );

    // `)` rotates the primary forward through document order → back to line 1.
    feed(&rpc, ")");
    assert_eq!(
        cursor(&rpc).await,
        (1, 0),
        "rotate moved the primary to line 1"
    );
}

/// A keep-selection verb (`~`) also runs across every selection.
#[tokio::test]
async fn copy_then_switch_case_toggles_every_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>abc<Esc>gg0:helix<CR>");
    feed(&rpc, "C~");
    assert_eq!(
        lines(&rpc).await,
        vec!["Abc", "Abc"],
        "case switched under both selections",
    );
}

/// `Alt-C` (spelled `<A-S-c>` — neovim carries shift in the modifier flag, not the
/// letter case) copies the selection onto the previous line.
#[tokio::test]
async fn copy_up_then_delete_edits_every_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>abc<Esc>:helix<CR>0");
    // Cursor starts on line 2; `Alt-Shift-C` copies the selection up to line 1.
    feed(&rpc, "<A-S-c>d");
    assert_eq!(
        lines(&rpc).await,
        vec!["bc", "bc"],
        "the first char of both lines was deleted",
    );
}

/// `,` drops every selection but the primary: after `C` grew a multi-selection,
/// a verb edits only the primary's line again.
#[tokio::test]
async fn keep_primary_drops_secondaries() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg0:helix<CR>");
    // `C` twice → selections on lines 1–3, the line-3 copy primary; `,` keeps it.
    feed(&rpc, "CC,");
    assert_eq!(
        cursor(&rpc).await,
        (3, 0),
        "the primary (line-3 copy) is kept"
    );
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["abc", "abc", "bc"],
        "only the primary's line was edited after `,`",
    );
}

/// `(` rotates the primary backward through document order (the `)` mirror; three
/// selections so forward and backward land on different neighbours).
#[tokio::test]
async fn rotate_backward_moves_primary_back() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<CR>abc<CR>abc<Esc>gg0:helix<CR>");
    feed(&rpc, "CC");
    assert_eq!(cursor(&rpc).await, (3, 0), "primary on the last copy");
    feed(&rpc, "(");
    assert_eq!(
        cursor(&rpc).await,
        (2, 0),
        "rotate backward moved the primary to line 2"
    );
}

/// `Alt-,` drops the *primary* selection and keeps the rest (the inverse of `,`).
/// Here three point selections are placed (lines 1-3); `Alt-,` removes line 3's,
/// and a following `d` deletes only the two survivors, proving line 3 was dropped.
#[tokio::test]
async fn remove_primary_drops_the_primary_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia<CR>b<CR>c<Esc>gg0:helix<CR>");
    // `CC` copies the point down to lines 2 and 3; the last copy (line 3) is primary.
    feed(&rpc, "CC");
    assert_eq!(cursor(&rpc).await, (3, 0), "primary on line 3");

    feed(&rpc, "<A-,>");
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["", "", "c"],
        "line 3 was dropped from the set; only lines 1-2 were deleted",
    );
}

// ----- rotate selection contents (`Alt-)` / `Alt-(`) -----------------------

/// `Alt-)` rotates the *text* of the selections forward: each selection's content
/// moves to the next selection in document order (wrapping). The selections stay
/// put; only what they hold moves.
#[tokio::test]
async fn rotate_contents_forward_shifts_each_selection_text() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia b c<Esc>0:helix<CR>");
    // Select the line, then select-within on each letter → three selections a/b/c.
    feed(&rpc, "xs[a-c]<CR>");
    // Forward: a→b's slot, b→c's slot, c→a's slot ⇒ "c a b".
    feed(&rpc, "<A-)>");
    assert_eq!(
        lines(&rpc).await,
        vec!["c a b"],
        "contents rotated one selection forward",
    );
}

/// `Alt-(` rotates the selection contents the other way.
#[tokio::test]
async fn rotate_contents_backward_shifts_each_selection_text() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia b c<Esc>0:helix<CR>");
    feed(&rpc, "xs[a-c]<CR>");
    // Backward: a→c's slot, b→a's slot, c→b's slot ⇒ "b c a".
    feed(&rpc, "<A-(>");
    assert_eq!(
        lines(&rpc).await,
        vec!["b c a"],
        "contents rotated one selection backward",
    );
}

/// Rotation re-fits selections of *unequal* width: the running byte offset keeps
/// every selection over its (rotated) content even as lengths change.
#[tokio::test]
async fn rotate_contents_handles_unequal_widths() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ione two three<Esc>0:helix<CR>");
    feed(&rpc, "xs[a-z]+<CR>");
    // Forward: one→two's slot, two→three's slot, three→one's slot.
    feed(&rpc, "<A-)>");
    assert_eq!(
        lines(&rpc).await,
        vec!["three one two"],
        "words rotated forward despite differing lengths",
    );
}

// ----- per-selection open (`o` / `O`) --------------------------------------

/// `o` (plugin insert-entry) opens a fresh line below *every* selection and enters
/// multi-cursor Insert — the per-selection open, not primary-only.
#[tokio::test]
async fn open_below_is_per_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaa<CR>bb<Esc>gg0:helix<CR>");
    // `C` copies the caret down to line 2 → two selections; `o` opens a line below
    // each and types into both at once.
    feed(&rpc, "CoX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["aa", "X", "bb", "X"],
        "a new line was opened and typed under every selection",
    );
    assert_eq!(mode(&rpc).await, "hn", "<Esc> resumed Helix normal");
}

/// `O` opens a fresh line *above* every selection.
#[tokio::test]
async fn open_above_is_per_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaa<CR>bb<Esc>gg0:helix<CR>");
    feed(&rpc, "COX<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["X", "aa", "X", "bb"],
        "a new line was opened and typed above every selection",
    );
}

/// After a per-selection `o`, each selection is a caret on its own new line — not a
/// span back to the pre-open line. A following `d` deletes exactly the one typed
/// char at each, proving the stale anchors were dropped.
#[tokio::test]
async fn open_leaves_a_caret_per_new_line() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iaa<CR>bb<Esc>gg0:helix<CR>");
    feed(&rpc, "CoX<Esc>");
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["aa", "", "bb", ""],
        "each caret deleted just its char — no span back to the old line",
    );
}

// ----- align selections (`&`) ----------------------------------------------

/// `&` pads each selection so its start lands on the same column — the classic
/// "align the `=` signs" transform. Selections on different columns line up.
#[tokio::test]
async fn align_selections_lines_up_the_columns() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ix = 1<CR>yy = 2<Esc>gg0:helix<CR>");
    // Select every `=` (one per line), then align their columns.
    feed(&rpc, "%s=<CR>&");
    assert_eq!(
        lines(&rpc).await,
        vec!["x  = 1", "yy = 2"],
        "a space was inserted so both `=` sit at the same column",
    );
}
