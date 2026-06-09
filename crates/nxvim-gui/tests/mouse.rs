//! Tier 1: the pure mouse helpers the GUI client feeds winit pointer events
//! through — pixel→cell conversion, the `nvim_input_mouse` modifier string, the
//! wheel-notch accumulator and direction, and the panel overlay hit-rects.
//! Black-box, no window, no GPU — the mouse analogue of the `keys` test. The
//! winit→RPC wiring itself lives in the event loop and isn't unit-testable
//! without a window; these cover the math it depends on.

use nxvim_gui::{
    button_name, cell_at, drain_notches, horizontal_action, mouse_modifier, panel_close_button,
    panel_content_rect, vertical_action, within,
};
use winit::event::MouseButton;
use winit::keyboard::ModifiersState;

#[test]
fn button_name_maps_the_three_forwarded_buttons() {
    assert_eq!(button_name(MouseButton::Left), Some("left"));
    assert_eq!(button_name(MouseButton::Right), Some("right"));
    assert_eq!(button_name(MouseButton::Middle), Some("middle"));
    // Buttons the server has no gesture for are dropped, not forwarded.
    assert_eq!(button_name(MouseButton::Back), None);
    assert_eq!(button_name(MouseButton::Forward), None);
    assert_eq!(button_name(MouseButton::Other(9)), None);
}

#[test]
fn modifier_string_is_ctrl_shift_alt_order() {
    assert_eq!(mouse_modifier(ModifiersState::empty()), "");
    assert_eq!(mouse_modifier(ModifiersState::SHIFT), "S");
    assert_eq!(mouse_modifier(ModifiersState::CONTROL), "C");
    assert_eq!(mouse_modifier(ModifiersState::ALT), "A");
    // Concatenated in a fixed C, S, A order (the server parses any order).
    assert_eq!(
        mouse_modifier(ModifiersState::CONTROL | ModifiersState::SHIFT),
        "CS"
    );
    assert_eq!(
        mouse_modifier(ModifiersState::CONTROL | ModifiersState::SHIFT | ModifiersState::ALT),
        "CSA"
    );
}

#[test]
fn pixel_position_maps_to_the_cell_it_falls_in() {
    // 8px-wide, 16px-tall cells: floor(px / cell).
    assert_eq!(cell_at(0.0, 0.0, 8.0, 16.0), (0, 0));
    assert_eq!(cell_at(7.9, 15.9, 8.0, 16.0), (0, 0)); // still cell (0,0)
    assert_eq!(cell_at(8.0, 16.0, 8.0, 16.0), (1, 1)); // crosses into (1,1)
    assert_eq!(cell_at(20.0, 35.0, 8.0, 16.0), (2, 2));
    // A negative coordinate (pointer left the window top/left) clamps to 0.
    assert_eq!(cell_at(-5.0, -5.0, 8.0, 16.0), (0, 0));
}

#[test]
fn within_is_half_open_on_each_axis() {
    // [x, x+w) × [y, y+h): the top-left corner is inside, the bottom-right isn't.
    assert!(within(2, 3, 2, 3, 4, 2));
    assert!(within(5, 4, 2, 3, 4, 2)); // x=5 is the last column (2+4-1)
    assert!(!within(6, 3, 2, 3, 4, 2)); // x=6 == x+w is outside
    assert!(!within(2, 5, 2, 3, 4, 2)); // y=5 == y+h is outside
    assert!(!within(1, 3, 2, 3, 4, 2)); // left of the rect
}

#[test]
fn wheel_notches_emit_whole_lines_and_keep_the_remainder() {
    // A wheel mouse: one whole line per detent, emitted immediately.
    let mut acc = 0.0;
    assert_eq!(drain_notches(1.0, &mut acc), 1);
    assert_eq!(drain_notches(-2.0, &mut acc), -2);
    // A trackpad: fractional pixels-as-lines accumulate until they cross a line.
    let mut acc = 0.0;
    assert_eq!(drain_notches(0.4, &mut acc), 0);
    assert_eq!(drain_notches(0.4, &mut acc), 0);
    assert_eq!(drain_notches(0.4, &mut acc), 1); // 1.2 → emit 1, keep 0.2

    // Direction never flips from rounding: a small negative stays non-positive.
    let mut acc = 0.0;
    assert_eq!(drain_notches(-0.3, &mut acc), 0);
}

#[test]
fn wheel_direction_follows_winit_sign_convention() {
    // winit positive delta = content moves down/right (reveals earlier content),
    // i.e. a scroll up / left.
    assert_eq!(vertical_action(1), Some("up"));
    assert_eq!(vertical_action(-1), Some("down"));
    assert_eq!(vertical_action(0), None);
    assert_eq!(horizontal_action(1), Some("left"));
    assert_eq!(horizontal_action(-1), Some("right"));
    assert_eq!(horizontal_action(0), None);
}

#[test]
fn panel_overlay_rects_sit_above_the_command_row() {
    // A 80×24 grid with a 5-row-content panel: the panel claims height+1 = 6 rows
    // above the one command row, so its border row is 24 - 1 - 6 = 17.
    let (brow, bcols) = panel_close_button(80, 24, 5).unwrap();
    assert_eq!(brow, 17);
    assert_eq!(bcols, 77..80); // the `[X]` is the last three columns

    // Content sits one row below the border, 5 rows tall, full width.
    assert_eq!(panel_content_rect(80, 24, 5), Some((0, 18, 80, 5)));
    // No room (a tiny grid) → None rather than a wrapped rect.
    assert_eq!(panel_close_button(2, 3, 5), None);
    assert_eq!(panel_content_rect(80, 3, 5), None);
}
