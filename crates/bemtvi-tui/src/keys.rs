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
///
/// Assumes the kitty keyboard protocol is **off** (the legacy encoding the C0
/// fallback below exists for); the live event loop calls
/// [`encode_key_with`](encode_key_with) with the actual protocol state.
pub fn encode_key(ev: KeyEvent) -> Option<String> {
    encode_key_with(ev, false)
}

/// [`encode_key`] with knowledge of whether the kitty keyboard protocol is
/// active on the terminal (the `keyboard_protocol` capability the client pushed
/// at attach). The protocol changes what a `Char('4'..'7') + CONTROL` event can
/// mean — see the C0 fallback below — so the caller that knows must say so.
pub fn encode_key_with(ev: KeyEvent, kitty_keyboard: bool) -> Option<String> {
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
    // the Alt modifier rides along (`<C-A-]>`).
    //
    // Under the kitty protocol the same crossterm event is *not* a legacy byte: the
    // terminal reports the real keys as CSI-u sequences (`CSI 52;4u` for Ctrl+4,
    // `CSI 28;4u` for Ctrl+\), and crossterm decodes a digit's codepoint into the
    // same `Char('4'..'7') + CONTROL` shape. Remapping there would fold the real
    // Ctrl+4/5/6/7 keys onto `<C-^>`/`<C-_>`/`<C-\>`/`<C-]>` — exactly the
    // distinguishability the protocol was pushed to buy — so the fallback must be
    // gated on the protocol being off. (The terminal reports `<C-\>` &c. directly
    // as their real char + CONTROL once the protocol is on, so nothing the fallback
    // exists for is lost.)
    if ctrl && !kitty_keyboard {
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

    // The other CSI-u shape: an xterm-family terminal reports a Ctrl chord by its
    // *control* codepoint (`CSI 28;5u` for Ctrl+\) rather than the base key's
    // printable codepoint (`CSI 92;5u` for the backslash key itself). crossterm
    // decodes that as `Char('\x1c') + CONTROL` — a shape the legacy C0 decode
    // never produces (it yields '4'..'7', handled above) — and the notation
    // encoder has no mapping for it: emitting the raw control char inside
    // `<C-…>` sends a byte the server parses as a key that matches nothing, so a
    // `<C-\>` mapping silently dies on those terminals. Canonicalize the control
    // codepoints to the keys they fold from: 0x1C..=0x1F are the four named keys
    // above, 0x01..=0x1A fold back to their letters (lowercased — the neovim
    // model), and 0x00 is the Ctrl+Space chord of an XKB-off xterm (its keysym
    // for Ctrl+Space / Ctrl+@ is NUL), folded onto `<C-Space>` — the same key the
    // kitty/XKB-on paths report, so the default `<C-Space>` completion trigger
    // survives there. Correct with the kitty protocol on or off: a terminal only
    // sends a control codepoint for the Ctrl chord that produces it, so no
    // distinct key is folded onto another. (Tab/CR/Esc never reach here —
    // crossterm maps their codepoints to the named key codes first.)
    if ctrl {
        if let KeyCode::Char(c) = ev.code {
            if (c as u32) < 0x20 && c != '\t' && c != '\r' && c != '\x1b' {
                let name = match c {
                    '\0' => String::from("space"),
                    '\x1c' => String::from("\\"),
                    '\x1d' => String::from("]"),
                    '\x1e' => String::from("^"),
                    '\x1f' => String::from("_"),
                    _ => ((c as u8 + 0x40) as char).to_ascii_lowercase().to_string(),
                };
                let a = if alt { "A-" } else { "" };
                let s = if shift { "S-" } else { "" };
                return Some(format!("<C-{a}{s}{name}>"));
            }
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
