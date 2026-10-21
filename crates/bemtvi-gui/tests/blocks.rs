//! Tier 1: block elements (`▀`, `▄`, `█`, …) are drawn as rectangles filling their
//! cells, not as font glyphs. The cell is taller than the font's em (the line spacing),
//! so a glyph's half block covers half the em and pixel art made of them showed seams
//! and offsets between rows. Black-box, no window, no GPU: the geometry and the split
//! of a styled run into shaped text plus drawn blocks.

use bemtvi_gui::{block_element_rects, split_block_elements, Seg};

/// `c`'s rectangles, rounded to make the fractions easy to compare.
fn rects(c: char) -> Vec<(f32, f32, f32, f32, f32)> {
    block_element_rects(c)
}

#[test]
fn half_blocks_fill_exactly_half_the_cell() {
    assert_eq!(rects('▀'), vec![(0.0, 0.0, 1.0, 0.5, 1.0)]);
    assert_eq!(rects('▄'), vec![(0.0, 0.5, 1.0, 1.0, 1.0)]);
    assert_eq!(rects('▌'), vec![(0.0, 0.0, 0.5, 1.0, 1.0)]);
    assert_eq!(rects('▐'), vec![(0.5, 0.0, 1.0, 1.0, 1.0)]);
}

#[test]
fn the_full_block_and_the_eighths_reach_the_cell_edges() {
    assert_eq!(rects('█'), vec![(0.0, 0.0, 1.0, 1.0, 1.0)]);
    assert_eq!(rects('▁'), vec![(0.0, 0.875, 1.0, 1.0, 1.0)]);
    assert_eq!(rects('▏'), vec![(0.0, 0.0, 0.125, 1.0, 1.0)]);
    assert_eq!(rects('▔'), vec![(0.0, 0.0, 1.0, 0.125, 1.0)]);
    assert_eq!(rects('▕'), vec![(0.875, 0.0, 1.0, 1.0, 1.0)]);
}

#[test]
fn shades_fill_the_cell_at_their_density() {
    assert_eq!(rects('░'), vec![(0.0, 0.0, 1.0, 1.0, 0.25)]);
    assert_eq!(rects('▒'), vec![(0.0, 0.0, 1.0, 1.0, 0.5)]);
    assert_eq!(rects('▓'), vec![(0.0, 0.0, 1.0, 1.0, 0.75)]);
}

#[test]
fn quadrants_fill_their_quarters() {
    // ▚: upper left and lower right.
    assert_eq!(
        rects('▚'),
        vec![(0.0, 0.0, 0.5, 0.5, 1.0), (0.5, 0.5, 1.0, 1.0, 1.0)]
    );
    // ▟: all but the upper left.
    assert_eq!(rects('▟').len(), 3);
}

#[test]
fn every_block_element_draws_and_nothing_else_does() {
    for c in '\u{2580}'..='\u{259F}' {
        assert!(!rects(c).is_empty(), "U+{:04X} draws nothing", c as u32);
    }
    for c in ['a', ' ', '─', '│', '■', '\u{257F}', '\u{25A0}'] {
        assert!(rects(c).is_empty(), "{c:?} is not a block element");
    }
}

#[test]
fn a_run_shapes_its_blocks_as_spaces_and_draws_them_in_their_cells() {
    // A wide character before a block moves it by two cells.
    let segs = vec![
        Seg::plain("a▀".to_string(), 0x111111),
        Seg::plain("世▄b".to_string(), 0x222222),
    ];
    let (shaped, blocks) = split_block_elements(&segs);
    let texts: Vec<&str> = shaped.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["a ", "世 b"],
        "blocks become spaces, columns kept"
    );
    assert_eq!(blocks, vec![(1, '▀', 0x111111), (4, '▄', 0x222222)]);
}

#[test]
fn a_run_without_blocks_is_untouched() {
    let segs = vec![Seg::plain("plain text".to_string(), 0x111111)];
    let (shaped, blocks) = split_block_elements(&segs);
    assert_eq!(shaped[0].text, "plain text");
    assert!(blocks.is_empty());
}
