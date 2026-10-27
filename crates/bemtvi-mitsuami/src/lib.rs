//! bemtvi in a mitsuami application: the editor as a view an app puts in its own
//! layout, the way it puts a `TextArea` there.
//!
//! [`Editor`] is the whole editor (core, Lua, config, plugins, LSP, the edit-host split)
//! on a [`Session`] the app started, drawn on a `GpuSurface` that fills what the layout
//! gives it. [`EditorHandle`] is the app's way in: quitting it, and the threads behind
//! it. `bemtvi-gui`'s own window is one of these, full size.
//!
//! ```ignore
//! let editor = EditorHandle::new();
//! let session = SessionOptions::new()
//!     .file("notes.md")
//!     .config(Config::Dir(app_config_dir))
//!     .shada(Shada::Namespace("myapp".into()))
//!     .spawn()?;
//! Row::new().children((
//!     sidebar(),
//!     Editor::new(session, move || tabs.close_current()).handle(editor.clone()),
//! ))
//! ```
//!
//! The editor's threads (its server, and the client IO thread that talks to it) start
//! as it's mounted and are told to stop as it's unmounted; [`EditorHandle::close`] waits
//! for them, so an app that wants the editor's exit sequence (its shada) to land calls
//! it before the process ends.
//!
//! Underneath it's `bemtvi-gpu`'s toolkit-free client: this crate shows its surface in
//! mitsuami, translates mitsuami's input into the client's, and carries out what the
//! client asks for (repaints, the pointer's shape, the input method's caret, the
//! backdrop, file dialogs, the clipboard).

mod app;
mod editor;
mod input;

pub use editor::{Editor, EditorEvent, EditorHandle};

// How a session starts, from the client: re-exported so an app needs this crate only.
pub use bemtvi_gpu::remote::ConnectTarget;
pub use bemtvi_gpu::{
    Config, GlyphOverflow, GuiConfig, Session, SessionOptions, Shada,
    SINGLE_FILE_DISABLED_COMMANDS, SINGLE_FILE_DISABLED_PICKERS,
};
pub use bemtvi_server::ConfigSource;
pub use rmpv::Value;

// mitsuami's input in the client's types, and the key and mouse helpers over mitsuami's
// types this crate's tests drive end to end.
pub use bemtvi_gpu::mouse::{cell_at, drain_notches, horizontal_action, vertical_action};
pub use bemtvi_gpu::{
    dialog_action, open_dialog_verb, open_path_command, save_dialog_needed, DialogAction,
};
pub use bemtvi_view::{encode_paste, encode_text};
pub use input::{button_name, encode_input, input_event, is_paste, mouse_modifier};
