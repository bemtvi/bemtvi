//! Behavior tests for bemtvi, driven the way a real client drives it.
//!
//! These are deliberately *black box*: every test starts a real server on its
//! own thread, connects over the same msgpack-RPC a UI uses, sends vim
//! key-notation via `btv_input`, and asserts on observable results — buffer
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

#[path = "editing/foldtext.rs"]
mod foldtext;
#[path = "editing/sandbox_exprs.rs"]
mod sandbox_exprs;
#[path = "editing/sandbox_stateless.rs"]
mod sandbox_stateless;
#[path = "editing/subst_expr.rs"]
mod subst_expr;

#[path = "editing/changelist.rs"]
mod changelist;
#[path = "editing/clipboard.rs"]
mod clipboard;
#[path = "editing/colorcolumn.rs"]
mod colorcolumn;
#[path = "editing/commenting.rs"]
mod commenting;
#[path = "editing/core_editing.rs"]
mod core_editing;
#[path = "editing/dot_repeat.rs"]
mod dot_repeat;
#[path = "editing/encoding.rs"]
mod encoding;
#[path = "editing/endofline.rs"]
mod endofline;
#[path = "editing/ex_move.rs"]
mod ex_move;
#[path = "editing/ex_substitute.rs"]
mod ex_substitute;
#[path = "editing/explorer.rs"]
mod explorer;
#[path = "editing/fileformat.rs"]
mod fileformat;
#[path = "editing/folds.rs"]
mod folds;
#[path = "editing/global_cmd.rs"]
mod global_cmd;
#[path = "editing/helix_actions.rs"]
mod helix_actions;
#[path = "editing/helix_match.rs"]
mod helix_match;
#[path = "editing/helix_motions.rs"]
mod helix_motions;
#[path = "editing/helix_multi.rs"]
mod helix_multi;
#[path = "editing/helix_regex.rs"]
mod helix_regex;
#[path = "editing/helix_search.rs"]
mod helix_search;
#[path = "editing/helix_selections.rs"]
mod helix_selections;
#[path = "editing/helix_verbs.rs"]
mod helix_verbs;
#[path = "editing/highlights.rs"]
mod highlights;
#[path = "editing/indent_pairs.rs"]
mod indent_pairs;
#[path = "editing/jumplist.rs"]
mod jumplist;
#[path = "editing/listings.rs"]
mod listings;
#[path = "editing/lua_surface.rs"]
mod lua_surface;
#[path = "editing/macros.rs"]
mod macros;
#[path = "editing/marks.rs"]
mod marks;
#[path = "editing/multicursor.rs"]
mod multicursor;
#[path = "editing/numbers.rs"]
mod numbers;
#[path = "editing/padding.rs"]
mod padding;
#[path = "editing/paste.rs"]
mod paste;
#[path = "editing/registers.rs"]
mod registers;
#[path = "editing/rendering.rs"]
mod rendering;
#[path = "editing/report.rs"]
mod report;
#[path = "editing/scrolloff.rs"]
mod scrolloff;
#[path = "editing/search.rs"]
mod search;
#[path = "editing/shift.rs"]
mod shift;
#[path = "editing/showcmd.rs"]
mod showcmd;
#[path = "editing/statusline.rs"]
mod statusline;
#[path = "editing/text_objects.rs"]
mod text_objects;
#[path = "editing/undo.rs"]
mod undo;
#[path = "editing/visual_replace.rs"]
mod visual_replace;
