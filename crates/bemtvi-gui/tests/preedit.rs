//! Tier 1: an input method's composition (the kana typed so far, before it's
//! committed) as inline virtual text. The GUI adds it to the cursor row as one more
//! inline `virt_text` placement at the cursor, so the row's splice lays it out in
//! its own cells and every column-keyed overlay to its right shifts past it; on the
//! command line it splits the typed text at the caret. Black-box, no window, no
//! GPU: the pure helpers the painted frame is built from.

use bemtvi_gui::{seg_bg_spans, split_at_char, virt_inline_shift, with_preedit, Seg};
use bemtvi_view::VirtPlacement;

const INLINE: u8 = 1;

#[test]
fn the_composition_is_an_inline_placement_at_the_cursor() {
    let row = with_preedit(&[], 4, "にほ", Some(7));
    assert_eq!(
        row,
        vec![VirtPlacement {
            pos: INLINE,
            col: 4,
            hl_mode: 0,
            chunks: vec![("にほ".to_string(), Some(7))],
        }]
    );
}

#[test]
fn the_composition_pushes_the_text_after_the_cursor_right() {
    // "にほ" is four cells wide, composed at column 4.
    let row = with_preedit(&[], 4, "にほ", None);
    // The glyph that was under the cursor, and everything after it, moves right by
    // the composition's width; the text before the cursor stays put.
    assert_eq!(virt_inline_shift(&row, 0, 4, true), 4);
    assert_eq!(virt_inline_shift(&row, 0, 9, true), 4);
    assert_eq!(virt_inline_shift(&row, 0, 3, true), 0);
    // A span ending at the cursor (a selection up to it) isn't stretched over it.
    assert_eq!(virt_inline_shift(&row, 0, 4, false), 0);
}

#[test]
fn the_composition_sits_after_an_extmarks_inline_text_at_the_cursor() {
    // An extmark's inline text at the cursor column is drawn before the cursor, so
    // the composition, which starts at the cursor, comes after it.
    let mark = VirtPlacement {
        pos: INLINE,
        col: 4,
        hl_mode: 0,
        chunks: vec![("ab".to_string(), None)],
    };
    let row = with_preedit(std::slice::from_ref(&mark), 4, "x", None);
    assert_eq!(row.len(), 2);
    assert_eq!(row[0], mark);
    assert_eq!(row[1].chunks[0].0, "x");
    // Both push the glyph at the cursor.
    assert_eq!(virt_inline_shift(&row, 0, 4, true), 3);
}

#[test]
fn the_command_line_splits_at_the_caret_counted_in_chars() {
    assert_eq!(split_at_char("e foo", 2), ("e ", "foo"));
    // The caret counts chars, not bytes: past a two-byte `é`.
    assert_eq!(split_at_char("éa", 1), ("é", "a"));
    // At, or past, the end: everything is before it.
    assert_eq!(split_at_char("ab", 2), ("ab", ""));
    assert_eq!(split_at_char("ab", 9), ("ab", ""));
}

/// A segment with background `bg` (a `BtvImePreedit` composition, a `Visual`-like
/// badge).
fn filled(text: &str, bg: u32) -> Seg {
    Seg {
        bg: Some(bg),
        ..Seg::plain(text.to_string(), 0xffffff)
    }
}

#[test]
fn a_wide_composition_is_filled_across_both_its_cells() {
    // "ab" then the composition "にほ" (two 2-cell chars), then "cd" with a fill.
    let segs = [
        Seg::plain("ab".to_string(), 0xffffff),
        filled("にほ", 0x112233),
        filled("cd", 0x445566),
    ];
    assert_eq!(
        seg_bg_spans(&segs, 10),
        vec![(12, 4, 0x112233), (16, 2, 0x445566)],
        "each wide char's background must cover its two cells, and what follows starts after them"
    );
}
