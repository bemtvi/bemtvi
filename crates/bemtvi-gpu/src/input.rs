//! Input from the host, in the client's own types, and keys → vim key-notation.
//!
//! A host's toolkit reports each key on the editor's surface three ways, and the
//! adapter translates them into a [`InputEvent::Key`]: where the key is on the keyboard
//! ([`KeyCode`]), what it typed through the platform's text input (`text`: Shift,
//! AltGr, Option and dead keys already composed in), and the character it types with
//! no modifiers in the keyboard's layout (`unmodified`). [`encode_input`] maps a press
//! onto the neutral [`bemtvi_view::Key`] and hands it to [`bemtvi_view::notation`] for
//! the `<...>` spelling the server's `btv_input` expects. A key with no editor meaning
//! (a bare modifier, a dead key) yields `None` and is dropped — the same contract the
//! TUI follows.
//!
//! Pointer positions are in **points** over the surface (the surface's own coordinates,
//! before its scale factor); the client scales them to the renderer's pixels.

use bemtvi_view::{encode_text, notation, notation_cmd, Key as VimKey};

/// The modifiers held with a key or a pointer event. `meta` is Command on macOS, the
/// Super/Windows key elsewhere.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

/// Where a key is on the keyboard, as far as the client cares: the keys that type no
/// character, by name, and the rest by the character they type on a US layout (the
/// fallback for a chord's base character when the host can't say what the key types
/// in the user's own layout).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Escape,
    Enter,
    NumpadEnter,
    Backspace,
    Tab,
    Delete,
    Insert,
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Home,
    End,
    PageUp,
    PageDown,
    /// `F1` … `F12` (`F(1)` … `F(12)`); others aren't keys the editor names.
    F(u8),
    /// A key that types this character on a US layout (`KeyA` → `'a'`, `Digit1` → `'1'`).
    Character(char),
    /// Anything else: a bare modifier, a media key, a key the host doesn't name.
    Other,
}

/// A mouse button.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Primary,
    Secondary,
    Middle,
    Other,
}

/// How far a wheel or trackpad scrolled. Positive goes towards the end of the content
/// (later lines, further right), with the user's scrolling direction already applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScrollDelta {
    /// Whole lines (a mouse wheel's notches).
    Lines { x: f32, y: f32 },
    /// Points (a trackpad's smooth scroll).
    Points { x: f32, y: f32 },
}

/// A position over the surface, in points.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

/// Something the user did to the editor's surface.
#[derive(Clone, Debug, PartialEq)]
pub enum InputEvent {
    /// A key went down (`pressed`, a repeat included) or up.
    Key {
        code: KeyCode,
        pressed: bool,
        /// What it typed through the platform's text input, if it typed anything.
        text: Option<String>,
        /// What it types with no modifiers in the keyboard's layout.
        unmodified: Option<char>,
        modifiers: Modifiers,
    },
    /// Text an input method committed (composed accents, CJK, …).
    Text(String),
    /// An input method's composition in progress (empty: cancelled), with its caret
    /// (a char offset), for the client to draw at the cursor.
    Preedit { text: String, cursor: Option<usize> },
    /// An input method taking back typed text: `before` characters before the cursor,
    /// `after` after it (macOS's accents replacing the letter a held key typed).
    DeleteSurrounding { before: usize, after: usize },
    PointerMoved {
        position: Point,
        modifiers: Modifiers,
    },
    Button {
        button: MouseButton,
        pressed: bool,
        position: Point,
        modifiers: Modifiers,
    },
    Scroll {
        delta: ScrollDelta,
        modifiers: Modifiers,
    },
}

/// Encode an input event as vim key-notation, or `None` for one with no editor
/// meaning: a key release, a bare modifier, a dead key, a pointer event.
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
/// - **An input method's commit** ([`InputEvent::Text`]) is typed text too
///   (composed accents, CJK).
/// - **An input method taking back typed text** ([`InputEvent::DeleteSurrounding`])
///   is `<BS>`s before the cursor and `<Del>`s after it.
pub fn encode_input(input: &InputEvent, option_chords: bool) -> Option<String> {
    let (code, text, unmodified, mods) = match input {
        InputEvent::Key {
            code,
            pressed: true,
            text,
            unmodified,
            modifiers,
        } => (*code, text.as_deref(), *unmodified, *modifiers),
        InputEvent::Text(text) => return typed(text),
        InputEvent::DeleteSurrounding { before, after } => {
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
        let base = unmodified.or(match code {
            KeyCode::Character(c) => Some(c),
            _ => None,
        })?;
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
        F(n @ 1..=12) => VimKey::Function(n),
        _ => return None,
    })
}

/// The `btv_input_mouse` button name for a pointer button, or `None` for one the
/// server has no gesture for (back/forward/other). `"left"` carries the
/// focus-follows-click + selection machinery; `"right"` is the `'mousemodel'`
/// branch (extend / popup-setpos); `"middle"` pastes the `"*` register. Mirrors
/// the TUI, which forwards the same three names.
pub fn button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Primary => Some("left"),
        MouseButton::Secondary => Some("right"),
        MouseButton::Middle => Some("middle"),
        MouseButton::Other => None,
    }
}

/// The `btv_input_mouse` modifier string for the live modifier state — the shared
/// [`bemtvi_view::mouse_modifier`] over the held flags. (Shift is a real chord
/// here, unlike typed text where the platform folds it into the character — a mouse
/// event carries no character to fold it into.)
pub fn mouse_modifier(mods: Modifiers) -> String {
    bemtvi_view::mouse_modifier(mods.control, mods.shift, mods.alt)
}
