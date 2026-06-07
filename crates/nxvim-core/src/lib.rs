//! Core editor model for nxvim.
//!
//! This crate is the rust-native analogue of neovim's editor core (`buffer.c`,
//! `normal.c`, `ops.c`, `option.c`, `undo.c`, `grid.c`). It is intentionally
//! free of any I/O, async, or transport concerns: it is a synchronous,
//! deterministic state machine that turns input keys and ex-commands into
//! mutations of buffer state, and projects that state into a [`View`].
//!
//! The async server ([`nxvim-server`]) drives this model; the TUI client
//! ([`nxvim-tui`]) only ever sees the [`View`] (serialized over RPC) and lays
//! out its regions with its own widgets.

pub mod buffer;
pub mod editor;
pub mod highlight;
pub mod input;
pub mod mode;
pub mod options;
pub mod search;
pub mod statusline;
pub mod syntax;
pub mod unicode;
pub mod view;

pub use buffer::{Buffer, BufferEdit, EditBatch};
pub use editor::{
    command_status, language_of_path, BorderStyle, BufferId, CommandStatus, Cursor, Editor,
    FloatAnchor, FloatConfig, FloatRelative, TabId, WindowConfigSpec, WindowId,
};
pub use highlight::{parse_color, Highlights, HlDef, Rgb, Style};
pub use input::{parse_keys, Key, KeyCode};
pub use mode::Mode;
pub use options::{BufferOptions, Options, WindowOptions};
pub use syntax::{IndentParams, OpenOutcome, Span, SyntaxEngine};
pub use view::{PanelView, TabView, View, ViewRect, WindowView};
