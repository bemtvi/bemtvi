//! Input key-notation encoding, shared by every client.
//!
//! Each frontend decodes its own native key events (crossterm for the TUI, winit
//! for a GUI), maps them to the toolkit-neutral [`Key`] enum, and calls
//! [`notation`] to get the vim key-notation string the server's `btv_input`
//! expects (`"i"`, `"<Esc>"`, `"<C-w>"`, …). [`encode_paste`] is fully neutral —
//! it turns a bracketed-paste payload into one notation string directly.

/// A toolkit-neutral key press: a character, or one of the named keys the server
/// understands in `<...>` notation. Each frontend maps its native key code onto
/// this; a key with no mapping yields `None` from the frontend and is dropped.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Char(char),
    Esc,
    Enter,
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
    /// A function key `<F1>`..`<F12>` (terminals report up to F12; the notation is
    /// open-ended). The `u8` is the number, so `Function(5)` notates as `<F5>`.
    Function(u8),
}

/// Translate a neutral key press (with its `ctrl`/`alt`/`shift` modifiers) into vim
/// key-notation. A bare character is sent literally — except `<`, which becomes
/// `<lt>` so it can't open a `<...>` form; with a modifier the character is
/// wrapped (`<C-x>`, `<A-x>`, `<C-A-x>`). Named keys are always wrapped, carrying
/// the same modifier prefix.
///
/// `shift` on a **bare** printable character adds no `S-`: the platform layout has
/// already folded it into the character itself (`A` for Shift+a). But a printable
/// with a ctrl/alt modifier *does* carry `S-` (`<C-S-c>`), because there the kitty
/// keyboard protocol reports shift as a separate modifier rather than folding it in —
/// and the letter is lowercased so it matches neovim's case-insensitive modified-key
/// model. Named keys always notate shift as `S-` (`<S-Tab>`). This mirrors core's
/// `key_to_notation`, and is what lets Shift+Tab reach the server as `<S-Tab>` and
/// Ctrl+Shift+c as `<C-S-c>` rather than bare `<Tab>` / `<C-c>`.
pub fn notation(ctrl: bool, alt: bool, shift: bool, key: Key) -> String {
    let mut prefix = String::new();
    if ctrl {
        prefix.push_str("C-");
    }
    if alt {
        prefix.push_str("A-");
    }
    // The named-key prefix additionally carries `S-`; the character arms ignore it.
    let named_prefix = if shift {
        format!("{prefix}S-")
    } else {
        prefix.clone()
    };
    let wrap = |name: &str| format!("<{named_prefix}{name}>");

    match key {
        Key::Char(c) => {
            if !prefix.is_empty() {
                // With a ctrl/alt modifier held, shift is a *distinct* modifier
                // the kitty keyboard protocol reports separately — it is NOT
                // folded into the character the way a bare `Shift+a` → `A` is.
                // Carry it as the explicit `S-` flag (`named_prefix`) and lowercase
                // the letter (the platform upcases `Shift+c` to `C`), matching
                // neovim's model and the server's `parse_special`, so a `<C-S-c>` /
                // `<A-S-c>` mapping actually matches. Dropping it here sent a bare
                // `<C-c>` and the remap could never fire.
                let c = if shift { c.to_ascii_lowercase() } else { c };
                // A literal '>' would terminate the `<...>` form early on the
                // server (its scan ends at the first '>', so `<C->>` falls apart
                // into five literal keys); use the `gt` named escape, which the
                // server resolves back to '>'. A '<' inside the form is
                // unambiguous and stays inline (`<C-<>`), matching core.
                if c == '>' {
                    format!("<{named_prefix}gt>")
                } else {
                    format!("<{named_prefix}{c}>")
                }
            } else if c == '<' {
                "<lt>".to_string()
            } else {
                c.to_string()
            }
        }
        Key::Esc => wrap("Esc"),
        Key::Enter => wrap("CR"),
        Key::Backspace => wrap("BS"),
        Key::Tab => wrap("Tab"),
        Key::Delete => wrap("Del"),
        Key::Left => wrap("Left"),
        Key::Right => wrap("Right"),
        Key::Up => wrap("Up"),
        Key::Down => wrap("Down"),
        Key::Home => wrap("Home"),
        Key::End => wrap("End"),
        Key::PageUp => wrap("PageUp"),
        Key::PageDown => wrap("PageDown"),
        Key::Function(n) => wrap(&format!("F{n}")),
    }
}

/// Encode a bracketed-paste payload as a single vim key-notation string.
///
/// A terminal (or GUI) with bracketed paste delivers an entire paste as one
/// event carrying the whole text, so the client forwards it as **one**
/// `btv_input` (and the server does one redraw) instead of one notification —
/// and one full redraw — per character. That per-character round-trip is what
/// makes an unbracketed paste crawl in visibly.
///
/// The text is the raw clipboard string, so the few characters that are special
/// to the server's notation parser (and to the editor's per-key model) are
/// escaped here: a literal `<` becomes `<lt>` (otherwise it would open a
/// `<...>` form), `\t` becomes `<Tab>`, and a line break becomes `<CR>` so it
/// goes through the Enter path rather than inserting a stray `\n` char. A
/// `\r\n` pair collapses to a single `<CR>`. Everything else passes through
/// verbatim.
///
/// The payload is wrapped in `<PasteStart>` / `<PasteEnd>` — the notation form of
/// the terminal's own bracketed-paste markers. That is what tells the server these
/// keys are *text the user already had* rather than keys they are typing, so insert
/// mode puts them in literally: without it every encoded `<CR>` would pick up an
/// auto-indent that stacks on top of the indentation the pasted line already
/// carries (each line drifting further right), an encoded `<Tab>` would be rewritten
/// by `expandtab`/`softtabstop`, and auto-pairs would double the payload's closers.
/// An empty `text` encodes to the empty string — no brackets, nothing to paste.
///
/// Text that is *not* a paste — an IME commit, say — wants the escaping without the
/// brackets: [`encode_text`].
pub fn encode_paste(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(text.len() + PASTE_START.len() + PASTE_END.len());
    out.push_str(PASTE_START);
    out.push_str(&encode_text(text));
    out.push_str(PASTE_END);
    out
}

/// Encode a run of literal text as vim key-notation — [`encode_paste`] without the
/// bracketed-paste markers.
///
/// This is the escaping half: `<` → `<lt>`, `\t` → `<Tab>`, a line break → `<CR>`
/// (a `\r\n` pair collapsing to one), everything else verbatim. Use it for text
/// that is typed rather than pasted — the GUI's IME commits (dead-key accents,
/// AltGr, CJK composition), which are the user's own keystrokes arriving as one
/// string and should still drive auto-pairs and the rest of insert mode.
pub fn encode_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => out.push_str("<lt>"),
            '\t' => out.push_str("<Tab>"),
            '\n' => out.push_str("<CR>"),
            '\r' => {
                out.push_str("<CR>");
                // Swallow the '\n' of a CRLF pair so it yields one line break.
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// The notation bracket a paste payload opens with — see [`encode_paste`].
const PASTE_START: &str = "<PasteStart>";
/// The notation bracket a paste payload closes with — see [`encode_paste`].
const PASTE_END: &str = "<PasteEnd>";

/// The `btv_input_mouse` modifier string for a mouse event's live modifier state —
/// e.g. Ctrl+Shift → `"CS"`. The server's parser accepts the chars in any order
/// with the `-` separator optional, so concatenation is enough. Drives
/// shift-click (extend the selection) and Ctrl/Alt gestures; each frontend maps
/// its native modifier flags to the three booleans.
pub fn mouse_modifier(ctrl: bool, shift: bool, alt: bool) -> String {
    let mut s = String::new();
    if ctrl {
        s.push('C');
    }
    if shift {
        s.push('S');
    }
    if alt {
        s.push('A');
    }
    s
}
