//! The scroll-marker thumb a client sizes from a window's `scrollbars` span.

use bemtvi_view::ScrollSpan;

fn span(start: u64, shown: u64, total: u64) -> ScrollSpan {
    ScrollSpan {
        start,
        shown,
        total,
    }
}

#[test]
fn everything_in_view_draws_no_thumb() {
    assert_eq!(span(0, 20, 20).thumb(100.0, 4.0), None);
    assert_eq!(span(0, 20, 5).thumb(100.0, 4.0), None);
    assert_eq!(span(0, 0, 0).thumb(100.0, 4.0), None);
}

#[test]
fn the_thumb_is_the_shown_part_at_its_place() {
    assert_eq!(span(0, 25, 100).thumb(100.0, 4.0), Some((0.0, 25.0)));
    assert_eq!(span(50, 25, 100).thumb(100.0, 4.0), Some((50.0, 25.0)));
    assert_eq!(span(75, 25, 100).thumb(200.0, 4.0), Some((150.0, 50.0)));
}

#[test]
fn a_long_buffer_keeps_a_thumb_you_can_see() {
    let (off, len) = span(5000, 20, 10_000).thumb(100.0, 8.0).unwrap();
    assert_eq!(len, 8.0);
    assert!((off - 50.0).abs() < 0.01);
    // At the very end it stays on the track.
    let (off, len) = span(9980, 20, 10_000).thumb(100.0, 8.0).unwrap();
    assert!(off + len <= 100.0);
}

#[test]
fn a_view_scrolled_past_the_end_sits_at_the_track_end() {
    // 'scrolloff' lets the last lines rise up the window.
    let (off, len) = span(90, 20, 100).thumb(110.0, 4.0).unwrap();
    assert_eq!(off + len, 110.0);
    assert_eq!(len, 20.0);
}
