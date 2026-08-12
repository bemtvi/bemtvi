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

use bemtvi_gui::{ScrollAnim, ScrollFrame};
use bemtvi_view::ScrollData;

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
}
