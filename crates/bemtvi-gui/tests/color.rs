//! Tier 1: the sRGB→linear conversion the GUI quad pipeline uses for background
//! fills (statusline blocks, float chrome, the base bar). Black-box, no window, no
//! GPU. Guards the regression where `color_to_rgba` mis-ordered the bytes of a
//! `glyphon::Color` (`0xAARRGGBB`), swapping the red and blue channels — invisible
//! on desaturated chrome but glaring on a saturated bemtvi-line mode block (a blue
//! NORMAL fill rendered orange, while green/purple — with R == B — looked fine).

use bemtvi_gui::{clear_color, color_to_rgba, srgb_to_color, srgb_to_color_rgba};

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

/// `'guiopacity'` at 100 clears with exactly the opaque `Normal` background the GUI
/// always painted: same linear colour as the quad path, alpha 1.
#[test]
fn full_opacity_clears_with_the_opaque_background() {
    for c in [0x1e_1e_24, 0x5f_af_ff, 0xff_ff_ff, 0x00_00_00] {
        let [r, g, b, a] = clear_color(c, 100);
        let [er, eg, eb, _] = srgb_to_color_rgba(c, 1.0);
        assert_eq!((r, g, b, a), (er, eg, eb, 1.0), "#{c:06x} at 100%");
    }
}

/// Below 100 the clear is premultiplied for the compositor, which reads the stored
/// (sRGB-encoded) surface: each encoded channel must come out as `srgb × alpha`, so
/// the backdrop shows through by `1 − alpha` without shifting the hue. Scaling the
/// *linear* value instead would store a much darker colour (sRGB encoding is not
/// linear), and the background would read near-black over the Mica.
#[test]
fn partial_opacity_premultiplies_in_the_stored_space() {
    let encode = |l: f32| {
        if l <= 0.003_130_8 {
            l * 12.92
        } else {
            1.055 * l.powf(1.0 / 2.4) - 0.055
        }
    };
    let bg = 0x80_40_c0;
    for pct in [0u8, 25, 50, 85] {
        let [r, g, b, a] = clear_color(bg, pct);
        let alpha = pct as f32 / 100.0;
        assert!((a - alpha).abs() < 1e-6, "alpha at {pct}%");
        for (got, byte) in [(r, 0x80), (g, 0x40), (b, 0xc0)] {
            let want = byte as f32 / 255.0 * alpha;
            assert!(
                (encode(got) - want).abs() < 1e-4,
                "channel {byte:#x} at {pct}%: stored {} want {want}",
                encode(got)
            );
        }
    }
}

/// Fully transparent clears to nothing at all: no tint left over the backdrop.
#[test]
fn zero_opacity_clears_to_transparent() {
    assert_eq!(clear_color(0xff_ff_ff, 0), [0.0, 0.0, 0.0, 0.0]);
}
