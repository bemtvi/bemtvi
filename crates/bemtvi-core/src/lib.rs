//! Core editor model for bemtvi.
//!
//! This crate is the rust-native analogue of neovim's editor core (`buffer.c`,
//! `normal.c`, `ops.c`, `option.c`, `undo.c`, `grid.c`). It is intentionally
//! free of any I/O, async, or transport concerns: it is a synchronous,
//! deterministic state machine that turns input keys and ex-commands into
//! mutations of buffer state, and projects that state into a [`View`].
//!
//! The async server ([`bemtvi-server`]) drives this model; the TUI client
//! ([`bemtvi-tui`]) only ever sees the [`View`] (serialized over RPC) and lays
//! out its regions with its own widgets.

pub mod buffer;
pub mod clipboard;
pub mod editor;
pub mod encoding;
pub mod extmark;
pub mod fuzzy;
pub mod glob;
pub mod highlight;
pub mod host;
pub mod indent;
pub mod input;
pub mod markdown;
pub mod mode;
pub mod options;
pub mod sandbox;
pub mod search;
pub mod snippet;
pub mod statusline;
pub mod stdpath;
pub mod syntax;
pub mod unicode;
pub mod view;

pub use buffer::{dir_listing, Buffer, BufferEdit, BufferKind, DiskChange, EditBatch};
pub use clipboard::Clipboard;
pub use editor::{
    command_pending_after, command_status, known_filetypes, language_of_help_doc, language_of_path,
    place_aligned, resolve_language, shebang_filetype, AcceptBehavior, Align, BorderStyle,
    BufferId, ClickSurface, CmdlineCandidate, CmdlineCompleteReq, CommandContinuation,
    CommandPending, CommandStatus, CommitOutcome, CompleteConfig, CompleteKeys, Cursor, DecorScope,
    DecorViewport, DeferredCmd, DocsSection, Editor, Extent, FileChangeAction, FileChangeReason,
    FileChangelist, FileFolds, FileMarkEntry, FilterSeed, FloatAnchor, FloatConfig, FloatRelative,
    GlobalMarkEntry, InputHistoryEntry, JumpPos, LocListEntry, Margin, MenuGeom, MenuItem,
    MenuMetrics, MenuPlacement, MouseClick, MousePos, NumberedMark, PendingOpen, PendingQuitAll,
    PendingSave, PendingSnapshot, PersistState, PickerRun, PluginEntry, PluginNamespace, PreWrite,
    PreviewScroll, PreviewTarget, PromptField, PromptPos, QfAction, QfEntry, QfList, QfStack,
    QfWhich, RegisterEntry, RowLayout, SessionDock, SessionState, SessionTab, SessionWindow,
    ShadaRequest, StatuslineClick, TabId, TerminalOp, UndoEntry, UndoTreeView, WheelGesture,
    WindowConfigSpec, WindowId, WriteEvent, WriteScope, SIGNATURE_MARKER, SIGNATURE_MARKER_COL,
    SIGNATURE_PARAM_INDENT,
};
pub use encoding::Encoding;
pub use extmark::{
    Extmark, ExtmarkStore, HlMode, VirtChunk, VirtDecor, VirtLineRows, VirtTextPos,
    DEFAULT_PRIORITY, SEMANTIC_HL_PRIORITY, SPECIAL_KEY_PRIORITY, TS_HL_PRIORITY,
};
pub use highlight::{parse_color, Highlights, HlDef, Rgb, Style, WinHl};
pub use host::{DirEntry, FileStat, HostFs, StdHostFs};
pub use input::{
    key_to_notation, parse_keys, parse_keys_raw, replace_termcodes, Key, KeyCode, MouseAction,
    MouseButton, MouseEvent, MouseKind, WheelDir,
};
pub use mode::{KeyContext, Mode};
pub use options::{
    effective_history_scope, BufferOptions, HistoryScope, Options, Padding, SignColumn,
    WindowOptions,
};
pub use snippet::{parse_snippet, ParsedSnippet, SnippetError, TabStop};
pub use syntax::{
    parse_query_inherits, query_extends, FoldRange, IndentParams, OpenOutcome, Span, SyntaxEngine,
    ENGINE_QUERY_NAMES,
};
pub use view::{ContentFloatView, MenuView, TabView, View, ViewRect, WindowView};
