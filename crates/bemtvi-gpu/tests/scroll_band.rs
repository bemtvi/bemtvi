//! Tier 1: the scroll-slide band snapshot — every overlay layer the gesture
//! carries must survive into the interpolated [`ScrollFrame`], or that layer
//! blinks out for the duration of the slide and snaps back on settle. Black-box,
//! no window, no GPU (the paint itself needs a GPU and is validated by running
//! the client; these lock the band *data contract* the paint reads).
//!
//! Regression anchor: the GUI's `ScrollAnim` was a field-by-field port of the
//! TUI's `Animation` and silently dropped `diagnostics_virt`, so inline
//! diagnostic text vanished during every slide in the GUI only.

use std::time::Duration;

use bemtvi_gpu::{band_bg_rects, row_segments, ScrollAnim, ScrollFrame};
use bemtvi_view::{ScrollData, Style};

/// A minimal three-row gesture with one overlay entry per layer under test.
fn gesture() -> ScrollData {
    ScrollData {
        from_row: 0.0,
        to_row: 2.0,
        from_cursor_row: 0.0,
        to_cursor_row: 2.0,
        duration: Duration::from_millis(150),
        lines: vec!["alpha".into(), "beta".into(), "gamma".into()],
        numbers: vec![Some(1), Some(2), Some(3)],
        diagnostics_virt: vec![None, Some(("■ E123: oops".into(), 1, None)), None],
        diagnostics: vec![vec![], vec![(0, 4, 1, None)], vec![]],
        diagnostics_signs: vec![None, Some(("E".into(), 1, None)), None],
        gutter_text: vec![Some(vec![(" 12".into(), None)]), None, None],
        ..ScrollData::default()
    }
}

#[test]
fn band_frame_carries_inline_diagnostics() {
    let data = gesture();
    let anim = ScrollAnim::new(&data);
    let frame = ScrollFrame::of(&anim);
    assert_eq!(
        frame.diagnostics_virt,
        &data.diagnostics_virt[..],
        "inline diagnostic virtual text must ride the slide, not blink out"
    );
    // The sibling diagnostic layers were carried all along — keep them locked.
    assert_eq!(frame.diagnostics, &data.diagnostics[..]);
    assert_eq!(frame.diagnostics_signs, &data.diagnostics_signs[..]);
    // …and so does the gutter text column, so a diff's numbers slide with their lines.
    assert_eq!(frame.gutter_text, &data.gutter_text[..]);
}

/// Inline `code` keeps its background mid-slide. The band row's highlight spans
/// carry the `@markup.raw` style (with its `bg`) like the settled row's, and the
/// band paints a background rect for every run that set one, at the row's
/// sub-pixel offset, clamped to the text area. Regression: the band painted only
/// the glyphs, so markdown inline code lost its background for the whole slide
/// and the background came back when it settled.
#[test]
fn band_row_paints_a_span_background_at_its_sub_pixel_offset() {
    const CODE_BG: u32 = 0x30_30_30;
    let styles = vec![Style {
        fg: Some(0x80_c0_80),
        bg: Some(CODE_BG),
        ..Style::default()
    }];
    // "see `code` here", the backticked run [4, 10) on the code style.
    let hl = vec![(4, 10, "markup.raw".to_string(), Some(0))];
    let segs = row_segments("see `code` here", &hl, &styles, 0xc0_c0_c0, 0, 0);
    let (cw, ch) = (10.0, 20.0);
    // Laid out from column 2, the row's top 4px above the text area's top edge (a
    // partly scrolled-off row): the rect starts at the text area's edge, 16px tall.
    let rects = band_bg_rects(&segs, 2, -4.0, (cw, ch), (0.0, 200.0));
    assert_eq!(rects, vec![(60.0, 0.0, 60.0, 16.0, CODE_BG)]);
    // Fully inside: the whole cell height, at the fractional y.
    let rects = band_bg_rects(&segs, 2, 30.5, (cw, ch), (0.0, 200.0));
    assert_eq!(rects, vec![(60.0, 30.5, 60.0, 20.0, CODE_BG)]);
    // Scrolled wholly past the text area: nothing paints outside it.
    assert!(band_bg_rects(&segs, 2, 200.0, (cw, ch), (0.0, 200.0)).is_empty());
}

/// The band frame names the cursor's line only when the cursor has a row in the
/// band. A caret a modeless wheel left off screen is pinned to an edge row, and the
/// gutter took that row's line for the cursor's: its number lit in `CursorLineNr`
/// for the length of every later notch's slide, then cleared on settle.
#[test]
fn band_frame_has_no_cursor_line_for_a_cursor_off_the_band() {
    let data = gesture();
    let frame_of = |data: &ScrollData| ScrollFrame::of(&ScrollAnim::new(data)).cursor_line;
    // At progress 0 the cursor sits on band row 0, line 1.
    assert_eq!(
        frame_of(&data),
        Some(1),
        "a cursor in the band is on its row's line"
    );
    let off = ScrollData {
        cursor_off_band: true,
        ..gesture()
    };
    assert_eq!(
        frame_of(&off),
        None,
        "the edge row it is pinned to isn't its line"
    );
}
