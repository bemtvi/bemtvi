//! Helix editing model — match mode (`m`).
//!
//! `mm` jumps to the matching bracket; `mi{obj}` / `ma{obj}` select the inner /
//! around text object at each selection's head (reusing the vim text-object
//! engine); `ms{char}` / `md{char}` / `mr{from}{to}` add / delete / replace the
//! surrounding delimiter pair. These drive the opt-in mode (`:helix`) and assert on
//! buffer contents and the rendered selection span.

use crate::support::*;

/// `mm` jumps the cursor from an opening bracket to its match (like vim's `%`).
#[tokio::test]
async fn match_bracket_jumps_to_the_pair() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    // Cursor on '(' (col 2); `mm` lands on the matching ')' (col 7).
    feed(&rpc, "llmm");
    assert_eq!(cursor(&rpc).await, (1, 7), "jumped to the matching bracket");
}

/// `mi(` selects the text *inside* the innermost parentheses at the cursor.
#[tokio::test]
async fn match_inside_pair_selects_the_content() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    // Put the head inside the parens, then `mi(` selects "a, b" (cols 3..=6).
    let map = redraw_after(&rpc, &mut incoming, "llllmi(").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((3, 7)),
        "the inner text is selected, excluding the brackets",
    );
}

/// `ma(` selects *around* the parentheses — the content plus both brackets.
#[tokio::test]
async fn match_around_pair_includes_the_brackets() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    let map = redraw_after(&rpc, &mut incoming, "llllma(").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((2, 8)),
        "the selection spans the '(' through the ')'",
    );
}

/// `miw` selects the word under the cursor (an inner-word text object).
#[tokio::test]
async fn match_inside_word_selects_the_word() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    let map = redraw_after(&rpc, &mut incoming, "miw").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 5)),
        "the whole word 'hello' is selected",
    );
}

/// `ms{char}` wraps the selection with a delimiter pair. As in Helix, the inserted
/// delimiters become *part of* the selection (the head lands on the closer).
#[tokio::test]
async fn surround_add_wraps_the_selection_including_delimiters() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ihello world<Esc>0:helix<CR>");
    // `w` selects "hello " (cols 0..=5); `ms(` wraps it in parens.
    let map = redraw_after(&rpc, &mut incoming, "wms(").await;
    assert_eq!(
        lines(&rpc).await,
        vec!["(hello )world"],
        "the selection is wrapped in the delimiter pair",
    );
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((0, 8)),
        "the whole '(hello )' — brackets included — stays selected",
    );
}

/// `ms` wraps *every* selection, not just the primary, and the delimiters land in
/// each selection — a following `d` deletes the whole wrapped span on both lines.
#[tokio::test]
async fn surround_add_wraps_all_selections() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ixx<CR>yy<Esc>:helix<CR>k0");
    // Select "xx" on line 0, copy the selection down to line 1 (`C`), wrap both.
    feed(&rpc, "wCms(");
    assert_eq!(
        lines(&rpc).await,
        vec!["(xx)", "(yy)"],
        "each of the two selections was wrapped",
    );
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["", ""],
        "both selections covered their whole '(..)' wrap, delimiters included",
    );
}

/// `md` deletes the surrounding pair around *every* selection, restoring each
/// original selection (a following `d` removes just the originally-selected char).
#[tokio::test]
async fn surround_delete_removes_all_pairs() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "i(a)<CR>(b)<Esc>:helix<CR>k0l");
    // Point selection on 'a', copy down to 'b', strip the parens on both lines.
    feed(&rpc, "Cmd(");
    assert_eq!(
        lines(&rpc).await,
        vec!["a", "b"],
        "both surrounding pairs were removed",
    );
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["", ""],
        "each selection was restored onto its original char",
    );
}

/// `mr` replaces the surrounding pair around *every* selection, restoring each
/// original selection.
#[tokio::test]
async fn surround_replace_swaps_all_pairs() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "i(a)<CR>(b)<Esc>:helix<CR>k0l");
    feed(&rpc, "Cmr([");
    assert_eq!(
        lines(&rpc).await,
        vec!["[a]", "[b]"],
        "both pairs became square brackets",
    );
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["[]", "[]"],
        "each selection was restored onto its original char, not the new brackets",
    );
}

/// `md`/`mr` restore the *original* selection rather than jumping to the inner
/// content: after stripping the parens around a one-char selection inside "(abc)",
/// a following `d` deletes only that char.
#[tokio::test]
async fn surround_delete_keeps_the_original_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "i(abc)<Esc>:helix<CR>0ll");
    // Point selection on 'b'; `md(` strips the parens, keeping the selection on 'b'.
    feed(&rpc, "md(");
    assert_eq!(lines(&rpc).await, vec!["abc"], "the parens were removed");
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["ac"],
        "only the originally-selected 'b' was deleted, not the whole inner content",
    );
}

/// `md`/`mr` work with *any* delimiter `ms` can add — not just brackets/quotes —
/// by scanning outward for the nearest occurrence on each side.
#[tokio::test]
async fn surround_arbitrary_delimiter_add_delete_replace() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ihello<Esc>:helix<CR>0");
    // Wrap the whole word with '*', then swap the pair to '/', then delete it.
    feed(&rpc, "%ms*");
    assert_eq!(lines(&rpc).await, vec!["*hello*"], "wrapped with '*'");
    feed(&rpc, "mr*/");
    assert_eq!(
        lines(&rpc).await,
        vec!["/hello/"],
        "'*' pair replaced with '/'"
    );
    feed(&rpc, "md/");
    assert_eq!(lines(&rpc).await, vec!["hello"], "'/' pair deleted");
}

/// `md{char}` deletes the surrounding delimiter pair, keeping the inner content.
#[tokio::test]
async fn surround_delete_removes_the_pair() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    // Head inside the parens; `md(` strips the brackets.
    feed(&rpc, "llllmd(");
    assert_eq!(
        lines(&rpc).await,
        vec!["fna, b"],
        "both brackets were removed, the content kept",
    );
}

/// `mr{from}{to}` replaces a surrounding pair with a different one.
#[tokio::test]
async fn surround_replace_swaps_the_pair() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    feed(&rpc, "llllmr([");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn[a, b]"],
        "the parentheses became square brackets",
    );
}

/// `mr` is two-stage: after `mr{from}` (before the replacement char) the `{from}`
/// delimiters are highlighted and nothing is applied yet; the `{to}` key applies.
#[tokio::test]
async fn surround_replace_previews_the_delimiters() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    // `mr(` lights up the '(' (col 2) and ')' (col 7) — the pair that will change.
    let map = redraw_after(&rpc, &mut incoming, "llllmr(").await;
    assert_eq!(
        view_selection(&map).first().copied().flatten(),
        Some((2, 3)),
        "the opening delimiter is highlighted (primary)",
    );
    assert_eq!(
        view_secondary_selection(&map)[0],
        vec![(7, 8)],
        "the closing delimiter is highlighted (secondary)",
    );
    assert_eq!(
        lines(&rpc).await,
        vec!["fn(a, b)"],
        "nothing is applied until the replacement char is typed",
    );
    // The `{to}` key applies the swap.
    feed(&rpc, "[");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn[a, b]"],
        "the pair was replaced once the second key arrived",
    );
}

/// `<Esc>` after `mr{from}` (mid-preview) cancels and restores the original
/// selection rather than leaving the delimiters highlighted.
#[tokio::test]
async fn surround_replace_escape_restores_original_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    // Point selection on ',' (col 4); preview the parens, then cancel.
    feed(&rpc, "llllmr(<Esc>");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn(a, b)"],
        "the cancel touched nothing",
    );
    // The original one-char selection is back: `d` removes just the ','.
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec!["fn(a b)"],
        "the original selection was restored on cancel",
    );
}

/// A `<Esc>` mid-sequence cancels match mode without touching the buffer.
#[tokio::test]
async fn escape_cancels_match_mode() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifn(a, b)<Esc>0:helix<CR>");
    feed(&rpc, "m<Esc>d");
    assert_eq!(
        lines(&rpc).await,
        vec!["n(a, b)"],
        "match mode was cancelled; the later `d` deleted the char under the cursor",
    );
}
