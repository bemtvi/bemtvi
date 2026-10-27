//! Embedding bemtvi in a mitsuami app: `cargo run -p bemtvi-mitsuami --example embed`.
//!
//! A notes app: the notes in `examples/embed/notes` down the side, the editor beside
//! them, and a line under it showing what the editor tells the app. The editor runs
//! `examples/embed` as its whole config (its `init.lua` says what to try there), keeps
//! its shada apart from your own bemtvi's, and knows the app as `embed-example`.
//!
//! - Click a note: the app opens it in the editor and gives the editor the focus
//!   (`EditorHandle::open`, `focus`).
//! - Type in a note: "ideas.md changed (unsaved)" (`btv.host.forward`).
//! - `<Space>p` in normal mode: "preview asked for ideas.md" (`btv.host.send`).
//! - File › Save (Ctrl+S, ⌘S on macOS): the app writes the note with `:w`; the editor
//!   never sees the chord.
//! - `:q` quits the editor, and the window closes (`on_exit`). The close button asks the
//!   editor first (`:qa`), which refuses while a note is unsaved.

use std::cell::Cell;
use std::path::{Path, PathBuf};

use bemtvi_mitsuami::Session;
use bemtvi_mitsuami::{Config, Editor, EditorEvent, EditorHandle, SessionOptions, Shada, Value};
use mitsuami::prelude::*;

fn example_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/embed")
}

fn notes() -> Vec<PathBuf> {
    let mut notes: Vec<PathBuf> = std::fs::read_dir(example_dir().join("notes"))
        .expect("examples/embed/notes")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    notes.sort();
    notes
}

fn base_name(path: &str) -> String {
    Path::new(path).file_name().map_or_else(
        || path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

fn field<'a>(payload: &'a Value, key: &str) -> Option<&'a Value> {
    payload
        .as_map()?
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
}

/// The app's line for what the editor told it.
fn describe(event: &EditorEvent) -> String {
    let EditorEvent::Host { name, payload } = event;
    let file = |key| {
        field(payload, key)
            .and_then(Value::as_str)
            .map_or_else(|| "the note".to_owned(), base_name)
    };
    match name.as_str() {
        "changed" if field(payload, "modified") == Some(&Value::from(true)) => {
            format!("{} changed (unsaved)", file("name"))
        }
        "changed" => format!("{} changed", file("name")),
        "wrote" => format!("{} written", file("name")),
        "preview" => format!("preview asked for {}", file("file")),
        other => format!("{other}: {payload}"),
    }
}

fn main() -> anyhow::Result<()> {
    let notes = notes();
    let first = notes.first().expect("a note in examples/embed/notes");
    let session = SessionOptions::new()
        .file(first.to_string_lossy())
        .config(Config::Dir(example_dir()))
        .shada(Shada::Namespace("bemtvi-embed-example".into()))
        .host("embed-example")
        .spawn()?;
    let editor = EditorHandle::new();
    let ui_editor = editor.clone();
    mitsuami::App::new()
        .id("bemtvi-embed-example")
        .name("Notes")
        .open(move || window(ui_editor, session, notes))
        .run();
    // Let the editor's exit sequence (its shada) land before the process ends.
    if editor.close() {
        eprintln!("embed: an editor server thread panicked");
        std::process::exit(101);
    }
    Ok(())
}

fn window(editor: EditorHandle, session: Session, notes: Vec<PathBuf>) -> Window {
    let open = signal(true);
    let status = signal(String::from("Click a note, or type in the editor."));

    // The app's own shortcut, on a chord the editor would otherwise get.
    let saver = editor.clone();
    set_menu(
        MenuBar::new().menu(
            Menu::new("File").item(
                MenuItem::new("Save")
                    .shortcut(Shortcut::primary('s'))
                    .on_select(move || match saver.command("w") {
                        Ok(()) => status.set("saved from the app's menu".into()),
                        Err(e) => status.set(format!("can't save: {e:#}")),
                    }),
            ),
        ),
    );

    let closing = editor.clone();
    let session = Cell::new(Some(session));
    Window::new("Notes")
        .size(Size::new(1000.0, 650.0))
        .open(open)
        .on_close_request(move || {
            // `:qa` closes the window through `on_exit`, or refuses (E37) and it stays.
            if closing.request_quit().is_err() {
                open.set(false);
            }
        })
        .content(move || {
            let session = session.take().expect("the window's content is built once");
            let list: Vec<Button> = notes
                .iter()
                .map(|path| {
                    let (editor, path) = (editor.clone(), path.clone());
                    let name = base_name(&path.to_string_lossy());
                    Button::new(name).on_click(move || {
                        if let Err(e) = editor.open(&path).and_then(|()| editor.focus()) {
                            status.set(format!("can't open it: {e:#}"));
                        }
                    })
                })
                .collect();
            Row::new().size(100.pct(), 100.pct()).children((
                Column::new()
                    .width(200.px())
                    .padding(12.px())
                    .gap(Spacing::Sm)
                    .children(list),
                Column::new().grow(1.0).height(100.pct()).children((
                    Editor::new(session, move || open.set(false))
                        .handle(editor.clone())
                        .on_event(move |event| status.set(describe(&event)))
                        .native_dialogs(false)
                        .autofocus(true),
                    Text::new(move || status.get()).padding(8.px()),
                )),
            ))
        })
}
