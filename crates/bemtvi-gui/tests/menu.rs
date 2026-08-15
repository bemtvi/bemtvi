//! Tier 1: where the floating menu box's top-left cell goes. Black-box, no window,
//! no GPU.
//!
//! Guards the reported bug: the **command-line wildmenu** was anchored at the
//! focused window's region origin, like every other menu. The command line is not
//! inside that region — it is a screen row spanning the full width at column 0 — so
//! with a left dock open (a file tree, `:Plugins`, any left-docked panel) the
//! completion list floated a dock's width to the right of the token it was
//! completing, while the command line itself stayed put. The docs float beside it
//! looked right because it goes through the normal window path with server-computed
//! screen geometry, which is what made the offset so obvious.

use bemtvi_gui::menu_box_origin;

/// A left dock 30 cells wide: the focused window's text anchor starts past it.
const DOCKED_ANCHOR: (u16, u16) = (30, 4);
/// The command line's screen row on a 25-row grid.
const CMD_ROW: u16 = 24;

#[test]
fn the_cmdline_wildmenu_ignores_the_focused_regions_origin() {
    // `:e src/` with a file tree docked left. `menu.col` (3) is a column *within the
    // command line*, so the box belongs at screen column 3 — the bug put it at 33,
    // out over the buffer text and nowhere near the token being completed.
    let (x, _) = menu_box_origin(true, DOCKED_ANCHOR, 3, 0, 0, CMD_ROW, 6);
    assert_eq!(
        x, 3,
        "the wildmenu must anchor to the screen, not the region"
    );
}

#[test]
fn the_cmdline_wildmenu_grows_upward_from_the_command_row() {
    // It floats *above* the input, flush against it: the box's bottom edge is the
    // row above the command line, so its top is `cmd_row - box_h`.
    let (_, y) = menu_box_origin(true, DOCKED_ANCHOR, 3, 0, 0, CMD_ROW, 6);
    assert_eq!(y, 18);
}

#[test]
fn a_wildmenu_taller_than_the_screen_clamps_to_the_top() {
    // More candidates than rows above the command line: the box starts at row 0
    // rather than wrapping around into the bottom of the grid.
    let (_, y) = menu_box_origin(true, DOCKED_ANCHOR, 0, 0, 0, 5, 40);
    assert_eq!(y, 0);
}

#[test]
fn a_window_anchored_menu_keeps_the_region_origin() {
    // The insert-completion popup / `btv.ui.select` anchor inside the focused
    // window, so they DO carry the region origin — the branch the wildmenu was
    // wrongly sharing. `menu.col` 5 past a text anchor at 30, one cell left so the
    // box's left border doesn't push the list off the word, and `menu.row` 2 below.
    assert_eq!(
        menu_box_origin(false, DOCKED_ANCHOR, 5, 2, 1, CMD_ROW, 6),
        (34, 6)
    );
}

#[test]
fn a_window_anchored_menu_at_column_zero_does_not_underflow() {
    // A bordered popup (`left_shift` 0) at the very left of an undocked window.
    assert_eq!(menu_box_origin(false, (0, 0), 0, 0, 1, CMD_ROW, 6), (0, 0));
}
