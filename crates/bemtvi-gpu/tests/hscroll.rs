//! Tier 1: the horizontal-scroll (`leftcol`) screen-column mapping the GUI renderer
//! lays text and overlays out with. Black-box, no window, no GPU — the painted
//! frame needs a real client, but the column math below is what put scrolled text
//! over the gutter and out from under the cursor (the regression these pin down).

use bemtvi_gpu::{col_to_screen, overlay_span_cols, text_run_origin};

/// The window's text area in these cases starts at column 8 (a sign column + a
/// number gutter to its left); the exact value doesn't matter, only that the
/// mapping is measured from it.
const TEXT_X0: u16 = 8;

#[test]
fn first_visible_column_paints_at_the_text_origin_not_over_the_gutter() {
    // `row_segments`/`splice_inlay` emit the run from the first *visible* buffer
    // column (== leftcol), so its origin must land exactly at the text area's left
    // edge — never `leftcol` cells back into the number gutter, which is what the
    // pre-fix `text_x0 - leftcol` origin did.
    for leftcol in [0u16, 1, 3, 7, 20] {
        assert_eq!(
            text_run_origin(TEXT_X0, leftcol),
            TEXT_X0,
            "leftcol={leftcol}: the run starts at the text origin"
        );
        // The bug shoved it leftcol cells into the gutter.
        if leftcol > 0 {
            assert_ne!(
                text_run_origin(TEXT_X0, leftcol),
                TEXT_X0.saturating_sub(leftcol),
                "leftcol={leftcol}: must not double-subtract the scroll"
            );
        }
    }
}

#[test]
fn text_origin_agrees_with_the_overlay_mapping_for_the_first_visible_column() {
    // The cursor / selection / search overlays place a buffer column at
    // `col_to_screen`. The text run begins at the first visible column (== leftcol),
    // so the two must coincide there or the glyphs drift off their overlays.
    for leftcol in [0u16, 2, 5, 13] {
        assert_eq!(
            text_run_origin(TEXT_X0, leftcol),
            col_to_screen(TEXT_X0, leftcol, leftcol),
        );
    }
}

#[test]
fn columns_slide_left_by_the_scroll_and_clamp_off_screen() {
    // A column at or past leftcol shifts left by exactly leftcol cells.
    assert_eq!(col_to_screen(TEXT_X0, 5, 0), TEXT_X0 + 5);
    assert_eq!(col_to_screen(TEXT_X0, 5, 2), TEXT_X0 + 3);
    assert_eq!(col_to_screen(TEXT_X0, 5, 5), TEXT_X0);
    // A column scrolled off the left clamps to the text origin (it isn't painted,
    // but the mapping must not underflow past the gutter).
    assert_eq!(col_to_screen(TEXT_X0, 1, 5), TEXT_X0);
}

/// The window's right edge (exclusive) in the overlay cases below: the text area is
/// columns `[TEXT_X0, RIGHT)`, and the next split starts at `RIGHT`.
const RIGHT: u16 = 40;

#[test]
fn an_overlay_past_the_right_edge_stops_at_the_window() {
    // Under `nowrap` the server sends a selection / search span in buffer columns,
    // whatever the window's width: a match at columns 30..60 of a long line. The
    // part past the edge belongs to no cell of this window, and painting it put the
    // tint over the split to the right.
    assert_eq!(
        overlay_span_cols(TEXT_X0, RIGHT, (30, 60), 0, &[], &[]),
        Some((38, RIGHT)),
        "the span is cut at the window's right edge"
    );
    // Scrolled sideways, the same span is measured from leftcol and still cut there.
    assert_eq!(
        overlay_span_cols(TEXT_X0, RIGHT, (30, 60), 10, &[], &[]),
        Some((28, RIGHT))
    );
}

#[test]
fn an_overlay_wholly_past_the_right_edge_paints_nothing() {
    // A match that starts beyond the visible text has no cell in this window.
    assert_eq!(
        overlay_span_cols(TEXT_X0, RIGHT, (50, 55), 0, &[], &[]),
        None
    );
    assert_eq!(
        overlay_span_cols(TEXT_X0, RIGHT, (32, 36), 0, &[], &[]),
        None
    );
}

#[test]
fn an_overlay_inside_the_window_is_untouched() {
    assert_eq!(
        overlay_span_cols(TEXT_X0, RIGHT, (2, 7), 0, &[], &[]),
        Some((10, 15))
    );
    assert_eq!(
        overlay_span_cols(TEXT_X0, RIGHT, (12, 17), 10, &[], &[]),
        Some((10, 15))
    );
}
