//! Helix editing model — Phase 4a: selection-set verbs (no text edit).
//!
//! These transform the selection *set* itself: `x` extends line-wise, `%` selects
//! the whole file, `_` trims to non-whitespace, `Alt-;` flips anchor/head, `;`
//! collapses each selection to a cursor, `,` keeps only the primary. (The
//! multi-selection *spawners* — `s`/`S`/`K`/`C` — land in the 4b slice.) Tests
//! assert on the rendered selection span, cursor head, and buffer text.

use crate::support::*;

/// `x` extends the selection to the whole line; a repeat grows one line downward.
#[tokio::test]
async fn extend_line_selects_then_grows_downward() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>gg0:helix<CR>");

    // First `x` selects all of line 1 (cols 0..=2 → span [0,3)).
    let map = redraw_after(&rpc, &mut incoming, "x").await;
    assert_eq!(cursor(&rpc).await, (1, 2), "head at end of line 1");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 3)),
        "the whole first line is selected",
    );

    // A second `x` grows the selection down onto line 2.
    let map2 = redraw_after(&rpc, &mut incoming, "x").await;
    assert_eq!(cursor(&rpc).await, (2, 2), "head grew down to line 2");
    let sel = view_selection(&map2);
    // A multi-line charwise selection highlights the first line through its
    // trailing newline cell (span extends one past the last char).
    assert_eq!(
        sel.first().copied().flatten(),
        Some((0, 4)),
        "line 1 still full"
    );
    assert_eq!(
        sel.get(1).copied().flatten(),
        Some((0, 3)),
        "line 2 now selected too"
    );
}

/// `X` extends the selection to the whole line; a repeat grows one line *upward*
/// (Helix's `extend_line_above`, the mirror of `x`).
#[tokio::test]
async fn extend_line_above_selects_then_grows_upward() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbb<CR>ccc<Esc>:helix<CR>");

    // First `X` selects all of line 3; the head sits on the growing (top) end.
    let map = redraw_after(&rpc, &mut incoming, "X").await;
    assert_eq!(cursor(&rpc).await, (3, 0), "head at the start of line 3");
    assert_eq!(
        view_selection(&map).get(2).copied().flatten(),
        Some((0, 3)),
        "the whole third line is selected",
    );

    // A second `X` grows the selection up onto line 2.
    let map2 = redraw_after(&rpc, &mut incoming, "X").await;
    assert_eq!(cursor(&rpc).await, (2, 0), "head grew up to line 2");
    let sel = view_selection(&map2);
    assert_eq!(
        sel.get(1).copied().flatten(),
        Some((0, 4)),
        "line 2 selected (through its trailing newline cell)",
    );
    assert_eq!(
        sel.get(2).copied().flatten(),
        Some((0, 3)),
        "line 3 still full"
    );
    assert_eq!(sel.first().copied().flatten(), None, "line 1 is untouched");
}

/// `%` selects the entire file as a single selection.
#[tokio::test]
async fn percent_selects_whole_file() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "iaaa<CR>bbbb<Esc>gg0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "%").await;
    assert_eq!(
        cursor(&rpc).await,
        (2, 3),
        "head at the last char of the file"
    );
    let sel = view_selection(&map);
    // Line 1 highlights through its trailing newline cell (multi-line selection).
    assert_eq!(
        sel.first().copied().flatten(),
        Some((0, 4)),
        "line 1 fully covered"
    );
    assert_eq!(
        sel.get(1).copied().flatten(),
        Some((0, 4)),
        "line 2 fully covered"
    );
}

/// `_` trims a selection down to its non-whitespace content.
#[tokio::test]
async fn trim_drops_surrounding_whitespace() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "i  hi<Esc>0:helix<CR>");

    // `x` selects the whole line "  hi" (cols 0..=3); `_` trims to "hi" (cols 2..=3).
    let map = redraw_after(&rpc, &mut incoming, "x_").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((2, 4)),
        "leading whitespace was trimmed off the selection",
    );
    assert_eq!(
        cursor(&rpc).await,
        (1, 3),
        "head sits on the last non-blank"
    );
}

/// `Alt-;` flips the anchor and head — the span is unchanged but the moving end
/// swaps, so the cursor jumps to the other end.
#[tokio::test]
async fn flip_swaps_anchor_and_head() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    // `w` selects "hello " with the head on col 5; `Alt-;` moves the head to col 0.
    let map = redraw_after(&rpc, &mut incoming, "w<A-;>").await;
    assert_eq!(cursor(&rpc).await, (1, 0), "the head is now on the far end");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 6)),
        "the span is unchanged by the flip",
    );
}

/// `;` collapses a wide selection to a 1-wide cursor at the head.
#[tokio::test]
async fn collapse_reduces_selection_to_a_cursor() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");

    let map = redraw_after(&rpc, &mut incoming, "w;").await;
    assert_eq!(cursor(&rpc).await, (1, 5), "cursor stays at the head");
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((5, 6)),
        "the selection collapsed to a 1-wide block at the head",
    );
}
