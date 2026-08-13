//! What a *file* is allowed to cost the image preview before it is decoded.
//!
//! `'imagepreview'` decodes whatever the user opens or the picker highlights, so
//! the bytes are as untrusted as any other file in a repo. A PNG's header
//! declares its dimensions, and a decoder believes it: the output buffer is
//! allocated from `width * height * bytes-per-pixel` *before* the pixel data is
//! read. That is the classic decompression bomb — a file of a few kilobytes whose
//! header claims a bitmap of gigabytes.
//!
//! The `image` crate's defaults do not stop it: the strict dimension check is off
//! and only a best-effort allocation cap remains, which decoders are free to
//! ignore. The preview sets explicit `Limits`, so an impossible header is refused
//! at parse time — before the allocator is asked for anything.
//!
//! The fixtures are built by hand (`pngbuild`) rather than by an encoder, because
//! the whole point is a file whose *declared* size is not a size anyone would want
//! materialised — an encoder would have to materialise it to write it.

mod pngbuild;

use bemtvi_view::images::decode_bytes;
use pngbuild::gray_png;

/// The decode-time edge cap, mirrored from `images.rs`. A file past it must be
/// refused on its header alone.
const DECODE_EDGE_CAP: u32 = 16_384;

/// An ordinary image still decodes — the limits must not cost the feature.
#[test]
fn an_ordinary_image_still_decodes() {
    let img = decode_bytes(&gray_png(64, 64), 100).expect("a 64x64 png must decode");
    assert_eq!(img.width(), 64);
    assert_eq!(img.height(), 64);
}

/// Well past any real photo (8K is 7680x4320) but well within the cap: this is the
/// headroom that makes the cap safe to apply.
#[test]
fn a_large_but_plausible_image_still_decodes() {
    assert!(
        decode_bytes(&gray_png(8_000, 1), 100).is_some(),
        "8000px is a real image size and must keep decoding"
    );
    assert!(
        decode_bytes(&gray_png(DECODE_EDGE_CAP, 1), 100).is_some(),
        "the cap itself is inclusive"
    );
}

/// One pixel past the cap is refused. The check is on the *declared* dimension, so
/// it happens on the header — the file below is 16 KB and never becomes a bitmap.
#[test]
fn an_image_one_pixel_past_the_cap_is_refused() {
    assert!(
        decode_bytes(&gray_png(DECODE_EDGE_CAP + 1, 1), 100).is_none(),
        "a dimension past the cap must be refused"
    );
}

/// A header claiming a dimension no real image has, refused on the header alone —
/// the file below is 60 KB and never becomes a bitmap. This is the shape of a
/// decompression bomb: the *declared* size, not the delivered bytes, is what a
/// decoder allocates from.
///
/// (A fully-realised bomb — a 20000x20000 header with a complete pixel stream,
/// which is a ~400 MB allocation from a ~100 KB file — is not built here: the
/// fixture would have to be generated through a compressor at test time. Note
/// what that means for coverage: the **dimension** cap is what these tests pin,
/// and it is also the cap the `image` crate documents as the strict one. The
/// companion `max_alloc` bound is best-effort by the crate's own definition —
/// decoders may ignore it — so it is a second line, not the guarantee.)
#[test]
fn a_header_claiming_an_impossible_width_is_refused_without_decoding() {
    let bomb = gray_png(60_000, 1);
    assert!(
        bomb.len() < 128 * 1024,
        "the fixture must stay small — that asymmetry is what makes it a bomb"
    );
    assert!(
        decode_bytes(&bomb, 100).is_none(),
        "a 60000px-wide header must be refused, not allocated for"
    );
}
