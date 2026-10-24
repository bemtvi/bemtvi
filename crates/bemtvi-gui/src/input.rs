//! Surface keys → vim key-notation.
//!
//! The GUI's analogue of the TUI's `encode_key`. mitsuami reports each key on the
//! editor's surface three ways: where it is on the keyboard ([`KeyCode`]), what it
//! typed through the platform's text input (`text`: Shift, AltGr, Option and dead
//! keys already composed in), and the character it types with no modifiers in the
//! keyboard's layout (`unmodified`). This maps a press onto the toolkit-neutral
//! [`bemtvi_view::Key`] and hands it to [`bemtvi_view::notation`] for the `<...>`
//! spelling the server's `btv_input` expects. A key with no editor meaning (a bare
//! modifier, a dead key) yields `None` and is dropped — the same contract the TUI
//! follows.

use bemtvi_view::{encode_text, notation, notation_cmd, Key as VimKey};
use mitsuami::prelude::{KeyCode, Modifiers, SurfaceInput};

/// Encode a surface input as vim key-notation, or `None` for one with no editor
/// meaning: a key release, a bare modifier, a dead key.
/// `option_chords` is macOS's policy (below).
///
/// - **Named keys** (Escape, Enter, arrows, F-keys, …) come from where the key is,
///   with their modifiers notated (`<S-Tab>`, `<C-Left>`).
/// - **A chord** (Control, Alt or Command held) is the key's layout character with
///   the modifiers: `<C-w>`, `<A-x>`, `<D-z>` (Command is neovim's `D-`, Super off
///   macOS). Command+V never gets here: the caller claims it as a paste. Control+I/M/[/H stay `<C-i>`/`<C-m>`/`<C-[>`/`<C-h>`,
///   apart from Tab/Enter/Escape/Backspace, since those are named by their key.
///   On macOS Option composes a character (Option+C types `ç`); `option_chords`
///   makes it `<A-c>` instead, so `<A-…>` bindings work there too.
/// - **AltGr typing.** Windows reports AltGr as Control+Alt, so `AltGr+E` (`€` on
///   European layouts) arrives with both held — but it *typed* `€`, which the
///   platform says in `text`, while a real Control+Alt chord types nothing. So a key
///   that typed text with Control and Alt held types it, unmodified.
/// - **Anything else** sends what it typed: Shift is already folded in (`A`, `!`),
///   and a dead key's accent with it (`é`). Several characters (a dead key that
///   didn't compose, `´x`) go through the literal-text encoder rather than being
///   truncated — still typing, so not `encode_paste`.
/// - **An input method's commit** ([`SurfaceInput::Text`]) is typed text too
///   (composed accents, CJK).
/// - **An input method taking back typed text** ([`SurfaceInput::DeleteSurrounding`],
///   macOS's accents replacing the letter a held key typed) is `<BS>`s before the
///   cursor and `<Del>`s after it.
pub fn encode_input(input: &SurfaceInput, option_chords: bool) -> Option<String> {
    let (code, text, unmodified, mods) = match input {
        SurfaceInput::Key {
            code,
            pressed: true,
            text,
            unmodified,
            modifiers,
            ..
        } => (*code, text.as_deref(), *unmodified, *modifiers),
        SurfaceInput::Text(text) => return typed(text),
        SurfaceInput::DeleteSurrounding { before, after } => {
            let keys = "<BS>".repeat(*before) + &"<Del>".repeat(*after);
            return Some(keys).filter(|k| !k.is_empty());
        }
        _ => return None,
    };
    if let Some(key) = named(code) {
        return Some(notation_cmd(
            mods.control,
            mods.alt,
            mods.meta,
            mods.shift,
            key,
        ));
    }
    let altgr = !option_chords && mods.control && mods.alt;
    if let Some(text) = text.filter(|_| altgr || !(mods.control || mods.alt || mods.meta)) {
        return typed(text);
    }
    if mods.control || mods.alt || mods.meta {
        // The key's own character, Shift carried as `S-` (`<C-S-c>`, `<D-S-z>`).
        let base = unmodified.or_else(|| code.us_char())?;
        return Some(notation_cmd(
            mods.control,
            mods.alt,
            mods.meta,
            mods.shift,
            VimKey::Char(base),
        ));
    }
    None
}

/// Encode text the user typed (one key's characters, or an input method's commit):
/// a single character through [`notation`] (so `<` is `<lt>`), several through the
/// literal-text encoder. `None` for nothing typed.
fn typed(text: &str) -> Option<String> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (None, _) => None,
        (Some(c), None) => Some(notation(false, false, false, VimKey::Char(c))),
        _ => Some(encode_text(text)).filter(|t| !t.is_empty()),
    }
}

/// Whether a key press is a "paste from system clipboard" gesture: Cmd+V (macOS),
/// Ctrl+Shift+V (Linux/Windows terminals), or Shift+Insert (universal). None of these
/// collide with vim's `<C-v>` (literal-insert / blockwise visual), so the client can
/// claim them without shadowing an editor key. `V` is the layout's, not the key's
/// place (`unmodified`), as a shortcut names it.
pub fn is_paste(code: KeyCode, unmodified: Option<char>, mods: Modifiers) -> bool {
    let is_v = unmodified.is_some_and(|c| c.eq_ignore_ascii_case(&'v'));
    let shift_insert = code == KeyCode::Insert && mods.shift;
    (is_v && ((mods.meta && !mods.control) || (mods.control && mods.shift))) || shift_insert
}

/// The neutral [`VimKey`] for a key that types no character, by where it is.
fn named(code: KeyCode) -> Option<VimKey> {
    use KeyCode::*;
    Some(match code {
        Escape => VimKey::Esc,
        Enter | NumpadEnter => VimKey::Enter,
        Backspace => VimKey::Backspace,
        Tab => VimKey::Tab,
        Delete => VimKey::Delete,
        ArrowLeft => VimKey::Left,
        ArrowRight => VimKey::Right,
        ArrowUp => VimKey::Up,
        ArrowDown => VimKey::Down,
        Home => VimKey::Home,
        End => VimKey::End,
        PageUp => VimKey::PageUp,
        PageDown => VimKey::PageDown,
        F1 => VimKey::Function(1),
        F2 => VimKey::Function(2),
        F3 => VimKey::Function(3),
        F4 => VimKey::Function(4),
        F5 => VimKey::Function(5),
        F6 => VimKey::Function(6),
        F7 => VimKey::Function(7),
        F8 => VimKey::Function(8),
        F9 => VimKey::Function(9),
        F10 => VimKey::Function(10),
        F11 => VimKey::Function(11),
        F12 => VimKey::Function(12),
        _ => return None,
    })
}
