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
pub mod clipboard;
pub mod editor;
pub mod encoding;
pub mod extmark;
pub mod fuzzy;
pub mod highlight;
pub mod host;
pub mod input;
pub mod mode;
pub mod options;
pub mod search;
pub mod statusline;
pub mod syntax;
pub mod unicode;
pub mod view;

pub use buffer::{Buffer, BufferEdit, DiskChange, EditBatch};
pub use clipboard::Clipboard;
pub use editor::{
    command_status, language_of_path, BorderStyle, BufferId, CommandStatus, CompleteConfig,
    CompleteKeys, Cursor, Editor, FileChangeAction, FileChangeReason, FileChangelist,
    FileMarkEntry, FloatAnchor, FloatConfig, FloatRelative, GlobalMarkEntry, JumpPos, MenuExtent,
    MenuItem, MenuPlacement, NumberedMark, PendingOpen, PendingQuitAll, PendingSave, PersistState,
    PreviewScroll, PreviewTarget, PromptPos, RegisterEntry, ShadaRequest, TabId, TerminalOp,
    UndoEntry, UndoTreeView, WindowConfigSpec, WindowId,
};
pub use encoding::Encoding;
pub use extmark::{Extmark, ExtmarkStore, DEFAULT_PRIORITY, SEMANTIC_HL_PRIORITY, TS_HL_PRIORITY};
pub use highlight::{parse_color, Highlights, HlDef, Rgb, Style};
pub use host::{DirEntry, FileStat, HostFs, StdHostFs};
pub use input::{key_to_notation, parse_keys, Key, KeyCode, MouseAction, MouseButton, MouseEvent};
pub use mode::Mode;
pub use options::{BufferOptions, Options, WindowOptions};
pub use syntax::{IndentParams, OpenOutcome, Span, SyntaxEngine};
pub use view::{MenuView, PanelView, TabView, View, ViewRect, WindowView};
