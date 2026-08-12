//! Helix editing model — Phase 4c: selection-regex verbs + paste.
//!
//! `s`/`S`/`K`/`Alt-K` open a regex prompt (a `/`-style command line) that
//! transforms the selection set: `s` selects one range per match within each
//! selection, `S` splits on the regex, `K`/`Alt-K` keep/remove selections that
//! contain a match. `p`/`P` paste after/before the selection. Tests drive the
//! prompt with `<CR>` and assert on the resulting buffer after a following verb.

use crate::support::*;

/// Per visible row, the screen-column spans of the `search` highlight channel —
/// what the Helix selection-regex prompt lights up as a live preview.
fn search_spans(view: &[(Value, Value)]) -> Vec<Vec<(u64, u64)>> {
    view_get(view, "search")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    row.as_array()
                        .map(|spans| {
                            spans
                                .iter()
                                .filter_map(|v| {
                                    let p = v.as_array()?;
                                    Some((p.first()?.as_u64()?, p.get(1)?.as_u64()?))
                                })
                                .collect()
                        })
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `s` replaces the selection with one selection per regex match; a following `d`
/// then deletes every match.
#[tokio::test]
async fn select_regex_then_delete_all_matches() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>0:helix<CR>");
    // Select the whole line, then select-within on "foo" → two selections; delete.
    feed(&rpc, "xsfoo<CR>d");
    assert_eq!(
        lines(&rpc).await,
        vec![" bar "],
        "both `foo` matches were selected and deleted",
    );
}

/// While typing the `s` pattern, the prompt previews the matches *within* the
/// selection live (the would-be new selections), via the search highlight channel.
#[tokio::test]
async fn select_regex_previews_matches_while_typing() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>0:helix<CR>");
    // Select the whole line, open the `s` prompt, and type "foo" *without* <CR>.
    let map = redraw_after(&rpc, &mut incoming, "xsfoo").await;
    assert_eq!(
        search_spans(&map).first().cloned().unwrap_or_default(),
        vec![(0, 3), (8, 11)],
        "both `foo` matches inside the selection are previewed live",
    );
    // A narrower pattern updates the preview on the next keystroke.
    let map2 = redraw_after(&rpc, &mut incoming, "<BS>").await;
    assert_eq!(
        search_spans(&map2).first().cloned().unwrap_or_default(),
        vec![(0, 2), (8, 10)],
        "the preview follows the pattern as it is edited",
    );
}

/// The `s` preview is clipped to the selection: a match *outside* the selection is
/// not previewed.
#[tokio::test]
async fn select_regex_preview_is_clipped_to_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>0:helix<CR>");
    // Select only the first word ("foo", cols 0..=2) via a word motion, then type
    // the `s` pattern — only the in-selection `foo` previews, not the trailing one.
    let map = redraw_after(&rpc, &mut incoming, "esfoo").await;
    assert_eq!(
        search_spans(&map).first().cloned().unwrap_or_default(),
        vec![(0, 3)],
        "only the match inside the selection is previewed",
    );
}

/// `S` splits the selection on the regex — the gaps between matches become the new
/// selections.
#[tokio::test]
async fn split_regex_then_delete_all_parts() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ia,b,c<Esc>0:helix<CR>");
    // Split the whole line on "," → selections "a","b","c"; deleting all leaves the
    // separators.
    feed(&rpc, "xS,<CR>d");
    assert_eq!(
        lines(&rpc).await,
        vec![",,"],
        "the parts between commas were deleted"
    );
}

/// `K` keeps only the selections that match; the rest are dropped.
#[tokio::test]
async fn keep_matching_selections() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>0:helix<CR>");
    // Split into words on space (foo/bar/foo), keep only those matching "foo",
    // then delete → only the two `foo`s go.
    feed(&rpc, "xS <CR>Kfoo<CR>d");
    assert_eq!(
        lines(&rpc).await,
        vec![" bar "],
        "only the matching selections were deleted"
    );
}

/// `Alt-K` (`<A-S-k>`) removes the selections that match, keeping the rest.
#[tokio::test]
async fn remove_matching_selections() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>0:helix<CR>");
    // Split into words, remove those matching "foo" (keeping "bar"), then delete.
    feed(&rpc, "xS <CR><A-S-k>foo<CR>d");
    assert_eq!(
        lines(&rpc).await,
        vec!["foo  foo"],
        "only the non-matching `bar` was deleted"
    );
}

/// An invalid pattern is reported (E383), not silently applied, and the selection
/// set is left intact.
#[tokio::test]
async fn invalid_regex_reports_and_keeps_selection() {
    let (rpc, mut incoming) = start(None).await;
    feed(&rpc, "ifoo bar foo<Esc>0:helix<CR>x");
    let map = redraw_after(&rpc, &mut incoming, "s(<CR>").await;
    assert!(
        view_str(&map, "message").contains("E383"),
        "an invalid pattern reports loudly, got: {:?}",
        view_str(&map, "message"),
    );
    // The whole-line selection survives — a following `d` still deletes it all.
    feed(&rpc, "d");
    assert_eq!(
        lines(&rpc).await,
        vec![""],
        "the selection was untouched by the failed regex"
    );
}

/// `p` pastes the register after the selection.
#[tokio::test]
async fn paste_after_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iabc<Esc>0:helix<CR>");
    // Select the line, yank it, then paste after → the text is duplicated.
    feed(&rpc, "xyp");
    assert_eq!(
        lines(&rpc).await,
        vec!["abcabc"],
        "the yanked selection was pasted after it"
    );
}

/// With multiple selections, each pastes its *own* slice from the last multi-yank
/// (not a broadcast of one register) — the distinguishing per-selection behavior.
#[tokio::test]
async fn multi_paste_is_per_selection() {
    let (rpc, _i) = start(None).await;
    feed(&rpc, "iab<CR>cd<Esc>gg0:helix<CR>");
    // A selection at col 0 of each line (point at 'a' / 'c'); yank captures each
    // one's own char, then paste after duplicates each in place.
    feed(&rpc, "Cyp");
    assert_eq!(
        lines(&rpc).await,
        vec!["aab", "ccd"],
        "line 1 pasted its own 'a' and line 2 its own 'c' (not a broadcast)",
    );
}
