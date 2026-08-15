//! Tier 1: how the GUI paints the text cursor over a glyph. Black-box, no window,
//! no GPU — the pure colour choice and the pure recolor of the covered grapheme.
//!
//! Guards the regression this file was written for: the block cursor was a
//! *translucent* foreground-coloured quad laid over the glyph, on the theory that
//! the glyph should show through. It doesn't read as a cursor — on a dark theme a
//! 50%-opacity light block over light text is low contrast both ways, and the glyph
//! under it is the one thing you most need to see. Every terminal, and bemtvi's own
//! web client, does the opposite: an opaque block with the glyph re-drawn inverted
//! on top. That is what these tests pin.

use bemtvi_gui::{apply_cursor_fg, block_cursor_colors, Seg};

const NORMAL_FG: u32 = 0xab_b2_bf; // One Dark text
const NORMAL_BG: u32 = 0x28_2c_34; // One Dark background

fn seg(text: &str, fg: u32) -> Seg {
    Seg::plain(text.to_string(), fg)
}

fn texts(segs: &[Seg]) -> Vec<(&str, u32)> {
    segs.iter().map(|s| (s.text.as_str(), s.fg)).collect()
}

// ---------------------------------------------------------------------------
// `block_cursor_colors(theme_fg, theme_bg, normal_fg, normal_bg)` -> (block, glyph)
// ---------------------------------------------------------------------------

#[test]
fn an_unthemed_cursor_is_reverse_video() {
    // No `Cursor` group: the block takes the text colour and the glyph the
    // background — the cell inverted, which is what a terminal draws and what the
    // web client's `.cur-block` rule already did.
    let (block, glyph) = block_cursor_colors(None, None, NORMAL_FG, NORMAL_BG);
    assert_eq!(block, NORMAL_FG);
    assert_eq!(glyph, NORMAL_BG);
}

#[test]
fn a_themed_cursor_wins_both_halves() {
    // `hi Cursor guifg=#282c34 guibg=#528bff` — the colorscheme's own cursor colour.
    let (block, glyph) =
        block_cursor_colors(Some(0x28_2c_34), Some(0x52_8b_ff), NORMAL_FG, NORMAL_BG);
    assert_eq!(block, 0x52_8b_ff);
    assert_eq!(glyph, 0x28_2c_34);
}

#[test]
fn a_half_themed_cursor_fills_the_other_half_from_normal() {
    // A theme that sets only the block colour still gets a readable glyph, and one
    // that sets only the glyph colour still gets a block. (`hi Cursor gui=reverse`
    // sets neither and lands on the reverse-video case above — same result.)
    let (block, glyph) = block_cursor_colors(None, Some(0x52_8b_ff), NORMAL_FG, NORMAL_BG);
    assert_eq!((block, glyph), (0x52_8b_ff, NORMAL_BG));
    let (block, glyph) = block_cursor_colors(Some(0xff_00_00), None, NORMAL_FG, NORMAL_BG);
    assert_eq!((block, glyph), (NORMAL_FG, 0xff_00_00));
}

#[test]
fn the_cursor_never_paints_itself_invisible() {
    // The one combination that must not survive: a block the same colour as the
    // glyph on it. A theme whose `Cursor` sets fg == bg (or matches `Normal` exactly)
    // would render an unreadable cell, so the glyph falls back to the inverse.
    let (block, glyph) =
        block_cursor_colors(Some(0x52_8b_ff), Some(0x52_8b_ff), NORMAL_FG, NORMAL_BG);
    assert_ne!(block, glyph, "an invisible cursor glyph");
}

// ---------------------------------------------------------------------------
// `apply_cursor_fg(segments, leftcol, cursor_col, width, fg)` — re-colour just the
// grapheme(s) the block covers, in the same pre-splice column space the search
// spans use.
// ---------------------------------------------------------------------------

const CUR: u32 = 0x28_2c_34;

#[test]
fn recolors_only_the_glyph_under_the_cursor() {
    let out = apply_cursor_fg(vec![seg("hello", NORMAL_FG)], 0, 2, 1, CUR);
    assert_eq!(
        texts(&out),
        vec![("he", NORMAL_FG), ("l", CUR), ("lo", NORMAL_FG)]
    );
}

#[test]
fn covers_the_whole_of_a_wide_grapheme() {
    // The cursor envelops a two-cell CJK glyph (`cursor_width` is 2), and the glyph
    // is one cluster: it inverts as a unit, not half of it.
    let out = apply_cursor_fg(vec![seg("a沖b", NORMAL_FG)], 0, 1, 2, CUR);
    assert_eq!(
        texts(&out),
        vec![("a", NORMAL_FG), ("沖", CUR), ("b", NORMAL_FG)]
    );
}

#[test]
fn counts_columns_from_leftcol_when_scrolled() {
    // `row_segments` has already dropped the columns scrolled off the left, so the
    // walk starts at `leftcol` — the same convention `apply_search_fg` follows. With
    // the line scrolled 10 columns, cursor column 12 is the third visible glyph.
    let out = apply_cursor_fg(vec![seg("abcde", NORMAL_FG)], 10, 12, 1, CUR);
    assert_eq!(
        texts(&out),
        vec![("ab", NORMAL_FG), ("c", CUR), ("de", NORMAL_FG)]
    );
}

#[test]
fn keeps_each_run_s_own_style_around_the_cursor() {
    // The recolor splits one syntax run and leaves the others alone — bold/italic and
    // every other colour on the row survive it.
    let out = apply_cursor_fg(
        vec![seg("let ", 0xc6_78_dd), seg("x", 0xe0_6c_75)],
        0,
        4,
        1,
        CUR,
    );
    assert_eq!(texts(&out), vec![("let ", 0xc6_78_dd), ("x", CUR)]);
}

#[test]
fn a_cursor_past_the_end_of_the_line_recolors_nothing() {
    // On the virtual cell after the last character (`$` in insert, an empty line):
    // there is no glyph to invert, and the row must come back untouched.
    let out = apply_cursor_fg(vec![seg("hi", NORMAL_FG)], 0, 2, 1, CUR);
    assert_eq!(texts(&out), vec![("hi", NORMAL_FG)]);
}

#[test]
fn an_empty_row_is_returned_as_is() {
    assert!(apply_cursor_fg(Vec::new(), 0, 0, 1, CUR).is_empty());
}
