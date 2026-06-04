//! Tier 1: a bracketed-paste `Event::Paste(text)` is encoded as a *single* vim
//! key-notation string so the whole paste rides one `nvim_input` (one redraw),
//! instead of arriving as one keystroke per character. Black-box, no process:
//! drives the public `encode_paste` the client uses on a paste event.

use nxvim_tui::encode_paste;

#[test]
fn plain_text_passes_through_verbatim() {
    assert_eq!(encode_paste("hello world"), "hello world");
}

#[test]
fn newlines_become_carriage_returns() {
    // A multi-line paste must split lines via `<CR>` (KeyCode::Enter), not by
    // inserting a literal '\n' char — the editor advances the cursor as if it
    // were a printable on a raw newline, corrupting the line/col.
    assert_eq!(encode_paste("a\nb\nc"), "a<CR>b<CR>c");
}

#[test]
fn crlf_collapses_to_a_single_carriage_return() {
    // Windows / clipboard `\r\n` must not produce two line breaks.
    assert_eq!(encode_paste("a\r\nb"), "a<CR>b");
}

#[test]
fn tabs_become_tab_notation() {
    assert_eq!(encode_paste("a\tb"), "a<Tab>b");
}

#[test]
fn literal_less_than_is_escaped() {
    // Otherwise `<` opens a `<...>` notation form the server would try to parse.
    assert_eq!(encode_paste("a < b <C-x>"), "a <lt> b <lt>C-x>");
}
