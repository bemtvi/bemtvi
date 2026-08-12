//! Tier 1: the pure rect-subtraction the GUI float pass uses to clip a lower
//! float's text out of a higher float's opaque background. Black-box, no window,
//! no GPU. Guards the regression where stacking two floats showed the lower
//! float's text bleeding through the upper one (one overlay layer draws all
//! backgrounds before all glyphs, so the cover-up is done on the CPU here).

use bemtvi_gui::rect_subtract;

type R = (i32, i32, i32, i32); // (left, top, right, bottom)

/// Total covered area of a set of rects (they're disjoint by construction, so a
/// plain sum is the union area). Lets a test assert the pieces tile `a \ hole`.
fn area(rects: &[R]) -> i64 {
    rects
        .iter()
        .map(|&(l, t, r, b)| (r - l) as i64 * (b - t) as i64)
        .sum()
}

fn contains(rects: &[R], x: i32, y: i32) -> bool {
    rects
        .iter()
        .any(|&(l, t, r, b)| x >= l && x < r && y >= t && y < b)
}

#[test]
fn a_disjoint_hole_leaves_the_rect_whole() {
    let a: R = (0, 0, 10, 10);
    assert_eq!(rect_subtract(a, (20, 20, 30, 30)), vec![a]);
    // Edge-touching counts as disjoint (zero-area overlap).
    assert_eq!(rect_subtract(a, (10, 0, 20, 10)), vec![a]);
}

#[test]
fn a_covering_hole_erases_the_rect() {
    let a: R = (5, 5, 15, 15);
    assert!(rect_subtract(a, (0, 0, 20, 20)).is_empty());
    assert!(rect_subtract(a, a).is_empty());
}

#[test]
fn a_centered_hole_leaves_a_frame_of_four_pieces() {
    // A hole in the middle of `a` carves out exactly the four border strips, and
    // their total area is `a` minus the hole — no gaps, no double-counting.
    let a: R = (0, 0, 30, 30);
    let hole: R = (10, 10, 20, 20);
    let pieces = rect_subtract(a, hole);
    assert_eq!(pieces.len(), 4, "a fully-interior hole yields four strips");
    assert_eq!(area(&pieces), 30 * 30 - 10 * 10);
    // The hole's interior is uncovered; cells just outside it remain covered.
    assert!(!contains(&pieces, 15, 15), "the hole stays empty");
    assert!(contains(&pieces, 5, 15), "left of the hole is kept");
    assert!(contains(&pieces, 25, 15), "right of the hole is kept");
    assert!(contains(&pieces, 15, 5), "above the hole is kept");
    assert!(contains(&pieces, 15, 25), "below the hole is kept");
}

#[test]
fn a_hole_over_one_edge_leaves_only_the_uncovered_side() {
    // A float covering the right half of a text row leaves just the left half — the
    // common stacked-float case (the upper float overlaps part of the lower's row).
    let a: R = (0, 0, 20, 4);
    let pieces = rect_subtract(a, (10, 0, 25, 4));
    assert_eq!(pieces, vec![(0, 0, 10, 4)]);
    assert!(contains(&pieces, 3, 1), "the exposed left half is kept");
    assert!(!contains(&pieces, 12, 1), "the covered right half is gone");
}
