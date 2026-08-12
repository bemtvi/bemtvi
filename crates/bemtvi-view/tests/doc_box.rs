//! Tier-1 tests for the completion popup's documentation-preview geometry — the
//! pure cell math both clients lay the box out with. Black-box, no server, no
//! toolkit: an area, a popup rect and the doc lines in, the box rect out.
//!
//! This is the geometry the GUI used to re-derive by hand, having dropped the room
//! clamps on the way: the box overran the area's right/bottom edge, and when neither
//! side of the popup had room it was drawn on top of the popup anyway.

use bemtvi_view::{doc_box, wrap_chars, CellRect};

/// An 80×24 text area with a popup of `w`×`h` at `(x, y)` in it.
fn popup(x: u16, y: u16, w: u16, h: u16) -> (CellRect, CellRect) {
    (CellRect::new(0, 0, 80, 24), CellRect::new(x, y, w, h))
}

#[test]
fn the_box_sits_to_the_popups_right_when_there_is_room() {
    let (area, pop) = popup(4, 2, 20, 8);
    let b = doc_box(area, pop, &["a doc line".into()]).expect("room on the right");
    assert_eq!(b.x, pop.right(), "top-aligned box, just past the popup");
    assert_eq!(b.y, pop.y);
    assert_eq!(b.w, 12, "10 content chars + the border ring");
    assert_eq!(b.h, 3, "one wrapped row + the border ring");
}

#[test]
fn the_box_flips_left_when_the_right_edge_is_close() {
    let (area, pop) = popup(60, 2, 18, 8); // popup right edge at 78, of 80
    let b = doc_box(area, pop, &["a doc line".into()]).expect("room on the left");
    assert_eq!(b.right(), pop.x, "the box ends where the popup starts");
    assert!(b.w >= 3);
}

#[test]
fn the_box_width_is_clamped_to_the_room_beside_the_popup() {
    // The regression: a long doc line wants a 50-wide box, but only 6 cells are left
    // to the popup's right. The box must fit them, not overrun the area's edge.
    let (area, pop) = popup(20, 2, 54, 8); // right edge at 74, of 80 → 6 cells
    let long = "x".repeat(200);
    let b = doc_box(area, pop, &[long]).expect("6 cells is enough to draw in");
    assert_eq!(b.x, pop.right());
    assert_eq!(b.w, 6, "clamped to the room available");
    assert!(
        b.right() <= area.right(),
        "never past the area's right edge"
    );
}

#[test]
fn the_box_height_is_clamped_to_the_rows_below_the_popup() {
    // The regression: a tall doc under a popup near the bottom must stop at the
    // area's last row rather than being drawn off the end of it.
    let (area, pop) = popup(4, 20, 20, 3); // popup top at row 20, of 24
    let doc: Vec<String> = (0..30).map(|i| format!("line {i}")).collect();
    let b = doc_box(area, pop, &doc).expect("4 rows is enough to draw in");
    assert_eq!(b.y, pop.y);
    assert_eq!(b.h, 4, "clamped to the rows left below the popup's top");
    assert!(b.bottom() <= area.bottom(), "never past the area's bottom");
}

#[test]
fn a_long_single_line_gets_the_rows_its_wrapped_form_needs() {
    // Height counts *wrapped* rows, not doc lines: one 101-char line in a box 50
    // content cells wide is 3 rows, not 1 (the GUI sized it off the raw line count
    // and truncated the rest of the text).
    let (area, pop) = popup(4, 2, 20, 8);
    let b = doc_box(area, pop, &["y".repeat(101)]).expect("room on the right");
    assert_eq!(b.w, 52, "capped at 50 content chars + the border ring");
    assert_eq!(b.h, 5, "ceil(101 / 50) = 3 rows, plus the border ring");
}

#[test]
fn no_box_when_neither_side_of_the_popup_has_room() {
    // The regression: the GUI drew the box at `popup.x - width` (saturating to 0)
    // regardless, landing it on top of the popup it was meant to sit beside.
    let area = CellRect::new(0, 0, 24, 24);
    let pop = CellRect::new(1, 2, 22, 8); // 1 cell left, 1 cell right — neither fits
    assert_eq!(doc_box(area, pop, &["a doc line".into()]), None);
}

#[test]
fn no_box_without_docs() {
    let (area, pop) = popup(4, 2, 20, 8);
    assert_eq!(doc_box(area, pop, &[]), None);
}

#[test]
fn the_box_is_capped_so_a_huge_doc_cannot_swallow_the_screen() {
    let area = CellRect::new(0, 0, 200, 60);
    let pop = CellRect::new(4, 2, 20, 8);
    let doc: Vec<String> = (0..500).map(|_| "z".repeat(300)).collect();
    let b = doc_box(area, pop, &doc).expect("plenty of room");
    assert_eq!(b.w, 52, "50 content cols max");
    assert_eq!(b.h, 14, "12 content rows max");
}

#[test]
fn wrap_chars_yields_exactly_the_rows_the_height_was_sized_for() {
    // The GUI paints the box's rows from `wrap_chars`, so the two must agree: a line
    // of N chars in a W-wide box is ceil(N / W) rows.
    assert_eq!(wrap_chars("abcdef", 2), ["ab", "cd", "ef"]);
    assert_eq!(wrap_chars("abcde", 2), ["ab", "cd", "e"]);
    // An empty line still occupies one row (the `max(1)` in the height math).
    assert_eq!(wrap_chars("", 4), [""]);
    // Chars, not bytes: a multi-byte char is one cell's worth here.
    assert_eq!(wrap_chars("héllo", 2), ["hé", "ll", "o"]);
}
