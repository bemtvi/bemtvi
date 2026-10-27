//! A single-file editor: `cargo run -p bemtvi-gui --example single_file [file]`.
//!
//! The editor an app embeds to edit one given file (`examples/single-file/sample.txt`
//! unless you name another), started with `SessionOptions::single_file`: locked to the
//! file, no Lua or processes, and user commands only from its config,
//! `examples/single-file`, whose `init.lua` says what to try. Its shada is kept apart
//! from your own bemtvi's. `:q` closes the window; so does the close button, unless the
//! file has unsaved changes (`:qa` refuses, E37).

use std::path::{Path, PathBuf};

use bemtvi_gui::embed::{Config, Editor, EditorHandle, SessionOptions, Shada};
use bemtvi_gui::Session;
use mitsuami::prelude::*;

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/single-file")
}

fn main() -> anyhow::Result<()> {
    let file = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| example_dir().join("sample.txt"));
    let title = file
        .file_name()
        .map_or_else(|| "Editor".to_owned(), |n| n.to_string_lossy().into_owned());
    let session = SessionOptions::single_file(file.to_string_lossy())
        .config(Config::Dir(example_dir()))
        .shada(Shada::Namespace("bemtvi-single-file-example".into()))
        .spawn()?;
    let editor = EditorHandle::new();
    let ui_editor = editor.clone();
    mitsuami::App::new()
        .id("bemtvi-single-file-example")
        .name("Single file")
        .open(move || window(ui_editor, session, title))
        .run();
    // Let the editor's exit sequence (its shada) land before the process ends.
    if editor.close() {
        eprintln!("single_file: an editor server thread panicked");
        std::process::exit(101);
    }
    Ok(())
}

fn window(editor: EditorHandle, session: Session, title: String) -> Window {
    let open = signal(true);
    let closing = editor.clone();
    let session = std::cell::Cell::new(Some(session));
    Window::new(title)
        .size(Size::new(800.0, 560.0))
        .open(open)
        .on_close_request(move || {
            // `:qa` closes the window through `on_exit`, or refuses (E37) and it stays.
            if closing.request_quit().is_err() {
                open.set(false);
            }
        })
        .content(move || {
            let session = session.take().expect("the window's content is built once");
            Column::new().size(100.pct(), 100.pct()).child(
                Editor::new(session, move || open.set(false))
                    .handle(editor.clone())
                    .native_dialogs(false)
                    .autofocus(true),
            )
        })
}
