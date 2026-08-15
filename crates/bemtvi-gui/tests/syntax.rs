//! Tier 1: the pure per-row syntax-coloring layer the GUI renderer feeds each text
//! row through — splitting a row into colored runs from its highlight spans, and
//! the built-in capture-group fallback that colors a buffer when no colorscheme is
//! loaded. Black-box, no window, no GPU. This guards the regression where the GUI
//! showed *no* syntax highlighting without a colorscheme (it ignored the span's
//! group name, unlike the TUI's `group_style`).

use bemtvi_gui::{apply_search_fg, group_fallback, row_segments, Seg};
use bemtvi_view::{HlSpan, Style};

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
    assert!(group_fallback("keyword").fg.is_some());
    assert!(group_fallback("string").fg.is_some());
    assert!(group_fallback("function.call").fg.is_some()); // sub-capture → major "function"
    let comment = group_fallback("comment.line");
    assert!(comment.fg.is_some());
    assert!(comment.italic, "comments render italic");
    // Three different families get three different colors.
    let kw = group_fallback("keyword").fg;
    let st = group_fallback("string").fg;
    let ty = group_fallback("type").fg;
    assert!(kw != st && st != ty && kw != ty);
    // An unknown group sets nothing, so the run keeps the editor's default fg.
    assert_eq!(group_fallback("definitely.not.a.group"), Style::default());
}

#[test]
fn an_unprintable_control_char_falls_back_to_the_special_key_look() {
    // The regression: the TUI's `group_style` gives `SpecialKey` — the `^X` / `<xx>`
    // overlay the server paints over an unprintable control char — a standout bold
    // foreground, but the GUI's hand-copied fallback table had dropped the arm, so
    // with no colorscheme loaded the same token read as plain text in the GUI only.
    let special = group_fallback("SpecialKey");
    assert!(
        special.fg.is_some(),
        "the <xx> token must not paint in the default fg"
    );
    assert!(special.bold, "SpecialKey is bold, as in the TUI");
    // And it reaches a row's segments: the server sends the overlay as a span with
    // no resolved `style_id` when no colorscheme defines the group.
    let hl: Vec<HlSpan> = vec![(1, 5, "SpecialKey".into(), None)]; // `<81>` in `a<81>b`
    let segs = row_segments("a<81>b", &hl, &[], FG, BG, 0);
    let token = seg_of(&segs, "<81>");
    assert_eq!(token.fg, special.fg.unwrap());
    assert!(token.bold, "the span's bold attribute reaches the Seg");
    assert_eq!(color_of(&segs, "a"), FG, "plain text keeps the default fg");
}

#[test]
fn row_without_colorscheme_still_colors_spans_from_their_group() {
    // The regression: a span with no resolved `style_id` (empty `styles`) must
    // still be colored from its capture group, not painted in the default fg.
    let line = "let x";
    let hl: Vec<HlSpan> = vec![(0, 3, "keyword".into(), None)]; // "let"
    let segs = row_segments(line, &hl, &[], FG, BG, 0);
    let keyword_color = group_fallback("keyword").fg.unwrap();
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

// ── Wide glyphs: spans are SCREEN COLUMNS, not char indices ──────────────────
// The server ships every highlight span in display cells (`unicode::virtcol`), so a
// row walk that treats one char as one column drifts the moment the line holds a
// CJK glyph, an emoji, or a grapheme cluster whose char count differs from its cell
// count. Each case below is a column/char divergence the old walk got wrong.

/// The concatenated text of every segment — what the row actually paints.
fn painted(segs: &[Seg]) -> String {
    segs.iter().map(|s| s.text.as_str()).collect()
}

#[test]
fn a_span_over_cjk_stops_at_its_column_not_its_char_count() {
    // "日本ab": 日 owns cols 0–1, 本 cols 2–3, then a=4, b=5. A span of 0..4 covers
    // exactly the two kanji — by char index it would swallow "ab" as well.
    let styles = vec![Style {
        fg: Some(0x11_22_33),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(0, 4, "string".into(), Some(0))];
    let segs = row_segments("日本ab", &hl, &styles, FG, BG, 0);
    assert_eq!(
        painted(&segs),
        "日本ab",
        "every glyph is still painted once"
    );
    assert_eq!(color_of(&segs, "日本"), 0x11_22_33);
    assert_eq!(color_of(&segs, "ab"), FG, "the span ends at column 4");
}

#[test]
fn a_span_after_cjk_starts_at_its_column() {
    // The mirror case: a span at cols 4..6 is "ab". Indexed by char it would start
    // past the end of a 4-char line and paint nothing at all.
    let styles = vec![Style {
        fg: Some(0x44_55_66),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(4, 6, "keyword".into(), Some(0))];
    let segs = row_segments("日本ab", &hl, &styles, FG, BG, 0);
    assert_eq!(painted(&segs), "日本ab");
    assert_eq!(color_of(&segs, "日本"), FG);
    assert_eq!(color_of(&segs, "ab"), 0x44_55_66);
}

#[test]
fn a_span_after_an_emoji_starts_at_its_column() {
    // A lone emoji is ONE char but TWO columns: 😀 owns cols 0–1, so "ab" is 2..4.
    let styles = vec![Style {
        fg: Some(0x77_88_99),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(2, 4, "keyword".into(), Some(0))];
    let segs = row_segments("\u{1f600}ab", &hl, &styles, FG, BG, 0);
    assert_eq!(painted(&segs), "\u{1f600}ab");
    assert_eq!(color_of(&segs, "\u{1f600}"), FG);
    assert_eq!(color_of(&segs, "ab"), 0x77_88_99);
}

#[test]
fn a_span_over_a_zwj_cluster_covers_its_two_columns() {
    // The opposite divergence: a ZWJ family emoji is FIVE chars but ONE cluster of
    // two columns, so "ab" begins at column 2 — by char index the span would land
    // deep inside the cluster and split it apart.
    let family = "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}";
    let styles = vec![Style {
        fg: Some(0xaa_bb_cc),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(2, 4, "keyword".into(), Some(0))];
    let segs = row_segments(&format!("{family}ab"), &hl, &styles, FG, BG, 0);
    assert_eq!(painted(&segs), format!("{family}ab"));
    assert_eq!(color_of(&segs, family), FG, "the cluster stays whole");
    assert_eq!(color_of(&segs, "ab"), 0xaa_bb_cc);
}

#[test]
fn a_span_over_an_emoji_modifier_cluster_covers_its_two_columns() {
    // `🤴🏼` (PRINCE + a Fitzpatrick modifier) is two chars whose individual widths
    // are 2 each, but the cluster paints in 2 cells — so "ab" starts at column 2.
    let styles = vec![Style {
        fg: Some(0xde_ad_be),
        ..Default::default()
    }];
    let hl: Vec<HlSpan> = vec![(2, 4, "keyword".into(), Some(0))];
    let segs = row_segments("\u{1f934}\u{1f3fc}ab", &hl, &styles, FG, BG, 0);
    assert_eq!(painted(&segs), "\u{1f934}\u{1f3fc}ab");
    assert_eq!(color_of(&segs, "\u{1f934}\u{1f3fc}"), FG);
    assert_eq!(color_of(&segs, "ab"), 0xde_ad_be);
}

#[test]
fn horizontal_scroll_drops_columns_not_chars() {
    // `leftcol` is a screen column: scrolling past 日本 (4 cells) leaves "ab".
    // Dropping four *chars* would eat the whole line.
    let segs = row_segments("日本ab", &[], &[], FG, BG, 4);
    assert_eq!(painted(&segs), "ab");
}

#[test]
fn a_search_recolor_after_cjk_lands_on_its_column() {
    // `apply_search_fg` walks the same column grid: the match at cols 4..6 is "ab".
    let base = vec![Seg::plain("日本ab".to_string(), FG)];
    let out = apply_search_fg(base, &[(4, 6)], None, 0, Some(0x0f_0f_0f), None);
    assert_eq!(painted(&out), "日本ab");
    assert_eq!(color_of(&out, "日本"), FG);
    assert_eq!(color_of(&out, "ab"), 0x0f_0f_0f);
}
