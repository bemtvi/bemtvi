//! Core editor model for nxvim.
//!
//! This crate is the rust-native analogue of neovim's editor core (`buffer.c`,
//! `normal.c`, `ops.c`, `option.c`, `undo.c`, `grid.c`). It is intentionally
//! free of any I/O, async, or transport concerns: it is a synchronous,
//! deterministic state machine that turns input keys and ex-commands into
//! mutations of buffer state, and renders that state into a [`Screen`].
//!
//! The async server ([`nxvim-server`]) drives this model; the TUI client
//! ([`nxvim-tui`]) only ever sees the rendered [`Screen`] (serialized over RPC).

pub mod buffer;
pub mod editor;
pub mod input;
pub mod mode;
pub mod screen;

pub use buffer::Buffer;
pub use editor::{Cursor, Editor};
pub use input::{parse_keys, Key, KeyCode};
pub use mode::Mode;
pub use screen::Screen;
