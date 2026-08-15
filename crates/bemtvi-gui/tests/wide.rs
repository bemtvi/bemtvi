//! Tier 1: the wide-glyph / emoji mask, tested as the pure function the renderer
//! uses. Black-box, no shaping, no GPU — the GUI analogue of the `syntax`/`inlay`
//! Tier-1 tests. (Which clusters are "off-grid" is decided by actual shaping at
//! runtime; here we hand `mask_segments` the byte ranges directly and check it holds
//! the grid by substituting cell-width spaces.)

use bemtvi_gui::{
    cluster_scale, is_letterform, italic_runs, mask_segments, offgrid_clusters, Ink, Seg,
};

fn seg(text: &str, fg: u32) -> Seg {
    Seg::plain(text.to_string(), fg)
}

// `offgrid_clusters(text, glyphs)` decides which grapheme clusters the renderer must
// mask to spaces (and redraw separately) because the font's shaped advance disagrees
// with the editor's display-width grid. `glyphs` is `(start_byte, advance_in_cells)`.

#[test]
fn flags_a_wide_grapheme_whose_glyph_snapped_narrow() {
    // "❤️" (U+2764 U+FE0F) is display-width 2, but a monospace font renders its base
    // as a single-cell dingbat and cosmic-text splits off the VS16 as a zero-advance
    // glyph. The cluster's total advance (1) disagrees with its width (2), so it must
    // be flagged — else the text after it slides a cell left of the 2-cell cursor.
    let heart = "\u{2764}\u{FE0F}"; // bytes 0..6
    let glyphs = [(0usize, 1.0f32), (3usize, 0.0f32)];
    assert_eq!(offgrid_clusters(heart, &glyphs), vec![(0, 6)]);
}

#[test]
fn flags_a_narrow_grapheme_whose_glyph_is_double_wide() {
    // The mirror case: a width-1 symbol (a Powerline separator, U+E0B0) that a fallback
    // font draws two cells wide. Advance 2 ≠ width 1 → flag it, so it doesn't shove the
    // rest of the line a cell right.
    let sep = "\u{E0B0}"; // bytes 0..3, display width 1
    let glyphs = [(0usize, 2.0f32)];
    assert_eq!(offgrid_clusters(sep, &glyphs), vec![(0, 3)]);
}

#[test]
fn leaves_a_correctly_snapped_wide_grapheme_inline() {
    // A CJK char whose glyph snapped to two cells matches its width — stays inline.
    let cjk = "\u{4F60}"; // 你, bytes 0..3, width 2
    let glyphs = [(0usize, 2.0f32)];
    assert!(offgrid_clusters(cjk, &glyphs).is_empty());
}

#[test]
fn leaves_ascii_inline() {
    let glyphs = [(0usize, 1.0f32), (1, 1.0), (2, 1.0)];
    assert!(offgrid_clusters("abc", &glyphs).is_empty());
}

#[test]
fn flags_an_emoji_with_a_fractional_advance() {
    // A colour-emoji glyph from a non-monospace font keeps its native (unsnapped)
    // advance — 1.6 cells here — which is neither its width (2) nor a whole number.
    let grin = "\u{1F600}"; // 😀, bytes 0..4, width 2
    let glyphs = [(0usize, 1.6f32)];
    assert_eq!(offgrid_clusters(grin, &glyphs), vec![(0, 4)]);
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

// ---------------------------------------------------------------------------
// `cluster_scale(max, box_w, box_h, Some((advance, ink)))` sizes one off-grid cluster
// to the cells the mask reserved for it. Every measurement below is a real one, taken
// by shaping these exact characters against the system fonts at 15pt (cell 9×20px).
// ---------------------------------------------------------------------------

/// The cell box at 15pt: 9px wide, 20px of line height.
const CELL_W: f32 = 9.0;
const CELL_H: f32 = 20.0;

fn ink(left: f32, width: f32, height: f32) -> Ink {
    Ink {
        left,
        width,
        height,
    }
}

#[test]
fn shrinks_a_nerd_icon_to_its_single_cell() {
    // `\u{e620}` from Symbols Nerd Font Mono: a full-em glyph — 15px of advance and ink
    // — in the ONE cell the editor reserves for a Private Use codepoint. At the bare
    // 1.2 ceiling it would paint 18px of ink over a 9px cell and collide with the next
    // character; fitted, it fills the cell exactly.
    let s = cluster_scale(1.2, CELL_W, CELL_H, Some((15.0, ink(0.0, 15.0, 15.0))));
    assert!((s - 0.6).abs() < 1e-4, "expected fit-to-cell 0.6, got {s}");
}

#[test]
fn leaves_a_color_emoji_at_the_ceiling() {
    // `🤴🏼` from Noto Color Emoji: 18.6px advance already ≈ its two reserved cells
    // (18px), so the width fit (18/18.6 ≈ 0.97) is not what binds — but neither does it
    // need shrinking below a legible size. The point of the ceiling is that a glyph
    // which already fits is not pulled around.
    let s = cluster_scale(
        1.2,
        2.0 * CELL_W,
        CELL_H,
        Some((18.6, ink(0.0, 18.0, 17.0))),
    );
    assert!((0.9..=1.2).contains(&s), "expected ≈1, got {s}");
}

#[test]
fn leaves_a_narrow_glyph_at_the_ceiling() {
    // `❤️` from DejaVu Sans Mono: 9px advance, 10px of ink, in a two-cell (18px)
    // reservation. It fits with room to spare, so the ceiling wins untouched — the
    // fit must never scale a glyph *up* past `emoji_scale`.
    let s = cluster_scale(1.2, 2.0 * CELL_W, CELL_H, Some((9.0, ink(0.0, 10.0, 8.0))));
    assert!((s - 1.2).abs() < 1e-4, "expected the 1.2 ceiling, got {s}");
}

#[test]
fn fits_cjk_to_its_two_cells() {
    // `縄` from Noto Sans CJK SC: 15px advance/ink in two reserved cells (18px). 1.2×
    // lands exactly on 18px, so the ceiling and the fit agree — CJK is unchanged.
    let s = cluster_scale(
        1.2,
        2.0 * CELL_W,
        CELL_H,
        Some((15.0, ink(0.0, 15.0, 15.0))),
    );
    assert!((s - 1.2).abs() < 1e-4, "expected 1.2, got {s}");
}

#[test]
fn counts_ink_that_overflows_the_advance() {
    // A glyph whose ink paints past its own advance (a swash, an overhanging icon) is
    // sized by the ink, not the advance: 9px advance but ink reaching 18px must halve
    // to sit inside one cell, where an advance-only fit would leave it double-wide.
    let s = cluster_scale(1.2, CELL_W, CELL_H, Some((9.0, ink(0.0, 18.0, 12.0))));
    assert!((s - 0.5).abs() < 1e-4, "expected ink-driven 0.5, got {s}");
}

#[test]
fn caps_a_tall_glyph_to_the_line_height() {
    // Height binds where width does not: 4px of advance/ink but 40px tall in a 20px
    // line. Unfitted it would overprint the rows above and below.
    let s = cluster_scale(1.2, CELL_W, CELL_H, Some((4.0, ink(0.0, 4.0, 40.0))));
    assert!(
        (s - 0.5).abs() < 1e-4,
        "expected height-driven 0.5, got {s}"
    );
}

#[test]
fn never_shrinks_a_glyph_away() {
    // A pathologically wide fallback (200px of ink in one cell) floors at 0.25 rather
    // than fitting to an invisible 0.045 — better an overlapping glyph than a missing one.
    let s = cluster_scale(1.2, CELL_W, CELL_H, Some((200.0, ink(0.0, 200.0, 15.0))));
    assert!((s - 0.25).abs() < 1e-4, "expected the 0.25 floor, got {s}");
}

#[test]
fn keeps_the_ceiling_for_an_unrasterised_glyph() {
    // No ink box (a blank or missing glyph): nothing to fit, so the configured ceiling
    // stands rather than some invented default.
    assert!((cluster_scale(1.2, CELL_W, CELL_H, None) - 1.2).abs() < 1e-4);
}

// ---------------------------------------------------------------------------
// `italic_runs(text, covers)` splits a styled segment so italic is asked for only on
// the characters the primary font can draw. `covers` stands in for the real font
// database: here, "ASCII is in the coding font, anything else falls back".
// ---------------------------------------------------------------------------

/// Stands in for the real font database: "the coding font has ASCII, nothing else".
/// Composed with `is_letterform` exactly as the renderer composes them.
fn ascii_only(cluster: &str) -> bool {
    is_letterform(cluster) && cluster.is_ascii()
}

#[test]
fn leaves_an_all_text_comment_as_one_italic_run() {
    // The common case — no icons, no CJK. One run, slanted, so shaping is unchanged
    // from before the split existed.
    assert_eq!(
        italic_runs("-- a plain comment", &ascii_only),
        vec![("-- a plain comment", true)]
    );
}

#[test]
fn splits_an_icon_out_of_an_italic_comment() {
    // `\u{e60b}` is a Nerd Font icon: it resolves to Symbols Nerd Font, which has no
    // italic, so it must not be slanted while the words around it are.
    assert_eq!(
        italic_runs("-- \u{e60b} init.lua", &ascii_only),
        vec![("-- ", true), ("\u{e60b}", false), (" init.lua", true)]
    );
}

#[test]
fn splits_a_wide_char_out_of_an_italic_comment() {
    // Same for CJK — the kanji come from a CJK fallback font and stay upright.
    assert_eq!(
        italic_runs("-- 沖縄県 done", &ascii_only),
        vec![("-- ", true), ("沖縄県", false), (" done", true)]
    );
}

#[test]
fn merges_adjacent_uncovered_chars_into_one_run() {
    // Consecutive fallback characters are one run, not one per char — the split exists
    // to change attrs, not to fragment shaping.
    assert_eq!(
        italic_runs("a😀😱b", &ascii_only),
        vec![("a", true), ("😀😱", false), ("b", true)]
    );
}

#[test]
fn handles_a_segment_that_is_entirely_uncovered() {
    // A comment that is nothing but icons: one upright run, no empty leading run.
    assert_eq!(
        italic_runs("\u{e60b}\u{e620}", &ascii_only),
        vec![("\u{e60b}\u{e620}", false)]
    );
}

#[test]
fn emits_nothing_for_empty_text() {
    assert!(italic_runs("", &ascii_only).is_empty());
}

#[test]
fn run_boundaries_are_char_aligned_not_byte_aligned() {
    // The split slices `text` by byte index, so a multi-byte boundary must land on a
    // char edge — reassembling the runs has to give the original string back.
    let text = "x沖y😀z";
    let runs = italic_runs(text, &ascii_only);
    let rejoined: String = runs.iter().map(|(t, _)| *t).collect();
    assert_eq!(rejoined, text);
    assert_eq!(runs.len(), 5);
}

// ---------------------------------------------------------------------------
// `is_letterform(cluster)` is the class gate that font coverage cannot supply: a coding
// font ships box-drawing and arrows, so coverage alone would let them lean.
// ---------------------------------------------------------------------------

#[test]
fn ascii_is_always_a_letterform() {
    // Letters and digits, but also the punctuation Unicode files under Symbol — `+ = <`
    // are `Sm`. They are ordinary code punctuation and must lean with the words around
    // them, or an italic comment renders with upright gaps in it.
    for c in [
        "a", "Z", "7", "-", ".", "+", "=", "<", ">", "|", "~", "$", "^", "`", " ",
    ] {
        assert!(is_letterform(c), "ASCII {c:?} should lean");
    }
}

#[test]
fn box_drawing_is_not_a_letterform() {
    // The reported case. These tile edge to edge — a skewed `├` no longer meets the `─`
    // beside it — and a coding font covers them, so only the class gate excludes them.
    for c in ["├", "└", "─", "│", "┌", "┘", "█", "▄", "░"] {
        assert!(!is_letterform(c), "box-drawing {c:?} must stay upright");
    }
}

#[test]
fn symbols_and_icons_are_not_letterforms() {
    // Arrows, geometric shapes, dingbats, and the private-use range Nerd Fonts live in.
    for c in [
        "→",
        "★",
        "▶",
        "❤",
        "✓",
        "°",
        "\u{e60b}",
        "\u{e0b3}",
        "\u{f0219}",
    ] {
        assert!(!is_letterform(c), "symbol {c:?} must stay upright");
    }
}

#[test]
fn non_ascii_letters_are_letterforms() {
    // The gate must not turn into "ASCII only" — a comment in French, German, Greek or
    // Russian still leans, and those letters do have real italic forms.
    for c in ["á", "ß", "ç", "ø", "λ", "д", "ñ"] {
        assert!(is_letterform(c), "letter {c:?} should lean");
    }
}

#[test]
fn wide_clusters_are_never_letterforms() {
    // Han is alphabetic, so only the width test excludes it — and it must be excluded:
    // a double-width glyph comes from a fallback with no italic, and slanting it breaks
    // the cell grid.
    for c in ["沖", "縄", "お", "＋", "😀"] {
        assert!(!is_letterform(c), "wide {c:?} must stay upright");
    }
}

#[test]
fn an_empty_cluster_is_not_a_letterform() {
    assert!(!is_letterform(""));
}

#[test]
fn splits_box_drawing_out_of_an_italic_comment() {
    // End to end through the splitter, with the tree-drawing comment that prompted this:
    // the words lean, the box-drawing character between them does not.
    assert_eq!(
        italic_runs("-- ├ init.lua", &ascii_only),
        vec![("-- ", true), ("├", false), (" init.lua", true)]
    );
}

#[test]
fn a_grapheme_with_combining_marks_stays_one_run() {
    // The walk is per grapheme, not per char: splitting `e` from its combining acute
    // would hand shaping two runs that can no longer compose into one glyph.
    let text = "cafe\u{301}";
    let runs = italic_runs(text, &|_| true);
    assert_eq!(runs, vec![(text, true)]);
}
