//! Tier 1: the pure "recolor glyphs under a search match" layer the GUI renderer
//! feeds each text row through — applying the theme's `Search` / `IncSearch`
//! *foreground* to the glyphs a match covers, the GUI analogue of the TUI's search
//! highlight. Black-box, no window, no GPU. Guards the regression where the GUI
//! painted only the search *background* and kept each glyph's syntax fg, so a
//! `Search` group written dark-on-bright (or the current-match `IncSearch`) rendered
//! light-on-bright and was nearly invisible. The painted frame itself needs a GPU;
//! this covers the recolor the paint depends on.

use nxvim_gui::{apply_search_fg, Seg};

const FG: u32 = 0xc0_c0_c0;
const SEARCH_FG: u32 = 0x11_11_11;
const INC_FG: u32 = 0x22_22_22;

/// A single plain run over the whole row, so a recolor splits it by column.
fn row(text: &str) -> Vec<Seg> {
    vec![Seg::plain(text.into(), FG)]
}

/// The fg of the run whose text is `needle` (panics if the split didn't produce it).
fn fg_of(segs: &[Seg], needle: &str) -> u32 {
    segs.iter().find(|s| s.text == needle).unwrap().fg
}

/// The visible string the segments lay out left to right (recolor must not drop text).
fn text(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn a_match_takes_the_search_foreground_and_the_rest_keeps_its_own() {
    // "the theme" with a match on the leading "the" [0,3).
    let out = apply_search_fg(
        row("the theme"),
        &[(0, 3)],
        None,
        0,
        Some(SEARCH_FG),
        Some(INC_FG),
    );
    assert_eq!(text(&out), "the theme", "no text lost");
    assert_eq!(fg_of(&out, "the"), SEARCH_FG, "matched run recolored");
    assert_eq!(fg_of(&out, " theme"), FG, "unmatched run keeps its fg");
}

#[test]
fn the_incsearch_current_match_wins_over_hlsearch_on_a_shared_cell() {
    // Both a plain hlsearch span and the incsearch preview cover [0,3); the current
    // match's color must win.
    let out = apply_search_fg(
        row("the x"),
        &[(0, 3)],
        Some((0, 3)),
        0,
        Some(SEARCH_FG),
        Some(INC_FG),
    );
    assert_eq!(fg_of(&out, "the"), INC_FG, "incsearch (current match) wins");
}

#[test]
fn a_none_foreground_leaves_the_glyphs_untouched() {
    // A `Search` group with no fg set: only the bg quad paints (elsewhere), the
    // glyphs keep their syntax color. Nothing to recolor → the input is returned.
    let out = apply_search_fg(row("the x"), &[(0, 3)], None, 0, None, None);
    assert_eq!(out.len(), 1, "untouched: still one run");
    assert_eq!(fg_of(&out, "the x"), FG);
}

#[test]
fn spans_are_read_in_the_pre_scroll_column_space() {
    // Under leftcol=4 the base run covers columns [4, …). A match at [4,7) lands on
    // the run's first three chars.
    let out = apply_search_fg(row("match"), &[(4, 7)], None, 4, Some(SEARCH_FG), None);
    assert_eq!(text(&out), "match");
    assert_eq!(fg_of(&out, "mat"), SEARCH_FG);
    assert_eq!(fg_of(&out, "ch"), FG);
}

#[test]
fn multiple_matches_on_a_row_each_recolor() {
    // "e" at columns 2 and 6 of "the theme": two 1-wide matches.
    let out = apply_search_fg(
        row("the theme"),
        &[(2, 3), (6, 7)],
        None,
        0,
        Some(SEARCH_FG),
        None,
    );
    assert_eq!(text(&out), "the theme");
    // Both 'e's carry the search fg; the surrounding text does not.
    let colored: usize = out
        .iter()
        .filter(|s| s.fg == SEARCH_FG)
        .map(|s| s.text.len())
        .sum();
    assert_eq!(colored, 2, "exactly the two matched cells are recolored");
}
