//! Crossterm key events → vim key-notation.
//!
//! The notation rules and the bracketed-paste encoder are frontend-agnostic and
//! live in [`nxvim_view`]; this module only maps a crossterm [`KeyEvent`] onto the
//! neutral [`Key`](nxvim_view::Key) and defers to [`nxvim_view::notation`].

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nxvim_view::{notation, Key};

/// Translate a crossterm key event into vim key-notation.
///
/// Public so the crossterm -> vim key-notation contract can be exercised by
/// integration tests in `nxvim-tui/tests/keys.rs`. Returns `None` for a key code
/// with no notation mapping (so the caller drops it).
pub fn encode_key(ev: KeyEvent) -> Option<String> {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let key = match ev.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Tab => Key::Tab,
        KeyCode::Delete => Key::Delete,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        _ => return None,
    };
    Some(notation(ctrl, alt, key))
}
