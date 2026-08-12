//! Tier 1: the sRGB→linear conversion the GUI quad pipeline uses for background
//! fills (statusline blocks, float chrome, the base bar). Black-box, no window, no
//! GPU. Guards the regression where `color_to_rgba` mis-ordered the bytes of a
//! `glyphon::Color` (`0xAARRGGBB`), swapping the red and blue channels — invisible
//! on desaturated chrome but glaring on a saturated bemtvi-line mode block (a blue
//! NORMAL fill rendered orange, while green/purple — with R == B — looked fine).

use bemtvi_gui::{color_to_rgba, srgb_to_color, srgb_to_color_rgba};

/// The quad path (`color_to_rgba ∘ srgb_to_color`) and the direct path
/// (`srgb_to_color_rgba`) must agree: both take a packed `0xRRGGBB` and yield the
/// same linear RGBA. They diverge only when the channel order is wrong.
fn assert_paths_agree(c: u32) {
    let via_color = color_to_rgba(srgb_to_color(c));
    let direct = srgb_to_color_rgba(c, 1.0);
    assert_eq!(
        via_color, direct,
        "quad-path conversion of #{c:06x} disagrees with the direct path: \
         {via_color:?} vs {direct:?}"
    );
}

#[test]
fn quad_conversion_preserves_channel_order() {
    // The diagnostic case: R != B != G. With the channel swap this rendered as a
    // completely different hue (the bemtvi-line NORMAL block bug).
    assert_paths_agree(0x5f_af_ff); // bemtvi-line NORMAL blue
    assert_paths_agree(0x5f_af_5f); // INSERT green  (R == B — invariant under the swap)
    assert_paths_agree(0xd7_87_d7); // VISUAL purple (R == B — invariant under the swap)
    assert_paths_agree(0xff_af_00); // command amber
    assert_paths_agree(0x12_34_56); // an arbitrary asymmetric color
}

#[test]
fn red_and_blue_are_not_swapped() {
    // A pure-red fill must land in the red channel, not the blue one.
    let [r, g, b, _a] = color_to_rgba(srgb_to_color(0xff_00_00));
    assert!(r > 0.9, "red channel should be ~full, got {r}");
    assert_eq!(g, 0.0, "green channel should be empty");
    assert_eq!(b, 0.0, "blue channel should be empty, got {b}");
}
