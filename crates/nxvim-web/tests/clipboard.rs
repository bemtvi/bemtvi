//! The clipboard register (`"+`/`"*`) on the web client.
//!
//! The browser's clipboard API is async, so the web build bridges the core's
//! synchronous `Clipboard` through a cache: the JS layer pushes yanks out via
//! [`WebEditor::take_clipboard_write`] and seeds external copies in via
//! [`WebEditor::set_clipboard_text`]. These tests drive that cache directly,
//! standing in for the `navigator.clipboard` round-trip the page performs.

use nxvim_web::WebEditor;

/// `WebEditor::new` installs a wasm-bindgen panic hook that aborts off-wasm; drop it
/// back to the default so a failing assertion unwinds and reports normally.
fn web(w: usize, h: usize) -> WebEditor {
    let ed = WebEditor::new(w, h);
    let _ = std::panic::take_hook();
    ed
}

/// A charwise copy from another app (no trailing newline) pastes inline with `"+p`.
#[test]
fn paste_charwise_from_system_clipboard() {
    let mut ed = web(80, 24);
    ed.set_clipboard_text("hello"); // as if navigator.clipboard.readText() resolved
    ed.input("\"+p");
    assert_eq!(ed.buffer_text(), "hello\n");
}

/// A linewise copy (trailing newline) pastes as a whole new line with `"+p`.
#[test]
fn paste_linewise_from_system_clipboard() {
    let mut ed = web(80, 24);
    ed.set_clipboard_text("a line\n");
    ed.input("\"+p");
    assert_eq!(ed.buffer_text(), "\na line\n");
}

/// Yanking to `"+` stages the text for the JS layer to push to the system clipboard,
/// and round-trips back through `"+p`.
#[test]
fn yank_to_clipboard_then_paste() {
    let mut ed = web(80, 24);
    ed.load_file("note.txt", "foo\nbar\n");
    ed.input("\"+yy");

    // The page would push this to navigator.clipboard.writeText; linewise carries
    // the trailing newline.
    assert_eq!(ed.take_clipboard_write().as_deref(), Some("foo\n"));
    // Draining it leaves nothing pending for the next keystroke.
    assert_eq!(ed.take_clipboard_write(), None);

    // The same cache still serves a paste: "+p drops the yanked line below the cursor.
    ed.input("G\"+p");
    assert_eq!(ed.buffer_text(), "foo\nbar\nfoo\n");
}
