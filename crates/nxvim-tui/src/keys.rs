//! Crossterm key events → vim key-notation.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Translate a crossterm key event into vim key-notation.
///
/// Public so the crossterm -> vim key-notation contract can be exercised by
/// integration tests in `nxvim-tui/tests/keys.rs`.
pub fn encode_key(ev: KeyEvent) -> Option<String> {
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    let alt = ev.modifiers.contains(KeyModifiers::ALT);

    let mut prefix = String::new();
    if ctrl {
        prefix.push_str("C-");
    }
    if alt {
        prefix.push_str("A-");
    }
    let wrap = |name: &str| format!("<{prefix}{name}>");

    let notation = match ev.code {
        KeyCode::Char(c) => {
            if !prefix.is_empty() {
                format!("<{prefix}{c}>")
            } else if c == '<' {
                "<lt>".to_string()
            } else {
                c.to_string()
            }
        }
        KeyCode::Esc => wrap("Esc"),
        KeyCode::Enter => wrap("CR"),
        KeyCode::Backspace => wrap("BS"),
        KeyCode::Tab => wrap("Tab"),
        KeyCode::Delete => wrap("Del"),
        KeyCode::Left => wrap("Left"),
        KeyCode::Right => wrap("Right"),
        KeyCode::Up => wrap("Up"),
        KeyCode::Down => wrap("Down"),
        KeyCode::Home => wrap("Home"),
        KeyCode::End => wrap("End"),
        KeyCode::PageUp => wrap("PageUp"),
        KeyCode::PageDown => wrap("PageDown"),
        _ => return None,
    };
    Some(notation)
}

/// Encode a bracketed-paste payload as a single vim key-notation string.
///
/// A terminal with bracketed paste enabled delivers an entire paste as one
/// [`Event::Paste`](crossterm::event::Event::Paste) carrying the whole text, so
/// the client forwards it as **one** `nvim_input` (and the server does one
/// redraw) instead of one notification — and one full redraw — per character.
/// That per-character round-trip is what makes an unbracketed paste crawl in
/// visibly.
///
/// The text is the raw clipboard string, so the few characters that are special
/// to the server's notation parser (and to the editor's per-key model) are
/// escaped here: a literal `<` becomes `<lt>` (otherwise it would open a
/// `<...>` form), `\t` becomes `<Tab>`, and a line break becomes `<CR>` so it
/// goes through `KeyCode::Enter` rather than inserting a stray `\n` char. A
/// `\r\n` pair collapses to a single `<CR>`. Everything else passes through
/// verbatim. Public so the contract is exercised by `nxvim-tui/tests/paste.rs`.
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
