//! Input keys and termcode parsing.
//!
//! The server receives input from clients as vim-style key notation strings
//! (e.g. `"i"`, `"<Esc>"`, `"<C-w>"`, `"jjj"`), exactly like neovim's
//! `nvim_input`. [`parse_keys`] turns such a string into a sequence of [`Key`]
//! values that the editor model consumes. The TUI client performs the inverse
//! mapping (crossterm key events -> notation) before sending.

/// A logical key, independent of any terminal encoding.
///
/// `Hash` is derived so the server's keymap trie can key a node's children by
/// `Key` in a `HashMap` (see `nxvim-server`'s `keymap.rs`). It is the sole
/// concession the user-mapping engine asks of the otherwise mapping-unaware core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub code: KeyCode,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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

/// Render a [`Key`] back into vim key-notation — the inverse of [`parse_keys`]
/// for a single key. A plain printable key is its own character (`f` → `"f"`); a
/// special key or a modified key becomes a `<...>` form (`<Esc>`, `<C-w>`,
/// `<S-Tab>`), the same notation [`parse_keys`] consumes, so the two round-trip.
///
/// This is what `vim.fn.getcharstr` / `vim.on_key` hand to Lua: nxvim has no
/// terminal-byte key encoding, so its notation *is* the key's external form.
pub fn key_to_notation(key: Key) -> String {
    // `(named, base)`: whether the code needs a `<...>` wrapper on its own, and
    // its notation stem.
    let (named, base): (bool, String) = match key.code {
        KeyCode::Char(' ') => (true, "Space".to_string()),
        // A bare '<' is written `<lt>` so the result re-parses to '<' and never
        // opens a spurious special form.
        KeyCode::Char('<') if !key.ctrl && !key.alt => (true, "lt".to_string()),
        KeyCode::Char(c) => (false, c.to_string()),
        KeyCode::Enter => (true, "CR".to_string()),
        KeyCode::Esc => (true, "Esc".to_string()),
        KeyCode::Backspace => (true, "BS".to_string()),
        KeyCode::Tab => (true, "Tab".to_string()),
        KeyCode::Delete => (true, "Del".to_string()),
        KeyCode::Left => (true, "Left".to_string()),
        KeyCode::Right => (true, "Right".to_string()),
        KeyCode::Up => (true, "Up".to_string()),
        KeyCode::Down => (true, "Down".to_string()),
        KeyCode::Home => (true, "Home".to_string()),
        KeyCode::End => (true, "End".to_string()),
        KeyCode::PageUp => (true, "PageUp".to_string()),
        KeyCode::PageDown => (true, "PageDown".to_string()),
    };
    // A plain printable char with no ctrl/alt is bare (`f`, `F`, `5`); shift is
    // already baked into the character, so it adds no `S-` prefix.
    if !named && !key.ctrl && !key.alt {
        return base;
    }
    let mut s = String::from("<");
    if key.ctrl {
        s.push_str("C-");
    }
    if key.alt {
        s.push_str("A-");
    }
    // Shift is only notated for the named keys (`<S-Tab>`); on a printable char it
    // is already reflected in the character itself.
    if key.shift && named {
        s.push_str("S-");
    }
    s.push_str(&base);
    s.push('>');
    s
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
