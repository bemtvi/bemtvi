//! mitsuami's surface input → `bemtvi-gpu`'s [`InputEvent`], and the key and mouse
//! helpers over mitsuami's types the gui's tests drive end to end.
//!
//! mitsuami reports each key on the editor's surface three ways — where it is on the
//! keyboard ([`m::KeyCode`]), what it typed through the platform's text input, and the
//! character it types with no modifiers in the keyboard's layout — which carry over
//! unchanged. Its pointer positions are points over the surface, as the client takes
//! them.

use bemtvi_gpu::{InputEvent, KeyCode, Modifiers, MouseButton, Point, ScrollDelta};
use mitsuami::prelude as m;

/// The client's event for a mitsuami surface input, or `None` for one the client has
/// no use for (the pointer leaving, raw motion).
pub fn input_event(input: &m::SurfaceInput) -> Option<InputEvent> {
    Some(match input {
        m::SurfaceInput::Key {
            code,
            pressed,
            modifiers,
            text,
            unmodified,
            ..
        } => InputEvent::Key {
            code: key_code(*code),
            pressed: *pressed,
            text: text.clone(),
            unmodified: *unmodified,
            modifiers: modifiers_of(*modifiers),
        },
        m::SurfaceInput::Text(text) => InputEvent::Text(text.clone()),
        m::SurfaceInput::Preedit { text, cursor } => InputEvent::Preedit {
            text: text.clone(),
            cursor: *cursor,
        },
        m::SurfaceInput::DeleteSurrounding { before, after } => InputEvent::DeleteSurrounding {
            before: *before,
            after: *after,
        },
        m::SurfaceInput::PointerMoved {
            position,
            modifiers,
        } => InputEvent::PointerMoved {
            position: point(*position),
            modifiers: modifiers_of(*modifiers),
        },
        m::SurfaceInput::Button {
            button,
            pressed,
            position,
            modifiers,
        } => InputEvent::Button {
            button: button_of(*button),
            pressed: *pressed,
            position: point(*position),
            modifiers: modifiers_of(*modifiers),
        },
        m::SurfaceInput::Scroll { delta, modifiers } => InputEvent::Scroll {
            delta: match *delta {
                m::ScrollDelta::Lines { x, y } => ScrollDelta::Lines { x, y },
                m::ScrollDelta::Points { x, y } => ScrollDelta::Points { x, y },
            },
            modifiers: modifiers_of(*modifiers),
        },
        _ => return None,
    })
}

/// The modifiers held, in the client's type.
pub fn modifiers_of(mods: m::Modifiers) -> Modifiers {
    Modifiers {
        shift: mods.shift,
        control: mods.control,
        alt: mods.alt,
        meta: mods.meta,
    }
}

/// A key's place, in the client's type: the keys it names by name, the rest by the
/// character they type on a US layout.
pub fn key_code(code: m::KeyCode) -> KeyCode {
    use m::KeyCode as K;
    match code {
        K::Escape => KeyCode::Escape,
        K::Enter => KeyCode::Enter,
        K::NumpadEnter => KeyCode::NumpadEnter,
        K::Backspace => KeyCode::Backspace,
        K::Tab => KeyCode::Tab,
        K::Delete => KeyCode::Delete,
        K::Insert => KeyCode::Insert,
        K::ArrowLeft => KeyCode::ArrowLeft,
        K::ArrowRight => KeyCode::ArrowRight,
        K::ArrowUp => KeyCode::ArrowUp,
        K::ArrowDown => KeyCode::ArrowDown,
        K::Home => KeyCode::Home,
        K::End => KeyCode::End,
        K::PageUp => KeyCode::PageUp,
        K::PageDown => KeyCode::PageDown,
        K::F1 => KeyCode::F(1),
        K::F2 => KeyCode::F(2),
        K::F3 => KeyCode::F(3),
        K::F4 => KeyCode::F(4),
        K::F5 => KeyCode::F(5),
        K::F6 => KeyCode::F(6),
        K::F7 => KeyCode::F(7),
        K::F8 => KeyCode::F(8),
        K::F9 => KeyCode::F(9),
        K::F10 => KeyCode::F(10),
        K::F11 => KeyCode::F(11),
        K::F12 => KeyCode::F(12),
        other => other.us_char().map_or(KeyCode::Other, KeyCode::Character),
    }
}

fn button_of(button: m::MouseButton) -> MouseButton {
    match button {
        m::MouseButton::Primary => MouseButton::Primary,
        m::MouseButton::Secondary => MouseButton::Secondary,
        m::MouseButton::Middle => MouseButton::Middle,
        _ => MouseButton::Other,
    }
}

fn point(p: m::Point) -> Point {
    Point { x: p.x, y: p.y }
}

/// [`bemtvi_gpu::encode_input`] for a mitsuami input: vim key-notation, or `None` for
/// one with no editor meaning.
pub fn encode_input(input: &m::SurfaceInput, option_chords: bool) -> Option<String> {
    bemtvi_gpu::encode_input(&input_event(input)?, option_chords)
}

/// [`bemtvi_gpu::is_paste`] for a mitsuami key: Cmd+V, Ctrl+Shift+V or Shift+Insert.
pub fn is_paste(code: m::KeyCode, unmodified: Option<char>, mods: m::Modifiers) -> bool {
    bemtvi_gpu::is_paste(key_code(code), unmodified, modifiers_of(mods))
}

/// [`bemtvi_gpu::button_name`] for a mitsuami button.
pub fn button_name(button: m::MouseButton) -> Option<&'static str> {
    bemtvi_gpu::button_name(button_of(button))
}

/// [`bemtvi_gpu::mouse_modifier`] for mitsuami's modifier flags.
pub fn mouse_modifier(mods: m::Modifiers) -> String {
    bemtvi_gpu::mouse_modifier(modifiers_of(mods))
}
