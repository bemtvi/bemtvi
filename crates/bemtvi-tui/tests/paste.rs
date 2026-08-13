//! Tier 1: a bracketed-paste `Event::Paste(text)` is encoded as a *single* vim
//! key-notation string so the whole paste rides one `btv_input` (one redraw),
//! instead of arriving as one keystroke per character. Black-box, no process:
//! drives the public `encode_paste` the client uses on a paste event.

use bemtvi_view::encode_paste;

/// The payload of an encoded paste, with the `<PasteStart>` / `<PasteEnd>`
/// brackets stripped — so each case below reads as the escaping it is about.
/// Asserts the brackets are there, since they are what puts the server in paste
/// mode (see [`brackets_wrap_the_payload`]).
fn payload(text: &str) -> String {
    let enc = encode_paste(text);
    let inner = enc
        .strip_prefix("<PasteStart>")
        .and_then(|s| s.strip_suffix("<PasteEnd>"))
        .unwrap_or_else(|| panic!("paste is not bracketed: {enc:?}"));
    inner.to_string()
}

#[test]
fn brackets_wrap_the_payload() {
    // The brackets are the signal that these keys are text the user already had,
    // not keys they typed: without them the server auto-indents every encoded
    // `<CR>`, so pasted text that carries its own indentation drifts right.
    assert_eq!(encode_paste("hi"), "<PasteStart>hi<PasteEnd>");
}

#[test]
fn an_empty_paste_encodes_to_nothing() {
    // Nothing to paste — and the client skips an empty notation, so it must not
    // send a pair of bare brackets that opens paste mode and never carries text.
    assert_eq!(encode_paste(""), "");
}

#[test]
fn plain_text_passes_through_verbatim() {
    assert_eq!(payload("hello world"), "hello world");
}

#[test]
fn newlines_become_carriage_returns() {
    // A multi-line paste must split lines via `<CR>` (KeyCode::Enter), not by
    // inserting a literal '\n' char — the editor advances the cursor as if it
    // were a printable on a raw newline, corrupting the line/col.
    assert_eq!(payload("a\nb\nc"), "a<CR>b<CR>c");
}

#[test]
fn crlf_collapses_to_a_single_carriage_return() {
    // Windows / clipboard `\r\n` must not produce two line breaks.
    assert_eq!(payload("a\r\nb"), "a<CR>b");
}

#[test]
fn tabs_become_tab_notation() {
    assert_eq!(payload("a\tb"), "a<Tab>b");
}

#[test]
fn literal_less_than_is_escaped() {
    // Otherwise `<` opens a `<...>` notation form the server would try to parse.
    assert_eq!(payload("a < b <C-x>"), "a <lt> b <lt>C-x>");
}
