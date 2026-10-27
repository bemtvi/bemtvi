//! Tier 1: the configured font list (`'guifont'`) resolved per character, as the
//! renderer shapes it. Black-box, no window, no GPU, and no dependence on the
//! machine's fonts: a font database holding only the tiny fonts in `tests/fonts/`
//! (see its `make.py` for what each draws).

use std::path::PathBuf;

use bemtvi_gpu::{runs_by, shape_with_fonts, TestGlyph};
use bemtvi_view::FontSpec;

fn fonts() -> Vec<PathBuf> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fonts");
    [
        "BtvTestMono.ttf",
        "BtvTestMonoItalic.ttf",
        "BtvTestSecond.ttf",
        "BtvTestWide.ttf",
    ]
    .iter()
    .map(|f| dir.join(f))
    .collect()
}

fn list(names: &[&str]) -> Vec<FontSpec> {
    names.iter().map(|n| FontSpec::named(n)).collect()
}

/// `(text, family)` for each glyph.
fn drawn_by(glyphs: &[TestGlyph]) -> Vec<(&str, &str)> {
    glyphs
        .iter()
        .map(|g| (g.text.as_str(), g.family.as_str()))
        .collect()
}

#[test]
fn each_character_goes_to_the_first_font_that_has_it() {
    let (glyphs, missing) = shape_with_fonts(
        &fonts(),
        &list(&["BtvTest Mono", "BtvTest Second"]),
        "abxy",
        (false, false),
    );
    assert!(missing.is_empty());
    assert_eq!(
        drawn_by(&glyphs),
        vec![
            ("a", "BtvTest Mono"),
            ("b", "BtvTest Mono"),
            ("x", "BtvTest Second"),
            ("y", "BtvTest Second"),
        ]
    );
}

#[test]
fn the_order_is_the_lists_even_where_a_later_font_also_has_it() {
    // Both draw `a`; the one listed first wins it.
    let (glyphs, _) = shape_with_fonts(
        &fonts(),
        &list(&["BtvTest Second", "BtvTest Mono"]),
        "ab",
        (false, false),
    );
    assert_eq!(
        drawn_by(&glyphs),
        vec![("a", "BtvTest Second"), ("b", "BtvTest Mono")]
    );
    // And a wide script follows the list too, not a per-script platform choice.
    let (glyphs, _) = shape_with_fonts(
        &fonts(),
        &list(&["BtvTest Mono", "BtvTest Wide", "BtvTest Second"]),
        "x\u{4e2d}",
        (false, false),
    );
    assert_eq!(
        drawn_by(&glyphs),
        vec![("x", "BtvTest Wide"), ("\u{4e2d}", "BtvTest Wide")]
    );
}

#[test]
fn a_font_draws_its_characters_at_its_own_weight_and_scale() {
    let second = FontSpec {
        weight: Some(600),
        scale: Some(1.5),
        ..FontSpec::named("BtvTest Second")
    };
    let specs = vec![FontSpec::named("BtvTest Mono"), second];
    let (glyphs, _) = shape_with_fonts(&fonts(), &specs, "ax", (false, false));
    let at = |t: &str| glyphs.iter().find(|g| g.text == t).expect("a glyph");
    assert_eq!((at("a").weight, at("a").font_size), (400, 10.0));
    assert_eq!((at("x").weight, at("x").font_size), (600, 15.0));
    // Bold text stays bold whatever the font's own weight.
    let (glyphs, _) = shape_with_fonts(&fonts(), &specs, "x", (true, false));
    assert_eq!(glyphs[0].weight, 700);
}

#[test]
fn a_fonts_features_apply_to_the_characters_it_draws() {
    // BtvTest Mono's `ss01` swaps `a` for an alternate glyph.
    let plain = shape_with_fonts(&fonts(), &list(&["BtvTest Mono"]), "a", (false, false)).0;
    let ss01 = FontSpec {
        features: vec![("ss01".to_string(), 1)],
        ..FontSpec::named("BtvTest Mono")
    };
    let alt = shape_with_fonts(&fonts(), &[ss01], "a", (false, false)).0;
    assert_eq!(alt[0].family, "BtvTest Mono");
    assert_ne!(
        alt[0].glyph_id, plain[0].glyph_id,
        "ss01 picked the alternate"
    );
}

#[test]
fn a_font_that_isnt_installed_is_reported_and_the_rest_still_draw() {
    let (glyphs, missing) = shape_with_fonts(
        &fonts(),
        &list(&["Fira Kode", "btvtest mono", "BtvTest Second"]),
        "ax",
        (false, false),
    );
    assert_eq!(missing, vec!["Fira Kode".to_string()]);
    // A name matches whatever its case.
    assert_eq!(
        drawn_by(&glyphs),
        vec![("a", "BtvTest Mono"), ("x", "BtvTest Second")]
    );
}

#[test]
fn runs_split_where_the_key_changes() {
    let digit = |g: &str| g.chars().all(|c| c.is_ascii_digit());
    assert_eq!(
        runs_by("ab12c", &digit),
        vec![("ab", false), ("12", true), ("c", false)]
    );
    // A grapheme cluster is never split.
    assert_eq!(
        runs_by("e\u{301}1", &digit),
        vec![("e\u{301}", false), ("1", true)]
    );
    assert!(runs_by("", &digit).is_empty());
}

#[test]
fn italic_text_takes_the_italic_face_where_it_has_the_character() {
    // BtvTest Mono's italic face draws `a` and `b` only, and has no GPOS table.
    let (glyphs, _) = shape_with_fonts(&fonts(), &list(&["BtvTest Mono"]), "ac", (false, true));
    let slanted: Vec<(&str, bool)> = glyphs.iter().map(|g| (g.text.as_str(), g.italic)).collect();
    assert_eq!(slanted, vec![("a", true), ("c", false)]);
}

#[test]
fn a_missing_first_font_gives_its_place_to_the_system_monospace() {
    // Not the next font in the list: that may be a symbol or emoji font, which would
    // then draw the plain text it happens to cover. (The test database's monospace is
    // BtvTest Mono.)
    let (glyphs, missing) = shape_with_fonts(
        &fonts(),
        &list(&["Fira Kode", "BtvTest Second"]),
        "ax",
        (false, false),
    );
    assert_eq!(missing, vec!["Fira Kode".to_string()]);
    assert_eq!(
        drawn_by(&glyphs),
        vec![("a", "BtvTest Mono"), ("x", "BtvTest Second")]
    );
}

#[test]
fn a_variation_selector_or_joiner_doesnt_decide_the_font() {
    // `b` + VS16: no font has the selector, but it rides with the `b`, so the font
    // that has `b` draws the pair, at its own weight.
    let specs = vec![
        FontSpec::named("BtvTest Second"),
        FontSpec {
            weight: Some(600),
            ..FontSpec::named("BtvTest Mono")
        },
    ];
    let (glyphs, _) = shape_with_fonts(&fonts(), &specs, "b\u{FE0F}", (false, false));
    assert_eq!(glyphs[0].family, "BtvTest Mono");
    assert_eq!(glyphs[0].weight, 600);
}
