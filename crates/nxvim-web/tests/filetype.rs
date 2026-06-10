//! Filetype projection on the web open path.
//!
//! The serverless web build has no Lua and no in-process treesitter engine — it
//! highlights in the browser (`web/highlight.js`) from the view's per-window
//! `filetype`. So the contract this build must uphold is: the window JSON carries
//! the buffer's *effective* treesitter language (override or extension), which is
//! what lets `:set filetype=…` highlight a buffer whose name the extension table
//! misses. These drive the real `WebEditor` exactly as `web/index.html` does.

use nxvim_web::WebEditor;
use serde_json::Value;

/// The focused window's `filetype` field in the editor's projected view JSON.
fn filetype(ed: &mut WebEditor) -> String {
    let view: Value = serde_json::from_str(&ed.view_json()).expect("view_json is valid JSON");
    let windows = view["windows"].as_array().expect("windows array");
    let w = windows
        .iter()
        .find(|w| w["focused"].as_bool() == Some(true))
        .or_else(|| windows.first())
        .expect("at least one window");
    w["filetype"].as_str().unwrap_or("").to_string()
}

#[test]
fn set_filetype_projects_the_override_into_the_view() {
    let mut ed = WebEditor::new(80, 24);
    // `WebEditor::new` installs a wasm panic hook that aborts on a host target;
    // drop it so a failing assertion unwinds and reports normally.
    let _ = std::panic::take_hook();

    // A `.txt` buffer: the extension table misses it, so no filetype to start.
    ed.load_file("notes.txt", "fn main() {}\n");
    assert_eq!(filetype(&mut ed), "", "a .txt buffer has no filetype by default");

    // `:set filetype=rust` forces the language; the view now carries it, so the
    // browser highlighter picks the rust grammar for this otherwise-bare buffer.
    ed.command("set filetype=rust");
    assert_eq!(
        filetype(&mut ed),
        "rust",
        ":set filetype=rust must project `rust` into the window view"
    );

    // `:set filetype&` resets to the extension default — none, for `.txt`.
    ed.command("set filetype&");
    assert_eq!(
        filetype(&mut ed),
        "",
        ":set filetype& must clear the override back to the .txt default"
    );
}

#[test]
fn known_extension_projects_its_language_without_an_override() {
    let mut ed = WebEditor::new(80, 24);
    let _ = std::panic::take_hook();

    // No override: the effective filetype comes from the extension table, so a
    // `.rs` file projects `rust` and the browser highlights it with no `:set`.
    ed.load_file("main.rs", "fn main() {}\n");
    assert_eq!(
        filetype(&mut ed),
        "rust",
        "a .rs buffer projects its extension-derived language"
    );

    // `:set ft=` (empty) is the explicit off switch even for a known extension.
    ed.command("set filetype=");
    assert_eq!(
        filetype(&mut ed),
        "",
        ":set ft= must darken even a recognized extension"
    );
}
