//! Undo behavior on the web open path.
//!
//! The web client has no filesystem, so it opens files through [`WebEditor::load_file`]
//! (→ `Editor::load_str`) rather than `:e`. These drive the real `WebEditor` exactly
//! as `web/index.html` does — load text, feed vim keys, read the buffer back.

use nxvim_web::WebEditor;

const FILE: &str = "package main\n\nfunc main() {}\n";

/// Opening a file then undoing the first edit must return to the *file*, not to an
/// empty buffer. Regression: `load_str` replaced the throwaway buffer's text without
/// re-baselining its undo tree, so the tree's root was still the empty `[No Name]`
/// state and the first `u` reverted the whole file away.
#[test]
fn undo_after_first_edit_restores_loaded_file() {
    let mut ed = WebEditor::new(80, 24);
    // `WebEditor::new` installs a wasm-bindgen panic hook that, on a non-wasm host,
    // aborts (it forwards to an imported JS `console.error`). Drop it back to the
    // default hook so a failing assertion unwinds and reports normally here.
    let _ = std::panic::take_hook();

    ed.load_file("greeter.go", FILE);
    assert_eq!(ed.buffer_text(), FILE, "file should load verbatim");

    // First edit: delete the top line.
    ed.input("dd");
    assert_eq!(
        ed.buffer_text(),
        "\nfunc main() {}\n",
        "dd removes the first line"
    );

    // Undo: must restore the loaded file, not clear the buffer.
    ed.input("u");
    assert_eq!(
        ed.buffer_text(),
        FILE,
        "u should undo the dd back to the loaded file"
    );
}
