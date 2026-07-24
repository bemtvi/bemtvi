//! Input keys and termcode parsing.
//!
//! The server receives input from clients as vim-style key notation strings
//! (e.g. `"i"`, `"<Esc>"`, `"<C-w>"`, `"jjj"`), exactly like neovim's
//! `nx_input`. [`parse_keys`] turns such a string into a sequence of [`Key`]
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
    /// A function key `<F1>`..`<F12>` (and beyond — terminals/`crossterm` report up to
    /// `F12`, but the notation is open-ended). Carried as a mappable key like the named
    /// keys above so a plugin can bind `<F5>` and a client (TUI/GUI) can deliver it.
    Function(u8),
    /// A mouse gesture as a mappable key — vim's `<LeftMouse>` / `<2-LeftMouse>` /
    /// `<RightMouse>` / `<MiddleMouse>` (press), `<LeftDrag>` / `<RightDrag>` /
    /// `<MiddleDrag>` (drag), and `<LeftRelease>` / … (release). `kind` is which phase
    /// of the gesture; `clicks` is the multi-click count for a *press* (1 single, 2
    /// double, …) and always 1 for a drag / release. These never reach `Editor::input`
    /// as text — the server resolves the gesture against the keymaps (firing a bound
    /// mapping or falling back to the default gesture) and only the *notation* round
    /// trip touches the core. `button` is always one of `Left`/`Right`/`Middle`.
    Mouse {
        button: MouseButton,
        clicks: u8,
        kind: MouseKind,
    },
    /// A scroll-wheel notch as a mappable key — vim's `<ScrollWheelUp>` /
    /// `<ScrollWheelDown>` / `<ScrollWheelLeft>` / `<ScrollWheelRight>` (with optional
    /// modifiers). Like [`KeyCode::Mouse`], the server resolves it against the keymaps
    /// (firing a bound mapping or falling back to the default scroll); only the
    /// notation round trip touches the core.
    ScrollWheel(WheelDir),
}

/// Which phase of a mouse gesture a mappable [`KeyCode::Mouse`] key is: a button
/// `Press` (`<LeftMouse>`), a `Drag` while held (`<LeftDrag>`), or a `Release`
/// (`<LeftRelease>`). The `kind` is part of the key's identity, so `<LeftMouse>`,
/// `<LeftDrag>`, and `<LeftRelease>` are three distinct, separately-mappable keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseKind {
    Press,
    Drag,
    Release,
}

/// A scroll-wheel direction — the notch a [`KeyCode::ScrollWheel`] key (and the
/// `<ScrollWheel*>` notation) names. `Up`/`Down` scroll the buffer vertically,
/// `Left`/`Right` horizontally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WheelDir {
    Up,
    Down,
    Left,
    Right,
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

/// Which physical mouse control a [`MouseEvent`] came from — the `button`
/// argument of neovim's `nx_input_mouse`. `Wheel` is the scroll wheel (its
/// direction lives in [`MouseAction`]); `Move` is a bare pointer move
/// (`'mousemoveevent'`); `X1`/`X2` are the back/forward thumb buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Wheel,
    Move,
    X1,
    X2,
}

/// What a mouse button did — the `action` argument of `nx_input_mouse`. For an
/// ordinary button this is press / drag / release; for [`MouseButton::Wheel`] it
/// is a scroll *direction* (`WheelUp`/`Down`/`Left`/`Right`); for
/// [`MouseButton::Move`] it is [`MouseAction::MoveTo`].
///
/// Note neovim's deliberate API naming: a wheel `action = "up"` means *scroll the
/// content toward the top of the buffer* — we model the observable behavior, so
/// [`MouseAction::WheelUp`] scrolls up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MouseAction {
    Press,
    Drag,
    Release,
    MoveTo,
    WheelUp,
    WheelDown,
    WheelLeft,
    WheelRight,
}

/// A single mouse gesture at a screen cell — nxvim's analogue of a parsed
/// [`Key`], fed to `Editor::mouse`. Coordinates are **global** zero-based screen
/// cells (`grid 0`, the same space redraw events use); the editor owns the
/// hit-test from a cell back to a window and buffer position, so every front end
/// (TUI now, GUI later) only has to forward the raw cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MouseEvent {
    pub button: MouseButton,
    pub action: MouseAction,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    /// Global screen row (0-based).
    pub row: usize,
    /// Global screen column (0-based).
    pub col: usize,
    /// Server-stamped receive time in milliseconds, from a monotonic clock the
    /// server owns. [`MouseEvent::parse`] leaves this `0` — the wire call carries
    /// no timestamp; the server fills it in right after parsing so the editor's
    /// multi-click detection (`'mousetime'`) compares pure deltas without reading
    /// any clock itself, keeping `nxvim-core` time-free and deterministic in tests.
    pub stamp_ms: u64,
}

impl MouseEvent {
    /// Parse the `nx_input_mouse(button, action, modifier, row, col)` arguments
    /// into a [`MouseEvent`]. An unknown `button`, or an `action` that doesn't fit
    /// the button (e.g. `"press"` on the wheel, or `"up"` on the left button), is
    /// an `Err` naming the offending value — a malformed RPC call fails loud at the
    /// boundary rather than being silently coerced. `grid` is not taken: nxvim is
    /// single-grid, so the editor always hit-tests the global cell itself.
    pub fn parse(
        button: &str,
        action: &str,
        modifier: &str,
        row: usize,
        col: usize,
    ) -> Result<MouseEvent, String> {
        let button = match button {
            "left" => MouseButton::Left,
            "right" => MouseButton::Right,
            "middle" => MouseButton::Middle,
            "wheel" => MouseButton::Wheel,
            "move" => MouseButton::Move,
            "x1" => MouseButton::X1,
            "x2" => MouseButton::X2,
            other => return Err(format!("nx_input_mouse: unknown button {other:?}")),
        };
        let action = match button {
            MouseButton::Wheel => match action {
                "up" => MouseAction::WheelUp,
                "down" => MouseAction::WheelDown,
                "left" => MouseAction::WheelLeft,
                "right" => MouseAction::WheelRight,
                other => {
                    return Err(format!(
                        "nx_input_mouse: wheel action must be up/down/left/right, got {other:?}"
                    ))
                }
            },
            // A bare move ignores its action string (neovim does the same).
            MouseButton::Move => MouseAction::MoveTo,
            _ => match action {
                "press" => MouseAction::Press,
                "drag" => MouseAction::Drag,
                "release" => MouseAction::Release,
                other => {
                    return Err(format!(
                        "nx_input_mouse: button action must be press/drag/release, got {other:?}"
                    ))
                }
            },
        };
        let (ctrl, alt, shift) = parse_mouse_modifier(modifier)?;
        Ok(MouseEvent {
            button,
            action,
            ctrl,
            alt,
            shift,
            row,
            col,
            // The server stamps this from its clock immediately after parsing.
            stamp_ms: 0,
        })
    }
}

/// Parse the `modifier` argument of `nx_input_mouse` — a run of modifier chars
/// with the `-` separator optional, so `"C-S"`, `"cs"`, and `"CS"` all mean
/// Ctrl+Shift. Returns `(ctrl, alt, shift)`. An unrecognized char is an `Err`
/// (fail loud), matching the rest of [`MouseEvent::parse`].
fn parse_mouse_modifier(modifier: &str) -> Result<(bool, bool, bool), String> {
    let (mut ctrl, mut alt, mut shift) = (false, false, false);
    for c in modifier.chars() {
        match c.to_ascii_uppercase() {
            'C' => ctrl = true,
            'A' | 'M' | 'D' => alt = true,
            'S' => shift = true,
            '-' => {} // optional separator
            other => return Err(format!("nx_input_mouse: unknown modifier {other:?}")),
        }
    }
    Ok((ctrl, alt, shift))
}

/// Parse a vim-style key-notation string into a list of [`Key`]s.
///
/// Recognizes literal characters and `<...>` special forms with optional
/// `C-`, `S-`, `A-`/`M-` modifier prefixes. Unknown `<...>` names are skipped.
///
/// Folds the four Ctrl-chords that share a byte with a named terminal key onto that
/// key (`<C-i>`→`<Tab>`, `<C-m>`→`<CR>`, `<C-[>`→`<Esc>`, `<C-h>`→`<BS>`; see
/// [`canonical_ctrl_alias`]) — the right default for a terminal that cannot tell the
/// two spellings apart. A caller that KNOWS the terminal disambiguates them (the
/// kitty keyboard protocol is active) parses with [`parse_keys_raw`] instead, so a
/// distinct `<C-i>` is preserved.
pub fn parse_keys(input: &str) -> Vec<Key> {
    parse_keys_inner(input, true)
}

/// [`parse_keys`] WITHOUT the terminal-alias fold: `<C-i>` stays a distinct
/// Ctrl-`i`, not `<Tab>`, and likewise for `<C-m>`/`<C-[>`/`<C-h>`. Used at the two
/// boundaries that see real terminal semantics — the keymap LHS and the live input
/// stream — when the client has the kitty keyboard protocol on and so CAN deliver
/// those chords apart from their named twins. Everywhere else (feedkeys, mapping
/// RHS, the command-grammar lookahead) keeps [`parse_keys`]'s legacy fold.
pub fn parse_keys_raw(input: &str) -> Vec<Key> {
    parse_keys_inner(input, false)
}

fn parse_keys_inner(input: &str, fold_aliases: bool) -> Vec<Key> {
    let chars: Vec<char> = input.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '<' {
            if let Some(end) = chars[i + 1..].iter().position(|&c| c == '>') {
                let inner: String = chars[i + 1..i + 1 + end].iter().collect();
                if let Some(key) = parse_special(&inner) {
                    // The fold collapses `<C-i>`→`<Tab>` &c.; the raw path keeps
                    // them distinct (protocol-on terminals). A raw *control byte*
                    // (below) already arrives as its named key, so it needs neither.
                    out.push(if fold_aliases {
                        canonical_ctrl_alias(key)
                    } else {
                        key
                    });
                    i += end + 2;
                    continue;
                }
                // `<lt>` is the canonical escape for a literal '<'.
            }
            out.push(Key::char('<'));
            i += 1;
        } else {
            // A raw control byte (0x00–0x1f) that `nvim_replace_termcodes` emitted
            // is decoded back into its key event, so a feed string built with
            // `replace_termcodes` round-trips through `nvim_feedkeys`.
            out.push(key_from_control_byte(chars[i]).unwrap_or_else(|| Key::char(chars[i])));
            i += 1;
        }
    }
    out
}

/// Decode a raw ASCII control byte (as emitted by [`replace_termcodes`], and by
/// neovim's `nvim_replace_termcodes`) back into its [`Key`]. Returns `None` for a
/// non-control character, which the caller keeps as a literal `Key::char`.
///
/// The named terminal keys take precedence over their Ctrl-letter alias at the
/// same byte (`\r` → `<CR>` not `<C-m>`, `\t` → `<Tab>`, `\x1b` → `<Esc>`,
/// `\x08` → `<BS>`), matching how vim canonicalizes them.
fn key_from_control_byte(c: char) -> Option<Key> {
    let b = c as u32;
    let plain = |code| Key::new(code);
    // `byte ^ 0x40` recovers the ASCII letter/symbol of a Ctrl-chord (0x0f → 'O').
    let ctrl = |b: u32| char::from_u32(b ^ 0x40).map(|ch| Key::ctrl(ch.to_ascii_lowercase()));
    match b {
        0x0d => Some(plain(KeyCode::Enter)),
        0x1b => Some(plain(KeyCode::Esc)),
        0x09 => Some(plain(KeyCode::Tab)),
        0x08 => Some(plain(KeyCode::Backspace)),
        // Ctrl-@ / Ctrl-A..Z / Ctrl-\ Ctrl-] Ctrl-^ Ctrl-_ (minus the named keys above).
        0x00..=0x1f => ctrl(b),
        _ => None,
    }
}

/// Translate vim key-notation into the terminal-byte encoding neovim's
/// `nvim_replace_termcodes` produces — for the keys that HAVE a single-byte ASCII
/// form: the Ctrl-chords (`<C-o>` → `\x0f`), the named control keys (`<CR>` →
/// `\r`, `<Esc>` → `\x1b`, `<Tab>` → `\t`, `<BS>` → `\x08`), `<Space>`, and
/// `<lt>` → `<`.
///
/// A key with no ASCII byte (`<Del>`, arrows, function keys, mouse, or an Alt-modified
/// chord) has no encoding nxvim can emit — neovim renders those with K_SPECIAL
/// multi-byte sequences nxvim doesn't model — so it is left as its `<...>`
/// notation, which [`parse_keys`] still consumes so the round-trip through
/// `nvim_feedkeys` holds.
pub fn replace_termcodes(input: &str) -> String {
    let mut out = String::new();
    for key in parse_keys(input) {
        match key_to_termcode(key) {
            Some(c) => out.push(c),
            None => out.push_str(&key_to_notation(key)),
        }
    }
    out
}

/// The single ASCII/control byte a [`Key`] encodes to, if it has one; `None` for
/// keys that only have a K_SPECIAL multi-byte form in vim. The inverse of
/// [`key_from_control_byte`] over the bytes both cover.
fn key_to_termcode(key: Key) -> Option<char> {
    // An Alt chord has no single-byte form here (neovim uses ESC-prefix / K_SPECIAL).
    if key.alt {
        return None;
    }
    match key.code {
        KeyCode::Char(c) => {
            if key.ctrl {
                // `toupper(c) ^ 0x40` is vim's CTRL_CHR: 'o'/'O' → 0x0f. Only valid
                // when it lands in the control range (letters and @[\]^_?).
                let u = (c.to_ascii_uppercase() as u32) ^ 0x40;
                (u <= 0x1f || u == 0x7f)
                    .then(|| char::from_u32(u))
                    .flatten()
            } else {
                // Shift is already baked into `c`; a bare printable is itself.
                Some(c)
            }
        }
        // The named keys map to their control byte only in plain form; a modified
        // variant (`<S-Tab>`, `<C-CR>`) is a distinct K_SPECIAL key — keep it as
        // notation.
        _ if key.ctrl || key.shift => None,
        KeyCode::Enter => Some('\r'),
        KeyCode::Esc => Some('\u{1b}'),
        KeyCode::Tab => Some('\t'),
        KeyCode::Backspace => Some('\u{08}'),
        // `<Del>` and the cursor/function keys have no ASCII byte (K_SPECIAL in
        // vim) — keep them as notation so `parse_keys` round-trips them.
        _ => None,
    }
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
        // A *modified* '>' must use its named escape (`<C-gt>`): written inline
        // as `<C->>` the parser would close the form at the FIRST '>', so it
        // could never round-trip. (A bare '>' stays literal, and a modified '<'
        // is fine inline — `<C-<>` parses.)
        KeyCode::Char('>') if key.ctrl || key.alt => (true, "gt".to_string()),
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
        KeyCode::Function(n) => (true, format!("F{n}")),
        KeyCode::Mouse {
            button,
            clicks,
            kind,
        } => {
            let btn = match button {
                MouseButton::Left => "Left",
                MouseButton::Right => "Right",
                MouseButton::Middle => "Middle",
                // Only Left/Right/Middle are ever built into a Mouse key; the wheel /
                // move / thumb buttons are gestures, not mappable keys.
                _ => "Left",
            };
            let name = match kind {
                MouseKind::Press => format!("{btn}Mouse"),
                MouseKind::Drag => format!("{btn}Drag"),
                MouseKind::Release => format!("{btn}Release"),
            };
            // `<2-LeftMouse>` for a multi-click press; drag / release carry no count.
            if kind == MouseKind::Press && clicks > 1 {
                (true, format!("{clicks}-{name}"))
            } else {
                (true, name)
            }
        }
        KeyCode::ScrollWheel(dir) => {
            let d = match dir {
                WheelDir::Up => "Up",
                WheelDir::Down => "Down",
                WheelDir::Left => "Left",
                WheelDir::Right => "Right",
            };
            (true, format!("ScrollWheel{d}"))
        }
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

/// Parse a mouse-gesture notation stem (after modifier prefixes were stripped) into
/// its `(button, click-count, kind)`: `"leftmouse"` → `(Left, 1, Press)`,
/// `"2-leftmouse"` → `(Left, 2, Press)`, `"rightdrag"` → `(Right, 1, Drag)`,
/// `"middlerelease"` → `(Middle, 1, Release)`. `None` for anything that isn't a
/// `Left`/`Right`/`Middle` mouse name (so a non-mouse `<...>` falls through to the
/// ordinary special-key table). A press's count is capped at 4 (vim escalates no
/// further); a drag / release carries no count, so it is forced to 1.
fn parse_mouse_notation(rest: &str) -> Option<(MouseButton, u8, MouseKind)> {
    let lower = rest.to_ascii_lowercase();
    let (clicks, name): (u8, &str) = match lower.split_once('-') {
        Some((n, name)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => {
            (n.parse().ok()?, name)
        }
        _ => (1, lower.as_str()),
    };
    if clicks == 0 || clicks > 4 {
        return None;
    }
    let (button, kind) = match name {
        "leftmouse" => (MouseButton::Left, MouseKind::Press),
        "rightmouse" => (MouseButton::Right, MouseKind::Press),
        "middlemouse" => (MouseButton::Middle, MouseKind::Press),
        "leftdrag" => (MouseButton::Left, MouseKind::Drag),
        "rightdrag" => (MouseButton::Right, MouseKind::Drag),
        "middledrag" => (MouseButton::Middle, MouseKind::Drag),
        "leftrelease" => (MouseButton::Left, MouseKind::Release),
        "rightrelease" => (MouseButton::Right, MouseKind::Release),
        "middlerelease" => (MouseButton::Middle, MouseKind::Release),
        _ => return None,
    };
    // Only a press carries a multi-click count; a drag / release is always single.
    let clicks = if kind == MouseKind::Press { clicks } else { 1 };
    Some((button, clicks, kind))
}

/// Parse the inside of a `<...>` notation form into a faithful [`Key`] — no
/// terminal-alias fold. [`parse_keys_inner`] applies [`canonical_ctrl_alias`] on top
/// when folding is requested, so `<C-i>` &c. stay distinct here.
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

    // A mouse-button notation (`LeftMouse`, `2-RightMouse`, …), after the generic
    // modifier strip above has peeled any `C-`/`S-`/`A-`. The optional `N-` click
    // count is handled here (the `2` of `2-LeftMouse` isn't a modifier, so the loop
    // left it on `rest`).
    if let Some((button, clicks, kind)) = parse_mouse_notation(rest) {
        return Some(Key {
            code: KeyCode::Mouse {
                button,
                clicks,
                kind,
            },
            ctrl,
            alt,
            shift,
        });
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
        "scrollwheelup" => KeyCode::ScrollWheel(WheelDir::Up),
        "scrollwheeldown" => KeyCode::ScrollWheel(WheelDir::Down),
        "scrollwheelleft" => KeyCode::ScrollWheel(WheelDir::Left),
        "scrollwheelright" => KeyCode::ScrollWheel(WheelDir::Right),
        // Function keys `<F1>`, `<F12>`, … — an `f` followed by a 1+ digit number.
        fk if fk.len() >= 2
            && fk.starts_with('f')
            && fk[1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            KeyCode::Function(fk[1..].parse().ok()?)
        }
        other => {
            // A single character. nxvim follows neovim's model: a *modified* key
            // does not distinguish letter casing (`<A-C>` == `<A-c>`); shift is
            // carried by the `S-` modifier flag (`<A-S-c>`, `<C-S-x>`) — already set
            // by the strip loop above — not by an uppercase letter. So the char is
            // taken from the lowercased match string.
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

/// Fold the four Ctrl-chords that share a byte with a *named* terminal key onto
/// that named key: `<C-i>` → `<Tab>` (0x09), `<C-m>` → `<CR>` (0x0d), `<C-[>` →
/// `<Esc>` (0x1b), `<C-h>` → `<BS>` (0x08). Vim canonicalizes these the same way,
/// and for the same reason: a *legacy* terminal cannot tell the two spellings apart,
/// so they are one key with two names.
///
/// This is applied by [`parse_keys`] (the legacy default) but NOT by
/// [`parse_keys_raw`]: when the kitty keyboard protocol is active a terminal *can*
/// deliver `<C-i>` distinct from `<Tab>`, and the server parses the LHS + live input
/// with the raw variant so the two stay separate keys (vim/neovim do the same once
/// the protocol is on). Without the fold on the legacy path, a client that can only
/// *produce* the named form (crossterm decodes 0x09 as `KeyCode::Tab`, no CONTROL)
/// would leave `vim.keymap.set("n", "<C-i>", …)` a mapping that can never fire — a
/// silent no-op. This is the notation twin of [`key_from_control_byte`], which
/// resolves the same collisions for a raw control byte.
///
/// Only an otherwise-unmodified Ctrl-chord folds: `<C-S-i>` / `<C-A-i>` carry a
/// modifier a terminal *can* distinguish (kitty keyboard protocol), so they stay
/// as they are rather than silently losing the extra modifier.
///
/// One consequence worth knowing: in a terminal buffer, `<C-h>` on the folded path
/// feeds the same bytes as `<BS>` (0x7f, the DEL convention a PTY expects) rather
/// than a literal 0x08. The other three are byte-identical either way.
fn canonical_ctrl_alias(key: Key) -> Key {
    if !key.ctrl || key.alt || key.shift {
        return key;
    }
    let code = match key.code {
        KeyCode::Char('i') => KeyCode::Tab,
        KeyCode::Char('m') => KeyCode::Enter,
        KeyCode::Char('[') => KeyCode::Esc,
        KeyCode::Char('h') => KeyCode::Backspace,
        _ => return key,
    };
    Key::new(code)
}
