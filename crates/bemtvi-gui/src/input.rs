//! winit key events → vim key-notation.
//!
//! The GUI's analogue of the TUI's `encode_key`: it maps a winit
//! [`winit::keyboard::Key`] + the active [`ModifiersState`] onto the
//! toolkit-neutral [`bemtvi_view::Key`], then hands it to
//! [`bemtvi_view::notation`] for the `<...>` spelling the server's `btv_input`
//! expects. A key with no mapping (a bare modifier, a dead key) yields `None`
//! and is dropped — the same contract the TUI follows.

use bemtvi_view::{encode_text, notation, Key as VimKey};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Encode a winit logical key + modifiers as vim key-notation, or `None` for a
/// key with no editor meaning (bare modifiers, dead/unidentified keys).
///
/// Shift is already folded into the `Character` payload winit reports (`A` for
/// Shift+a), so for a printable key it adds no separate modifier — `notation` only
/// notates shift for **named** keys (`<S-Tab>`), where winit *does* report it as a
/// modifier (Shift+Tab arrives as `Tab` + `shift_key()`, not a folded character).
/// A control combo still arrives as the base character (`Ctrl+w` → `Character("w")`
/// + `control_key()`), which yields `<C-w>`.
pub fn encode_key(logical: &Key, mods: ModifiersState) -> Option<String> {
    // A multi-character payload (some layouts / compose fallbacks emit several
    // characters for one keystroke — winit's `Key::Character` is a string, not a
    // char) can't be a `<...>` chord; feed it through the literal-text encoder
    // (the IME-commit path) rather than silently truncating to the first char. Not
    // `encode_paste`: this is still the user typing, so it must not open a
    // bracketed-paste span.
    if let Key::Character(s) = logical {
        if s.chars().count() > 1 {
            let text = encode_text(s);
            return (!text.is_empty()).then_some(text);
        }
    }
    let key = translate(logical)?;
    Some(notation(
        mods.control_key(),
        mods.alt_key(),
        mods.shift_key(),
        key,
    ))
}

/// Whether a keystroke that arrived with **Ctrl+Alt** held is actually *AltGr
/// typing*, not a `<C-A-…>` chord. Windows reports AltGr as Ctrl+Alt, so `AltGr+E`
/// on a European layout arrives as `Character("€")` + `control_key()` +
/// `alt_key()` — encoding that as a chord of the un-composed base key sends
/// `<C-A-e>` and the typed `€` never reaches the buffer. The tell is the layout
/// having *composed* the logical key into a different character than the
/// unmodified base key (`logical` vs winit's `key_without_modifiers`).
///
/// Deliberately conservative:
/// - Shift held ⇒ `false` — Shift folds into the logical character (`#` from `3`),
///   so a difference proves nothing; a `<C-A-S-…>` chord keeps chord behavior.
/// - A case-only difference ⇒ `false` — that's CapsLock, not composition.
/// - Anything but Ctrl+Alt on two `Character` keys ⇒ `false`.
///
/// The caller additionally skips this on macOS, where Option composes on its own
/// and is deliberately mapped to `<A-…>` chords via `key_without_modifiers`.
pub fn altgr_composed(logical: &Key, base: &Key, mods: ModifiersState) -> bool {
    if !mods.control_key() || !mods.alt_key() || mods.shift_key() || mods.super_key() {
        return false;
    }
    match (logical, base) {
        (Key::Character(l), Key::Character(b)) => l.to_lowercase() != b.to_lowercase(),
        _ => false,
    }
}

/// Whether `logical` + `mods` is a "paste from system clipboard" gesture: Cmd+V
/// (macOS), Ctrl+Shift+V (Linux/Windows terminals), or Shift+Insert (universal).
/// None of these collide with vim's `<C-v>` (literal-insert / blockwise visual),
/// so the client can claim them without shadowing an editor key. The client reads
/// the clipboard and feeds it through [`bemtvi_view::encode_paste`].
pub fn is_paste(logical: &Key, mods: ModifiersState) -> bool {
    let is_v = matches!(logical, Key::Character(c) if c.eq_ignore_ascii_case("v"));
    let shift_insert = matches!(logical, Key::Named(NamedKey::Insert)) && mods.shift_key();
    (is_v && mods.super_key()) || (is_v && mods.control_key() && mods.shift_key()) || shift_insert
}

/// Map a winit logical key onto the neutral [`VimKey`], or `None` if it has no
/// editor meaning.
fn translate(logical: &Key) -> Option<VimKey> {
    match logical {
        // A typed character (already shifted by the platform layout).
        Key::Character(s) => s.chars().next().map(VimKey::Char),
        Key::Named(named) => match named {
            NamedKey::Space => Some(VimKey::Char(' ')),
            NamedKey::Escape => Some(VimKey::Esc),
            NamedKey::Enter => Some(VimKey::Enter),
            NamedKey::Backspace => Some(VimKey::Backspace),
            NamedKey::Tab => Some(VimKey::Tab),
            NamedKey::Delete => Some(VimKey::Delete),
            NamedKey::ArrowLeft => Some(VimKey::Left),
            NamedKey::ArrowRight => Some(VimKey::Right),
            NamedKey::ArrowUp => Some(VimKey::Up),
            NamedKey::ArrowDown => Some(VimKey::Down),
            NamedKey::Home => Some(VimKey::Home),
            NamedKey::End => Some(VimKey::End),
            NamedKey::PageUp => Some(VimKey::PageUp),
            NamedKey::PageDown => Some(VimKey::PageDown),
            // Function keys F1-F12 (winit names each variant individually).
            NamedKey::F1 => Some(VimKey::Function(1)),
            NamedKey::F2 => Some(VimKey::Function(2)),
            NamedKey::F3 => Some(VimKey::Function(3)),
            NamedKey::F4 => Some(VimKey::Function(4)),
            NamedKey::F5 => Some(VimKey::Function(5)),
            NamedKey::F6 => Some(VimKey::Function(6)),
            NamedKey::F7 => Some(VimKey::Function(7)),
            NamedKey::F8 => Some(VimKey::Function(8)),
            NamedKey::F9 => Some(VimKey::Function(9)),
            NamedKey::F10 => Some(VimKey::Function(10)),
            NamedKey::F11 => Some(VimKey::Function(11)),
            NamedKey::F12 => Some(VimKey::Function(12)),
            _ => None,
        },
        _ => None,
    }
}
