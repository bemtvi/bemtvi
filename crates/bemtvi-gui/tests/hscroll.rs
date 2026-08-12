//! Tier 1: the horizontal-scroll (`leftcol`) screen-column mapping the GUI renderer
//! lays text and overlays out with. Black-box, no window, no GPU — the painted
//! frame needs a real client, but the column math below is what put scrolled text
//! over the gutter and out from under the cursor (the regression these pin down).

use bemtvi_gui::{col_to_screen, text_run_origin};

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
