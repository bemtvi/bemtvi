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
fn shift_tab_is_back_tab_notation() {
    // crossterm reports Shift+Tab as the dedicated `BackTab` code (no SHIFT
    // modifier on most terminals); it must reach the server as `<S-Tab>`, the
    // notation the cmdline wildmenu / snippet tabstop nav bind. Previously it hit
    // the catch-all and was dropped, so Shift+Tab did nothing.
    assert_eq!(
        note(KeyCode::BackTab, KeyModifiers::NONE).as_deref(),
        Some("<S-Tab>")
    );
    // Some terminals (kitty/CSI-u protocols) instead send Tab + SHIFT — same key.
    assert_eq!(
        note(KeyCode::BackTab, KeyModifiers::SHIFT).as_deref(),
        Some("<S-Tab>")
    );
    assert_eq!(
        note(KeyCode::Tab, KeyModifiers::SHIFT).as_deref(),
        Some("<S-Tab>")
    );
}

#[test]
fn legacy_c0_control_bytes_map_to_vim_notation() {
    // A legacy terminal (no kitty keyboard protocol — nxvim doesn't enable it)
    // sends the C0 bytes 0x1C..=0x1F for CTRL-\, CTRL-], CTRL-^ and CTRL-_.
    // crossterm decodes those bytes as Ctrl+'4'..'7' (see its unix parse.rs), so
    // without remapping, <C-]> (help tag jump), <C-^> (alternate file) and the
    // others never reach the server and the keypress does nothing.
    assert_eq!(
        note(KeyCode::Char('4'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-\\>")
    );
    assert_eq!(
        note(KeyCode::Char('5'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-]>")
    );
    assert_eq!(
        note(KeyCode::Char('6'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-^>")
    );
    assert_eq!(
        note(KeyCode::Char('7'), KeyModifiers::CONTROL).as_deref(),
        Some("<C-_>")
    );
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
