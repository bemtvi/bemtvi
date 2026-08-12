//! Crossterm key events → vim key-notation.
//!
//! The notation rules and the bracketed-paste encoder are frontend-agnostic and
//! live in [`bemtvi_view`]; this module only maps a crossterm [`KeyEvent`] onto the
//! neutral [`Key`](bemtvi_view::Key) and defers to [`bemtvi_view::notation`].

use bemtvi_view::{notation, Key};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Translate a crossterm key event into vim key-notation.
///
/// Public so the crossterm -> vim key-notation contract can be exercised by
/// integration tests in `bemtvi-tui/tests/keys.rs`. Returns `None` for a key code
/// with no notation mapping (so the caller drops it).
pub fn encode_key(ev: KeyEvent) -> Option<String> {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);
    let mut shift = ev.modifiers.contains(KeyModifiers::SHIFT);

    // A legacy terminal sends the C0 control bytes 0x1C..=0x1F for CTRL-\, CTRL-],
    // CTRL-^ and CTRL-_, but crossterm decodes them as Ctrl+'4'..'7' (its unix
    // parse.rs maps `c - 0x1C + b'4'`). This is the fallback for when the kitty
    // keyboard protocol is *off* (a terminal that doesn't support it, or a query
    // that timed out — see `KeyboardEnhancement` in lib.rs): without the protocol a
    // real digit can't reach here carrying CONTROL, so these always came from those
    // bytes (an ESC-prefixed one additionally carries ALT). Remap them to vim's
    // canonical notation so bindings like <C-]> (help tag jump) and <C-^> (alternate
    // file) actually fire instead of arriving as <C-5>/<C-6> and matching nothing;
    // the Alt modifier rides along (`<C-A-]>`). When the protocol *is* on, the
    // terminal reports `<C-\>` &c. directly as their real char + CONTROL, so this
    // branch never matches and is harmlessly inert.
    if ctrl {
        if let KeyCode::Char(c @ '4'..='7') = ev.code {
            let name = match c {
                '4' => '\\',
                '5' => ']',
                '6' => '^',
                _ => '_',
            };
            let a = if alt { "A-" } else { "" };
            return Some(format!("<C-{a}{name}>"));
        }
    }

    let key = match ev.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Esc => Key::Esc,
        KeyCode::Enter => Key::Enter,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Tab => Key::Tab,
        // crossterm reports Shift+Tab as the dedicated `BackTab` code — usually
        // *without* the SHIFT modifier — so fold it back into Tab + shift here, so
        // it reaches the server as `<S-Tab>` (the cmdline wildmenu / snippet tabstop
        // backward key). Without this it hit the catch-all and was silently dropped.
        KeyCode::BackTab => {
            shift = true;
            Key::Tab
        }
        KeyCode::Delete => Key::Delete,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n) => Key::Function(n),
        _ => return None,
    };
    Some(notation(ctrl, alt, shift, key))
}
