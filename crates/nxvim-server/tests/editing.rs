//! Behavior tests for nxvim, driven the way a real client drives it.
//!
//! These are deliberately *black box*: every test starts a real server on its
//! own thread, connects over the same msgpack-RPC a UI uses, sends vim
//! key-notation via `nvim_input`, and asserts on observable results — buffer
//! contents (`nvim_buf_get_lines`), the bytes written to disk, or the rendered
//! screen. Nothing reaches into the editor's internals. We verify *what the
//! editor does*, not how it's built.
//!
//! The suite is split into per-concern submodules; [`support`] holds the shared
//! harness re-exports and the editing-specific fixtures they lean on. (This file
//! is an integration-test crate root, so each submodule needs an explicit
//! `#[path]` into the `editing/` directory.)

#[path = "editing/support.rs"]
mod support;

#[path = "editing/changelist.rs"]
mod changelist;
#[path = "editing/clipboard.rs"]
mod clipboard;
#[path = "editing/core_editing.rs"]
mod core_editing;
#[path = "editing/dot_repeat.rs"]
mod dot_repeat;
#[path = "editing/encoding.rs"]
mod encoding;
#[path = "editing/ex_substitute.rs"]
mod ex_substitute;
#[path = "editing/explorer.rs"]
mod explorer;
#[path = "editing/global_cmd.rs"]
mod global_cmd;
#[path = "editing/highlights.rs"]
mod highlights;
#[path = "editing/jumplist.rs"]
mod jumplist;
#[path = "editing/listings.rs"]
mod listings;
#[path = "editing/lua_surface.rs"]
mod lua_surface;
#[path = "editing/marks.rs"]
mod marks;
#[path = "editing/multicursor.rs"]
mod multicursor;
#[path = "editing/numbers.rs"]
mod numbers;
#[path = "editing/registers.rs"]
mod registers;
#[path = "editing/rendering.rs"]
mod rendering;
#[path = "editing/search.rs"]
mod search;
#[path = "editing/statusline.rs"]
mod statusline;
#[path = "editing/text_objects.rs"]
mod text_objects;
