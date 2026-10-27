//! Tier 1: the client's own input events → vim key-notation, with no toolkit. (The gui's
//! `keys.rs` drives the same encoder through mitsuami's events, end to end.)

use bemtvi_gpu::{
    button_name, encode_input, is_paste, mouse_modifier, InputEvent, KeyCode, Modifiers,
    MouseButton, Point, ScrollDelta,
};

const NONE: Modifiers = Modifiers {
    shift: false,
    control: false,
    alt: false,
    meta: false,
};

fn press(code: KeyCode, text: Option<&str>, unmodified: Option<char>, mods: Modifiers) -> String {
    let event = InputEvent::Key {
        code,
        pressed: true,
        text: text.map(str::to_owned),
        unmodified,
        modifiers: mods,
    };
    encode_input(&event, false).unwrap_or_default()
}

#[test]
fn typed_text_and_named_keys() {
    assert_eq!(
        press(KeyCode::Character('a'), Some("a"), Some('a'), NONE),
        "a"
    );
    assert_eq!(
        press(KeyCode::Character('<'), Some("<"), Some('<'), NONE),
        "<lt>"
    );
    assert_eq!(press(KeyCode::Escape, None, None, NONE), "<Esc>");
    assert_eq!(press(KeyCode::F(5), None, None, NONE), "<F5>");
    let shift = Modifiers {
        shift: true,
        ..NONE
    };
    assert_eq!(press(KeyCode::Tab, None, None, shift), "<S-Tab>");
    // Past F12 isn't a key the editor names.
    assert_eq!(press(KeyCode::F(13), None, None, NONE), "");
}

#[test]
fn a_chord_is_the_layout_character_or_the_us_one() {
    let control = Modifiers {
        control: true,
        ..NONE
    };
    // The layout's character wins (a Dvorak `w` where QWERTY has `,`)…
    assert_eq!(
        press(KeyCode::Character(','), None, Some('w'), control),
        "<C-w>"
    );
    // …and the US one stands in when the host can't say.
    assert_eq!(press(KeyCode::Character('w'), None, None, control), "<C-w>");
    // A key with neither has no chord to send.
    assert_eq!(press(KeyCode::Other, None, None, control), "");
}

#[test]
fn altgr_types_what_it_typed() {
    let control_alt = Modifiers {
        control: true,
        alt: true,
        ..NONE
    };
    assert_eq!(
        press(KeyCode::Character('e'), Some("€"), Some('e'), control_alt),
        "€"
    );
}

#[test]
fn what_carries_no_keys() {
    let release = InputEvent::Key {
        code: KeyCode::Character('a'),
        pressed: false,
        text: Some("a".into()),
        unmodified: Some('a'),
        modifiers: NONE,
    };
    assert_eq!(encode_input(&release, false), None);
    let moved = InputEvent::PointerMoved {
        position: Point { x: 1.0, y: 2.0 },
        modifiers: NONE,
    };
    assert_eq!(encode_input(&moved, false), None);
    let scrolled = InputEvent::Scroll {
        delta: ScrollDelta::Lines { x: 0.0, y: 1.0 },
        modifiers: NONE,
    };
    assert_eq!(encode_input(&scrolled, false), None);
}

#[test]
fn input_methods() {
    assert_eq!(
        encode_input(&InputEvent::Text("é".into()), false).as_deref(),
        Some("é")
    );
    assert_eq!(
        encode_input(
            &InputEvent::DeleteSurrounding {
                before: 2,
                after: 1
            },
            false
        )
        .as_deref(),
        Some("<BS><BS><Del>")
    );
}

#[test]
fn paste_gestures() {
    let meta = Modifiers { meta: true, ..NONE };
    let control_shift = Modifiers {
        control: true,
        shift: true,
        ..NONE
    };
    let shift = Modifiers {
        shift: true,
        ..NONE
    };
    assert!(is_paste(KeyCode::Character('v'), Some('v'), meta));
    assert!(is_paste(KeyCode::Character('v'), Some('v'), control_shift));
    assert!(is_paste(KeyCode::Insert, None, shift));
    let control = Modifiers {
        control: true,
        ..NONE
    };
    assert!(
        !is_paste(KeyCode::Character('v'), Some('v'), control),
        "<C-v> is vim's"
    );
}

#[test]
fn mouse_names() {
    assert_eq!(button_name(MouseButton::Primary), Some("left"));
    assert_eq!(button_name(MouseButton::Middle), Some("middle"));
    assert_eq!(button_name(MouseButton::Other), None);
    let control_shift = Modifiers {
        control: true,
        shift: true,
        ..NONE
    };
    assert_eq!(
        mouse_modifier(control_shift),
        bemtvi_view::mouse_modifier(true, true, false)
    );
}
