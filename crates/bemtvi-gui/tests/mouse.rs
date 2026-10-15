//! Tier 1: the pure mouse helpers the GUI client feeds the surface's pointer events
//! through — pixel→cell conversion, the `btv_input_mouse` modifier string, and
//! the wheel-notch accumulator and direction. Black-box, no window, no GPU — the
//! mouse analogue of the `keys` test. The surface→RPC wiring itself lives in the
//! client and isn't unit-testable without a window; these cover the math it
//! depends on.

use bemtvi_gui::{
    button_name, cell_at, drain_notches, horizontal_action, mouse_modifier, vertical_action,
};
use mitsuami::prelude::{Modifiers, MouseButton};

#[test]
fn button_name_maps_the_three_forwarded_buttons() {
    assert_eq!(button_name(MouseButton::Primary), Some("left"));
    assert_eq!(button_name(MouseButton::Secondary), Some("right"));
    assert_eq!(button_name(MouseButton::Middle), Some("middle"));
    // Buttons the server has no gesture for are dropped, not forwarded.
    assert_eq!(button_name(MouseButton::Back), None);
    assert_eq!(button_name(MouseButton::Forward), None);
    assert_eq!(button_name(MouseButton::Other(9)), None);
}

#[test]
fn modifier_string_is_ctrl_shift_alt_order() {
    let none = Modifiers::default();
    let shift = Modifiers {
        shift: true,
        ..none
    };
    let control = Modifiers {
        control: true,
        ..none
    };
    let alt = Modifiers { alt: true, ..none };
    assert_eq!(mouse_modifier(none), "");
    assert_eq!(mouse_modifier(shift), "S");
    assert_eq!(mouse_modifier(control), "C");
    assert_eq!(mouse_modifier(alt), "A");
    // Concatenated in a fixed C, S, A order (the server parses any order).
    assert_eq!(
        mouse_modifier(Modifiers {
            shift: true,
            ..control
        }),
        "CS"
    );
    assert_eq!(
        mouse_modifier(Modifiers {
            shift: true,
            alt: true,
            ..control
        }),
        "CSA"
    );
    // Command / Super has no mouse modifier letter.
    assert_eq!(mouse_modifier(Modifiers { meta: true, ..none }), "");
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
fn wheel_notches_emit_whole_lines_and_keep_the_remainder() {
    // A wheel mouse: one whole line per detent, emitted immediately.
    let mut acc = 0.0;
    assert_eq!(drain_notches(1.0, &mut acc), 1);
    assert_eq!(drain_notches(-2.0, &mut acc), -2);
    // A trackpad: fractional points-as-lines accumulate until they cross a line.
    let mut acc = 0.0;
    assert_eq!(drain_notches(0.4, &mut acc), 0);
    assert_eq!(drain_notches(0.4, &mut acc), 0);
    assert_eq!(drain_notches(0.4, &mut acc), 1); // 1.2 → emit 1, keep 0.2

    // Direction never flips from rounding: a small negative stays non-positive.
    let mut acc = 0.0;
    assert_eq!(drain_notches(-0.3, &mut acc), 0);
}

#[test]
fn wheel_direction_follows_mitsuami_sign_convention() {
    // mitsuami's positive scroll goes towards the end of the content (the user's
    // scrolling direction already applied): down for y, right for x.
    assert_eq!(vertical_action(1), Some("down"));
    assert_eq!(vertical_action(-1), Some("up"));
    assert_eq!(vertical_action(0), None);
    assert_eq!(horizontal_action(1), Some("right"));
    assert_eq!(horizontal_action(-1), Some("left"));
    assert_eq!(horizontal_action(0), None);
}
