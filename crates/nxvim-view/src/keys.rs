//! Input key-notation encoding, shared by every client.
//!
//! Each frontend decodes its own native key events (crossterm for the TUI, winit
//! for a GUI), maps them to the toolkit-neutral [`Key`] enum, and calls
//! [`notation`] to get the vim key-notation string the server's `nx_input`
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
/// `shift` is notated **only for named keys** (`<S-Tab>`): on a printable character
/// the platform layout has already folded shift into the character itself (`A` for
/// Shift+a), so it adds no `S-` there. This mirrors core's `key_to_notation`, and is
/// what lets Shift+Tab reach the server as `<S-Tab>` rather than a bare `<Tab>`.
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
                format!("<{prefix}{c}>")
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
/// `nx_input` (and the server does one redraw) instead of one notification —
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
pub fn encode_paste(text: &str) -> String {
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
