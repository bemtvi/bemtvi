//! Tier 1: the wide-glyph / emoji mask, tested as the pure function the renderer
//! uses. Black-box, no shaping, no GPU — the GUI analogue of the `syntax`/`inlay`
//! Tier-1 tests. (Which clusters are "off-grid" is decided by actual shaping at
//! runtime; here we hand `mask_segments` the byte ranges directly and check it holds
//! the grid by substituting cell-width spaces.)

use nxvim_gui::{mask_segments, Seg};

fn seg(text: &str, fg: u32) -> Seg {
    Seg::plain(text.to_string(), fg)
}

#[test]
fn masks_an_emoji_with_its_cell_width_in_spaces() {
    // "a😀b": the emoji is bytes 1..5 (4 UTF-8 bytes) and is two cells wide, so it
    // becomes two spaces — the surrounding "a"/"b" keep their columns.
    let out = mask_segments(&[seg("a😀b", 0xfff)], &[(1, 5)]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "a  b");
    assert_eq!(out[0].fg, 0xfff);
}

#[test]
fn masks_multiple_clusters_in_one_run() {
    // "x😀y😀z": two emoji (bytes 1..5 and 6..10), each → two spaces.
    let out = mask_segments(&[seg("x😀y😀z", 1)], &[(1, 5), (6, 10)]);
    assert_eq!(out[0].text, "x  y  z");
}

#[test]
fn mask_is_per_segment_and_preserves_style() {
    // The bad range spans concatenated coordinates: seg0 "a😀" is bytes 0..5, seg1
    // "b" is 5..6. The emoji (1..5) lives entirely in seg0, which becomes "a  ";
    // seg1 is untouched. Each segment keeps its own color/weight.
    let mut bold = seg("a😀", 0x111);
    bold.bold = true;
    let out = mask_segments(&[bold, seg("b", 0x222)], &[(1, 5)]);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].text, "a  ");
    assert!(out[0].bold);
    assert_eq!(out[0].fg, 0x111);
    assert_eq!(out[1].text, "b");
    assert_eq!(out[1].fg, 0x222);
}

#[test]
fn no_bad_ranges_returns_segments_unchanged() {
    // A pure-ASCII (all-narrow) line — the common case — is passed through verbatim.
    let out = mask_segments(&[seg("hello", 7)], &[]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].text, "hello");
}

#[test]
fn masks_a_wide_cjk_range_as_two_spaces_each() {
    // A range covering "你好" (each char 3 bytes, two cells) → four spaces. (In
    // practice CJK snaps and isn't masked, but the width accounting must still hold.)
    let out = mask_segments(&[seg("你好", 9)], &[(0, 6)]);
    assert_eq!(out[0].text, "    "); // 2 chars × 2 cells = 4 spaces
}
