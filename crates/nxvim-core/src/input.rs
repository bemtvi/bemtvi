//! Input keys and termcode parsing.
//!
//! The server receives input from clients as vim-style key notation strings
//! (e.g. `"i"`, `"<Esc>"`, `"<C-w>"`, `"jjj"`), exactly like neovim's
//! `nvim_input`. [`parse_keys`] turns such a string into a sequence of [`Key`]
//! values that the editor model consumes. The TUI client performs the inverse
//! mapping (crossterm key events -> notation) before sending.

/// A logical key, independent of any terminal encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Tab,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
}

impl Key {
    pub fn new(code: KeyCode) -> Self {
        Key {
            code,
            ctrl: false,
            alt: false,
            shift: false,
        }
    }

    pub fn char(c: char) -> Self {
        Key::new(KeyCode::Char(c))
    }

    pub fn ctrl(c: char) -> Self {
        Key {
            code: KeyCode::Char(c),
            ctrl: true,
            alt: false,
            shift: false,
        }
    }

    /// Returns the bare character if this key is an unmodified (or shift-only)
    /// printable character.
    pub fn as_char(self) -> Option<char> {
        match self.code {
            KeyCode::Char(c) if !self.ctrl && !self.alt => Some(c),
            _ => None,
        }
    }
}

/// Parse a vim-style key-notation string into a list of [`Key`]s.
///
/// Recognizes literal characters and `<...>` special forms with optional
/// `C-`, `S-`, `A-`/`M-` modifier prefixes. Unknown `<...>` names are skipped.
pub fn parse_keys(input: &str) -> Vec<Key> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '>') {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                if let Some(key) = parse_special(&inner) {
                    out.push(key);
                    i += end + 2;
                    continue;
                }
                // `<lt>` is the canonical escape for a literal '<'.
            }
            out.push(Key::char('<'));
            i += 1;
        } else {
            out.push(Key::char(chars[i]));
            i += 1;
        }
    }
    out
}

fn parse_special(inner: &str) -> Option<Key> {
    let mut ctrl = false;
    let mut shift = false;
    let mut alt = false;

    // Split modifier prefixes like `C-`, `S-`, `A-`/`M-`.
    let mut rest = inner;
    loop {
        let bytes = rest.as_bytes();
        if bytes.len() >= 2 && bytes[1] == b'-' {
            match bytes[0].to_ascii_uppercase() {
                b'C' => ctrl = true,
                b'S' => shift = true,
                b'A' | b'M' => alt = true,
                _ => break,
            }
            rest = &rest[2..];
        } else {
            break;
        }
    }

    let code = match rest.to_ascii_lowercase().as_str() {
        "cr" | "enter" | "return" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "bs" | "backspace" => KeyCode::Backspace,
        "del" | "delete" => KeyCode::Delete,
        "space" => KeyCode::Char(' '),
        "lt" => KeyCode::Char('<'),
        "gt" => KeyCode::Char('>'),
        "bar" => KeyCode::Char('|'),
        "bslash" => KeyCode::Char('\\'),
        "nul" => KeyCode::Char('\0'),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" => KeyCode::PageUp,
        "pagedown" => KeyCode::PageDown,
        other => {
            let mut it = other.chars();
            let c = it.next()?;
            if it.next().is_some() {
                return None; // multi-char name we don't recognize
            }
            KeyCode::Char(c)
        }
    };

    Some(Key {
        code,
        ctrl,
        alt,
        shift,
    })
}
