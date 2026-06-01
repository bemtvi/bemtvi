//! Tier 1: the crossterm `KeyEvent` -> vim key-notation translation, tested as
//! the public function the client uses. Black-box, no process, no timing.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nxvim_tui::encode_key;

fn note(code: KeyCode, mods: KeyModifiers) -> Option<String> {
    encode_key(KeyEvent::new(code, mods))
}

#[test]
fn plain_char_is_itself() {
    assert_eq!(
        note(KeyCode::Char('a'), KeyModifiers::NONE).as_deref(),
        Some("a")
    );
}

#[test]
fn special_keys_use_angle_notation() {
    assert_eq!(
        note(KeyCode::Esc, KeyModifiers::NONE).as_deref(),
        Some("<Esc>")
    );
    assert_eq!(
        note(KeyCode::Enter, KeyModifiers::NONE).as_deref(),
        Some("<CR>")
    );
    assert_eq!(
        note(KeyCode::Backspace, KeyModifiers::NONE).as_deref(),
        Some("<BS>")
    );
    assert_eq!(
        note(KeyCode::Tab, KeyModifiers::NONE).as_deref(),
        Some("<Tab>")
    );
}

#[test]
fn ctrl_and_alt_get_prefixed() {
    assert_eq!(
        note(KeyCode::Char('w'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-w>")
    );
    assert_eq!(
        note(KeyCode::Char('x'), KeyModifiers::ALT).as_deref(),
        Some("<A-x>")
    );
}

#[test]
fn literal_less_than_is_escaped() {
    assert_eq!(
        note(KeyCode::Char('<'), KeyModifiers::NONE).as_deref(),
        Some("<lt>")
    );
}

#[test]
fn navigation_keys_use_angle_notation() {
    assert_eq!(
        note(KeyCode::Left, KeyModifiers::NONE).as_deref(),
        Some("<Left>")
    );
    assert_eq!(
        note(KeyCode::Right, KeyModifiers::NONE).as_deref(),
        Some("<Right>")
    );
    assert_eq!(
        note(KeyCode::Up, KeyModifiers::NONE).as_deref(),
        Some("<Up>")
    );
    assert_eq!(
        note(KeyCode::Down, KeyModifiers::NONE).as_deref(),
        Some("<Down>")
    );
    assert_eq!(
        note(KeyCode::Home, KeyModifiers::NONE).as_deref(),
        Some("<Home>")
    );
    assert_eq!(
        note(KeyCode::End, KeyModifiers::NONE).as_deref(),
        Some("<End>")
    );
    assert_eq!(
        note(KeyCode::PageUp, KeyModifiers::NONE).as_deref(),
        Some("<PageUp>")
    );
    assert_eq!(
        note(KeyCode::PageDown, KeyModifiers::NONE).as_deref(),
        Some("<PageDown>")
    );
    assert_eq!(
        note(KeyCode::Delete, KeyModifiers::NONE).as_deref(),
        Some("<Del>")
    );
}

#[test]
fn unmapped_keys_return_none() {
    assert_eq!(note(KeyCode::F(1), KeyModifiers::NONE), None);
}

#[test]
fn combined_ctrl_alt_prefixes_both() {
    assert_eq!(
        note(
            KeyCode::Char('a'),
            KeyModifiers::CONTROL | KeyModifiers::ALT
        )
        .as_deref(),
        Some("<C-A-a>")
    );
}
