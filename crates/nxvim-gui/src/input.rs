//! winit key events → vim key-notation.
//!
//! The GUI's analogue of the TUI's `encode_key`: it maps a winit
//! [`winit::keyboard::Key`] + the active [`ModifiersState`] onto the
//! toolkit-neutral [`nxvim_view::Key`], then hands it to
//! [`nxvim_view::notation`] for the `<...>` spelling the server's `nvim_input`
//! expects. A key with no mapping (a bare modifier, a dead key) yields `None`
//! and is dropped — the same contract the TUI follows.

use nxvim_view::{notation, Key as VimKey};
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
    let key = translate(logical)?;
    Some(notation(
        mods.control_key(),
        mods.alt_key(),
        mods.shift_key(),
        key,
    ))
}

/// Whether `logical` + `mods` is a "paste from system clipboard" gesture: Cmd+V
/// (macOS), Ctrl+Shift+V (Linux/Windows terminals), or Shift+Insert (universal).
/// None of these collide with vim's `<C-v>` (literal-insert / blockwise visual),
/// so the client can claim them without shadowing an editor key. The client reads
/// the clipboard and feeds it through [`nxvim_view::encode_paste`].
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
            _ => None,
        },
        _ => None,
    }
}
