//! Tier 1: the pure per-row syntax-coloring layer the GUI renderer feeds each text
//! row through — splitting a row into colored runs from its highlight spans, and
//! the built-in capture-group fallback that colors a buffer when no colorscheme is
//! loaded. Black-box, no window, no GPU. This guards the regression where the GUI
//! showed *no* syntax highlighting without a colorscheme (it ignored the span's
//! group name, unlike the TUI's `group_style`).

use nxvim_gui::{group_fallback, row_segments, Seg};
use nxvim_view::{HlSpan, Style};

const FG: u32 = 0xc0_c0_c0; // the renderer's DEFAULT_FG stand-in
const BG: u32 = 0x10_10_10;

/// The fg color of the run whose text is `needle`.
fn color_of(segs: &[Seg], needle: &str) -> u32 {
    segs.iter().find(|s| s.text == needle).unwrap().fg
}

/// The run whose text is `needle`.
fn seg_of<'a>(segs: &'a [Seg], needle: &str) -> &'a Seg {
    segs.iter().find(|s| s.text == needle).unwrap()
}

#[test]
fn group_fallback_colors_keywords_strings_and_comments_distinctly() {
    // Major component keys the color; comments are italic.
    assert_ne!(group_fallback("keyword", FG).0, FG);
    assert_ne!(group_fallback("string", FG).0, FG);
    assert_ne!(group_fallback("function.call", FG).0, FG); // sub-capture → major "function"
    let (comment, italic) = group_fallback("comment.line", FG);
    assert_ne!(comment, FG);
    assert!(italic, "comments render italic");
    // Three different families get three different colors.
    let kw = group_fallback("keyword", FG).0;
    let st = group_fallback("string", FG).0;
    let ty = group_fallback("type", FG).0;
    assert!(kw != st && st != ty && kw != ty);
    // An unknown group keeps the default fg (no spurious color).
    assert_eq!(group_fallback("definitely.not.a.group", FG), (FG, false));
}

#[test]
fn row_without_colorscheme_still_colors_spans_from_their_group() {
    // The regression: a span with no resolved `style_id` (empty `styles`) must
    // still be colored from its capture group, not painted in the default fg.
    let line = "let x";
    let hl: Vec<HlSpan> = vec![(0, 3, "keyword".into(), None)]; // "let"
    let segs = row_segments(line, &hl, &[], FG, BG, 0);
    let keyword_color = group_fallback("keyword", FG).0;
    assert_eq!(color_of(&segs, "let"), keyword_color);
    assert_ne!(color_of(&segs, "let"), FG, "keyword must not be default fg");
    // Text outside any span stays default fg.
    assert_eq!(color_of(&segs, " x"), FG);
}

#[test]
fn a_resolved_colorscheme_style_wins_over_the_group_fallback() {
    // When the server interned a style for the span (colorscheme loaded), the run
    // takes that style's fg — the fallback only fills in when `style_id` is None.
    let styles = vec![Style {
        fg: Some(0x11_22_33),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(0, 3, "keyword".into(), Some(0))];
    let segs = row_segments("let x", &hl, &styles, FG, BG, 0);
    assert_eq!(color_of(&segs, "let"), 0x11_22_33);
}

#[test]
fn a_span_style_background_is_carried_onto_the_segment() {
    // The regression: a span whose style sets a `bg` (a diff line tint, or any
    // colorscheme group with a background) must carry it onto the Seg so the GUI
    // paints a quad behind the glyph — it was dropped (bg: None), so backgrounds
    // showed in the TUI but not the GUI.
    let styles = vec![Style {
        fg: Some(0xc0_ca_f5),
        bg: Some(0x1e_2f_3b),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(0, 3, "DiffChange".into(), Some(0))];
    let segs = row_segments("let x", &hl, &styles, FG, BG, 0);
    assert_eq!(
        seg_of(&segs, "let").bg,
        Some(0x1e_2f_3b),
        "span bg must reach the Seg"
    );
    // Unspanned text carries no background.
    assert_eq!(seg_of(&segs, " x").bg, None);
}

#[test]
fn a_reverse_span_leaves_its_segment_background_unset() {
    // Reverse is painted by push_reverse_fills (a fg-colored quad), so the Seg's own
    // `bg` must stay None — otherwise the row would get a doubled fill.
    let styles = vec![Style {
        fg: Some(0x11_22_33),
        bg: Some(0x44_55_66),
        reverse: true,
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(0, 3, "Visual".into(), Some(0))];
    let segs = row_segments("let x", &hl, &styles, FG, BG, 0);
    assert_eq!(
        seg_of(&segs, "let").bg,
        None,
        "reverse run leaves its Seg bg unset"
    );
    // The glyph takes the style's bg as its fg (reverse-video).
    assert_eq!(color_of(&segs, "let"), 0x44_55_66);
}
